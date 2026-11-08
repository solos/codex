<p align="center"><strong>Codex CLI</strong> is a coding agent from OpenAI that runs locally on your computer.</p>
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI splash" width="80%" />
</p>

This repository is the [solos/codex](https://github.com/solos/codex) fork. It is
a local-first build of Codex with a built-in repository index, a Cargo-only
build, and no dependency on the upstream release infrastructure.

## What is different in this fork?

- **Local code index enabled by default.** Codex indexes the repository in
  SQLite and automatically injects relevant code context before a turn. Search
  combines full-text, trigram, symbol, path, and optional vector ranking.
- **No index service and no default network embeddings.** Discovery respects
  `.gitignore`, `.ignore`, and `.codexignore`, and skips hidden, generated,
  binary, oversized, and sensitive files.
- **Cargo-only builds.** Bazel build files and workflows have been removed.
- **rquickjs instead of V8.** Code Mode no longer requires a V8 toolchain.
- **Smaller runtime surface.** Upstream remote-control, cloud-task, analytics,
  and local voice runtime components are not included.
- **Fork-local releases.** Update checks, release notes, and package archives
  point at `solos/codex`.

## Quickstart

### Install a fork release

Download the package for your platform from the
[latest fork release](https://github.com/solos/codex/releases/latest). Releases
are unsigned archives named `codex-package-<target>.tar.gz` (with `.tar.zst`
also available):

| Platform | Target |
| --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

On macOS or Linux:

```shell
# Replace TARGET with the value from the table above.
TARGET=x86_64-unknown-linux-musl
mkdir -p codex-package
tar -xzf "codex-package-${TARGET}.tar.gz" -C codex-package
./codex-package/bin/codex
```

On macOS, clear the quarantine attribute after extracting an unsigned release:

```shell
xattr -dr com.apple.quarantine codex-package
```

On Windows:

```powershell
mkdir codex-package
tar -xzf codex-package-x86_64-pc-windows-msvc.tar.gz -C codex-package
.\codex-package\bin\codex.exe
```

The upstream `npm`, Homebrew, and `chatgpt.com/codex` installers install OpenAI's
upstream Codex build, not this fork.

### Build from source

```shell
git clone https://github.com/solos/codex.git
cd codex/codex-rs
cargo build --release --bin codex
./target/release/codex
```

For development, use `cargo run --bin codex -- "explain this codebase to me"`.

### Sign in

Run `codex` and select **Sign in with ChatGPT**, or configure an API key. See
the [Codex authentication documentation](https://developers.openai.com/codex/auth)
for details.

## Local code index

The index is enabled by default. Build it once for a repository, then Codex uses
it automatically to retrieve bounded code context for each turn:

```shell
codex index init .
codex index status .
codex index search "where are update checks configured"
```

Use `codex index update .` for an incremental refresh or `codex index watch .`
to keep the index current while editing. If the index is missing or unreadable,
Codex falls back to ignore-aware file search instead of failing the turn.

Index configuration is optional:

```toml
[features.code_index]
enabled = true
token_budget = 1200
max_results = 8
index_path = ".codex/index.sqlite"
embedding = false
debug = false
```

Set `enabled = false` to opt out. Vector retrieval remains off unless a host
explicitly installs an embedding provider and enables it.

See the [code index documentation](./codex-rs/code-index/README.md),
[performance report](./codex-rs/code-index/PERFORMANCE.md), and
[Chinese capability report](./codex-rs/code-index/IMPROVEMENTS.zh-CN.md) for
architecture and benchmarks.

## Documentation

- [**Codex CLI documentation**](https://developers.openai.com/codex)
- [**Contributing**](./docs/contributing.md)
- [**Installing and building**](./docs/install.md)
- [**Open source fund**](./docs/open-source-fund.md)

This repository is licensed under the [Apache-2.0 License](LICENSE).
