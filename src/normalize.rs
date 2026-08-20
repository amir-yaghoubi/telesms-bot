use phonenumber::{country, Mode};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NormalizeError {
    #[error("empty phone number")]
    Empty,
    #[error("invalid phone number: {raw}")]
    Invalid { raw: String },
}

pub fn normalize_e164(raw: &str, default_region: &str) -> Result<String, NormalizeError> {
    let cleaned: String = raw.chars().filter(|c| *c != ' ' && *c != '-').collect();

    if cleaned.is_empty() {
        return Err(NormalizeError::Empty);
    }

    let to_parse = if cleaned.starts_with("989") {
        format!("+{cleaned}")
    } else {
        cleaned
    };

    let region = default_region.parse::<country::Id>().ok();
    let number = phonenumber::parse(region, &to_parse).map_err(|_| NormalizeError::Invalid {
        raw: raw.to_string(),
    })?;

    Ok(number.format().mode(Mode::E164).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_national_09() {
        assert_eq!(
            normalize_e164("09121234567", "IR").unwrap(),
            "+989121234567"
        );
    }

    #[test]
    fn ir_without_plus() {
        assert_eq!(
            normalize_e164("989121234567", "IR").unwrap(),
            "+989121234567"
        );
    }

    #[test]
    fn already_e164() {
        assert_eq!(
            normalize_e164("+989121234567", "IR").unwrap(),
            "+989121234567"
        );
    }

    #[test]
    fn other_plus_kept() {
        assert_eq!(
            normalize_e164("+14155552671", "IR").unwrap(),
            "+14155552671"
        );
    }

    #[test]
    fn garbage_err() {
        assert!(matches!(
            normalize_e164("hello", "IR"),
            Err(NormalizeError::Invalid { .. })
        ));
    }

    #[test]
    fn empty_err() {
        assert!(matches!(
            normalize_e164("  ", "IR"),
            Err(NormalizeError::Empty)
        ));
    }
}
