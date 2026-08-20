use serde::Serialize;

use crate::normalize::normalize_e164;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CallForwardState {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e164: Option<String>,
}

pub fn ussd_query() -> &'static str {
    "*#21#"
}

pub fn ussd_disable() -> &'static str {
    "#21#"
}

pub fn ussd_enable(e164: &str) -> String {
    let digits: String = e164.chars().filter(|c| c.is_ascii_digit()).collect();
    format!("*21*{digits}#")
}

/// 3GPP AT+CCFC unconditional (reason 0) — preferred over USSD on QMI sticks.
pub fn ccfc_query() -> &'static str {
    "AT+CCFC=0,2"
}

pub fn ccfc_disable() -> &'static str {
    "AT+CCFC=0,4"
}

pub fn ccfc_enable(e164: &str) -> String {
    format!("AT+CCFC=0,3,\"{e164}\",145")
}

/// Parse `AT+CCFC=0,2` response lines (`+CCFC: <status>,<class>[,number,type]`).
pub fn parse_ccfc_reply(text: &str, default_region: &str) -> Result<CallForwardState, String> {
    let upper = text.to_ascii_uppercase();
    if upper.contains("+CME ERROR")
        || upper
            .lines()
            .any(|line| matches!(line.trim(), "ERROR" | "NO CARRIER"))
    {
        return Err(format!("AT+CCFC error: {text}"));
    }

    let mut saw_ccfc = false;
    let mut enabled_number: Option<String> = None;
    let mut enabled_without_number = false;

    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("+CCFC:")
            .or_else(|| line.strip_prefix("+ccfc:"))
        else {
            continue;
        };
        saw_ccfc = true;
        let rest = rest.trim();
        let mut parts = rest.splitn(4, ',');
        let status = parts
            .next()
            .map(str::trim)
            .unwrap_or("")
            .parse::<u32>()
            .map_err(|_| format!("bad +CCFC status in: {line}"))?;
        let _class = parts.next();
        if status != 1 {
            continue;
        }
        match parts.next().map(str::trim) {
            Some(raw) if !raw.is_empty() => {
                let raw = raw.trim_matches('"');
                if raw.is_empty() {
                    enabled_without_number = true;
                    continue;
                }
                let e164 = normalize_e164(raw, default_region).map_err(|e| e.to_string())?;
                enabled_number = Some(e164);
            }
            _ => enabled_without_number = true,
        }
    }

    if let Some(e164) = enabled_number {
        return Ok(CallForwardState {
            enabled: true,
            e164: Some(e164),
        });
    }
    if enabled_without_number {
        return Err(format!(
            "AT+CCFC reports active but has no number: {text}"
        ));
    }
    if saw_ccfc || upper.contains("OK") {
        return Ok(CallForwardState {
            enabled: false,
            e164: None,
        });
    }
    Err(format!("unparseable AT+CCFC reply: {text}"))
}

pub fn parse_ussd_reply(text: &str, default_region: &str) -> Result<CallForwardState, String> {
    let lower = text.to_ascii_lowercase();
    let disabled_markers = [
        "not forwarded",
        "not activated",
        "deactivated",
        "disabled",
        "not active",
        "inactive",
        "erased",
        "cancelled",
        "canceled",
    ];
    if disabled_markers.iter().any(|m| lower.contains(m)) {
        return Ok(CallForwardState {
            enabled: false,
            e164: None,
        });
    }

    let enabled_markers = ["forwarded", "activated", "active", "unconditional"];
    let looks_enabled = enabled_markers.iter().any(|m| lower.contains(m));
    if !looks_enabled {
        return Err(format!("unparseable ussd reply: {text}"));
    }

    // Prefer +E.164, else a long digit run (normalize).
    if let Some(plus) = extract_plus_number(text) {
        let e164 = normalize_e164(&plus, default_region).map_err(|e| e.to_string())?;
        return Ok(CallForwardState {
            enabled: true,
            e164: Some(e164),
        });
    }
    if let Some(digits) = extract_digit_run(text) {
        let e164 = normalize_e164(&digits, default_region).map_err(|e| e.to_string())?;
        return Ok(CallForwardState {
            enabled: true,
            e164: Some(e164),
        });
    }

    Err(format!(
        "ussd reply looks enabled but has no number: {text}"
    ))
}

