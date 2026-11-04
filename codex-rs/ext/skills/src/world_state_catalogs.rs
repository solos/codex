//! Renders prepared step catalogs without performing discovery or changing tool readiness.

use std::sync::Arc;

use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use serde::Deserialize;

use crate::catalog::SkillCatalog;
use crate::catalog_delta::CatalogDelta;
use crate::catalog_delta::CatalogUsage;
use crate::catalog_delta::skills_delta_section;
use crate::render::AvailableSkillsRender;
use crate::render::PreparedSkillCatalog;
use crate::render::RenderedSkillCatalogs;
use crate::render::SkillCatalogRenderPolicy;
use crate::render::SkillMetadataBudget;
use crate::render::render_prepared_skill_catalogs;
use crate::render::skill_metadata_budget;
use crate::render_observability::CatalogSurface;
use crate::render_observability::record_catalog_render;
use crate::state::EmittedCatalogBudgetWarnings;
use crate::state::HostSkillsCatalogInWorldState;
use crate::step_state::CatalogStatus;
use crate::step_state::SkillToolAuthoritySelector;
use crate::step_state::SkillsStepState;
use crate::world_state::CatalogRenderCallback;
use crate::world_state::executor_skills_world_state_section;
use crate::world_state::host_skills_world_state_section;

type CatalogWarningEmitter = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone, Copy, Eq, PartialEq)]
enum CatalogKind {
    Executor,
    Host,
}

impl CatalogKind {
    fn metrics_surface(self) -> CatalogSurface {
        match self {
            Self::Executor => CatalogSurface::ExecutorWorldState,
            Self::Host => CatalogSurface::HostWorldState,
        }
    }
}

struct CatalogContribution {
    catalog: SkillCatalog,
    status: CatalogStatus,
}

impl CatalogContribution {
    fn unavailable() -> Self {
        Self {
            catalog: SkillCatalog::default(),
            status: CatalogStatus::Unavailable,
        }
    }
}

struct CatalogContributions {
    executor: CatalogContribution,
    host: CatalogContribution,
}

struct RenderedCatalogContribution {
    kind: CatalogKind,
    status: CatalogStatus,
    rendered: Option<AvailableSkillsRender>,
}

struct CatalogRenderingContext<'a> {
    input: WorldStateContributionInput<'a>,
    step_state: Arc<SkillsStepState>,
    metadata_budget: SkillMetadataBudget,
    render_policy: SkillCatalogRenderPolicy,
    include_usage: bool,
    warning_emitter: CatalogWarningEmitter,
}

impl<'a> CatalogRenderingContext<'a> {
    fn new(
        event_sink: Arc<dyn ExtensionEventSink>,
        input: WorldStateContributionInput<'a>,
    ) -> Option<Self> {
        let step_state = input.step_store.get::<SkillsStepState>()?;
        let include_usage = input.model_info.include_skills_usage_instructions;
        let context_window = input.model_info.resolved_context_window();
        let metadata_budget =
            skill_metadata_budget(context_window, step_state.config.max_context_tokens);
        let emitted_warnings = input
            .turn_store
            .get_or_init(EmittedCatalogBudgetWarnings::default);
        let thread_id = input.thread_store.level_id().to_string();
        let turn_id = input.turn_id.to_string();
        let warning_emitter: CatalogWarningEmitter = Arc::new(move |message| {
            if emitted_warnings.insert(&message) {
                event_sink.emit_warning(ExtensionWarning {
                    thread_id: thread_id.clone(),
                    turn_id: Some(turn_id.clone()),
                    message,
                });
            }
        });
        let render_policy = if step_state.discovery_v2 {
            SkillCatalogRenderPolicy::DISCOVERY_V2
        } else {
            SkillCatalogRenderPolicy::EXTENSION_COMPATIBLE
        }
        .with_omitted_descriptions(step_state.config.omit_descriptions);
        Some(Self {
            input,
            step_state,
            metadata_budget,
            render_policy,
            include_usage,
            warning_emitter,
        })
    }

    fn render_world_state_catalogs(&self) -> [RenderedCatalogContribution; 2] {
        let catalogs = CatalogContributions {
            executor: CatalogContribution {
                catalog: self
                    .step_state
                    .catalog
                    .select_from_catalog(SkillToolAuthoritySelector::Executor),
                status: CatalogStatus::Enabled,
            },
            host: self.step_state.host_catalog.as_ref().map_or_else(
                CatalogContribution::unavailable,
                |catalog| CatalogContribution {
                    catalog: catalog.clone(),
                    status: CatalogStatus::Enabled,
                },
            ),
        };
        let executor_aliases = Some(
            self.step_state
                .catalog
                .aliases
                .for_authority(SkillToolAuthoritySelector::Executor),
        );
        let executor = PreparedSkillCatalog::with_alias_plan(
            &catalogs.executor.catalog,
            self.render_policy,
            executor_aliases,
        );
        let host_policy = SkillCatalogRenderPolicy::CORE_COMPATIBLE
            .with_omitted_descriptions(self.step_state.config.omit_descriptions);

        let mut rendered = if self.step_state.config.include_instructions {
            render_prepared_skill_catalogs(
                &executor,
                &PreparedSkillCatalog::new(&catalogs.host.catalog, host_policy),
                self.metadata_budget,
                self.include_usage,
            )
        } else {
            RenderedSkillCatalogs::default()
        };

        if self.step_state.config.catalog_deltas_enabled {
            let host = PreparedSkillCatalog::new(&catalogs.host.catalog, host_policy);
            for (catalog, rendered) in [
                (&executor, &mut rendered.executor),
                (&host, &mut rendered.host),
            ] {
                if let Some(rendered) = rendered {
                    rendered.entries = catalog.entries_for_render(rendered);
                }
            }
        }

        [
            (CatalogKind::Executor, catalogs.executor, rendered.executor),
            (CatalogKind::Host, catalogs.host, rendered.host),
        ]
        .map(|(kind, catalog, rendered)| RenderedCatalogContribution {
            kind,
            status: catalog.status,
            rendered,
        })
    }

