use crate::db::Classification;
use crate::sorting::config::SortingConfig;

/// The Sort a message is threaded under, or `None` for General.
pub fn decide_sort(
    c: Option<&Classification>,
    otp_code: Option<&str>,
    cfg: &SortingConfig,
) -> Option<String> {
    let name = match c {
        Some(c) if c.tags.iter().any(|t| t == "scam") => "scam".to_string(),
        _ if otp_code.is_some() => "otp".to_string(),
        Some(c) if c.confidence < cfg.thresholds.min_conf => return None,
        Some(c) => c.category.clone(),
        None => return None,
    };
    cfg.sorts.get(&name).filter(|s| s.thread).map(|_| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SortingConfig {
        SortingConfig::load(None).unwrap()
    }

    fn cls(category: &str, confidence: f64, tags: &[&str]) -> Classification {
        Classification {
            category: category.to_string(),
            confidence,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn scam_beats_otp() {
        let c = cls("promotional", 0.95, &["scam"]);
        assert_eq!(
            decide_sort(Some(&c), Some("402332"), &cfg()),
            Some("scam".to_string())
        );
    }

    #[test]
    fn otp_beats_category_and_low_confidence() {
        let c = cls("promotional", 0.2, &[]);
        assert_eq!(
            decide_sort(Some(&c), Some("402332"), &cfg()),
            Some("otp".to_string())
        );
    }

    #[test]
    fn low_confidence_stays_general() {
        let c = cls("promotional", 0.4, &[]);
        assert_eq!(decide_sort(Some(&c), None, &cfg()), None);
    }

    #[test]
    fn category_without_thread_stays_general() {
        let c = cls("transactional", 0.9, &[]);
        assert_eq!(decide_sort(Some(&c), None, &cfg()), None);
    }

    #[test]
    fn category_with_thread_is_sorted() {
        let c = cls("broadcast", 0.9, &[]);
        assert_eq!(
            decide_sort(Some(&c), None, &cfg()),
            Some("broadcast".to_string())
        );
    }

    #[test]
    fn unclassified_with_code_sorts_otp() {
        assert_eq!(
            decide_sort(None, Some("402332"), &cfg()),
            Some("otp".to_string())
        );
    }

    #[test]
    fn unclassified_without_code_is_general() {
        assert_eq!(decide_sort(None, None, &cfg()), None);
    }
}
