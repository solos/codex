use pretty_assertions::assert_eq;

use super::*;

#[test]
fn recent_invocations_refresh_recency_and_evict_old_skills() {
    let history = RecentSkillInvocations::default();
    for index in 0..=MAX_SHADOW_RESULTS {
        history.record(format!("skill-{index}"));
    }
    history.record("skill-1".to_string());

    let recent = history.snapshot();

    assert_eq!(MAX_SHADOW_RESULTS, recent.len());
    assert_eq!(Some("skill-1"), recent.first().map(String::as_str));
    assert_eq!(Some("skill-2"), recent.last().map(String::as_str));
    assert!(!recent.iter().any(|skill| skill == "skill-0"));
}

#[test]
fn rank_buckets_distinguish_results_above_twenty() {
    assert_eq!("11_20", rank_bucket(Some(20)));
    assert_eq!("21_50", rank_bucket(Some(21)));
    assert_eq!("21_50", rank_bucket(Some(50)));
    assert_eq!("miss", rank_bucket(Some(51)));
}

// These tests deliberately occupy the process-wide budget and must drain their
// workers before the next test starts.
static RANKING_TESTS: Semaphore = Semaphore::const_new(1);

/// Hold an actual blocking worker until the test releases it. The timeout keeps a
/// failing assertion from leaving runtime shutdown stuck on the test worker.
struct BlockedSelector {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl CheapSkillSelector for BlockedSelector {
    fn method(&self) -> &'static str {
        "blocked_test"
    }

    fn select(
        &self,
        _query: &str,
        _documents: &[SkillSelectionDocument<'_>],
        _limit: usize,
    ) -> CheapSkillSelection {
        self.started.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        CheapSkillSelection::default()
    }
}

#[tokio::test]
async fn ranking_budget_is_shared_and_held_until_blocking_work_finishes() {
    let _serial = RANKING_TESTS.acquire().await.unwrap();
    let mut workers = Vec::new();
    for index in 0..3 {
        let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
        let (release, releases) = std::sync::mpsc::channel();
        // Separate experiment instances must share the same process-wide budget.
        let experiment = Arc::new(ShadowSelectionExperiment {
            selectors: vec![Box::new(BlockedSelector {
                started,
                release: Mutex::new(releases),
            })],
            metrics_client: None,
        });
        let query = ShadowQuery {
            text: "test".to_string(),
            truncated: false,
        };
        let input = ShadowSelectionInput {
            catalog: SkillCatalog::default(),
            host_snapshot: None,
            task_snapshot: TaskContextSnapshot {
                query: query.clone(),
                recent_skills: Vec::new(),
            },
            query,
            eligible_ids: HashSet::new(),
            recent_skill_resources: Vec::new(),
        };
        let (invocations, pending) = tokio::sync::mpsc::unbounded_channel();
        drop(invocations);
        let mut worker = Box::pin(experiment.evaluate(input, pending, tracing::Span::none()));
        // Polling to the first await deterministically attempts admission.
        assert!(futures::poll!(worker.as_mut()).is_pending());
        if index < 2 {
            assert_eq!(
                Some(()),
                tokio::time::timeout(Duration::from_secs(10), starts.recv())
                    .await
                    .unwrap()
            );
            assert_eq!(1 - index, RANKING_SLOTS.available_permits());
        } else {
            assert!(starts.try_recv().is_err());
        }
        workers.push((tokio::spawn(worker), release, starts));
    }
    let (first, release_first, _) = workers.remove(0);
    release_first.send(()).unwrap();
    first.await.unwrap();
    let (second, release_second, _) = workers.remove(0);
    let (third, release_third, mut third_starts) = workers.remove(0);
    assert_eq!(
        Some(()),
        tokio::time::timeout(Duration::from_secs(10), third_starts.recv())
            .await
            .unwrap()
    );
    assert_eq!(0, RANKING_SLOTS.available_permits());

    // Cancelling evaluation cannot free a slot still occupied by blocking work.
    third.abort();
    assert!(third.await.unwrap_err().is_cancelled());
    assert_eq!(0, RANKING_SLOTS.available_permits());
    release_second.send(()).unwrap();
    release_third.send(()).unwrap();
    second.await.unwrap();
    let _all_returned =
        tokio::time::timeout(Duration::from_secs(10), RANKING_SLOTS.acquire_many(2))
            .await
            .unwrap()
            .unwrap();
}
