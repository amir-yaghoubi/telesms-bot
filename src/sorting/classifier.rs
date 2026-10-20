use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use thiserror::Error;

use crate::db::{Classification, StoredClassification};
use crate::sorting::config::SortingConfig;

#[derive(Debug, Error)]
pub enum ClassifyError {
    #[error("classifier request failed: {0}")]
    Http(String),
    #[error("classifier request timed out")]
    Timeout,
    #[error("malformed classifier answer: {0}")]
    Malformed(String),
    #[error("classifier circuit breaker is open")]
    BreakerOpen,
}

#[async_trait]
pub trait Classifier: Send + Sync {
    async fn classify(&self, e164: &str, body: &str)
        -> Result<StoredClassification, ClassifyError>;
}

pub struct SystemOneClassifier {
    url: String,
    model: String,
    questions: serde_json::Value,
    scam_threshold: f64,
    http: reqwest::Client,
}

impl SystemOneClassifier {
    pub fn new(url: String, model: String, cfg: &SortingConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.retry.timeout_secs))
            .build()
            .unwrap_or_default();
        Self {
            url,
            model,
            questions: cfg.questions.clone(),
            scam_threshold: cfg.thresholds.scam,
            http,
        }
    }
}

static LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:https?://)?[\w.-]+\.(?:ir|com|net|org|link|me|io|app)(?:/\S*)?").unwrap()
});
static OPTOUT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"لغو\s*[1۱]{2}").unwrap());
// 0998x/0999x ranges are SMS panel lines, not personal SIMs.
static PERSONAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\+989(?:[0-8]\d|9[0-7])\d{7}$").unwrap());

fn sender_kind(sender: &str) -> &'static str {
    if !sender.starts_with('+') {
        "alphanumeric brand id"
    } else if PERSONAL_RE.is_match(sender) {
        "personal mobile number"
    } else {
        "bulk SMS panel number"
    }
}

pub fn build_state(e164: &str, body: &str) -> serde_json::Value {
    let links: Vec<&str> = LINK_RE.find_iter(body).map(|m| m.as_str()).collect();
    serde_json::json!({
        "sender": e164,
        "sender_kind": sender_kind(e164),
        "text": body,
        "links": links,
        "has_optout_marker": OPTOUT_RE.is_match(body),
    })
}

fn parse_answers(
    answers: &serde_json::Value,
    scam_threshold: f64,
) -> Result<StoredClassification, ClassifyError> {
    let malformed = |what: &str| ClassifyError::Malformed(what.to_string());
    let cat = &answers["category"];
    let category = cat["choice"]
        .as_str()
        .ok_or_else(|| malformed("category.choice"))?;
    let confidence = cat["confidence"]
        .as_f64()
        .ok_or_else(|| malformed("category.confidence"))?;
    let probabilities = cat["probabilities"]
        .as_object()
        .ok_or_else(|| malformed("category.probabilities"))?;
    let scam = answers["is_scam"]["noul"]
        .as_f64()
        .ok_or_else(|| malformed("is_scam.noul"))?;
    let tags = if scam >= scam_threshold {
        vec!["scam".to_string()]
    } else {
        Vec::new()
    };
    Ok(StoredClassification {
        public: Classification {
            category: category.to_string(),
            confidence,
            tags,
        },
        scores: serde_json::json!({ "probabilities": probabilities, "is_scam": scam }),
    })
}

#[async_trait]
impl Classifier for SystemOneClassifier {
    async fn classify(
        &self,
        e164: &str,
        body: &str,
    ) -> Result<StoredClassification, ClassifyError> {
        let req = serde_json::json!({
            "model": self.model,
            "state": build_state(e164, body),
            "questions": self.questions,
        });
        let resp = self
            .http
            .post(&self.url)
            .json(&req)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(map_http_err)?;
        let json: serde_json::Value = resp.json().await.map_err(|e| {
            if e.is_timeout() {
                ClassifyError::Timeout
            } else {
                ClassifyError::Malformed(e.to_string())
            }
        })?;
        parse_answers(&json["answers"], self.scam_threshold)
    }
}

fn map_http_err(e: reqwest::Error) -> ClassifyError {
    if e.is_timeout() {
        ClassifyError::Timeout
    } else {
        ClassifyError::Http(e.to_string())
    }
}

/// Scripted classifier for tests: pops one answer per call.
pub struct FakeClassifier {
    pub answers: Mutex<VecDeque<Result<StoredClassification, ClassifyError>>>,
    pub calls: AtomicUsize,
}

