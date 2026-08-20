use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{self, Stream};
use futures_util::StreamExt;
use zbus::fdo::ObjectManagerProxy;
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use crate::call_forward::{
    parse_ussd_reply, ussd_disable, ussd_enable, ussd_query, CallForwardState,
};
use crate::modem::{
    radio_from_access_tech, sim_status, CallForward, IncomingSms, ModemError, ModemInfo, ModemLive,
    ModemState, Registration, SmsInbox, SmsModem,
};
use crate::normalize::normalize_e164;

const MM_DEST: &str = "org.freedesktop.ModemManager1";
const MM_PATH: &str = "/org/freedesktop/ModemManager1";
const MM_MODEM_IFACE: &str = "org.freedesktop.ModemManager1.Modem";
const USSD_TIMEOUT: Duration = Duration::from_secs(45);

/// PDU type 2 is submit (outbound). Deliver is 1.
pub fn sms_is_inbound(pdu_type: u32) -> bool {
    pdu_type != 2
}

fn mm_err(err: impl std::fmt::Display) -> ModemError {
    ModemError::Failed(err.to_string())
}

fn apply_ussd_reply(reply: Result<String, ModemError>) -> Result<(), ModemError> {
    reply.map(|_| ())
}

pub fn delete_already_gone_name(name: &str) -> bool {
    name == "org.freedesktop.DBus.Error.UnknownObject"
        || name == "org.freedesktop.ModemManager1.Error.Core.NotFound"
}

pub fn delete_already_gone(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::FDO(e) => matches!(e.as_ref(), zbus::fdo::Error::UnknownObject(_)),
        zbus::Error::MethodError(name, _, _) => delete_already_gone_name(name.as_str()),
        _ => {
            let s = err.to_string();
            s.contains("UnknownObject")
                || s.contains("org.freedesktop.ModemManager1.Error.Core.NotFound")
                || s.contains("No SMS found")
        }
    }
}

