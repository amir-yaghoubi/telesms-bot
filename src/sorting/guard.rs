use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::db::StoredClassification;
use crate::sorting::classifier::{Classifier, ClassifyError};
use crate::sorting::config::{BreakerPolicy, RetryPolicy};

#[derive(Default)]
struct BreakerState {
    failures: u32,
    opened_at: Option<Instant>,
    trial_in_flight: bool,
}

/// Wraps a classifier with per-attempt timeouts, retries and a circuit breaker.
pub struct GuardedClassifier {
    inner: Arc<dyn Classifier>,
    retry: RetryPolicy,
    breaker: BreakerPolicy,
    state: Mutex<BreakerState>,
}

impl GuardedClassifier {
    pub fn new(inner: Arc<dyn Classifier>, retry: RetryPolicy, breaker: BreakerPolicy) -> Self {
        Self {
            inner,
            retry,
            breaker,
            state: Mutex::new(BreakerState::default()),
        }
    }
}

impl GuardedClassifier {
    /// Decides whether a call may proceed. `Ok(true)` means it is the half-open trial.
    fn admit(&self) -> Result<bool, ClassifyError> {
        let mut st = self.state.lock().unwrap();
        let Some(opened_at) = st.opened_at else {
            return Ok(false);
        };
        let cooldown = Duration::from_secs(self.breaker.cooldown_secs);
        if opened_at.elapsed() < cooldown || st.trial_in_flight {
            return Err(ClassifyError::BreakerOpen);
        }
        st.trial_in_flight = true;
        Ok(true)
    }

    fn record(&self, ok: bool) {
        let mut st = self.state.lock().unwrap();
        st.trial_in_flight = false;
        if ok {
            st.failures = 0;
            st.opened_at = None;
            return;
        }
        st.failures += 1;
        if st.opened_at.is_some() || st.failures >= self.breaker.failures {
            st.opened_at = Some(Instant::now());
        }
    }

    async fn attempts(
        &self,
        e164: &str,
        body: &str,
    ) -> Result<StoredClassification, ClassifyError> {
        let timeout = Duration::from_secs(self.retry.timeout_secs);
        let attempts = self.retry.attempts.max(1);
        let mut last = ClassifyError::Timeout;
        for i in 0..attempts {
            if i > 0 {
                let idx = (i as usize - 1).min(self.retry.backoff_secs.len().saturating_sub(1));
                if let Some(secs) = self.retry.backoff_secs.get(idx) {
                    tokio::time::sleep(Duration::from_secs(*secs)).await;
                }
            }
            match tokio::time::timeout(timeout, self.inner.classify(e164, body)).await {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(e)) => last = e,
                Err(_) => last = ClassifyError::Timeout,
            }
        }
        Err(last)
    }
}

/// Clears the in-flight trial flag if the trial future is dropped before it records.
struct TrialGuard<'a> {
    state: &'a Mutex<BreakerState>,
    armed: bool,
}

impl Drop for TrialGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut st) = self.state.lock() {
                st.trial_in_flight = false;
            }
        }
    }
}

