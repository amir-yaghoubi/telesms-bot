use chrono::{Datelike, Utc};
use chrono_tz::Tz;
use serde::Serialize;

use crate::db::Db;
use crate::modem::{ModemInfo, ModemLive, ModemState, Radio, Registration, SimStatus};

pub enum ModemView {
    Offline,
    Live(ModemLive),
}

pub enum ForwardView {
    Off,
    On { label: String },
    Unavailable,
}

pub struct LastSms {
    pub label: String,
    pub when: String,
}

#[derive(Serialize)]
pub struct LastSmsJson<'a> {
    pub label: &'a str,
    pub when: &'a str,
}

#[derive(Serialize)]
pub struct OfflineModemJson {
    pub state: &'static str,
}

#[derive(Serialize)]
pub struct LiveModemJson<'a> {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_percent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi_dbm: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_tech: Option<&'static str>,
    pub sim: &'static str,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum ModemJson<'a> {
    Offline(OfflineModemJson),
    Live(LiveModemJson<'a>),
}

#[derive(Serialize)]
pub struct StatusJson<'a> {
    pub modem_uid: &'a str,
    pub modem: ModemJson<'a>,
    pub today_in: u32,
    pub today_out_ok: u32,
    pub today_out_fail: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fail_error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_in: Option<LastSmsJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_out: Option<LastSmsJson<'a>>,
    pub contacts_ok: bool,
}

fn radio_json(radio: Radio) -> &'static str {
    match radio {
        Radio::Gsm => "gsm",
        Radio::Umts => "umts",
        Radio::Lte => "lte",
        Radio::Nr => "nr",
    }
}

fn sim_json(sim: SimStatus) -> &'static str {
    match sim {
        SimStatus::Ok => "ok",
        SimStatus::Missing => "missing",
        SimStatus::PinRequired => "pin_required",
    }
}

fn registration_json(reg: Registration) -> &'static str {
    match reg {
        Registration::Home => "home",
        Registration::Roaming => "roaming",
    }
}

fn live_modem_json(live: &ModemLive) -> LiveModemJson<'_> {
    LiveModemJson {
        state: live.state.label().to_ascii_lowercase(),
        operator: live.operator.as_deref(),
        registration: live.registration.map(registration_json),
        signal_percent: live.signal_percent,
        rssi_dbm: live.rssi_dbm,
        access_tech: live.access_tech.map(radio_json),
        sim: sim_json(live.sim),
    }
}

pub fn status_json_from_snapshot(snap: &StatusSnapshot) -> StatusJson<'_> {
    let modem = match &snap.modem {
        ModemView::Offline => ModemJson::Offline(OfflineModemJson { state: "offline" }),
        ModemView::Live(live) => ModemJson::Live(live_modem_json(live)),
    };
    StatusJson {
        modem_uid: &snap.modem_uid,
        modem,
        today_in: snap.today_in,
        today_out_ok: snap.today_out_ok,
        today_out_fail: snap.today_out_fail,
        last_fail_error: snap.last_fail_error.as_deref(),
        last_in: snap.last_in.as_ref().map(|s| LastSmsJson {
            label: &s.label,
            when: &s.when,
        }),
        last_out: snap.last_out.as_ref().map(|s| LastSmsJson {
            label: &s.label,
            when: &s.when,
        }),
        contacts_ok: snap.contacts_ok,
    }
}

pub struct StatusSnapshot {
    pub modem_uid: String,
    pub modem: ModemView,
    pub forward: ForwardView,
    pub today_in: u32,
    pub today_out_ok: u32,
    pub today_out_fail: u32,
    pub last_fail_error: Option<String>,
    pub last_in: Option<LastSms>,
    pub last_out: Option<LastSms>,
    pub contacts_ok: bool,
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn signal_label(percent: Option<u32>) -> &'static str {
    match percent {
        None | Some(0) => "No signal",
        Some(1..=19) => "Poor",
        Some(20..=39) => "Fair",
        Some(40..=59) => "Good",
        Some(60..=79) => "Very good",
        Some(_) => "Excellent",
    }
}

pub fn radio_label(radio: Radio) -> &'static str {
    match radio {
        Radio::Gsm => "2G",
        Radio::Umts => "3G",
        Radio::Lte => "4G LTE",
        Radio::Nr => "5G",
    }
}

