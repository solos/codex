use super::*;
use crate::DiscoverablePluginInfo;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn verified_connector_install_completed_requires_accessible_connector() {
    let accessible_connectors = vec![AppInfo {
        id: "calendar".to_string(),
        name: "Google Calendar".to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: false,
        plugin_display_names: Vec::new(),
    }];

    assert!(verified_connector_install_completed(
        "calendar",
        &accessible_connectors,
    ));
    assert!(!verified_connector_install_completed(
        "gmail",
        &accessible_connectors,
    ));
}

#[test]
fn all_requested_connectors_picked_up_requires_every_expected_connector() {
    let accessible_connectors = vec![AppInfo {
        id: "calendar".to_string(),
        name: "Google Calendar".to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: false,
        plugin_display_names: Vec::new(),
    }];

    assert!(all_requested_connectors_picked_up(
        &["calendar".to_string()],
        &accessible_connectors,
    ));
    assert!(!all_requested_connectors_picked_up(
        &["calendar".to_string(), "gmail".to_string()],
        &accessible_connectors,
    ));
}
