use std::path::PathBuf;

use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

use super::SkillInterfaceAssetPolicy;
use super::SkillInterfaceFile;
use super::resolve_skill_interface;
use crate::SkillInterface;

fn absolute_path(path: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::try_from(
        std::env::current_dir()
            .expect("current directory")
            .join(path),
    )
    .expect("absolute path")
}

#[test]
fn resolves_local_interface_fields_and_assets() {
    let skill_dir = absolute_path("skill");
    let interface = SkillInterfaceFile {
        display_name: Some("  Demo   skill  ".to_string()),
        short_description: Some("  Short   description  ".to_string()),
        icon_small: Some(PathBuf::from("./assets/icon.svg")),
        icon_large: Some(PathBuf::from("assets/icon-large.png")),
        brand_color: Some(" #123ABC ".to_string()),
        default_prompt: Some("  Run   the demo  ".to_string()),
    };

    assert_eq!(
        resolve_skill_interface(
            Some(interface),
            &skill_dir,
            SkillInterfaceAssetPolicy::LocalOnly,
        ),
        Some(SkillInterface {
            display_name: Some("Demo skill".to_string()),
            short_description: Some("Short description".to_string()),
            icon_small: Some(skill_dir.join("assets/icon.svg")),
            icon_large: Some(skill_dir.join("assets/icon-large.png")),
            brand_color: Some("#123ABC".to_string()),
            default_prompt: Some("Run the demo".to_string()),
        })
    );
}

#[test]
fn rejects_invalid_local_interface_fields() {
    let skill_dir = absolute_path("skill");
    let interface = SkillInterfaceFile {
        display_name: None,
        short_description: None,
        icon_small: Some(PathBuf::from("../outside.svg")),
        icon_large: Some(PathBuf::from("icon.png")),
        brand_color: Some("blue".to_string()),
        default_prompt: Some("x".repeat(1025)),
    };

    assert_eq!(
        resolve_skill_interface(
            Some(interface),
            &skill_dir,
            SkillInterfaceAssetPolicy::LocalOnly,
        ),
        None
    );
}

#[test]
fn rejects_absolute_asset_paths() {
    let skill_dir = absolute_path("skill");
    let interface = SkillInterfaceFile {
        icon_small: Some(absolute_path("outside.svg").as_path().to_path_buf()),
        ..Default::default()
    };

    assert_eq!(
        resolve_skill_interface(
            Some(interface),
            &skill_dir,
            SkillInterfaceAssetPolicy::LocalOnly,
        ),
        None
    );
}

#[test]
fn drops_invalid_fields_without_discarding_valid_interface_fields() {
    let skill_dir = absolute_path("skill");
    let interface = SkillInterfaceFile {
        display_name: Some("Demo skill".to_string()),
        short_description: None,
        icon_small: Some(PathBuf::from("assets/icon.svg")),
        icon_large: Some(PathBuf::from("icon.png")),
        brand_color: Some("blue".to_string()),
        default_prompt: Some("x".repeat(1025)),
    };

    assert_eq!(
        resolve_skill_interface(
            Some(interface),
            &skill_dir,
            SkillInterfaceAssetPolicy::LocalOnly,
        ),
        Some(SkillInterface {
            display_name: Some("Demo skill".to_string()),
            short_description: None,
            icon_small: Some(skill_dir.join("assets/icon.svg")),
            icon_large: None,
            brand_color: None,
            default_prompt: None,
        })
    );
}
