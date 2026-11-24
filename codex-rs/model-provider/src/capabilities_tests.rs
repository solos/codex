use super::*;
use crate::create_model_provider;
use codex_model_provider_info::ModelProviderCapabilities;
use pretty_assertions::assert_eq;

#[test]
fn capability_overrides_preserve_unspecified_provider_defaults() {
    for info in [
        ModelProviderInfo::default(),
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        ModelProviderInfo {
            name: "Azure".to_string(),
            ..Default::default()
        },
    ] {
        let defaults = create_model_provider(info.clone(), /*auth_manager*/ None).capabilities();
        let provider = create_model_provider(
            ModelProviderInfo {
                capabilities: Some(ModelProviderCapabilities {
                    external_web_access: Some(false),
                    ..Default::default()
                }),
                ..info
            },
            /*auth_manager*/ None,
        );
        assert_eq!(
            provider.capabilities(),
            ProviderCapabilities {
                external_web_access: false,
                ..defaults
            },
        );
    }
}

#[test]
fn prompt_cache_breakpoints_require_an_explicit_override() {
    let info = ModelProviderInfo {
        name: "Custom".to_string(),
        ..Default::default()
    };
    let default_provider =
        create_model_provider(info.clone(), /*auth_manager*/ None).capabilities();
    assert!(!default_provider.prompt_cache_breakpoints);

    let enabled_provider = create_model_provider(
        ModelProviderInfo {
            capabilities: Some(ModelProviderCapabilities {
                prompt_cache_breakpoints: Some(true),
                ..Default::default()
            }),
            ..info
        },
        /*auth_manager*/ None,
    );
    assert!(enabled_provider.capabilities().prompt_cache_breakpoints);
}
