use regex::Regex;
use std::sync::LazyLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PatternError {
    #[error("code must be non-empty digits, got '{0}'")]
    InvalidCode(String),
    #[error("code '{0}' not found in text")]
    CodeNotFound(String),
    #[error("not enough context around code '{0}'")]
    NoContext(String),
    #[error("invalid pattern: {0}")]
    Invalid(String),
    #[error("pattern needs a literal letter outside the code group")]
    NoAnchor,
}

#[derive(Debug)]
pub struct CompiledPattern {
    pub id: i64,
    pub name: String,
    pub regex: Regex,
}

static OTP_KEYWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(کد|رمز|code|otp|password|pin)").unwrap());
static OTP_DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^\d])\d{4,8}(?:[^\d]|$)").unwrap());

pub fn normalize_digits(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            let n = c as u32;
            if (0x06F0..=0x06F9).contains(&n) {
                char::from_u32(n - 0x06F0 + u32::from(b'0')).unwrap_or(c)
            } else if (0x0660..=0x0669).contains(&n) {
                char::from_u32(n - 0x0660 + u32::from(b'0')).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn generalize_token(token: &str) -> String {
    let mut out = String::new();
    let mut literal = String::new();
    for c in token.chars() {
        if c.is_ascii_digit() {
            if !literal.is_empty() {
                out.push_str(&regex::escape(&literal));
                literal.clear();
            }
            if !out.ends_with(r"\d+") {
                out.push_str(r"\d+");
            }
        } else {
            literal.push(c);
        }
    }
    if !literal.is_empty() {
        out.push_str(&regex::escape(&literal));
    }
    out
}

fn has_letter_anchor(pattern: &str) -> bool {
    let mut stack: Vec<bool> = Vec::new();
    let mut chars = pattern.chars().peekable();
    let mut anchored = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '(' if chars.peek() == Some(&'?') => {
                chars.next();
                if matches!(chars.peek(), Some('P' | '<')) {
                    stack.push(true);
                    continue;
                }
                while chars
                    .next_if(|c| c.is_ascii_alphabetic() || *c == '-')
                    .is_some()
                {}
                if chars.next() == Some(':') {
                    stack.push(false);
                }
            }
            '(' => stack.push(true),
            ')' => {
                stack.pop();
            }
            c if c.is_alphabetic() && !stack.contains(&true) => anchored = true,
            _ => {}
        }
    }
    anchored
}

pub fn validate_pattern(pattern: &str) -> Result<Regex, PatternError> {
    let re = Regex::new(pattern).map_err(|e| PatternError::Invalid(e.to_string()))?;
    if re.captures_len() != 2 {
        return Err(PatternError::Invalid(format!(
            "expected exactly one capture group, found {}",
            re.captures_len() - 1
        )));
    }
    if !has_letter_anchor(pattern) {
        return Err(PatternError::NoAnchor);
    }
    Ok(re)
}

/// The pattern for the text before the code, anchored on the nearest token that
/// is not punctuation. Punctuation-only tokens between it and the code
/// ("کد : 1234") are kept, separated by optional whitespace.
fn label_pattern(pre: &str) -> Option<String> {
    let tokens: Vec<&str> = pre.split_whitespace().collect();
    let anchor = tokens
        .iter()
        .rposition(|token| token.chars().any(char::is_alphanumeric))?;
    if anchor + 1 == tokens.len() {
        return Some(format!("{}\\s+", generalize_token(tokens[anchor])));
    }
    let mut out = generalize_token(tokens[anchor]);
    for token in &tokens[anchor + 1..] {
        out.push_str("\\s*");
        out.push_str(&generalize_token(token));
    }
    out.push_str("\\s*");
    Some(out)
}

pub fn derive_pattern(body: &str, code: &str) -> Result<String, PatternError> {
    let body = normalize_digits(body);
    let code = normalize_digits(code);
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(PatternError::InvalidCode(code));
    }
    let chars: Vec<char> = body.chars().collect();
    let needle: Vec<char> = code.chars().collect();
    let n = needle.len();
    let mut start = None;
    for i in 0..chars.len().saturating_sub(n - 1) {
        if chars[i..i + n] != needle[..] {
            continue;
        }
        let before_free = i == 0 || !chars[i - 1].is_ascii_digit();
        let after_free = i + n == chars.len() || !chars[i + n].is_ascii_digit();
        if before_free && after_free {
            start = Some(i);
            break;
        }
    }
    let s = start.ok_or_else(|| PatternError::CodeNotFound(code.clone()))?;
    let pre: String = chars[..s].iter().collect();
    let post: String = chars[s + n..].iter().collect();
    let pattern = match label_pattern(&pre) {
        Some(label) => format!("{label}(\\d{{4,8}})"),
        None => match post.split_whitespace().next() {
            Some(word) => format!("(\\d{{4,8}})\\s+{}", generalize_token(word)),
            None => return Err(PatternError::NoContext(code)),
        },
    };

    let re = validate_pattern(&pattern)?;
    match re.captures(&body) {
        Some(caps) if caps.get(1).map(|m| m.as_str()) == Some(code.as_str()) => Ok(pattern),
        _ => Err(PatternError::Invalid(format!(
            "derived pattern failed self-test: {pattern}"
        ))),
    }
}