#[proxy(
    interface = "org.freedesktop.ModemManager1.Modem.Messaging",
    default_service = "org.freedesktop.ModemManager1"
)]
trait Messaging {
    fn create(&self, properties: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;

    fn list(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    fn delete(&self, path: &zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn added(&self, path: OwnedObjectPath, received: bool) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.ModemManager1.Sms",
    default_service = "org.freedesktop.ModemManager1"
)]
trait Sms {
    fn send(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn number(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn text(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn pdu_type(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn timestamp(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.freedesktop.ModemManager1.Modem",
    default_service = "org.freedesktop.ModemManager1"
)]
trait ModemDevice {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<i32>;
    #[zbus(property)]
    fn state_failed_reason(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn signal_quality(&self) -> zbus::Result<(u32, bool)>;
    #[zbus(property)]
    fn access_technologies(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn unlock_required(&self) -> zbus::Result<u32>;
}

#[proxy(
    interface = "org.freedesktop.ModemManager1.Modem.Modem3gpp",
    default_service = "org.freedesktop.ModemManager1"
)]
trait Modem3gpp {
    #[zbus(property)]
    fn operator_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn registration_state(&self) -> zbus::Result<u32>;
}

#[proxy(
    interface = "org.freedesktop.ModemManager1.Modem.Modem3gpp.Ussd",
    default_service = "org.freedesktop.ModemManager1"
)]
trait Ussd {
    fn initiate(&self, command: &str) -> zbus::Result<String>;
    fn cancel(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.ModemManager1.Modem.Signal",
    default_service = "org.freedesktop.ModemManager1"
)]
trait Signal {
    #[zbus(property)]
    fn gsm(&self) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
    #[zbus(property)]
    fn umts(&self) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
    #[zbus(property)]
    fn lte(&self) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
}

fn dict_f64(dict: &HashMap<String, OwnedValue>, key: &str) -> Option<f64> {
    dict.get(key).and_then(|v| f64::try_from(v).ok())
}

fn round_dbm(v: f64) -> i32 {
    v.round() as i32
}

fn rssi_from_signal(
    lte: Option<&HashMap<String, OwnedValue>>,
    umts: Option<&HashMap<String, OwnedValue>>,
    gsm: Option<&HashMap<String, OwnedValue>>,
) -> Option<i32> {
    if let Some(lte) = lte {
        if let Some(rsrp) = dict_f64(lte, "rsrp") {
            if rsrp.abs() > 0.1 {
                return Some(round_dbm(rsrp));
            }
        }
        if let Some(rssi) = dict_f64(lte, "rssi") {
            return Some(round_dbm(rssi));
        }
    }
    if let Some(umts) = umts {
        if let Some(rssi) = dict_f64(umts, "rssi") {
            return Some(round_dbm(rssi));
        }
    }
    if let Some(gsm) = gsm {
        if let Some(rssi) = dict_f64(gsm, "rssi") {
            return Some(round_dbm(rssi));
        }
    }
    None
}

fn path_error_is_stale(err: &ModemError) -> bool {
    match err {
        ModemError::NotFound(_) => true,
        ModemError::Failed(s) => {
            s.contains("UnknownObject")
                || s.contains("org.freedesktop.ModemManager1.Error.Core.NotFound")
        }
    }
}

#[derive(Clone)]
struct PathCache {
    path: Arc<Mutex<Option<OwnedObjectPath>>>,
}

impl PathCache {
    fn new() -> Self {
        Self {
            path: Arc::new(Mutex::new(None)),
        }
    }

    fn hit(&self) -> Option<OwnedObjectPath> {
        self.path.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn store(&self, path: OwnedObjectPath) {
        *self.path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }

    fn invalidate(&self) {
        *self.path.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn invalidate_if_stale(&self, err: &ModemError) -> bool {
        if path_error_is_stale(err) {
            self.invalidate();
            true
        } else {
            false
        }
    }
}

#[derive(Clone)]
struct CallForwardLock {
    inner: Arc<tokio::sync::Mutex<()>>,
}

impl CallForwardLock {
    fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.lock().await
    }
}

#[derive(Clone)]
pub struct MmModem {
    conn: Connection,
    uid: String,
    cached_path: PathCache,
    call_forward_lock: CallForwardLock,
}

impl MmModem {
    pub async fn connect() -> Result<Self, ModemError> {
        let uid = env::var("MODEM_UID").unwrap_or_else(|_| "dwm222".to_string());
        Self::connect_with_uid(uid).await
    }

    pub async fn connect_with_uid(uid: String) -> Result<Self, ModemError> {
        let conn = Connection::system().await.map_err(mm_err)?;
        Ok(Self {
            conn,
            uid,
            cached_path: PathCache::new(),
            call_forward_lock: CallForwardLock::new(),
        })
    }

    async fn lookup_path(&self) -> Result<OwnedObjectPath, ModemError> {
        let om = ObjectManagerProxy::builder(&self.conn)
            .destination(MM_DEST)
            .map_err(mm_err)?
            .path(MM_PATH)
            .map_err(mm_err)?
            .build()
            .await
            .map_err(mm_err)?;
        let objects = om.get_managed_objects().await.map_err(mm_err)?;
        for (path, ifaces) in objects {
            let Some(props) = ifaces.get(MM_MODEM_IFACE) else {
                continue;
            };
            let Some(device) = props.get("Device") else {
                continue;
            };
            let Ok(device) = <&str>::try_from(device) else {
                continue;
            };
            if device == self.uid {
                return Ok(path);
            }
        }
        Err(ModemError::NotFound(self.uid.clone()))
    }

    pub async fn resolve_path(&self) -> Result<OwnedObjectPath, ModemError> {
        if let Some(path) = self.cached_path.hit() {
            return Ok(path);
        }
        match self.lookup_path().await {
            Ok(path) => {
                self.cached_path.store(path.clone());
                Ok(path)
            }
            Err(err) => {
                self.cached_path.invalidate();
                Err(err)
            }
        }
    }

    async fn with_modem_path<F, Fut, T>(&self, mut op: F) -> Result<T, ModemError>
    where
        F: FnMut(OwnedObjectPath) -> Fut,
        Fut: Future<Output = Result<T, ModemError>>,
    {
        let path = self.resolve_path().await?;
        match op(path).await {
            Ok(v) => Ok(v),
            Err(err) => {
                if !self.cached_path.invalidate_if_stale(&err) {
                    return Err(err);
                }
                let path = self.resolve_path().await?;
                op(path).await
            }
        }
    }

    pub async fn list_sms(&self) -> Result<Vec<IncomingSms>, ModemError> {
        self.with_modem_path(|modem_path| {
            let conn = self.conn.clone();
            async move {
                let messaging = MessagingProxy::builder(&conn)
                    .path(&modem_path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;
                let paths = messaging.list().await.map_err(mm_err)?;
                let mut out = Vec::with_capacity(paths.len());
                for path in paths {
                    match load_incoming_sms_retry(&conn, &path).await {
                        Ok(sms) => out.push(sms),
                        Err(err) => tracing::warn!(path = %path, error = %err, "list sms skip"),
                    }
                }
                Ok(out)
            }
        })
        .await
    }

    pub async fn subscribe_added(&self) -> Result<impl Stream<Item = IncomingSms>, ModemError> {
        let (added, conn) = self
            .with_modem_path(|modem_path| {
                let conn = self.conn.clone();
                async move {
                    let messaging = MessagingProxy::builder(&conn)
                        .path(&modem_path)
                        .map_err(mm_err)?
                        .build()
                        .await
                        .map_err(mm_err)?;
                    let added = messaging.receive_added().await.map_err(mm_err)?;
                    Ok((added, conn))
                }
            })
            .await?;
        Ok(stream::unfold(
            (added, conn),
            |(mut added, conn)| async move {
                loop {
                    let sig = added.next().await?;
                    let Ok(args) = sig.args() else {
                        continue;
                    };
                    match load_incoming_sms_retry(&conn, args.path()).await {
                        Ok(sms) => return Some((sms, (added, conn))),
                        Err(err) => {
                            tracing::warn!(
                                path = %args.path(),
                                error = %err,
                                "failed to load added sms"
                            );
                            continue;
                        }
                    }
                }
            },
        ))
    }

    async fn ussd_initiate(&self, command: &str) -> Result<String, ModemError> {
        self.with_modem_path(|modem_path| {
            let conn = self.conn.clone();
            let command = command.to_string();
            async move {
                let ussd = UssdProxy::builder(&conn)
                    .path(&modem_path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;

                let _ = ussd.cancel().await;
                let result = tokio::time::timeout(USSD_TIMEOUT, ussd.initiate(&command)).await;
                let _ = ussd.cancel().await;

                let reply = result
                    .map_err(|_| ModemError::Failed("ussd timeout".into()))?
                    .map_err(mm_err)?;
                Ok(reply)
            }
        })
        .await
    }

    async fn ussd_roundtrip(
        &self,
        command: &str,
        default_region: &str,
    ) -> Result<CallForwardState, ModemError> {
        let reply = self.ussd_initiate(command).await?;
        parse_ussd_reply(&reply, default_region).map_err(ModemError::Failed)
    }
}

async fn load_incoming_sms(
    conn: &Connection,
    path: &OwnedObjectPath,
) -> Result<IncomingSms, ModemError> {
    let sms = SmsProxy::builder(conn)
        .path(path)
        .map_err(mm_err)?
        .build()
        .await
        .map_err(mm_err)?;
    let number = sms.number().await.map_err(mm_err)?;
    let text = sms.text().await.map_err(mm_err)?;
    let pdu_type = sms.pdu_type().await.map_err(mm_err)?;
    let timestamp = sms.timestamp().await.unwrap_or_default();
    // Deliver (1) is inbound; submit (2) is not; unknown with a number counts as inbound.
    let inbound = if pdu_type == 1 {
        true
    } else if !sms_is_inbound(pdu_type) {
        false
    } else {
        !number.is_empty()
    };
    Ok(IncomingSms {
        path: path.to_string(),
        e164: number,
        text,
        inbound,
        timestamp,
    })
}

fn sms_text_ready(text: &str) -> bool {
    !text.is_empty()
}

async fn load_incoming_sms_retry(
    conn: &Connection,
    path: &OwnedObjectPath,
) -> Result<IncomingSms, ModemError> {
    let mut last = None;
    for attempt in 0..20 {
        match load_incoming_sms(conn, path).await {
            Ok(sms) if !sms.inbound || sms_text_ready(&sms.text) => return Ok(sms),
            Ok(sms) => {
                tracing::debug!(
                    path = %path,
                    attempt,
                    e164 = %sms.e164,
                    "sms text not decoded yet"
                );
                last = None;
            }
            Err(err) => {
                tracing::debug!(
                    path = %path,
                    attempt,
                    error = %err,
                    "sms properties not ready"
                );
                last = Some(err);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if let Some(err) = last {
        return Err(err);
    }
    load_incoming_sms(conn, path).await
}

#[async_trait::async_trait]
impl SmsModem for MmModem {
    async fn send(&self, e164: &str, text: &str) -> Result<String, ModemError> {
        self.with_modem_path(|modem_path| {
            let conn = self.conn.clone();
            let e164 = e164.to_string();
            let text = text.to_string();
            async move {
                let messaging = MessagingProxy::builder(&conn)
                    .path(&modem_path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;
                let mut properties = HashMap::new();
                properties.insert("number", Value::from(e164.as_str()));
                properties.insert("text", Value::from(text.as_str()));
                let sms_path = messaging.create(properties).await.map_err(mm_err)?;
                let sms = SmsProxy::builder(&conn)
                    .path(&sms_path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;
                sms.send().await.map_err(mm_err)?;
                Ok(sms_path.to_string())
            }
        })
        .await
    }

    async fn delete(&self, path: &str) -> Result<(), ModemError> {
        self.with_modem_path(|modem_path| {
            let conn = self.conn.clone();
            let path = path.to_string();
            async move {
                let messaging = MessagingProxy::builder(&conn)
                    .path(&modem_path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;
                let object = zbus::zvariant::ObjectPath::try_from(path.as_str()).map_err(mm_err)?;
                match messaging.delete(&object).await {
                    Ok(()) => Ok(()),
                    Err(err) if delete_already_gone(&err) => Ok(()),
                    Err(err) => Err(mm_err(err)),
                }
            }
        })
        .await
    }
}

#[async_trait::async_trait]
impl ModemInfo for MmModem {
    async fn snapshot(&self) -> Result<ModemLive, ModemError> {
        self.with_modem_path(|path| {
            let conn = self.conn.clone();
            async move {
                let device = ModemDeviceProxy::builder(&conn)
                    .path(&path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;
                let state = ModemState::from_mm(device.state().await.map_err(mm_err)?);
                let failed_reason = device.state_failed_reason().await.ok().unwrap_or(0);
                let signal_percent = device
                    .signal_quality()
                    .await
                    .ok()
                    .map(|(percent, _)| percent);
                let access_tech = device
                    .access_technologies()
                    .await
                    .ok()
                    .and_then(radio_from_access_tech);
                let unlock = device.unlock_required().await.ok().unwrap_or(0);

                let gpp = Modem3gppProxy::builder(&conn)
                    .path(&path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;
                let operator = gpp
                    .operator_name()
                    .await
                    .ok()
                    .filter(|name| !name.is_empty());
                let registration = gpp
                    .registration_state()
                    .await
                    .ok()
                    .and_then(Registration::from_mm);

                let signal = SignalProxy::builder(&conn)
                    .path(&path)
                    .map_err(mm_err)?
                    .build()
                    .await
                    .map_err(mm_err)?;
                let gsm = signal.gsm().await.ok();
                let umts = signal.umts().await.ok();
                let lte = signal.lte().await.ok();
                let rssi_dbm = rssi_from_signal(lte.as_ref(), umts.as_ref(), gsm.as_ref());

                Ok(ModemLive {
                    state,
                    operator,
                    registration,
                    signal_percent,
                    rssi_dbm,
                    access_tech,
                    sim: sim_status(state, failed_reason, unlock),
                })
            }
        })
        .await
    }
}

#[async_trait::async_trait]
impl CallForward for MmModem {
    async fn query_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError> {
        let _guard = self.call_forward_lock.lock().await;
        self.ussd_roundtrip(ussd_query(), default_region).await
    }

    async fn set_forward(
        &self,
        e164: &str,
        default_region: &str,
    ) -> Result<CallForwardState, ModemError> {
        let _guard = self.call_forward_lock.lock().await;
        let e164 = normalize_e164(e164, default_region)
            .map_err(|err| ModemError::Failed(err.to_string()))?;
        apply_ussd_reply(self.ussd_initiate(&ussd_enable(&e164)).await)?;

        match self.ussd_roundtrip(ussd_query(), default_region).await {
            Ok(state) => Ok(state),
            Err(_) => Ok(CallForwardState {
                enabled: true,
                e164: Some(e164),
            }),
        }
    }

    async fn disable_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError> {
        let _guard = self.call_forward_lock.lock().await;
        apply_ussd_reply(self.ussd_initiate(ussd_disable()).await)?;

        match self.ussd_roundtrip(ussd_query(), default_region).await {
            Ok(state) => Ok(state),
            Err(_) => Ok(CallForwardState {
                enabled: false,
                e164: None,
            }),
        }
    }
}

#[async_trait::async_trait]
impl SmsInbox for MmModem {
    async fn list_sms(&self) -> Result<Vec<IncomingSms>, ModemError> {
        MmModem::list_sms(self).await
    }

    async fn subscribe_added(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = IncomingSms> + Send>>, ModemError> {
        let stream = MmModem::subscribe_added(self).await?;
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_pdu_is_not_inbound() {
        assert!(!sms_is_inbound(2));
        assert!(sms_is_inbound(1));
    }

    #[test]
    fn empty_inbound_text_is_not_decode_ready() {
        assert!(!sms_text_ready(""));
        assert!(sms_text_ready("hi"));
        assert!(sms_text_ready("سلام\nline"));
    }

    #[test]
    fn apply_ussd_reply_only_requires_initiate_success() {
        assert!(parse_ussd_reply("Operation completed", "IR").is_err());
        assert!(apply_ussd_reply(Ok("Operation completed".into())).is_ok());
        assert!(apply_ussd_reply(Err(ModemError::Failed("initiate failed".into()))).is_err());
    }

    #[test]
    fn unknown_object_fdo_is_already_gone() {
        let err = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject("gone".into())));
        assert!(delete_already_gone(&err));
        let other = zbus::Error::Failure("modem busy".into());
        assert!(!delete_already_gone(&other));
    }

    #[test]
    fn mm_core_not_found_is_already_gone() {
        assert!(delete_already_gone_name(
            "org.freedesktop.ModemManager1.Error.Core.NotFound"
        ));
        let err = zbus::Error::Failure(
            "org.freedesktop.ModemManager1.Error.Core.NotFound: \
             Not found: No SMS found with path '/org/freedesktop/ModemManager1/SMS/193'"
                .into(),
        );
        assert!(delete_already_gone(&err));
        assert!(!delete_already_gone_name(
            "org.freedesktop.ModemManager1.Error.Core.Failed"
        ));
    }

    #[test]
    fn rssi_prefers_lte_rsrp() {
        use zbus::zvariant::{OwnedValue, Value};
        let owned = |n: f64| -> OwnedValue { Value::from(n).try_into().unwrap() };
        let mut lte = std::collections::HashMap::new();
        lte.insert("rsrp".into(), owned(-102.4));
        lte.insert("rssi".into(), owned(-80.0));
        assert_eq!(rssi_from_signal(Some(&lte), None, None), Some(-102));
    }

    fn sample_path() -> OwnedObjectPath {
        zbus::zvariant::ObjectPath::try_from("/org/freedesktop/ModemManager1/Modem/0")
            .unwrap()
            .into()
    }

    #[test]
    fn path_cache_hit_store_invalidate() {
        let cache = PathCache::new();
        assert!(cache.hit().is_none());
        let path = sample_path();
        cache.store(path.clone());
        assert_eq!(
            cache.hit().as_ref().map(|p| p.as_str()),
            Some(path.as_str())
        );
        cache.invalidate();
        assert!(cache.hit().is_none());
    }

    #[test]
    fn stale_path_errors_invalidate_cache() {
        let cache = PathCache::new();
        cache.store(sample_path());
        assert!(!cache.invalidate_if_stale(&ModemError::Failed("modem busy".into())));
        assert!(cache.hit().is_some());
        assert!(cache.invalidate_if_stale(&ModemError::NotFound("dwm222".into())));
        assert!(cache.hit().is_none());
        cache.store(sample_path());
        assert!(cache.invalidate_if_stale(&ModemError::Failed(
            "org.freedesktop.DBus.Error.UnknownObject".into()
        )));
        assert!(cache.hit().is_none());
    }

    #[tokio::test]
    async fn call_forward_lock_serializes_clones() {
        let lock = CallForwardLock::new();
        let clone = lock.clone();
        let first = lock.lock().await;

        let waiter = tokio::spawn(async move {
            let _guard = clone.lock().await;
        });

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(first);
        waiter.await.unwrap();
    }
}
