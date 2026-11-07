# codex-utils-cargo-bin

Test helpers for locating Cargo-built binaries and fixtures.

Function behavior:

- `cargo_bin`: reads Cargo's `CARGO_BIN_EXE_*` environment variables and
  verifies that the selected path exists.
- `find_resource!`: resolves fixtures relative to `CARGO_MANIFEST_DIR`.
- `repo_root`: resolves the repository root from the checked-in
  `repo_root.marker`.
