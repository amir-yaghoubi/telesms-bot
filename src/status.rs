use chrono::{Datelike, Utc};
use chrono_tz::Tz;

use crate::db::Db;
use crate::modem::{ModemInfo, ModemLive, ModemState, Radio, Registration, SimStatus};

pub enum ModemView {
    Offline,
    Live(ModemLive),
}

pub struct LastSms {
    pub label: String,
    pub when: String,
}

pub struct StatusSnapshot {
    pub modem_uid: String,
    pub modem: ModemView,
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
    Ok(StatusSnapshot {
        modem_uid: modem_uid.to_string(),
        modem: modem_view,
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
        db.insert_inbound_at("/a", "+989111111111", "x", "2026-08-19T10:00:00+00:00")
            .unwrap();
        let id = db.upsert_contact("people/a", "Ali").unwrap();
        db.replace_contact_numbers(id, &["+989111111111".into()])
            .unwrap();
        let modem = crate::modem::FakeModem::default(); // live None → NotFound
        let now = Utc.with_ymd_and_hms(2026, 8, 19, 11, 0, 0).unwrap();
        let snap = gather(&modem, &db, chrono_tz::Asia::Tehran, "dwm222", now)
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
        let snap = gather(&modem, &db, chrono_tz::UTC, "dwm222", now)
            .await
            .unwrap();
        match snap.modem {
            ModemView::Live(live) => assert_eq!(live.state, ModemState::Registered),
            ModemView::Offline => panic!("expected live"),
        }
        assert!(snap.contacts_ok);
    }
}