impl FakeClassifier {
    pub fn new(answers: Vec<Result<StoredClassification, ClassifyError>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Classifier for FakeClassifier {
    async fn classify(
        &self,
        _e164: &str,
        _body: &str,
    ) -> Result<StoredClassification, ClassifyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(ClassifyError::Http("no scripted answer".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Classification;
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::Arc;

    type Seen = Arc<Mutex<Vec<Value>>>;

    async fn serve(reply: Value) -> (String, Seen) {
        let seen: Seen = Arc::default();
        let app = Router::new()
            .route(
                "/v1/systemone",
                post(
                    |State((reply, seen)): State<(Value, Seen)>, Json(req): Json<Value>| async move {
                        seen.lock().unwrap().push(req);
                        Json(reply)
                    },
                ),
            )
            .with_state((reply, seen.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/v1/systemone"), seen)
    }

    fn client(url: String) -> SystemOneClassifier {
        let cfg = SortingConfig::load(None).unwrap();
        SystemOneClassifier::new(url, "m.gguf".into(), &cfg)
    }

    #[test]
    fn sender_kind_matches_script() {
        let kind = |s: &str| {
            build_state(s, "x")["sender_kind"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(kind("+989121234567"), "personal mobile number");
        assert_eq!(kind("+989981600237"), "bulk SMS panel number");
        assert_eq!(kind("+989991600237"), "bulk SMS panel number");
        assert_eq!(kind("+9890006864"), "bulk SMS panel number");
        assert_eq!(kind("Bank"), "alphanumeric brand id");
    }

    #[test]
    fn build_state_extracts_links_and_optout() {
        let s = build_state("Bank", "see shop.example.ir/x now لغو ۱۱");
        assert_eq!(s["sender"], "Bank");
        assert_eq!(s["links"], json!(["shop.example.ir/x"]));
        assert_eq!(s["has_optout_marker"], true);
        let s = build_state("Bank", "hello");
        assert_eq!(s["links"], json!([]));
        assert_eq!(s["has_optout_marker"], false);
    }

    #[tokio::test]
    async fn parses_systemone_answers() {
        let (url, seen) = serve(json!({"answers": {
            "category": {"choice": "promotional", "confidence": 0.542,
                "probabilities": {"promotional": 0.608, "other": 0.136}},
            "is_scam": {"noul": 0.85}
        }}))
        .await;
        let got = client(url).classify("Bank", "تا ۵۰٪ تخفیف").await.unwrap();
        assert_eq!(
            got.public,
            Classification {
                category: "promotional".into(),
                confidence: 0.542,
                tags: vec!["scam".into()],
            }
        );
        assert_eq!(got.scores["is_scam"], 0.85);
        assert_eq!(got.scores["probabilities"]["promotional"], 0.608);

        let req = seen.lock().unwrap()[0].clone();
        assert_eq!(req["model"], "m.gguf");
        assert_eq!(req["state"]["sender_kind"], "alphanumeric brand id");
        assert_eq!(req["state"]["text"], "تا ۵۰٪ تخفیف");
        assert!(req["questions"]["category"].is_object());
        assert!(req["questions"].get("has_otp").is_none());
    }

    #[tokio::test]
    async fn below_scam_threshold_has_no_tags() {
        let (url, _) = serve(json!({"answers": {
            "category": {"choice": "other", "confidence": 0.9, "probabilities": {"other": 0.9}},
            "is_scam": {"noul": 0.79}
        }}))
        .await;
        let got = client(url).classify("Bank", "hi").await.unwrap();
        assert!(got.public.tags.is_empty());
    }

    #[tokio::test]
    async fn malformed_answers_are_an_error() {
        let (url, _) = serve(json!({"answers": {"category": {}}})).await;
        let err = client(url).classify("Bank", "hi").await.unwrap_err();
        assert!(matches!(err, ClassifyError::Malformed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn unreachable_server_is_http_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let err = client(format!("http://{addr}/v1/systemone"))
            .classify("Bank", "hi")
            .await
            .unwrap_err();
        assert!(matches!(err, ClassifyError::Http(_)), "{err:?}");
    }

    #[tokio::test]
    async fn fake_classifier_pops_scripted_answers() {
        let ok = StoredClassification {
            public: Classification {
                category: "other".into(),
                confidence: 1.0,
                tags: vec![],
            },
            scores: json!({}),
        };
        let f = FakeClassifier::new(vec![Err(ClassifyError::Timeout), Ok(ok)]);
        assert!(matches!(
            f.classify("a", "b").await,
            Err(ClassifyError::Timeout)
        ));
        assert!(f.classify("a", "b").await.is_ok());
        assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    }
}
