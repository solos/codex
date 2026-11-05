# Workflow Strategy

The workflows in this directory are split so that pull requests get fast, review-friendly signal while `main` still gets the full cross-platform verification pass.

## Pull Requests

- Required checks run against GitHub's synthetic merge commit, not the pull
  request head alone. This includes changes already on `main` and catches
  conflicts before they reach the branch.
- `bazel.yml` is the main pre-merge verification path for Rust code.
  It runs Bazel `test` and Bazel `clippy` on the supported Bazel targets,
  including the generated Rust test binaries needed to lint inline `#[cfg(test)]`
  code.
- `rust-ci.yml` keeps the Cargo-native PR checks intentionally small:
  - `cargo fmt --check`
  - `cargo shear`
  - `argument-comment-lint` on Linux, macOS, and Windows
  - `tools/argument-comment-lint` package tests when the lint or its workflow wiring changes

## Post-Merge On `main`

- `bazel.yml` also runs on pushes to `main`.
  This re-verifies the merged Bazel path and helps keep the BuildBuddy caches warm.
- `rust-ci-full.yml` is the full Cargo-native verification workflow.
  It keeps the heavier checks off the PR path while still validating them after merge:
  - the full Cargo `clippy` matrix
  - the full Cargo `nextest` matrix via per-platform archive-backed shards
  - release-profile Cargo builds
  - cross-platform `argument-comment-lint`

## Fork Releases

- `fork-packages.yml` is the fork-local packaging path. It builds unsigned
  archives for Linux (musl, x86_64 and arm64), macOS (arm64 and x64), and
  Windows x64, then attaches them to the GitHub release for the pushed tag.
- It triggers on `rust-v*.*.*` and `v*.*.*` tags, or manually through
  `workflow_dispatch` (optionally with a `release_tag` and `ref`).
- It intentionally skips `rust-release.yml`'s upstream-only codesigning, npm,
  R2, winget, and website publication steps, so it runs without OpenAI's
  secrets. The archives are unsigned: macOS users must clear the quarantine
  attribute before the first run.
- `rust-release.yml` is still the upstream release pipeline and is gated by
  the `ENABLE_RELEASES` repository variable on non-`openai/codex` repos.

## Rule Of Thumb

- If a build/test/clippy check can be expressed in Bazel, prefer putting the PR-time version in `bazel.yml`.
- Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency.
- Reserve `rust-ci-full.yml` for heavyweight Cargo-native coverage that Bazel does not replace yet.
