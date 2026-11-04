use super::*;
use crate::app::agents_overview_usage::AgentsOverviewUsage;
use crate::app::agents_overview_usage::usage_lines;
use crate::chatwidget::ThreadUsageOutcome;
use codex_app_server_protocol::AccountUpdatedNotification;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
use codex_app_server_protocol::ThreadUsage;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_protocol::account::PlanType;
use pretty_assertions::assert_eq;
#[test]
fn incomplete_billing_groups_do_not_display_partial_token_totals() {
    let estimate: ThreadUsage = serde_json::from_value(serde_json::json!({
        "threadId": "thread", "estimatedUsageCreditsMicros": 0, "estimatedUsageUsdMicros": null,
        "groups": [
            {"estimatedUsageCreditsMicros": 0, "inputTokens": 100, "outputTokens": 10},
            {"estimatedUsageCreditsMicros": 0, "inputTokens": null, "outputTokens": 20}
        ]
    }))
    .unwrap();
    let mut usage = AgentsOverviewUsage::default();
    usage.estimate = Some(estimate);
    let text = usage_lines(&usage, Some(PlanType::Business))
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    assert_eq!(text, vec!["Tokens: 30 out", "Est. usage: 0 credits"]);
}
