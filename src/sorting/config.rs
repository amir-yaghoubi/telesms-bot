use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::config::ConfigError;

#[derive(Clone, Debug, Deserialize)]
pub struct SortingConfig {
    pub questions: serde_json::Value,
    pub thresholds: Thresholds,
    pub retry: RetryPolicy,
    pub breaker: BreakerPolicy,
    pub sweep: SweepPolicy,
    pub sorts: BTreeMap<String, SortSpec>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Thresholds {
    pub scam: f64,
    pub min_conf: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub timeout_secs: u64,
    pub backoff_secs: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BreakerPolicy {
    pub failures: u32,
    pub cooldown_secs: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SweepPolicy {
    pub interval_secs: u64,
    pub window_days: i64,
    pub max_attempts: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SortSpec {
    pub thread: bool,
    pub title: String,
    pub emoji: String,
    pub muted: bool,
}

const DEFAULT_JSON: &str = include_str!("default.json");

impl SortingConfig {
    /// Loads the built-in defaults, or a complete config file (no partial merge).
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let invalid = |e: Box<dyn std::error::Error + Send + Sync>| ConfigError::Invalid {
            key: "SORTING_CONFIG_PATH",
            source: e,
        };
        let raw = match path {
            None => DEFAULT_JSON.to_string(),
            Some(p) => std::fs::read_to_string(p).map_err(|e| invalid(Box::new(e)))?,
        };
        serde_json::from_str(&raw).map_err(|e| invalid(Box::new(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_loads() {
        let c = SortingConfig::load(None).unwrap();
        assert!(c.sorts["promotional"].muted);
        assert!(!c.sorts["otp"].muted);
        assert!(!c.sorts["transactional"].thread);
        assert_eq!(c.thresholds.scam, 0.8);
        assert_eq!(c.retry.backoff_secs, vec![1, 3]);
        assert!(c.questions["category"]["criteria"].get("otp").is_none());
        assert!(c.questions.get("has_otp").is_none());
    }

    #[test]
    fn default_config_matches_spec_values() {
        let c = SortingConfig::load(None).unwrap();
        assert_eq!(c.thresholds.min_conf, 0.5);
        assert_eq!((c.retry.attempts, c.retry.timeout_secs), (3, 10));
        assert_eq!((c.breaker.failures, c.breaker.cooldown_secs), (3, 300));
        assert_eq!(
            (
                c.sweep.interval_secs,
                c.sweep.window_days,
                c.sweep.max_attempts
            ),
            (300, 7, 5)
        );
        assert_eq!(c.sorts.len(), 8);
        assert_eq!(c.sorts["scam"].emoji, "🟥");
        assert_eq!(c.sorts["other"].title, "معمولی");
        assert!(c.questions["is_scam"].is_object());
    }

    #[test]
    fn file_must_parse_fully() {
        let dir = std::env::temp_dir().join(format!("sorting-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let partial = dir.join("partial.json");
        std::fs::write(&partial, r#"{"thresholds":{"scam":0.9,"min_conf":0.4}}"#).unwrap();
        assert!(SortingConfig::load(Some(&partial)).is_err());
        assert!(SortingConfig::load(Some(&dir.join("missing.json"))).is_err());

        let full = dir.join("full.json");
        let mut v: serde_json::Value = serde_json::from_str(DEFAULT_JSON).unwrap();
        v["thresholds"]["scam"] = serde_json::json!(0.9);
        std::fs::write(&full, v.to_string()).unwrap();
        assert_eq!(
            SortingConfig::load(Some(&full)).unwrap().thresholds.scam,
            0.9
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
