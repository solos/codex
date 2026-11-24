use super::protected_target_in_apply_patch;
use super::protected_target_in_shell_command;
use std::path::Path;

#[test]
fn blocks_direct_git_hook_write() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");
    assert!(
        protected_target_in_shell_command(
            "printf '#!/bin/sh\\n' > .git/hooks/pre-commit",
            cwd,
            codex_home,
        )
        .is_some()
    );
}

#[test]
fn blocks_git_config_mutation() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");
    assert!(
        protected_target_in_shell_command("git config core.hooksPath /tmp/hooks", cwd, codex_home)
            .is_some(),
        "a plain git config write targets .git/config"
    );
    assert!(
        protected_target_in_shell_command("git config --get core.hooksPath", cwd, codex_home)
            .is_none()
    );
    assert!(
        protected_target_in_shell_command(
            "git config --global core.hooksPath /tmp/hooks",
            cwd,
            codex_home,
        )
        .is_none(),
        "global git config is outside the local .git/config redline"
    );
    assert!(
        protected_target_in_shell_command(
            "git config --file .git/config core.hooksPath /tmp/hooks",
            cwd,
            codex_home,
        )
        .is_some()
    );
}

#[test]
fn allows_read_only_git_metadata_access() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");
    assert!(protected_target_in_shell_command("git status --short", cwd, codex_home).is_none());
    assert!(protected_target_in_shell_command("cat .git/config", cwd, codex_home).is_none());
}

#[test]
fn blocks_codex_config_and_shell_startup_writes() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");
    assert!(
        protected_target_in_shell_command("echo x > ~/.codex/config.toml", cwd, codex_home)
            .is_some()
    );
    assert!(
        protected_target_in_shell_command("echo x > $CODEX_HOME/config.toml", cwd, codex_home)
            .is_some()
    );
    assert!(
        protected_target_in_shell_command("echo export > /home/user/.zshrc", cwd, codex_home)
            .is_some()
    );
}

#[test]
fn blocks_apply_patch_metadata_targets() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");
    let patch = "*** Begin Patch\n*** Add File: .git/hooks/post-checkout\n+x\n*** End Patch";
    assert!(protected_target_in_apply_patch(patch, cwd, codex_home).is_some());

    let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch";
    assert!(protected_target_in_apply_patch(patch, cwd, codex_home).is_none());
}

#[test]
fn blocks_apply_patch_invoked_through_the_shell() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");
    let command = "apply_patch <<'PATCH'\n*** Begin Patch\n*** Update File: .git/config\n@@\n-a\n+b\n*** End Patch\nPATCH";

    assert!(protected_target_in_shell_command(command, cwd, codex_home).is_some());
}

#[test]
fn allows_redirecting_text_that_mentions_a_protected_path() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");

    assert!(
        protected_target_in_shell_command(
            "echo 'inspect .git/config before changing it' > notes.txt",
            cwd,
            codex_home,
        )
        .is_none()
    );
}

#[test]
fn tracks_cd_into_git_metadata_before_relative_writes() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");

    assert!(
        protected_target_in_shell_command(
            "cd .git/hooks && printf '#!/bin/sh\\n' > pre-commit",
            cwd,
            codex_home,
        )
        .is_some()
    );
    assert!(
        protected_target_in_shell_command("cd .git && mkdir -p hooks", cwd, codex_home).is_some()
    );
}

#[test]
fn distinguishes_read_only_and_mutating_find_and_stream_editors() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");

    assert!(
        protected_target_in_shell_command(
            "find .git/hooks -type f -print > /tmp/hooks.txt",
            cwd,
            codex_home,
        )
        .is_none()
    );
    assert!(
        protected_target_in_shell_command(
            "sed -n '1,20p' .git/config > /tmp/config.txt",
            cwd,
            codex_home,
        )
        .is_none()
    );
    assert!(
        protected_target_in_shell_command("find .git/hooks -type f -delete", cwd, codex_home,)
            .is_some()
    );
    assert!(
        protected_target_in_shell_command("find .git/hooks -exec rm {} \\;", cwd, codex_home,)
            .is_some()
    );
    assert!(
        protected_target_in_shell_command(
            "sed -i 's/a/b/' .git/config > /tmp/edit.log",
            cwd,
            codex_home,
        )
        .is_some()
    );
}

#[test]
fn unwraps_command_prefixes_and_nested_shell_scripts() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");

    assert!(protected_target_in_shell_command("sudo rm .git/config", cwd, codex_home).is_some());
    assert!(
        protected_target_in_shell_command(
            "bash -lc 'cd .git && printf x > hooks/pre-commit'",
            cwd,
            codex_home,
        )
        .is_some()
    );
    assert!(
        protected_target_in_shell_command("bash -c 'cat .git/config'", cwd, codex_home).is_none()
    );
    assert!(protected_target_in_shell_command("echo rm .git/config", cwd, codex_home).is_none());
}

#[test]
fn still_checks_write_arguments_when_output_is_redirected() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");

    assert!(
        protected_target_in_shell_command(
            "cp replacement .git/config > /tmp/copy.log",
            cwd,
            codex_home,
        )
        .is_some()
    );
}

#[test]
fn blocks_dd_output_to_protected_paths() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");

    assert!(
        protected_target_in_shell_command("dd if=replacement of=.git/config", cwd, codex_home,)
            .is_some()
    );
}

#[test]
fn resolves_lexical_parent_segments_before_checking_protected_paths() {
    let cwd = Path::new("/workspace/repo");
    let codex_home = Path::new("/home/user/.codex");

    assert!(
        protected_target_in_shell_command("printf x > .git/hooks/../config", cwd, codex_home,)
            .is_some()
    );
    assert!(
        protected_target_in_shell_command(
            "printf x > .git/checkout/../../.git/config",
            cwd,
            codex_home,
        )
        .is_some()
    );
}
