//! Exercises config reloads while preparation is waiting on executor discovery.

use super::*;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;

struct PausedExecutor {
    entered: Notify,
    resume: Notify,
    queries: Mutex<Vec<bool>>,
}

impl SkillProvider for PausedExecutor {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            self.queries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(query.include_bundled_skills);
            self.entered.notify_one();
            self.resume.notified().await;
            Ok(SkillCatalog::default())
        })
    }

    fn read<'a>(
        &'a self,
        _request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async { panic!("executor reads are not expected") })
    }

    fn search(&self, _request: SkillSearchRequest) -> SkillProviderFuture<'_, SkillSearchResult> {
        Box::pin(async { panic!("executor searches are not expected") })
    }
}
