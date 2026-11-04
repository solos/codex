use std::fs;
use std::sync::Arc;

use codex_exec_server::LOCAL_FS;
use codex_protocol::protocol::Product;
use codex_protocol::protocol::SkillScope;
use codex_skills::SkillDependencies;
use codex_skills::SkillInterface;
use codex_skills::SkillMetadata;
use codex_skills::SkillPolicy;
use codex_skills::SkillToolDependency;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_plugins::PluginIdentity;
use codex_utils_plugins::PluginSkillRoot;
use codex_utils_plugins::SkillDiscoveryMode;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::loader::MAX_NAME_LEN;

use super::HostSkillRoot;
use super::load_host_skill_root;

fn write_skill(root: &TempDir, directory: &str, frontmatter: &str) -> AbsolutePathBuf {
    let skill_dir = root.path().join(directory);
    fs::create_dir_all(&skill_dir).expect("create skill directory");
    let skill_path = skill_dir.join("SKILL.md");
    fs::write(
        &skill_path,
        format!("---\n{frontmatter}\n---\n\n# Instructions\n"),
    )
    .expect("write skill");
    AbsolutePathBuf::from_absolute_path(fs::canonicalize(skill_path).expect("canonical skill path"))
        .expect("absolute skill path")
}

fn write_metadata(root: &TempDir, directory: &str, contents: &str) {
    let metadata_dir = root.path().join(directory).join("agents");
    fs::create_dir_all(&metadata_dir).expect("create metadata directory");
    fs::write(metadata_dir.join("openai.yaml"), contents).expect("write metadata");
}

fn root_for(temp_dir: &TempDir, scope: SkillScope) -> HostSkillRoot {
    HostSkillRoot::host(
        AbsolutePathBuf::from_absolute_path(temp_dir.path()).expect("absolute root"),
        scope,
        Arc::clone(&LOCAL_FS),
    )
}

struct PluginSkillFixture {
    root: TempDir,
    plugin_root: AbsolutePathBuf,
    skill_path: AbsolutePathBuf,
}

impl PluginSkillFixture {
    fn new() -> Self {
        let root = TempDir::new().expect("temp dir");
        let plugin_root = AbsolutePathBuf::from_absolute_path(
            fs::canonicalize(root.path()).expect("canonical plugin root"),
        )
        .expect("absolute plugin root");
        let skill_path = write_skill(
            &root,
            "skills/send-message",
            "name: send-message\ndescription: Send messages",
        );
        Self {
            root,
            plugin_root,
            skill_path,
        }
    }

    fn write_asset(&self, relative_path: &str) {
        let asset_path = self.root.path().join(relative_path);
        fs::create_dir_all(asset_path.parent().expect("asset parent"))
            .expect("create asset directory");
        fs::write(asset_path, "<svg/>").expect("write asset");
    }

    fn write_metadata(&self, contents: &str) {
        write_metadata(&self.root, "skills/send-message", contents);
    }
}

#[tokio::test]
async fn skips_hidden_host_skills() {
    let root = TempDir::new().expect("temp dir");
    let visible_path = write_skill(
        &root,
        "visible",
        "name: visible\ndescription: Visible skill",
    );
    write_skill(&root, ".hidden", "name: hidden\ndescription: Hidden skill");

    let snapshot = load_host_skill_root(root_for(&root, SkillScope::User)).await;

    assert_eq!(snapshot.errors, Vec::new());
    assert_eq!(snapshot.skills.len(), 1);
    assert_eq!(
        snapshot.skills[0].path_to_skills_md,
        PathUri::from_abs_path(&visible_path)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn follows_directory_symlinks_except_for_system_scope() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("temp dir");
    let target = TempDir::new().expect("target temp dir");
    let target_skill = write_skill(&target, "demo", "name: demo\ndescription: Symlinked skill");
    symlink(target.path().join("demo"), root.path().join("alias")).expect("create symlink");

    for scope in [SkillScope::User, SkillScope::Repo, SkillScope::Admin] {
        let snapshot = load_host_skill_root(root_for(&root, scope)).await;

        assert_eq!(snapshot.errors, Vec::new());
        assert_eq!(snapshot.skills.len(), 1);
        assert_eq!(
            (
                &snapshot.skills[0].path_to_skills_md,
                snapshot.skills[0].scope
            ),
            (&PathUri::from_abs_path(&target_skill), scope)
        );
        assert_eq!(
            snapshot
                .skill_discovery_path_by_path
                .get(&PathUri::from_abs_path(&target_skill)),
            Some(&PathUri::from_abs_path(
                &snapshot.root.join("alias/SKILL.md")
            ))
        );
    }

    let system_snapshot = load_host_skill_root(root_for(&root, SkillScope::System)).await;
    assert_eq!(system_snapshot.errors, Vec::new());
    assert_eq!(system_snapshot.skills, Vec::new());
}
