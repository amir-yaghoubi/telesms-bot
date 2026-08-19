use std::env;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_bot_token: String,
    pub telegram_user_id: i64,
    pub telegram_group_id: i64,
    pub modem_uid: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub google_token_path: PathBuf,
    pub database_path: PathBuf,
    pub contacts_sync_interval: Duration,
    pub default_region: String,
    pub status_tz: chrono_tz::Tz,
    pub sms_delete_enabled: bool,
    pub sms_delete_max_age: Duration,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required env var: {0}")]
    Missing(&'static str),
    #[error("invalid value for {key}: {source}")]
    Invalid {
        key: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl Config {
    pub fn from_env() -> Result<Config, ConfigError> {
        Ok(Config {
            telegram_bot_token: required("TELEGRAM_BOT_TOKEN")?,
            telegram_user_id: parse_required("TELEGRAM_USER_ID")?,
            telegram_group_id: parse_required("TELEGRAM_GROUP_ID")?,
            modem_uid: required("MODEM_UID")?,
            google_client_id: required("GOOGLE_CLIENT_ID")?,
            google_client_secret: required("GOOGLE_CLIENT_SECRET")?,
            google_token_path: PathBuf::from(
                env::var("GOOGLE_TOKEN_PATH")
                    .unwrap_or_else(|_| "./secrets/google-token.json".to_string()),
            ),
            database_path: PathBuf::from(
                env::var("DATABASE_PATH")
                    .unwrap_or_else(|_| "./data/telesms.sqlite".to_string()),
            ),
            contacts_sync_interval: Duration::from_secs(
                env::var("CONTACTS_SYNC_INTERVAL_SECS")
                    .ok()
                    .map(|v| {
                        v.parse::<u64>().map_err(|e| ConfigError::Invalid {
                            key: "CONTACTS_SYNC_INTERVAL_SECS",
                            source: Box::new(e),
                        })
                    })
                    .transpose()?
                    .unwrap_or(21600),
            ),
            default_region: env::var("DEFAULT_REGION").unwrap_or_else(|_| "IR".to_string()),
            status_tz: parse_tz(env::var("STATUS_TZ").ok())?,
            sms_delete_enabled: match env::var("SMS_DELETE_ENABLED") {
                Err(_) => true,
                Ok(v) => parse_sms_delete_enabled(&v)?,
            },
            sms_delete_max_age: Duration::from_secs(
                match env::var("SMS_DELETE_MAX_AGE_DAYS") {
                    Err(_) => 30,
                    Ok(v) if v.trim().is_empty() => 30,
                    Ok(v) => v.parse::<u64>().map_err(|e| ConfigError::Invalid {
                        key: "SMS_DELETE_MAX_AGE_DAYS",
                        source: Box::new(e),
                    })?,
                } * 86400,
            ),
        })
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
struct ParseError(String);

fn parse_tz(raw: Option<String>) -> Result<chrono_tz::Tz, ConfigError> {
    match raw {
        None => Ok(chrono_tz::Asia::Tehran),
        Some(s) if s.is_empty() => Ok(chrono_tz::Asia::Tehran),
        Some(s) => s.parse::<chrono_tz::Tz>().map_err(|e| ConfigError::Invalid {
            key: "STATUS_TZ",
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            )),
        }),
    }
}

fn parse_sms_delete_enabled(raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(ConfigError::Invalid {
            key: "SMS_DELETE_ENABLED",
            source: Box::new(ParseError(
                "expected true/1/yes or false/0/no".into(),
            )),
        }),
    }
}

fn required(key: &'static str) -> Result<String, ConfigError> {
    env::var(key).map_err(|_| ConfigError::Missing(key))
}

fn parse_required<T: std::str::FromStr>(key: &'static str) -> Result<T, ConfigError>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    required(key)?
        .parse()
        .map_err(|e| ConfigError::Invalid {
            key,
            source: Box::new(e),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_required_env() {
        std::env::set_var("TELEGRAM_BOT_TOKEN", "tok");
        std::env::set_var("TELEGRAM_USER_ID", "42");
        std::env::set_var("TELEGRAM_GROUP_ID", "-1001");
        std::env::set_var("MODEM_UID", "dwm222");
        std::env::set_var("GOOGLE_CLIENT_ID", "cid");
        std::env::set_var("GOOGLE_CLIENT_SECRET", "sec");
        // Clear optionals so prior tests cannot leak invalid/non-default values.
        std::env::remove_var("GOOGLE_TOKEN_PATH");
        std::env::remove_var("DATABASE_PATH");
        std::env::remove_var("CONTACTS_SYNC_INTERVAL_SECS");
        std::env::remove_var("DEFAULT_REGION");
        std::env::remove_var("SMS_DELETE_ENABLED");
        std::env::remove_var("SMS_DELETE_MAX_AGE_DAYS");
        std::env::remove_var("STATUS_TZ");
    }

    #[test]
    fn from_env_reads_required_fields() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TELEGRAM_BOT_TOKEN", "tok");
        std::env::set_var("TELEGRAM_USER_ID", "42");
        std::env::set_var("TELEGRAM_GROUP_ID", "-1001");
        std::env::set_var("MODEM_UID", "dwm222");
        std::env::set_var("GOOGLE_CLIENT_ID", "cid");
        std::env::set_var("GOOGLE_CLIENT_SECRET", "sec");
        std::env::remove_var("GOOGLE_TOKEN_PATH");
        std::env::remove_var("DATABASE_PATH");
        std::env::remove_var("CONTACTS_SYNC_INTERVAL_SECS");
        std::env::remove_var("DEFAULT_REGION");
        std::env::remove_var("SMS_DELETE_ENABLED");
        std::env::remove_var("SMS_DELETE_MAX_AGE_DAYS");
        std::env::remove_var("STATUS_TZ");
        let c = Config::from_env().unwrap();
        assert_eq!(c.telegram_user_id, 42);
        assert_eq!(c.modem_uid, "dwm222");
        assert_eq!(c.database_path.as_os_str(), "./data/telesms.sqlite");
        assert_eq!(c.google_token_path.as_os_str(), "./secrets/google-token.json");
        assert_eq!(c.contacts_sync_interval.as_secs(), 21600);
        assert_eq!(c.default_region, "IR");
        assert_eq!(c.status_tz, chrono_tz::Asia::Tehran);
        assert!(c.sms_delete_enabled);
        assert_eq!(c.sms_delete_max_age.as_secs(), 30 * 86400);
    }

    #[test]
    fn from_env_reads_status_tz() {
        let _g = ENV_LOCK.lock().unwrap();
        set_required_env();
        std::env::set_var("STATUS_TZ", "UTC");
        let c = Config::from_env().unwrap();
        assert_eq!(c.status_tz, chrono_tz::UTC);
        std::env::remove_var("STATUS_TZ");
    }

    #[test]
    fn from_env_rejects_bad_status_tz() {
        let _g = ENV_LOCK.lock().unwrap();
        set_required_env();
        std::env::set_var("STATUS_TZ", "NotAZone");
        let err = Config::from_env().unwrap_err();
        std::env::remove_var("STATUS_TZ");
        match err {
            ConfigError::Invalid { key, .. } => assert_eq!(key, "STATUS_TZ"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sms_delete_enabled_defaults_true() {
        let _g = ENV_LOCK.lock().unwrap();
        set_required_env();
        std::env::remove_var("SMS_DELETE_ENABLED");
        std::env::remove_var("SMS_DELETE_MAX_AGE_DAYS");
        let c = Config::from_env().unwrap();
        assert!(c.sms_delete_enabled);
        assert_eq!(c.sms_delete_max_age.as_secs(), 30 * 86400);
    }

    #[test]
    fn sms_delete_enabled_false_aliases() {
        let _g = ENV_LOCK.lock().unwrap();
        set_required_env();
        for v in ["false", "0", "no", "NO", "False"] {
            std::env::set_var("SMS_DELETE_ENABLED", v);
            let c = Config::from_env().unwrap();
            assert!(!c.sms_delete_enabled, "value {v}");
        }
        for v in ["true", "1", "yes", "YES", ""] {
            std::env::set_var("SMS_DELETE_ENABLED", v);
            assert!(Config::from_env().unwrap().sms_delete_enabled, "value {v:?}");
        }
    }

    #[test]
    fn sms_delete_enabled_rejects_garbage() {
        let _g = ENV_LOCK.lock().unwrap();
        set_required_env();
        std::env::set_var("SMS_DELETE_ENABLED", "maybe");
        match Config::from_env() {
            Err(ConfigError::Invalid { key, .. }) => assert_eq!(key, "SMS_DELETE_ENABLED"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sms_delete_max_age_parses_days() {
        let _g = ENV_LOCK.lock().unwrap();
        set_required_env();
        std::env::remove_var("SMS_DELETE_ENABLED");
        std::env::set_var("SMS_DELETE_MAX_AGE_DAYS", "7");
        let c = Config::from_env().unwrap();
        assert_eq!(c.sms_delete_max_age.as_secs(), 7 * 86400);
    }

    #[test]
    fn sms_delete_max_age_rejects_garbage() {
        let _g = ENV_LOCK.lock().unwrap();
        set_required_env();
        std::env::set_var("SMS_DELETE_MAX_AGE_DAYS", "abc");
        match Config::from_env() {
            Err(ConfigError::Invalid { key, .. }) => {
                assert_eq!(key, "SMS_DELETE_MAX_AGE_DAYS")
            }
            other => panic!("{other:?}"),
        }
    }
}
