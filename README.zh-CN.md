[English](./README.md) | **简体中文**

<p align="center"><strong>Codex CLI</strong> 是 OpenAI 推出的编码智能体，可在你的本地计算机上运行。</p>
<p align="center">
  <img src="https://github.com/openai/codex/blob/main/.github/codex-cli-splash.png" alt="Codex CLI 启动画面" width="80%" />
</p>

本仓库是 [solos/codex](https://github.com/solos/codex) 分支。它是一个本地优先的
Codex 构建版本，内置仓库索引，仅使用 Cargo 构建，并且不依赖上游发布基础设施。

## 这个分支有什么不同？

- **默认启用本地代码索引。** Codex 使用 SQLite 为仓库建立索引，并在每轮对话前自动注入相关的代码上下文。搜索结合全文、三元组、符号、路径和可选的向量排序。
- **没有索引服务，也不默认使用网络嵌入。** 文件发现遵循 `.gitignore`、`.ignore` 和 `.codexignore`，并跳过隐藏文件、生成文件、二进制文件、超大文件和敏感文件。
- **仅使用 Cargo 构建。** Bazel 构建文件和 workflow 已被移除。
- **使用 rquickjs 替代 V8。** Code Mode 不再需要 V8 工具链。
- **更小的运行时范围。** 不包含上游的远程控制、云任务、分析和本地语音运行时组件。
- **分支独立发布。** 更新检查、发布说明和软件包归档均指向 `solos/codex`。

## 快速开始

### 安装分支版本

从[最新的分支版本](https://github.com/solos/codex/releases/latest)下载适用于你的平台的软件包。
发布包是未签名的归档文件，命名为 `codex-package-<target>.tar.gz`（也提供 `.tar.zst`）：

| 平台 | 目标 |
| --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

在 macOS 或 Linux 上：

```shell
# 将 TARGET 替换为上表中的值。
TARGET=x86_64-unknown-linux-musl
mkdir -p codex-package
tar -xzf "codex-package-${TARGET}.tar.gz" -C codex-package
./codex-package/bin/codex
```

在 macOS 上，解压未签名的发布包后，请清除隔离属性：

```shell
xattr -dr com.apple.quarantine codex-package
```

在 Windows 上：

```powershell
mkdir codex-package
tar -xzf codex-package-x86_64-pc-windows-msvc.tar.gz -C codex-package
.\codex-package\bin\codex.exe
```

上游的 `npm`、Homebrew 和 `chatgpt.com/codex` 安装程序安装的是 OpenAI 的上游
Codex 构建版本，而不是本分支。

### 从源码构建

```shell
git clone https://github.com/solos/codex.git
cd codex/codex-rs
cargo build --release --bin codex
./target/release/codex
```

开发时，可以使用 `cargo run --bin codex -- "explain this codebase to me"`。

### 登录

运行 `codex`，然后选择 **Sign in with ChatGPT**，或者配置 API 密钥。详情请参阅
[Codex 身份验证文档](https://developers.openai.com/codex/auth)。

## 本地代码索引

索引默认启用。为一个仓库构建一次索引后，Codex 会在每轮对话中自动使用它来检索
有界范围内的代码上下文：

```shell
codex index init .
codex index status .
codex index search "where are update checks configured"
```

使用 `codex index update .` 进行增量刷新，或使用 `codex index watch .` 在编辑时保持
索引最新。如果索引缺失或无法读取，Codex 会回退到遵循忽略规则的文件搜索，而不是让
本轮对话失败。

索引配置是可选的：

```toml
[features.code_index]
enabled = true
token_budget = 1200
max_results = 8
index_path = ".codex/index.sqlite"
embedding = false
debug = false
```

设置 `enabled = false` 即可停用。除非宿主显式安装嵌入提供程序并启用它，否则向量
检索将保持关闭。

有关架构和基准测试，请参阅[代码索引文档](./codex-rs/code-index/README.md)、
[性能报告](./codex-rs/code-index/PERFORMANCE.md)和
[中文能力报告](./codex-rs/code-index/IMPROVEMENTS.zh-CN.md)。

## 文档

- [**Codex CLI 文档**](https://developers.openai.com/codex)
- [**贡献指南**](./docs/contributing.md)
- [**安装与构建**](./docs/install.md)
- [**开源基金**](./docs/open-source-fund.md)

本仓库基于 [Apache-2.0 许可证](LICENSE) 授权。
