use std::io;

use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::GetMetadataOptions;
use codex_exec_server::ReadFileOptions;
use codex_skills::ParsedSkillMetadata;
use codex_skills::SkillDependencies;
use codex_skills::SkillInterface;
use codex_skills::SkillInterfaceAssetPolicy;
use codex_skills::SkillParseError;
use codex_skills::SkillPolicy;
use codex_skills::parse_skill_metadata;
use codex_skills::resolve_skill_interface;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::SKILLS_METADATA_FILENAME;
use super::discovery::SkillMetadataDiscovery;

#[derive(Default)]
pub(super) struct LoadedSkillMetadata {
    pub(super) interface: Option<SkillInterface>,
    pub(super) dependencies: Option<SkillDependencies>,
    pub(super) policy: Option<SkillPolicy>,
}

pub(super) fn sanitize_single_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn validate_len(
    value: &str,
    max_len: usize,
    field_name: &'static str,
) -> Result<(), SkillParseError> {
    if value.is_empty() {
        return Err(SkillParseError::MissingField(field_name));
    }
    if value.chars().count() > max_len {
        return Err(SkillParseError::InvalidField {
            field: field_name,
            reason: format!("exceeds maximum length of {max_len} characters"),
        });
    }
    Ok(())
}