    fn build_world_state_section(
        &self,
        catalog: RenderedCatalogContribution,
        remaining: &mut SkillMetadataBudget,
    ) -> WorldStateSectionContribution {
        let RenderedCatalogContribution {
            kind,
            status,
            rendered,
        } = catalog;
        let report = rendered
            .as_ref()
            .map(|rendered| rendered.report.clone())
            .unwrap_or_default();
        let include_instructions = self.step_state.config.include_instructions;
        let catalog_deltas_enabled = self.step_state.config.catalog_deltas_enabled;
        let metrics = self.input.extension_metrics.clone();
        let warning_emitter = Arc::clone(&self.warning_emitter);
        let metadata_budget = self.metadata_budget;
        let on_render = move |render_report: &crate::render::SkillRenderReport| {
            if !include_instructions || status != CatalogStatus::Enabled {
                return;
            }
            record_catalog_render(
                metrics.as_deref(),
                kind.metrics_surface(),
                metadata_budget,
                render_report,
            );
            if let Some(message) = render_report.warning_message(catalog_deltas_enabled) {
                warning_emitter(message);
            }
        };

        if catalog_deltas_enabled {
            let (id, source) = match kind {
                CatalogKind::Executor => (
                    crate::world_state::SKILLS_WORLD_STATE_ID,
                    "Selected-environment",
                ),
                CatalogKind::Host => {
                    if status != CatalogStatus::Unavailable {
                        self.input.turn_store.insert(HostSkillsCatalogInWorldState);
                    }
                    (crate::world_state::HOST_SKILLS_WORLD_STATE_ID, "Host")
                }
            };
            return skills_delta_section(
                id,
                source,
                CatalogDelta {
                    rendered,
                    include_usage: self.include_usage,
                },
                self.input
                    .previous_world_state
                    .and_then(|state| state.get(id)),
                remaining,
                on_render,
            );
        }

        let body = rendered
            .and_then(|rendered| rendered.into_fragment(self.include_usage))
            .map(|fragment| fragment.body().into_text().unwrap_or_default());
        let render_report = report.clone();
        let on_render: CatalogRenderCallback = Box::new(move || on_render(&render_report));
        match kind {
            CatalogKind::Executor => executor_skills_world_state_section(
                body,
                include_instructions,
                self.input
                    .previous_world_state
                    .and_then(|state| state.get(crate::world_state::SKILLS_WORLD_STATE_ID)),
                on_render,
            ),
            CatalogKind::Host => {
                self.input.turn_store.insert(HostSkillsCatalogInWorldState);
                host_skills_world_state_section(body, include_instructions, &report, on_render)
            }
        }
    }
}

/// Renders catalog sections, retaining empty delta snapshots for shared budget accounting.
pub(crate) fn render_world_state_sections(
    event_sink: Arc<dyn ExtensionEventSink>,
    input: WorldStateContributionInput<'_>,
) -> Vec<WorldStateSectionContribution> {
    let Some(context) = CatalogRenderingContext::new(event_sink, input) else {
        return Vec::new();
    };
    let used = context
        .input
        .previous_world_state
        .into_iter()
        .flat_map(|state| {
            [
                crate::world_state::SKILLS_WORLD_STATE_ID,
                crate::world_state::HOST_SKILLS_WORLD_STATE_ID,
            ]
            .into_iter()
            .filter_map(|id| state.get(id))
        })
        .filter_map(|snapshot| snapshot.get("usage"))
        .filter_map(|usage| CatalogUsage::deserialize(usage).ok())
        .map(|usage| usage.cost(context.metadata_budget))
        .sum::<usize>();
    let mut remaining = context
        .metadata_budget
        .with_limit(context.metadata_budget.limit().saturating_sub(used));
    context
        .render_world_state_catalogs()
        .into_iter()
        .filter(|catalog| {
            context.step_state.config.catalog_deltas_enabled
                || catalog.status != CatalogStatus::Unavailable
        })
        .map(|catalog| context.build_world_state_section(catalog, &mut remaining))
        .collect()
}