pub fn format_relative(then: chrono::DateTime<Utc>, now: chrono::DateTime<Utc>, tz: Tz) -> String {
    let age = now.signed_duration_since(then);
    if age < chrono::Duration::minutes(1) {
        return "just now".into();
    }
    if age < chrono::Duration::minutes(60) {
        return format!("{}m ago", age.num_minutes());
    }
    if age < chrono::Duration::hours(24) {
        return format!("{}h ago", age.num_hours());
    }

    let then_local = then.with_timezone(&tz);
    let now_local = now.with_timezone(&tz);
    let yesterday = now_local.date_naive() - chrono::Duration::days(1);
    if then_local.date_naive() == yesterday {
        return format!("yesterday {}", then_local.format("%H:%M"));
    }
    if then_local.year() == now_local.year() {
        return format!(
            "{} {} {}",
            then_local.format("%b"),
            then_local.day(),
            then_local.format("%H:%M")
        );
    }
    format!("{}", then_local.format("%Y-%m-%d %H:%M"))
}

fn state_dot(state: ModemState) -> &'static str {
    match state {
        ModemState::Failed => "🔴",
        ModemState::Registered | ModemState::Connected => "🟢",
        _ => "🟡",
    }
}

fn registration_label(reg: Registration) -> &'static str {
    match reg {
        Registration::Home => "home",
        Registration::Roaming => "roaming",
    }
}

fn sim_line(sim: SimStatus) -> &'static str {
    match sim {
        SimStatus::Ok => "SIM ok",
        SimStatus::Missing => "SIM missing",
        SimStatus::PinRequired => "PIN required",
    }
}

fn format_modem_section(snap: &StatusSnapshot) -> String {
    match &snap.modem {
        ModemView::Offline => {
            format!(
                "<b>Modem</b>\n🔴 Offline\n<i>Stick not found ({})</i>",
                html_escape(&snap.modem_uid)
            )
        }
        ModemView::Live(live) => {
            let mut out = String::from("<b>Modem</b>\n");
            out.push_str(state_dot(live.state));
            out.push(' ');
            out.push_str(live.state.label());
            match &live.operator {
                Some(op) if !op.is_empty() => {
                    out.push_str(" · ");
                    out.push_str(&html_escape(op));
                }
                _ => out.push_str(" · no operator"),
            }
            if let Some(reg) = live.registration {
                out.push_str(" · ");
                out.push_str(registration_label(reg));
            }
            out.push('\n');

            let label = signal_label(live.signal_percent);
            if label == "No signal" {
                out.push_str("📶 No signal\n");
            } else {
                let percent = live.signal_percent.unwrap_or(0);
                out.push_str(&format!("📶 {label} ({percent}%)"));
                if let Some(radio) = live.access_tech {
                    out.push_str(" · ");
                    out.push_str(radio_label(radio));
                }
                if let Some(dbm) = live.rssi_dbm {
                    out.push_str(&format!(" · {dbm} dBm"));
                }
                out.push('\n');
            }

            out.push_str(sim_line(live.sim));
            out
        }
    }
}

fn format_forward_line(view: &ForwardView) -> String {
    match view {
        ForwardView::Off => "↪️ Forward · off".into(),
        ForwardView::On { label } => format!("↪️ Forward · {}", html_escape(label)),
        ForwardView::Unavailable => "↪️ Forward · unavailable".into(),
    }
}

pub fn today_start_rfc3339(now: chrono::DateTime<chrono::Utc>, tz: chrono_tz::Tz) -> String {
    let local = now.with_timezone(&tz);
    let midnight = local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_local_timezone(tz)
        .single()
        .expect("tz midnight");
    midnight.with_timezone(&chrono::Utc).to_rfc3339()
}