pub fn extract_code<'a>(
    patterns: &'a [CompiledPattern],
    body: &str,
) -> Option<(&'a CompiledPattern, String)> {
    let normalized = normalize_digits(body);
    patterns.iter().find_map(|p| {
        let code = p.regex.captures(&normalized)?.get(1)?.as_str().to_string();
        Some((p, code))
    })
}

pub fn looks_like_otp(body: &str) -> bool {
    let normalized = normalize_digits(body);
    OTP_KEYWORD.is_match(&normalized) && OTP_DIGITS.is_match(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPPFOOD_PASTE: &str = "[8/20/26 7:12 PM] Sms: بلو\nبفرمایید رمز پویا\nخرید\nاسنپ فود\nمبلغ: 11,400,000 ریال\nرمز: 022761\n19:21";
    const SNAPPFOOD_REAL_BODY: &str =
        "بلو\nبفرمایید رمز پویا\nخرید\nاسنپ پي\nمبلغ: 48,322,100 ریال\nرمز: 402332\n01:48";
    const NORANGAR_BODY: &str = "Code: 92239\nکد تائید ورود\nنورنگار\nلغو11";

    #[test]
    fn punctuation_before_the_code_walks_back_to_the_label() {
        for (body, code, matching, other) in [
            ("کد : 1234", "1234", ["کد : 5555", "کد: 5555"], "کد 5555"),
            (
                "Code : 1234",
                "1234",
                ["Code : 5555", "Code: 5555"],
                "Code 5555",
            ),
        ] {
            let p = derive_pattern(body, code).unwrap();
            let re = validate_pattern(&p).unwrap();
            for text in matching {
                assert_eq!(&re.captures(text).unwrap()[1], "5555", "{p} on {text}");
            }
            assert_eq!(&re.captures(other).map(|c| c[1].to_string()), &None);
        }
    }

    #[test]
    fn no_letter_before_the_code_falls_back_to_the_following_word() {
        let p = derive_pattern(": 1234 is your login", "1234").unwrap();
        let re = validate_pattern(&p).unwrap();
        assert_eq!(&re.captures("9999 is your login").unwrap()[1], "9999");
    }

    #[test]
    fn derives_label_anchor_from_plain_sample() {
        let p = derive_pattern(
            "بلو\nبفرمایید رمز پویا\nخرید\nاسنپ فود\nمبلغ: 11,400,000 ریال\nرمز: 022761\n19:21",
            "022761",
        )
        .unwrap();
        let re = validate_pattern(&p).unwrap();
        assert_eq!(&re.captures(SNAPPFOOD_REAL_BODY).unwrap()[1], "402332");
    }

    #[test]
    fn derives_from_norangar_sample_with_trailing_context() {
        let p = derive_pattern(NORANGAR_BODY, "92239").unwrap();
        let re = validate_pattern(&p).unwrap();
        assert_eq!(
            &re.captures(&normalize_digits(NORANGAR_BODY)).unwrap()[1],
            "92239"
        );
    }

    #[test]
    fn amount_only_bodies_do_not_match() {
        let p = derive_pattern(SNAPPFOOD_PASTE, "022761").unwrap();
        let re = validate_pattern(&p).unwrap();
        assert!(re.captures("مبلغ: 999,111 ریال").is_none());
    }

    #[test]
    fn captures_code_not_trailing_amount() {
        let p = derive_pattern("رمز: 4422 مبلغ: 900000", "4422").unwrap();
        let re = validate_pattern(&p).unwrap();
        assert_eq!(&re.captures("رمز: 4422 مبلغ: 900000").unwrap()[1], "4422");
    }

    #[test]
    fn generalizes_digits_inside_label_token() {
        let p = derive_pattern("کد2: 556677", "556677").unwrap();
        let re = validate_pattern(&p).unwrap();
        assert_eq!(&re.captures("کد3: 111222").unwrap()[1], "111222");
    }

    #[test]
    fn errors_when_no_context_around_code() {
        assert!(matches!(
            derive_pattern("5544", "5544"),
            Err(PatternError::NoContext(_))
        ));
    }

    #[test]
    fn rejects_code_embedded_in_longer_number() {
        assert!(matches!(
            derive_pattern("ref 0227612 end", "022761"),
            Err(PatternError::CodeNotFound(_))
        ));
        assert!(matches!(
            derive_pattern("ref 0022761 end", "22761"),
            Err(PatternError::CodeNotFound(_))
        ));
    }

    #[test]
    fn rejects_non_digit_and_empty_codes() {
        assert!(matches!(
            derive_pattern("abc", "x1y"),
            Err(PatternError::InvalidCode(_))
        ));
        assert!(matches!(
            derive_pattern("abc", ""),
            Err(PatternError::InvalidCode(_))
        ));
    }

    #[test]
    fn derived_pattern_needs_letter_anchor() {
        assert!(matches!(
            derive_pattern("ref 12 556677", "556677"),
            Err(PatternError::NoAnchor)
        ));
    }

    #[test]
    fn tolerates_whitespace_differences() {
        let p = derive_pattern(NORANGAR_BODY, "92239").unwrap();
        let re = validate_pattern(&p).unwrap();
        let variant = "Code:   92239\r\n\r\nکد تائید ورود نورنگار لغو11";
        assert_eq!(&re.captures(variant).unwrap()[1], "92239");
    }

    #[test]
    fn derives_with_persian_digit_code() {
        let p = derive_pattern("کد: ۵۵۴۴", "۵۵۴۴").unwrap();
        let re = validate_pattern(&p).unwrap();
        assert_eq!(&re.captures("کد: 5544").unwrap()[1], "5544");
    }

    #[test]
    fn normalizes_persian_and_arabic_digits() {
        assert_eq!(normalize_digits("۰۱۲۳۴۵۶۷۸۹"), "0123456789");
        assert_eq!(normalize_digits("٠١٢٣٤٥٦٧٨٩"), "0123456789");
        assert_eq!(normalize_digits("رمز: ۵۵۴۴"), "رمز: 5544");
        assert_eq!(normalize_digits("Code: 123"), "Code: 123");
    }

    #[test]
    fn rejects_pattern_without_letter_anchor() {
        assert!(matches!(
            validate_pattern(r":\s+(\d{4,8})"),
            Err(PatternError::NoAnchor)
        ));
        assert!(validate_pattern(r"(?i)(?:رمز|کد|code)\s*:?\s*(\d{4,8})").is_ok());
        assert!(validate_pattern(r"رمز:\s+(\d{4,8})").is_ok());
    }

    #[test]
    fn flag_groups_are_not_letter_anchors() {
        assert!(matches!(
            validate_pattern(r"(?i):\s+(\d{4,8})"),
            Err(PatternError::NoAnchor)
        ));
    }

    #[test]
    fn rejects_pattern_with_zero_or_two_groups() {
        assert!(validate_pattern(r"رمز:\s+\d{4,8}").is_err());
        assert!(validate_pattern(r"(رمز):\s+(\d{4,8})").is_err());
    }

    #[test]
    fn extracts_from_persian_digits() {
        let p = vec![CompiledPattern {
            id: 1,
            name: "ramz".into(),
            regex: validate_pattern(r"رمز:\s+(\d{4,8})").unwrap(),
        }];
        let (hit, code) = extract_code(&p, "رمز: ۴۰۲۳۳۲").unwrap();
        assert_eq!((hit.id, code.as_str()), (1, "402332"));
    }

    #[test]
    fn first_matching_pattern_wins() {
        let p = vec![
            CompiledPattern {
                id: 1,
                name: "ramz".into(),
                regex: validate_pattern(r"رمز:\s+(\d{4,8})").unwrap(),
            },
            CompiledPattern {
                id: 2,
                name: "generic".into(),
                regex: validate_pattern(r"(?i)(?:code)\s+(\d{4,8})").unwrap(),
            },
        ];
        let (hit, code) = extract_code(&p, "code 4433 رمز: 5544").unwrap();
        assert_eq!((hit.name.as_str(), code.as_str()), ("ramz", "5544"));
    }

    #[test]
    fn falls_back_to_later_patterns() {
        let p = vec![
            CompiledPattern {
                id: 1,
                name: "ramz".into(),
                regex: validate_pattern(r"رمز:\s+(\d{4,8})").unwrap(),
            },
            CompiledPattern {
                id: 2,
                name: "generic".into(),
                regex: validate_pattern(r"(?i)(?:code)\s+(\d{4,8})").unwrap(),
            },
        ];
        let (hit, code) = extract_code(&p, "code 778899").unwrap();
        assert_eq!((hit.id, code.as_str()), (2, "778899"));
    }

    #[test]
    fn no_match_returns_none() {
        let p = vec![CompiledPattern {
            id: 1,
            name: "ramz".into(),
            regex: validate_pattern(r"رمز:\s+(\d{4,8})").unwrap(),
        }];
        assert!(extract_code(&p, "hello there").is_none());
    }

    #[test]
    fn looks_like_otp_needs_keyword_and_standalone_digits() {
        assert!(looks_like_otp("کد ورود شما 5388 است"));
        assert!(looks_like_otp("Your code is ۲۴۲۱۳۵"));
        assert!(!looks_like_otp("مبلغ 11400000 ریال"));
        assert!(!looks_like_otp("کد تخفیف AVAL"));
        assert!(!looks_like_otp("code 123"));
    }
}
