//! Immutable, policy-checked skill catalogs captured for one step.

use crate::aliases::SkillCatalogAliases;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillSourceKind;
use crate::catalog::merge_catalogs;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Fully prepared before tool construction; retained unchanged by all step consumers.
#[derive(Debug)]
pub(crate) struct SkillsStepState {
    pub(crate) discovery_v2: bool,
    pub(crate) tools_enabled: bool,
    pub(crate) catalog: SkillStepCatalog,
    pub(crate) host_catalog: Option<SkillCatalog>,
    pub(crate) config: crate::SkillsExtensionConfig,
}

/// Policy-checked, enabled skill routes for one step. Hidden skills remain explicitly readable.
#[derive(Debug)]
pub(crate) struct SkillStepCatalog {
    pub(super) merged: SkillCatalog,
    pub(crate) aliases: SkillCatalogAliases,
    executor_warnings: Vec<String>,
}

impl SkillStepCatalog {
    pub(crate) fn new(
        executor: SkillCatalog,
        discovery_v2: bool,
        aliases: SkillCatalogAliases,
    ) -> Self {
        let executor_skill_count = executor.entries.len();
        let executor_warnings = executor.warnings.clone();
        let mut catalog = merge_catalogs(executor);
        catalog.entries.retain_mut(|entry| {
            if !entry.enabled {
                return false;
            }
            entry.sources.retain(|source| source.enabled);
            true
        });
        tracing::debug!(
            discovery_v2,
            executor_skill_count,
            enabled_skill_count = catalog.entries.len(),
            warning_count = catalog.warnings.len(),
            "prepared step skill catalog"
        );
        Self {
            merged: catalog,
            aliases,
            executor_warnings,
        }
    }

    /// Selects one authority from the prepared catalog without consulting thread state.
    pub(crate) fn select_from_catalog(
        &self,
        authority: SkillToolAuthoritySelector,
    ) -> SkillCatalog {
        let entries = self
            .merged
            .entries
            .iter()
            .filter(|entry| authority.matches(&entry.authority))
            .cloned()
            .collect();
        let warnings = match authority {
            SkillToolAuthoritySelector::Executor => self.executor_warnings.clone(),
        };
        SkillCatalog {
            is_partial: self.merged.is_partial,
            entries,
            warnings,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SkillToolAuthoritySelector {
    Executor,
}

impl SkillToolAuthoritySelector {
    pub(crate) fn matches(self, authority: &SkillAuthority) -> bool {
        match self {
            Self::Executor => authority.kind == SkillSourceKind::Executor,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogStatus {
    Unavailable,
    Enabled,
}