#[async_trait]
impl Classifier for GuardedClassifier {
    async fn classify(
        &self,
        e164: &str,
        body: &str,
    ) -> Result<StoredClassification, ClassifyError> {
        let trial = self.admit()?;
        let mut guard = TrialGuard {
            state: &self.state,
            armed: trial,
        };
        let result = self.attempts(e164, body).await;
        guard.armed = false;
        self.record(result.is_ok());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Classification;
    use crate::sorting::classifier::FakeClassifier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ok() -> StoredClassification {
        StoredClassification {
            public: Classification {
                category: "other".into(),
                confidence: 1.0,
                tags: vec![],
            },
            scores: serde_json::json!({}),
        }
    }

    fn http() -> Result<StoredClassification, ClassifyError> {
        Err(ClassifyError::Http("boom".into()))
    }

    fn retry() -> RetryPolicy {
        RetryPolicy {
            attempts: 3,
            timeout_secs: 10,
            backoff_secs: vec![1, 3],
        }
    }

    fn breaker() -> BreakerPolicy {
        BreakerPolicy {
            failures: 3,
            cooldown_secs: 300,
        }
    }

    fn guard(inner: Arc<dyn Classifier>) -> GuardedClassifier {
        GuardedClassifier::new(inner, retry(), breaker())
    }

    fn calls(f: &FakeClassifier) -> usize {
        f.calls.load(Ordering::SeqCst)
    }

    /// Inner classifier that never answers within the attempt timeout.
    struct Slow(AtomicUsize);

    #[async_trait]
    impl Classifier for Slow {
        async fn classify(&self, _: &str, _: &str) -> Result<StoredClassification, ClassifyError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(11)).await;
            Ok(ok())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_then_succeeds() {
        let fake = Arc::new(FakeClassifier::new(vec![http(), http(), Ok(ok())]));
        let g = guard(fake.clone());
        assert!(g.classify("a", "b").await.is_ok());
        assert_eq!(calls(&fake), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_attempts() {
        let fake = Arc::new(FakeClassifier::new(vec![http(), http(), http()]));
        let g = guard(fake.clone());
        assert!(matches!(
            g.classify("a", "b").await,
            Err(ClassifyError::Http(_))
        ));
        assert_eq!(calls(&fake), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_attempt_times_out() {
        let slow = Arc::new(Slow(AtomicUsize::new(0)));
        let g = guard(slow.clone());
        assert!(matches!(
            g.classify("a", "b").await,
            Err(ClassifyError::Timeout)
        ));
        assert_eq!(slow.0.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_reuses_last_value() {
        let fake = Arc::new(FakeClassifier::new(vec![http(), http(), http(), http()]));
        let policy = RetryPolicy {
            attempts: 4,
            timeout_secs: 10,
            backoff_secs: vec![1, 3],
        };
        let g = GuardedClassifier::new(fake, policy, breaker());
        let start = Instant::now();
        let _ = g.classify("a", "b").await;
        assert_eq!(start.elapsed(), Duration::from_secs(1 + 3 + 3));
    }

    async fn trip(g: &GuardedClassifier) {
        for _ in 0..3 {
            assert!(g.classify("a", "b").await.is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_opens_after_three_failed_messages() {
        let fake = Arc::new(FakeClassifier::new((0..9).map(|_| http()).collect()));
        let g = guard(fake.clone());
        trip(&g).await;
        assert!(matches!(
            g.classify("a", "b").await,
            Err(ClassifyError::BreakerOpen)
        ));
        assert_eq!(calls(&fake), 9);
    }

    #[tokio::test(start_paused = true)]
    async fn success_resets_failure_count() {
        let mut script: Vec<_> = (0..6).map(|_| http()).collect();
        script.push(Ok(ok()));
        script.extend((0..6).map(|_| http()));
        let fake = Arc::new(FakeClassifier::new(script));
        let g = guard(fake.clone());
        for _ in 0..2 {
            assert!(g.classify("a", "b").await.is_err());
        }
        assert!(g.classify("a", "b").await.is_ok());
        for _ in 0..2 {
            assert!(matches!(
                g.classify("a", "b").await,
                Err(ClassifyError::Http(_))
            ));
        }
        assert_eq!(calls(&fake), 13);
    }

    #[tokio::test(start_paused = true)]
    async fn breaker_half_opens_after_cooldown() {
        let mut script: Vec<_> = (0..9).map(|_| http()).collect();
        script.extend([Ok(ok()), Ok(ok())]);
        let fake = Arc::new(FakeClassifier::new(script));
        let g = guard(fake.clone());
        trip(&g).await;
        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(g.classify("a", "b").await.is_ok());
        assert!(g.classify("a", "b").await.is_ok());
        assert_eq!(calls(&fake), 11);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_trial_reopens_breaker() {
        let fake = Arc::new(FakeClassifier::new((0..12).map(|_| http()).collect()));
        let g = guard(fake.clone());
        trip(&g).await;
        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(matches!(
            g.classify("a", "b").await,
            Err(ClassifyError::Http(_))
        ));
        assert!(matches!(
            g.classify("a", "b").await,
            Err(ClassifyError::BreakerOpen)
        ));
        assert_eq!(calls(&fake), 12);
    }
}
