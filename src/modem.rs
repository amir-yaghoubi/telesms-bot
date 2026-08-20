use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use futures_util::stream::Stream;

use thiserror::Error;

use crate::call_forward::CallForwardState;

#[derive(Clone, Debug)]
pub struct IncomingSms {
    pub path: String,
    pub e164: String,
    pub text: String,
    pub inbound: bool,
    /// ModemManager SMS Timestamp, RFC3339 when present.
    pub timestamp: String,
}

#[derive(Debug, Error)]
pub enum ModemError {
    #[error("modem error: {0}")]
    Failed(String),
    #[error("modem not found: {0}")]
    NotFound(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModemState {
    Failed,
    Unknown,
    Initializing,
    Locked,
    Disabled,
    Disabling,
    Enabling,
    Enabled,
    Searching,
    Registered,
    Disconnecting,
    Connecting,
    Connected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registration {
    Home,
    Roaming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Radio {
    Gsm,
    Umts,
    Lte,
    Nr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimStatus {
    Ok,
    Missing,
    PinRequired,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModemLive {
    pub state: ModemState,
    pub operator: Option<String>,
    pub registration: Option<Registration>,
    pub signal_percent: Option<u32>,
    pub rssi_dbm: Option<i32>,
    pub access_tech: Option<Radio>,
    pub sim: SimStatus,
}

impl ModemState {
    pub fn from_mm(v: i32) -> Self {
        match v {
            -1 => Self::Failed,
            0 => Self::Unknown,
            1 => Self::Initializing,
            2 => Self::Locked,
            3 => Self::Disabled,
            4 => Self::Disabling,
            5 => Self::Enabling,
            6 => Self::Enabled,
            7 => Self::Searching,
            8 => Self::Registered,
            9 => Self::Disconnecting,
            10 => Self::Connecting,
            11 => Self::Connected,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Unknown => "Unknown",
            Self::Initializing => "Initializing",
            Self::Locked => "Locked",
            Self::Disabled => "Disabled",
            Self::Disabling => "Disabling",
            Self::Enabling => "Enabling",
            Self::Enabled => "Enabled",
            Self::Searching => "Searching",
            Self::Registered => "Registered",
            Self::Disconnecting => "Disconnecting",
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
        }
    }
}

impl Registration {
    pub fn from_mm(v: u32) -> Option<Self> {
        match v {
            1 | 6 => Some(Self::Home),
            5 | 7 => Some(Self::Roaming),
            _ => None,
        }
    }
}

pub fn radio_from_access_tech(bits: u32) -> Option<Radio> {
    if bits & (1 << 15) != 0 {
        Some(Radio::Nr)
    } else if bits & (1 << 14) != 0 {
        Some(Radio::Lte)
    } else if bits & (0b1_1111 << 5) != 0 {
        Some(Radio::Umts)
    } else if bits & (0b1111 << 1) != 0 {
        Some(Radio::Gsm)
    } else {
        None
    }
}

pub fn sim_status(state: ModemState, failed_reason: u32, unlock: u32) -> SimStatus {
    if state == ModemState::Failed && failed_reason == 2 {
        SimStatus::Missing
    } else if state == ModemState::Locked && unlock == 2 {
        SimStatus::PinRequired
    } else {
        SimStatus::Ok
    }
}

#[async_trait::async_trait]
pub trait SmsModem: Send + Sync {
    async fn send(&self, e164: &str, text: &str) -> Result<String, ModemError>;
    async fn delete(&self, path: &str) -> Result<(), ModemError>;
}

#[async_trait::async_trait]
pub trait SmsInbox: SmsModem {
    async fn list_sms(&self) -> Result<Vec<IncomingSms>, ModemError>;
    async fn subscribe_added(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = IncomingSms> + Send>>, ModemError>;
}

#[async_trait::async_trait]
pub trait ModemInfo: Send + Sync {
    async fn snapshot(&self) -> Result<ModemLive, ModemError>;
}

#[async_trait::async_trait]
pub trait CallForward: Send + Sync {
    async fn query_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError>;
    async fn set_forward(&self, e164: &str, default_region: &str) -> Result<CallForwardState, ModemError>;
    async fn disable_forward(&self, default_region: &str) -> Result<CallForwardState, ModemError>;
}

pub struct FakeModem {
    pub sent: Mutex<Vec<(String, String)>>,
    pub deleted: Mutex<Vec<String>>,
    pub fail: bool,
    pub delete_fail: bool,
    pub(crate) path_seq: AtomicU64,
    pub live: Mutex<Option<ModemLive>>,
    pub listed: Mutex<Vec<IncomingSms>>,
    pub forward: Mutex<CallForwardState>,
    pub forward_fail: bool,
}

impl Default for FakeModem {
    fn default() -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            deleted: Mutex::new(Vec::new()),
            fail: false,
            delete_fail: false,
            path_seq: AtomicU64::new(0),
            live: Mutex::new(None),
            listed: Mutex::new(Vec::new()),
            forward: Mutex::new(CallForwardState {
                enabled: false,
                e164: None,
            }),
            forward_fail: false,
        }
    }
}

#[async_trait::async_trait]
impl SmsModem for FakeModem {
    async fn send(&self, e164: &str, text: &str) -> Result<String, ModemError> {
        if self.fail {
            return Err(ModemError::Failed("error".into()));
        }
        let n = self.path_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let path = format!("/fake/sms/{n}");
        self.sent
            .lock()
            .expect("fake modem sent lock")
            .push((e164.to_string(), text.to_string()));
        Ok(path)
    }

    async fn delete(&self, path: &str) -> Result<(), ModemError> {
        if self.delete_fail {
            return Err(ModemError::Failed("delete failed".into()));
        }
        self.deleted
            .lock()
            .expect("fake modem deleted lock")
            .push(path.to_string());
        Ok(())
    }
}

#[async_trait::async_trait]
impl SmsInbox for FakeModem {
    async fn list_sms(&self) -> Result<Vec<IncomingSms>, ModemError> {
        Ok(self.listed.lock().expect("fake modem listed lock").clone())
    }

    async fn subscribe_added(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = IncomingSms> + Send>>, ModemError> {
        Ok(Box::pin(futures_util::stream::pending()))
    }
}

#[async_trait::async_trait]
impl ModemInfo for FakeModem {
    async fn snapshot(&self) -> Result<ModemLive, ModemError> {
        match self.live.lock().expect("fake modem live lock").clone() {
            Some(live) => Ok(live),
            None => Err(ModemError::NotFound("fake".into())),
        }
    }
}

#[async_trait::async_trait]
impl CallForward for FakeModem {
    async fn query_forward(&self, _default_region: &str) -> Result<CallForwardState, ModemError> {
        if self.forward_fail {
            return Err(ModemError::Failed("forward fail".into()));
        }
        Ok(self.forward.lock().expect("forward lock").clone())
    }

    async fn set_forward(&self, e164: &str, default_region: &str) -> Result<CallForwardState, ModemError> {
        if self.forward_fail {
            return Err(ModemError::Failed("forward fail".into()));
        }
        let e164 = crate::normalize::normalize_e164(e164, default_region)
            .map_err(|e| ModemError::Failed(e.to_string()))?;
        let st = CallForwardState {
            enabled: true,
            e164: Some(e164),
        };
        *self.forward.lock().expect("forward lock") = st.clone();
        Ok(st)
    }

    async fn disable_forward(&self, _default_region: &str) -> Result<CallForwardState, ModemError> {
        if self.forward_fail {
            return Err(ModemError::Failed("forward fail".into()));
        }
        let st = CallForwardState {
            enabled: false,
            e164: None,
        };
        *self.forward.lock().expect("forward lock") = st.clone();
        Ok(st)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_state_and_sim() {
        assert_eq!(ModemState::from_mm(8).label(), "Registered");
        assert_eq!(ModemState::from_mm(-1), ModemState::Failed);
        assert_eq!(sim_status(ModemState::Failed, 2, 1), SimStatus::Missing);
        assert_eq!(sim_status(ModemState::Locked, 0, 2), SimStatus::PinRequired);
        assert_eq!(sim_status(ModemState::Registered, 0, 3), SimStatus::Ok);
        assert_eq!(Registration::from_mm(1), Some(Registration::Home));
        assert_eq!(Registration::from_mm(5), Some(Registration::Roaming));
        assert_eq!(Registration::from_mm(0), None);
    }

    #[tokio::test]
    async fn send_returns_unique_paths_and_records() {
        let m = FakeModem::default();
        let a = m.send("+98912", "hi").await.unwrap();
        let b = m.send("+98913", "yo").await.unwrap();
        assert_eq!(a, "/fake/sms/1");
        assert_eq!(b, "/fake/sms/2");
        assert_ne!(a, b);
        assert_eq!(
            m.sent.lock().unwrap().as_slice(),
            &[
                ("+98912".into(), "hi".into()),
                ("+98913".into(), "yo".into())
            ]
        );
    }

    #[tokio::test]
    async fn delete_records_path_and_is_idempotent() {
        let m = FakeModem::default();
        m.delete("/sms/1").await.unwrap();
        m.delete("/sms/1").await.unwrap();
        assert_eq!(
            m.deleted.lock().unwrap().as_slice(),
            &["/sms/1".into(), "/sms/1".into()] as &[String]
        );
    }

    #[tokio::test]
    async fn send_fail_does_not_delete() {
        let m = FakeModem {
            fail: true,
            ..FakeModem::default()
        };
        assert!(m.send("+1", "x").await.is_err());
        assert!(m.deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn fake_call_forward_set_query_disable() {
        let m = FakeModem::default();
        assert!(!m.query_forward("IR").await.unwrap().enabled);
        let set = m.set_forward("+989121234567", "IR").await.unwrap();
        assert!(set.enabled);
        assert_eq!(set.e164.as_deref(), Some("+989121234567"));
        assert_eq!(m.query_forward("IR").await.unwrap(), set);
        let off = m.disable_forward("IR").await.unwrap();
        assert!(!off.enabled);
        assert!(off.e164.is_none());
    }

    #[tokio::test]
    async fn fake_call_forward_fail() {
        let m = FakeModem {
            forward_fail: true,
            ..FakeModem::default()
        };
        assert!(m.query_forward("IR").await.is_err());
    }
}