fn extract_plus_number(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start > 8 {
                return Some(text[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

fn extract_digit_run(text: &str) -> Option<String> {
    let mut best: Option<String> = None;
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            if cur.len() >= 10 && best.as_ref().map(|b| b.len()).unwrap_or(0) < cur.len() {
                best = Some(cur.clone());
            }
            cur.clear();
        }
    }
    if cur.len() >= 10 && best.as_ref().map(|b| b.len()).unwrap_or(0) < cur.len() {
        best = Some(cur);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ussd_codes() {
        assert_eq!(ussd_query(), "*#21#");
        assert_eq!(ussd_disable(), "#21#");
        assert_eq!(ussd_enable("+989121234567"), "*21*989121234567#");
    }

    #[test]
    fn ccfc_codes() {
        assert_eq!(ccfc_query(), "AT+CCFC=0,2");
        assert_eq!(ccfc_disable(), "AT+CCFC=0,4");
        assert_eq!(
            ccfc_enable("+989121234567"),
            "AT+CCFC=0,3,\"+989121234567\",145"
        );
    }

    #[test]
    fn parse_ccfc_disabled() {
        let st = parse_ccfc_reply("+CCFC: 0,7\r\nOK\r\n", "IR").unwrap();
        assert!(!st.enabled);
        assert!(st.e164.is_none());
    }

    #[test]
    fn parse_ccfc_enabled_international() {
        let st = parse_ccfc_reply("+CCFC: 1,1,\"+989121234567\",145\r\nOK\r\n", "IR").unwrap();
        assert!(st.enabled);
        assert_eq!(st.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn parse_ccfc_enabled_digits_only() {
        let st = parse_ccfc_reply("+CCFC: 1,1,\"989121234567\",145\r\nOK\r\n", "IR").unwrap();
        assert!(st.enabled);
        assert_eq!(st.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn parse_ccfc_prefers_active_class_with_number() {
        let st = parse_ccfc_reply(
            "+CCFC: 0,1\r\n+CCFC: 1,2,\"09121234567\",129\r\nOK\r\n",
            "IR",
        )
        .unwrap();
        assert!(st.enabled);
        assert_eq!(st.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn parse_ccfc_error_line() {
        assert!(parse_ccfc_reply("ERROR\r\n", "IR").is_err());
        assert!(parse_ccfc_reply("+CME ERROR: 3\r\n", "IR").is_err());
    }

    #[test]
    fn parse_disabled_phrases() {
        for s in [
            "Call Forwarding Unconditional Not Forwarded",
            "CFU deactivated",
            "not forwarded",
            "disabled",
            "Call forwarding not activated for 09121234567",
            "Call forwarding inactive for 09121234567",
        ] {
            let st = parse_ussd_reply(s, "IR").unwrap();
            assert!(!st.enabled, "{s}");
            assert!(st.e164.is_none(), "{s}");
        }
    }

    #[test]
    fn parse_enabled_with_plus_e164() {
        let st = parse_ussd_reply("Call Forwarding Unconditional +989121234567", "IR").unwrap();
        assert!(st.enabled);
        assert_eq!(st.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn parse_enabled_local_digits() {
        let st = parse_ussd_reply("Forwarded to 09121234567", "IR").unwrap();
        assert!(st.enabled);
        assert_eq!(st.e164.as_deref(), Some("+989121234567"));
    }

    #[test]
    fn parse_garbage_errors() {
        assert!(parse_ussd_reply("asdf qwerty", "IR").is_err());
    }

    #[test]
    fn unrelated_digit_run_does_not_enable_forwarding() {
        assert!(parse_ussd_reply("Account reference 09121234567", "IR").is_err());
    }
}
