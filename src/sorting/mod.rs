pub mod classifier;
pub mod config;
pub mod decide;
pub mod guard;
pub mod learn;
pub mod render;
pub mod sweep;

use std::path::Path;
use std::sync::Arc;

use crate::config::ConfigError;
use crate::db::{Db, DbError};
use crate::otp::{validate_pattern, CompiledPattern};
use classifier::{Classifier, SystemOneClassifier};
use config::SortingConfig;
use guard::GuardedClassifier;

/// Callback data of the button that teaches an OTP Pattern from its own message.
pub const TEACH_CALLBACK: &str = "otp:teach";

/// What the inbound flow needs to sort a message: the sort config and, when
/// one is configured, the Classifier.
#[derive(Clone)]
pub struct Sorting {
    pub config: Arc<SortingConfig>,
    pub classifier: Option<Arc<dyn Classifier>>,
}

impl Sorting {
    /// Embedded config and no Classifier: OTP Codes are still found and sorted.
    pub fn disabled() -> Self {
        Self {
            config: Arc::new(SortingConfig::load(None).expect("embedded sorting config")),
            classifier: None,
        }
    }

    /// The sort config from `config_path` (or the embedded one) and, when an
    /// endpoint and model are given, a guarded System One Classifier.
    pub fn build(
        classifier: Option<(&str, &str)>,
        config_path: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let config = Arc::new(SortingConfig::load(config_path)?);
        let classifier = classifier.map(|(url, model)| {
            let inner = Arc::new(SystemOneClassifier::new(
                url.to_string(),
                model.to_string(),
                &config,
            ));
            let guarded =
                GuardedClassifier::new(inner, config.retry.clone(), config.breaker.clone());
            Arc::new(guarded) as Arc<dyn Classifier>
        });
        Ok(Self { config, classifier })
    }

    /// The stored OTP Patterns, compiled. A pattern that no longer validates is skipped.
    pub fn patterns(&self, db: &Db) -> Result<Vec<CompiledPattern>, DbError> {
        let mut compiled = Vec::new();
        for row in db.otp_patterns()? {
            match validate_pattern(&row.pattern) {
                Ok(regex) => compiled.push(CompiledPattern {
                    id: row.id,
                    name: row.name,
                    regex,
                }),
                Err(err) => {
                    tracing::warn!(id = row.id, name = %row.name, error = %err, "skip invalid otp pattern");
                }
            }
        }
        Ok(compiled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_wires_the_classifier_only_when_configured() {
        let off = Sorting::build(None, None).unwrap();
        assert!(off.classifier.is_none());
        let on = Sorting::build(Some(("http://localhost:1/x", "m")), None).unwrap();
        assert!(on.classifier.is_some());
        assert!(Sorting::build(None, Some(Path::new("/nonexistent/sorting.json"))).is_err());
    }

    #[test]
    fn patterns_skip_invalid_stored_rows() {
        let db = Db::open_in_memory().unwrap();
        db.add_otp_pattern(Some("good"), r"رمز:\s+(\d{4,8})")
            .unwrap();
        db.add_otp_pattern(Some("bad"), r"(\d{4,8})(").unwrap();
        db.add_otp_pattern(Some("anchorless"), r"(\d{4,8})")
            .unwrap();
        let patterns = Sorting::disabled().patterns(&db).unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].name, "good");
    }
}
