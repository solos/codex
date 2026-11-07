use std::fs;

use codex_code_index::IndexInitOptions;
use codex_code_index::IndexStatusOptions;
use codex_code_index::index_status;
use codex_code_index::init_index;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[tokio::test]
async fn mini_fixture_repo_indexes_expected_sources() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("mini_repo");
    fs::create_dir_all(root.join(".git")).expect("git directory");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::create_dir_all(root.join("scripts")).expect("scripts");
    fs::create_dir_all(root.join("generated")).expect("generated");
    fs::create_dir_all(root.join("ignored")).expect("ignored");

    fs::write(
        root.join(".gitignore"),
        include_str!("fixtures/mini_repo/.gitignore"),
    )
    .expect("gitignore");
    fs::write(
        root.join(".codexignore"),
        include_str!("fixtures/mini_repo/.codexignore"),
    )
    .expect("codexignore");
    fs::write(
        root.join("README.md"),
        include_str!("fixtures/mini_repo/README.md"),
    )
    .expect("readme");
    fs::write(
        root.join("src/lib.rs"),
        include_str!("fixtures/mini_repo/src/lib.rs"),
    )
    .expect("rust source");
    fs::write(
        root.join("src/widget.ts"),
        include_str!("fixtures/mini_repo/src/widget.ts"),
    )
    .expect("typescript source");
    fs::write(
        root.join("scripts/task.py"),
        include_str!("fixtures/mini_repo/scripts/task.py"),
    )
    .expect("python source");
    fs::write(
        root.join("generated/skip.rs"),
        "pub fn generated_code() {}\n",
    )
    .expect("generated source");
    fs::write(
        root.join("ignored/private.rs"),
        include_str!("fixtures/mini_repo/ignored/private.rs"),
    )
    .expect("ignored source");
    fs::write(root.join("secrets.pem"), "not-a-real-secret\n").expect("sensitive file");
    fs::write(root.join("image.bin"), [0_u8, 1, 2, 3]).expect("binary file");

    let db_path = temp.path().join("index.sqlite");
    let report = init_index(IndexInitOptions {
        root: root.clone(),
        db_path: Some(db_path.clone()),
        ..IndexInitOptions::new(&root)
    })
    .await
    .expect("index fixture");

    assert_eq!(report.files_indexed, 4);
    assert_eq!(report.skipped_sensitive, 1);
    assert_eq!(report.skipped_binary, 1);

    let status = index_status(IndexStatusOptions {
        root: root.clone(),
        db_path: Some(db_path.clone()),
    })
    .await
    .expect("status");
    assert!(status.initialized);
    assert_eq!(status.file_count, 4);
    assert!(status.chunk_count >= 4);
    assert_eq!(status.languages.get("rust"), Some(&1));
    assert_eq!(status.languages.get("typescript"), Some(&1));
    assert_eq!(status.languages.get("python"), Some(&1));
    assert_eq!(status.languages.get("text"), Some(&1));

    let second = init_index(IndexInitOptions {
        root,
        db_path: Some(db_path),
        ..IndexInitOptions::new(".")
    })
    .await
    .expect("incremental fixture index");
    assert_eq!(second.files_indexed, 0);
    assert_eq!(second.files_unchanged, 4);
}
