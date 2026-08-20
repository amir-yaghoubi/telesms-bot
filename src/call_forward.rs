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
