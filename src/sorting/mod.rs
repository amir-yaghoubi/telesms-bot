pub mod classifier;
pub mod config;
pub mod decide;
pub mod guard;
pub mod render;

use std::sync::Arc;

use crate::db::{Db, DbError};
use crate::otp::{validate_pattern, CompiledPattern};
use classifier::Classifier;
use config::SortingConfig;

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