pub async fn gather(
    modem: &dyn ModemInfo,
    forward: Option<&dyn crate::modem::CallForward>,
    region: &str,
    db: &Db,
    tz: chrono_tz::Tz,
    modem_uid: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<StatusSnapshot, crate::db::DbError> {
    let modem_view = match modem.snapshot().await {
        Ok(live) => ModemView::Live(live),
        Err(err) => {
            tracing::warn!(error = %err, "status modem snapshot");
            ModemView::Offline
        }
    };
    let since = today_start_rfc3339(now, tz);
    let counts = db.today_sms_counts(&since)?;
    let last_in = map_last(db, db.last_inbound()?, now, tz)?;
    let last_out = map_last(db, db.last_outbound_ok()?, now, tz)?;
    let last_fail_error = if counts.sent_fail > 0 {
        db.last_outbound_fail_since(&since)?
    } else {
        None
    };
    let forward_view = match forward {
        None => ForwardView::Unavailable,
        Some(forward) => match forward.query_forward(region).await {
            Ok(st) if !st.enabled => ForwardView::Off,
            Ok(st) => {
                let e164 = st.e164.unwrap_or_default();
                let label = db
                    .find_contact_by_e164(&e164)?
                    .map(|c| c.display_name)
                    .unwrap_or(e164);
                ForwardView::On { label }
            }
            Err(err) => {
                tracing::warn!(error = %err, "status call forward query");
                ForwardView::Unavailable
            }
        },
    };
    Ok(StatusSnapshot {
        modem_uid: modem_uid.to_string(),
        modem: modem_view,
        forward: forward_view,
        today_in: counts.inbound,
        today_out_ok: counts.sent_ok,
        today_out_fail: counts.sent_fail,
        last_fail_error,
        last_in,
        last_out,
        contacts_ok: db.contacts_available(),
    })
}

fn map_last(
    db: &Db,
    row: Option<(String, String)>,
    now: chrono::DateTime<chrono::Utc>,
    tz: chrono_tz::Tz,
) -> Result<Option<LastSms>, crate::db::DbError> {
    let Some((e164, created_at)) = row else {
        return Ok(None);
    };
    let label = db
        .find_contact_by_e164(&e164)?
        .map(|c| c.display_name)
        .unwrap_or(e164);
    let when = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map(|dt| format_relative(dt.with_timezone(&chrono::Utc), now, tz))
        .unwrap_or_else(|_| "just now".into());
    Ok(Some(LastSms { label, when }))
}

pub fn format_status_html(snap: &StatusSnapshot) -> String {
    let mut out = format_modem_section(snap);
    out.push('\n');
    out.push_str(&format_forward_line(&snap.forward));
    out.push_str("\n\n");

    out.push_str("<b>Today</b>\n");
    out.push_str(&format!("↓ {} received\n", snap.today_in));
    out.push_str(&format!("↑ {} sent\n", snap.today_out_ok));
    out.push_str(&format!("✗ {} failed", snap.today_out_fail));
    if snap.today_out_fail > 0 {
        if let Some(err) = &snap.last_fail_error {
            let truncated: String = err.chars().take(120).collect();
            out.push('\n');
            out.push_str(&html_escape(&truncated));
        }
    }

    out.push_str("\n\n");
    out.push_str("<b>Last</b>\n");
    match (&snap.last_in, &snap.last_out) {
        (None, None) => out.push_str("none yet"),
        (Some(inn), None) => {
            out.push_str(&format!("↓ {} · {}", html_escape(&inn.label), inn.when));
        }
        (None, Some(out_sms)) => {
            out.push_str(&format!(
                "↑ {} · {}",
                html_escape(&out_sms.label),
                out_sms.when
            ));
        }
        (Some(inn), Some(out_sms)) => {
            out.push_str(&format!("↓ {} · {}\n", html_escape(&inn.label), inn.when));
            out.push_str(&format!(
                "↑ {} · {}",
                html_escape(&out_sms.label),
                out_sms.when
            ));
        }
    }

    out.push_str("\n\n");
    if snap.contacts_ok {
        out.push_str("Contacts · OK");
    } else {
        out.push_str("Contacts · unavailable");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::{ModemLive, ModemState, Radio, Registration, SimStatus};
    use chrono::{TimeZone, Utc};

    fn live_ok() -> ModemLive {
        ModemLive {
            state: ModemState::Registered,
            operator: Some("MTN Irancell".into()),
            registration: Some(Registration::Home),
            signal_percent: Some(38),
            rssi_dbm: Some(-93),
            access_tech: Some(Radio::Umts),
            sim: SimStatus::Ok,
        }
    }

    fn happy() -> StatusSnapshot {
        StatusSnapshot {
            modem_uid: "dwm222".into(),
            modem: ModemView::Live(live_ok()),
            forward: ForwardView::Off,
            today_in: 4,
            today_out_ok: 2,
            today_out_fail: 0,
            last_fail_error: None,
            last_in: Some(LastSms {
                label: "Ali".into(),
                when: "12m ago".into(),
            }),
            last_out: Some(LastSms {
                label: "Sara".into(),
                when: "3h ago".into(),
            }),
            contacts_ok: true,
        }
    }

    #[test]
    fn happy_path_html() {
        let html = format_status_html(&happy());
        assert_eq!(
            html,
            "<b>Modem</b>\n\
🟢 Registered · MTN Irancell · home\n\
📶 Fair (38%) · 3G · -93 dBm\n\
SIM ok\n\
↪️ Forward · off\n\
\n\
<b>Today</b>\n\
↓ 4 received\n\
↑ 2 sent\n\
✗ 0 failed\n\
\n\
<b>Last</b>\n\
↓ Ali · 12m ago\n\
↑ Sara · 3h ago\n\
\n\
Contacts · OK"
        );
    }

    #[test]
    fn forward_on_html() {
        let mut s = happy();
        s.forward = ForwardView::On {
            label: "Ali".into(),
        };
        let html = format_status_html(&s);
        assert!(html.contains("↪️ Forward · Ali"));
    }

    #[test]
    fn forward_unavailable_html() {
        let mut s = happy();
        s.forward = ForwardView::Unavailable;
        assert!(format_status_html(&s).contains("↪️ Forward · unavailable"));
    }

    #[test]
    fn offline_html() {
        let mut s = happy();
        s.modem = ModemView::Offline;
        let html = format_status_html(&s);
        assert!(html.contains("🔴 Offline"));
        assert!(html.contains("<i>Stick not found (dwm222)</i>"));
        assert!(!html.contains("📶"));
        assert!(!html.contains("SIM"));
        assert!(html.contains("↓ Ali · 12m ago"));
    }

    #[test]
    fn failed_send_html() {
        let mut s = happy();
        s.today_out_fail = 1;
        s.last_fail_error = Some("modem error: <timeout>".into());
        let html = format_status_html(&s);
        assert!(html.contains("✗ 1 failed"));
        assert!(html.contains("modem error: &lt;timeout&gt;"));
        assert!(!html.contains("<timeout>"));
    }

    #[test]
    fn searching_empty_html() {
        let s = StatusSnapshot {
            modem_uid: "dwm222".into(),
            modem: ModemView::Live(ModemLive {
                state: ModemState::Searching,
                operator: None,
                registration: None,
                signal_percent: Some(0),
                rssi_dbm: None,
                access_tech: None,
                sim: SimStatus::Ok,
            }),
            forward: ForwardView::Off,
            today_in: 0,
            today_out_ok: 0,
            today_out_fail: 0,
            last_fail_error: None,
            last_in: None,
            last_out: None,
            contacts_ok: false,
        };
        let html = format_status_html(&s);
        assert!(html.contains("🟡 Searching · no operator"));
        assert!(html.contains("📶 No signal"));
        assert!(html.contains("none yet"));
        assert!(html.contains("Contacts · unavailable"));
    }

    #[test]
    fn escapes_operator() {
        let mut s = happy();
        if let ModemView::Live(live) = &mut s.modem {
            live.operator = Some("A&B".into());
        }
        assert!(format_status_html(&s).contains("A&amp;B"));
    }

    #[test]
    fn signal_label_table() {
        assert_eq!(signal_label(None), "No signal");
        assert_eq!(signal_label(Some(0)), "No signal");
        assert_eq!(signal_label(Some(1)), "Poor");
        assert_eq!(signal_label(Some(19)), "Poor");
        assert_eq!(signal_label(Some(20)), "Fair");
        assert_eq!(signal_label(Some(38)), "Fair");
        assert_eq!(signal_label(Some(39)), "Fair");
        assert_eq!(signal_label(Some(40)), "Good");
        assert_eq!(signal_label(Some(80)), "Excellent");
        assert_eq!(signal_label(Some(100)), "Excellent");
    }

    #[test]
    fn radio_from_bits() {
        assert_eq!(
            crate::modem::radio_from_access_tech(1 << 15),
            Some(Radio::Nr)
        );
        assert_eq!(
            crate::modem::radio_from_access_tech(1 << 14),
            Some(Radio::Lte)
        );
        assert_eq!(
            crate::modem::radio_from_access_tech(1 << 5),
            Some(Radio::Umts)
        );
        assert_eq!(
            crate::modem::radio_from_access_tech(1 << 1),
            Some(Radio::Gsm)
        );
        assert_eq!(crate::modem::radio_from_access_tech(0), None);
        assert_eq!(radio_label(Radio::Umts), "3G");
        assert_eq!(radio_label(Radio::Lte), "4G LTE");
    }

    #[test]
    fn relative_time_table() {
        let tz = chrono_tz::Asia::Tehran;
        let now = Utc.with_ymd_and_hms(2026, 8, 19, 11, 0, 0).unwrap();
        assert_eq!(
            format_relative(now - chrono::Duration::seconds(30), now, tz),
            "just now"
        );
        assert_eq!(
            format_relative(now - chrono::Duration::minutes(12), now, tz),
            "12m ago"
        );
        assert_eq!(
            format_relative(now - chrono::Duration::hours(3), now, tz),
            "3h ago"
        );
        // Age < 24h wins even if the calendar day is yesterday.
        let same_day_window = Utc.with_ymd_and_hms(2026, 8, 18, 15, 10, 0).unwrap();
        assert_eq!(format_relative(same_day_window, now, tz), "19h ago");
        // 2026-08-18 05:10 UTC = 08:40 Tehran; 29h50m ago and calendar yesterday.
        let y = Utc.with_ymd_and_hms(2026, 8, 18, 5, 10, 0).unwrap();
        assert_eq!(format_relative(y, now, tz), "yesterday 08:40");
        let old = Utc.with_ymd_and_hms(2025, 12, 31, 5, 42, 0).unwrap();
        assert_eq!(format_relative(old, now, tz), "2025-12-31 09:12");
    }

    #[tokio::test]
    async fn gather_offline_still_has_counts() {
        let db = crate::db::Db::open_in_memory().unwrap();
        db.insert_inbound_at(
            "/a",
            "+989111111111",
            "x",
            "2026-08-19T10:00:00+00:00",
            None,
        )
        .unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989111111111".into()])
            .unwrap();
        let modem = crate::modem::FakeModem::default(); // live None → NotFound
        let now = Utc.with_ymd_and_hms(2026, 8, 19, 11, 0, 0).unwrap();
        let snap = gather(
            &modem,
            Some(&modem),
            "IR",
            &db,
            chrono_tz::Asia::Tehran,
            "dwm222",
            now,
        )
        .await
        .unwrap();
        assert!(matches!(snap.modem, ModemView::Offline));
        assert_eq!(snap.today_in, 1);
        assert_eq!(snap.last_in.as_ref().unwrap().label, "Ali");
    }

    #[tokio::test]
    async fn gather_uses_live_snapshot() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let modem = crate::modem::FakeModem::default();
        *modem.live.lock().unwrap() = Some(live_ok());
        let now = Utc.with_ymd_and_hms(2026, 8, 19, 11, 0, 0).unwrap();
        let snap = gather(
            &modem,
            Some(&modem),
            "IR",
            &db,
            chrono_tz::UTC,
            "dwm222",
            now,
        )
        .await
        .unwrap();
        match snap.modem {
            ModemView::Live(live) => assert_eq!(live.state, ModemState::Registered),
            ModemView::Offline => panic!("expected live"),
        }
        assert_eq!(
            modem
                .forward_queries
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(snap.contacts_ok);
    }

    #[tokio::test]
    async fn gather_soft_fails_forward() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let modem = crate::modem::FakeModem::default();
        *modem.live.lock().unwrap() = Some(live_ok());
        let forward = crate::modem::FakeModem {
            forward_fail: true,
            ..Default::default()
        };
        let now = Utc.with_ymd_and_hms(2026, 8, 19, 11, 0, 0).unwrap();
        let snap = gather(
            &modem,
            Some(&forward),
            "IR",
            &db,
            chrono_tz::UTC,
            "dwm222",
            now,
        )
        .await
        .unwrap();
        assert!(matches!(snap.forward, ForwardView::Unavailable));
    }

    #[tokio::test]
    async fn gather_without_forward_skips_forward_query() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let modem = crate::modem::FakeModem::default();
        let now = Utc.with_ymd_and_hms(2026, 8, 19, 11, 0, 0).unwrap();

        let snap = gather(&modem, None, "IR", &db, chrono_tz::UTC, "dwm222", now)
            .await
            .unwrap();

        assert!(matches!(snap.forward, ForwardView::Unavailable));
        assert_eq!(
            modem
                .forward_queries
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}
