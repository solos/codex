use super::*;
use codex_connectors::AppInfo;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn discoverable_tool_enums_use_expected_wire_names() {
    assert_eq!(
        json!({
            "tool_type": DiscoverableToolType::Connector,
            "action_type": DiscoverableToolAction::Install,
        }),
        json!({
            "tool_type": "connector",
            "action_type": "install",
        })
    );
}
