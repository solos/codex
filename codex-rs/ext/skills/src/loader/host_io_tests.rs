use std::fs;
use std::os::unix::fs::symlink;
use std::sync::Arc;

use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::LOCAL_FS;
use codex_protocol::protocol::SkillScope;
use codex_skills::SkillMetadata;
use codex_skills::SkillPolicy;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use tokio::sync::Semaphore;

use crate::loader::HostSkillRoot;
use crate::loader::MAX_CONCURRENT_ROOT_SCANS;
use crate::loader::io_test_support::ManifestMetadataBehavior;
use crate::loader::io_test_support::RecordingFileSystem;
use crate::loader::load_and_merge_host_skill_roots;
