use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::db::{Db, DbError};
use crate::events::{EventBus, MessageEvent};
use crate::sorting::classifier::{Classifier, ClassifyError};
use crate::sorting::config::SweepPolicy;
use crate::sorting::Sorting;

/// Which rows the `classify` command picks up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliMode {
    Missing,
    Failed,
    Force,
}

/// What one pass over the unclassified rows selects.
#[derive(Clone, Debug)]
pub struct SweepRun {
    pub window_days: Option<i64>,
    pub statuses: Vec<&'static str>,
    pub max_attempts: Option<u32>,
    pub batch: i64,
}

impl SweepRun {
    pub fn daemon(p: &SweepPolicy) -> Self {
        Self {
            window_days: Some(p.window_days),
            statuses: vec!["pending"],
            max_attempts: Some(p.max_attempts),
            batch: 50,
        }
    }

    pub fn cli(mode: CliMode) -> Self {
        let statuses = match mode {
            CliMode::Missing => vec!["pending"],
            CliMode::Failed => vec!["failed"],
            CliMode::Force => vec!["pending", "failed", "done"],
        };
        Self {
            window_days: None,
            statuses,
            max_attempts: None,
            batch: 50,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub classified: usize,
    pub failed: usize,
    pub stopped_by_breaker: bool,
}

/// Classifies the selected rows batch by batch until none are left.
///
/// `max_attempts` is the failure cap handed to the database; `run.max_attempts`
/// only filters what gets selected. Each row is tried at most once per call.
pub async fn classify_pending(
    db: &Db,
    classifier: &dyn Classifier,
    bus: Option<&EventBus>,
    run: &SweepRun,
    max_attempts: u32,
) -> Result<SweepReport, DbError> {
    let since = run
        .window_days
        .map(|d| (chrono::Utc::now() - chrono::Duration::days(d)).to_rfc3339());
    let mut report = SweepReport::default();
    let mut seen: HashSet<i64> = HashSet::new();
    loop {
        // Rows already handled may still match the selection, so look past them.
        let limit = run.batch + seen.len() as i64;
        let rows =
            db.pending_classification(since.as_deref(), &run.statuses, run.max_attempts, limit)?;
        let fresh: Vec<_> = rows
            .into_iter()
            .filter(|(id, _, _)| !seen.contains(id))
            .take(run.batch as usize)
            .collect();
        if fresh.is_empty() {
            return Ok(report);
        }
        for (id, e164, body) in fresh {
            seen.insert(id);
            match classifier.classify(&e164, &body).await {
                Ok(stored) => {
                    if let Some(row) = db.set_classification(id, &stored)? {
                        report.classified += 1;
                        if let Some(bus) = bus {
                            bus.publish(MessageEvent::updated(row));
                        }
                    }
                }
                Err(ClassifyError::BreakerOpen) => {
                    report.stopped_by_breaker = true;
                    return Ok(report);
                }
                Err(err) => {
                    tracing::warn!(id, error = %err, "classification failed");
                    db.record_classify_failure(id, max_attempts)?;
                    report.failed += 1;
                }
            }
        }
    }
}

/// Every `interval_secs`, classifies recent pending messages. Does nothing
/// without a Classifier.
pub async fn run_sweep(db: Arc<Db>, sorting: Sorting, bus: EventBus, cancel: CancellationToken) {
    let Some(classifier) = sorting.classifier.clone() else {
        return;
    };
    let policy = sorting.config.sweep.clone();
    let run = SweepRun::daemon(&policy);
    let mut ticker = tokio::time::interval(Duration::from_secs(policy.interval_secs.max(1)));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => {
                let pass = classify_pending(&db, classifier.as_ref(), Some(&bus), &run, policy.max_attempts);
                let result = tokio::select! {
                    _ = cancel.cancelled() => return,
                    r = pass => r,
                };
                match result {
                    Ok(r) if r.classified + r.failed > 0 || r.stopped_by_breaker => {
                        tracing::info!(
                            classified = r.classified,
                            failed = r.failed,
                            stopped_by_breaker = r.stopped_by_breaker,
                            "classification sweep"
                        );
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!(error = %err, "classification sweep failed"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Classification, StoredClassification};
    use crate::sorting::classifier::FakeClassifier;
    use crate::sorting::config::SortingConfig;
    use std::sync::atomic::Ordering;

    fn done() -> StoredClassification {
        StoredClassification {
            public: Classification {
                category: "transactional".into(),
                confidence: 0.9,
                tags: vec![],
            },
            scores: serde_json::json!({}),
        }
    }

    fn policy() -> SweepPolicy {
        SortingConfig::load(None).unwrap().sweep
    }

    fn insert(db: &Db, path: &str, age_days: i64) {
        let at = (chrono::Utc::now() - chrono::Duration::days(age_days)).to_rfc3339();
        db.insert_inbound_at(path, "+98912", "hi", &at, Some(1))
            .unwrap();
    }

    fn ids(db: &Db, statuses: &[&str]) -> Vec<i64> {
        db.pending_classification(None, statuses, None, 100)
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect()
    }

    #[tokio::test]
    async fn sweep_classifies_pending_and_announces_update() {
        let db = Db::open_in_memory().unwrap();
        insert(&db, "/1", 0);
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        let fake = FakeClassifier::new(vec![Ok(done())]);
        let p = policy();
        let report = classify_pending(&db, &fake, Some(&bus), &SweepRun::daemon(&p), 5)
            .await
            .unwrap();
        assert_eq!((report.classified, report.failed), (1, 0));
        let ev = rx.try_recv().unwrap();
        assert!(ev.updated);
        assert_eq!(ev.classification.unwrap().category, "transactional");
        assert!(ids(&db, &["pending"]).is_empty());
    }

    #[tokio::test]
    async fn sweep_skips_rows_outside_window() {
        let db = Db::open_in_memory().unwrap();
        insert(&db, "/old", 8);
        let fake = FakeClassifier::new(vec![Ok(done())]);
        let p = policy();
        let report = classify_pending(&db, &fake, None, &SweepRun::daemon(&p), 5)
            .await
            .unwrap();
        assert_eq!(report.classified, 0);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        assert_eq!(ids(&db, &["pending"]).len(), 1);
    }

    #[tokio::test]
    async fn sweep_marks_failed_after_max_attempts() {
        let db = Db::open_in_memory().unwrap();
        insert(&db, "/1", 0);
        let fake = FakeClassifier::new(vec![]);
        let p = policy();
        for _ in 0..5 {
            let report = classify_pending(&db, &fake, None, &SweepRun::daemon(&p), 5)
                .await
                .unwrap();
            assert_eq!((report.classified, report.failed), (0, 1));
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 5);
        assert_eq!(ids(&db, &["failed"]).len(), 1);
        let report = classify_pending(&db, &fake, None, &SweepRun::daemon(&p), 5)
            .await
            .unwrap();
        assert_eq!(report, SweepReport::default());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn sweep_stops_when_breaker_opens() {
        let db = Db::open_in_memory().unwrap();
        insert(&db, "/1", 0);
        insert(&db, "/2", 0);
        let fake = FakeClassifier::new(vec![Err(ClassifyError::BreakerOpen)]);
        let p = policy();
        let report = classify_pending(&db, &fake, None, &SweepRun::daemon(&p), 5)
            .await
            .unwrap();
        assert!(report.stopped_by_breaker);
        assert_eq!((report.classified, report.failed), (0, 0));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert_eq!(ids(&db, &["pending"]).len(), 2);
    }

    #[tokio::test]
    async fn cli_force_reclassifies_done_rows_once_without_events() {
        let db = Db::open_in_memory().unwrap();
        insert(&db, "/1", 0);
        insert(&db, "/2", 0);
        let first = FakeClassifier::new(vec![Ok(done()), Ok(done())]);
        classify_pending(&db, &first, None, &SweepRun::cli(CliMode::Missing), 5)
            .await
            .unwrap();
        assert_eq!(ids(&db, &["done"]).len(), 2);

        let fake = FakeClassifier::new(vec![Ok(done()), Ok(done())]);
        let report = classify_pending(&db, &fake, None, &SweepRun::cli(CliMode::Force), 5)
            .await
            .unwrap();
        assert_eq!(report.classified, 2);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cli_force_pages_past_the_first_batch() {
        let db = Db::open_in_memory().unwrap();
        for i in 0..5 {
            insert(&db, &format!("/{i}"), 0);
        }
        let fake = FakeClassifier::new((0..5).map(|_| Ok(done())).collect());
        let run = SweepRun {
            batch: 2,
            ..SweepRun::cli(CliMode::Force)
        };
        let report = classify_pending(&db, &fake, None, &run, 5).await.unwrap();
        assert_eq!(report.classified, 5);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn run_shapes_match_the_policy() {
        let p = policy();
        let d = SweepRun::daemon(&p);
        assert_eq!(
            (d.window_days, d.statuses, d.max_attempts, d.batch),
            (Some(7), vec!["pending"], Some(5), 50)
        );
        let f = SweepRun::cli(CliMode::Failed);
        assert_eq!(
            (f.window_days, f.statuses, f.max_attempts),
            (None, vec!["failed"], None)
        );
        assert_eq!(
            SweepRun::cli(CliMode::Force).statuses,
            vec!["pending", "failed", "done"]
        );
    }
}
