<p><picture><source media="(prefers-color-scheme: dark)" srcset="docs/brand/mark-dark.svg"><img src="docs/brand/mark-light.svg" width="56" height="56" alt=""></picture></p>

# byom

[![CI](https://github.com/baoengineer/byom/actions/workflows/ci.yml/badge.svg)](https://github.com/baoengineer/byom/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/baoengineer/byom)](https://github.com/baoengineer/byom/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

[English](README.md) | 简体中文

**在 Claude Code 里用你的 ChatGPT 套餐，无需 API key。** 用 ChatGPT 账号登录即可，GPT 会和 Claude 一起出现在 [Claude Code](https://claude.com/claude-code) 的 `/model` 选择器里。GLM、Kimi、MiniMax、DeepSeek、Gemini、Groq、OpenRouter，以及通过 Ollama 或 LM Studio 运行的本地模型，用法完全相同。byom 是 bring your own model（自带模型）的缩写。

![byom config, then Claude Code on GPT-5.6-Sol, then /model with Claude and GPT side by side](docs/demo/demo.gif)

Claude Code 本身不做任何修改。它的工具、subagent、hooks、MCP 服务器、skills、plan mode 和会话恢复都照常工作。byom 在本地运行一个小型路由器，按模型 ID 把每个请求发给对应的服务商：

```text
claude (official)
  ▼
byom router (127.0.0.1) ─┬─ openai/*        → ChatGPT plan (Sign in with ChatGPT)
                         ├─ zai/*, kimi/*, minimax/*, deepseek/*, openrouter/*, ollama/*, lmstudio/*
                         │                  → Anthropic-compatible APIs, forwarded
                         ├─ groq/*, gemini/*, mistral/*, cerebras/*, together/*, xai/*
                         │                  → OpenAI-compatible APIs, translated
                         └─ claude-*        → Anthropic: an API key, or Claude Code's own sign-in (opt-in)
```

Claude 也能调用其他模型。每个会话会加载一个 byom skill，并为每个模型生成一个 subagent。你可以说“找 GPT 给个第二意见”或者“让一个快模型扫一遍这些文件”，Claude 会自己选合适的模型。

byom 是独立项目，与 Anthropic、OpenAI 及其他任何服务商均无隶属或背书关系。

## 安装

支持 macOS 和 Linux。需要先装好 [Claude Code](https://code.claude.com/docs/en/setup)（已在 2.1.295 上测试），并至少有一个可用模型：ChatGPT Plus 或 Pro 套餐、任一受支持服务商的 API key、本地运行的 Ollama 或 LM Studio，或者 Claude（见 [Claude 模型](#claude-models)）。

用 Homebrew 安装：

```sh
brew install baoengineer/tap/byom
```

用 Cargo 安装：

```sh
cargo install byom
```

也可以从 [Releases](https://github.com/baoengineer/byom/releases) 下载二进制文件，以 Apple 芯片的 Mac 为例：

```sh
curl -LO https://github.com/baoengineer/byom/releases/latest/download/byom-v0.4.2-aarch64-apple-darwin.tar.gz
tar xzf byom-v0.4.2-aarch64-apple-darwin.tar.gz
mv byom-v0.4.2-aarch64-apple-darwin/byom ~/.local/bin/    # any directory on your PATH
xattr -d com.apple.quarantine ~/.local/bin/byom             # macOS: the binary is not notarized
```

每个压缩包都附带一个 `.sha256` 文件，可用来校验。或者用 Rust 1.89 及以上版本从最新源码构建：

```sh
cargo install --git https://github.com/baoengineer/byom
```

## 快速开始

```sh
byom login        # pick a provider: ChatGPT sign-in, or paste an API key
byom              # start Claude Code with every signed-in model
```

在 Claude Code 里，`/model` 会列出所有可用模型。Claude Code 自己的参数照常可用：`byom --resume <id>`、`byom -c`、`byom -p "..."`。想直接用某个模型启动，运行 `byom run openai/gpt-5.6-sol`，其他参数加在后面即可，比如 `byom run kimi/k3 --continue`。

`byom config` 会打开一个主界面，包含四个标签页：**Models**（模型列表，可设置主模型、后台模型和 subagent 模型）、**Providers**（登录状态和 API key）、**Roles**（模型槽位、别名、relay、上下文、传输方式）以及 **Usage**（请求数、token 数、预估费用）。

## 服务商

| 服务商 | ID | 登录方式 | 说明 |
|---|---|---|---|
| OpenAI ChatGPT 套餐 | `openai` | Sign in with ChatGPT | Plus 或 Pro；每个应用有单独的用量上限 |
| Z.ai GLM Coding Plan（智谱 GLM） | `zai` | API key | 适用于 Claude Code 等受支持的编程工具 |
| Kimi For Coding | `kimi` | API key | 需要 Kimi Code 会员 |
| Moonshot AI（月之暗面） | `moonshot` | API key | |
| MiniMax | `minimax` | API key | Token Plan 允许在第三方工具中使用 |
| DeepSeek | `deepseek` | API key | |
| OpenRouter | `openrouter` | API key | 在 `providers.openrouter.models` 里写上你要用的模型 |
| Groq、Mistral、Gemini、Cerebras、Together、xAI | `groq` … `xai` | API key | Gemini 使用 AI Studio 的 key |
| Ollama、LM Studio | `ollama`, `lmstudio` | 无需登录 | Ollama 0.14+，LM Studio 0.4.1+ |
| Anthropic (Claude) | `anthropic` | API key，或 Claude Code 自己的登录 | 见 [Claude 模型](#claude-models) |

其他任何兼容 Anthropic 或 OpenAI 接口的端点，都可以作为自定义服务商接入，详见 [docs/CONFIGURATION.md](docs/CONFIGURATION.md)。

<a id="claude-models"></a>

## Claude 模型

在 byom 里，Claude 模型默认关闭，需要你先选一种接入方式：

- **Anthropic API key**：`byom login anthropic`。请求会用这个 key 发往 Anthropic。
- **通过 relay 使用你的 Claude 套餐**：`byom config set relay true`。开启后，byom 会把 Claude 请求原样转发给 Anthropic，使用的是 Claude Code 自己的登录，byom 不保存其中任何内容。如果你同时保存了 API key，以 Claude Code 的登录为准。

开启 relay 前请先读这一段。Anthropic [文档中说明了](https://code.claude.com/docs/en/llm-gateway)可以在 Claude Code 和其 API 之间使用本地网关，但其 [Claude Code 条款](https://code.claude.com/docs/en/legal-and-compliance)也写明，第三方开发者不得代表其用户通过 Free、Pro 或 Max 套餐的凭据转发请求。你自己运行的工具是否属于这种情况，由 Anthropic 判断。因此 relay 由你自己决定，默认关闭。不开启时，byom 完全不会使用 Claude Code 的 Claude 登录，byom 会话中的 claude.ai connectors 也处于关闭状态。

## 在 Claude 里使用其他模型

每个 `byom` 会话都会加载一个小插件，不会往 `~/.claude` 写入任何东西：

- **byom skill** 告诉 Claude 什么时候适合交给别的模型（第二意见、低成本的批量任务、超长输入），以及怎么委派。Claude 通过 `byom --skill` 加载完整说明。
- **每个模型一个 subagent**，命名形如 `byom:openai-gpt-5-6-sol`。Claude 的 Agent 工具只能直接指定 Claude 模型，所以 subagent 和 workflow 要跑在其他模型上，就得靠这些 agent。
- `byom models --json` 为 Claude 提供实时模型列表：每个模型的 agent、上下文窗口、effort 档位、价格和状态（`ready`、`capped`、`no-key`、`signed-out`、`offline`）。没有运行的本地服务器不会出现在会话里。

## 多模型团队

Claude Code 的 [agent teams](https://code.claude.com/docs/en/agent-teams) 允许一个主会话创建多个队友，它们共享任务列表并互相发消息。配合 byom，每个队友可以跑在不同的服务商上：

```sh
byom config set teams true
```

然后直接让 Claude 组队，例如：“建一个团队：GPT-5.6-Sol 做 review，GLM-5.3 写实现，Kimi K3 写测试”。Claude 会根据每个队友对应的 byom agent（`byom:openai-gpt-5-6-sol`，…）选定模型，并协调分工。队友运行在主会话的终端里，因此共用它与 byom 的连接。agent teams 在 Claude Code 中仍是实验功能，每个队友都会单独消耗 token。

## 命令

```text
byom [claude flags]                   start Claude Code, as in byom --resume <id>
byom run <model> [claude flags]       start on a specific model
byom login [provider]                 sign in, or save an API key
byom models [--json] [--refresh]      the model roster
byom config [get|set|unset|path]      settings (no argument: the config screen)
byom doctor                           check Claude Code, sign-ins, the bridge and each provider
byom logs [-n N]                      recent requests: model, route, latency, tokens
```

其余命令（`logout`、`auth`、`status`、`restart`、`stop`、`--skill`）可以用 `byom --help` 查看。设置项和凭据说明见 [docs/CONFIGURATION.md](docs/CONFIGURATION.md)。

## 常见问题

**这样用合规吗？** byom 不会伪装成其他客户端，不读取其他工具的凭据，不做账号池，也不提供服务商禁止第三方工具使用的登录方式（Claude、Copilot、Gemini CLI 和 Antigravity 的订阅）。ChatGPT 走的是 OpenAI 的 [plan usage in open-source apps](https://developers.openai.com/siwc/token-sharing-open-source) 流程，使用 byom 自己的应用名。Z.ai、Kimi 和 MiniMax 的套餐都把 Claude Code 列为受支持的客户端，请求会保留 Claude Code 自己的客户端标识。Claude 的情况见 [Claude 模型](#claude-models)。各服务商自己的套餐限制照常适用。

**会改动 Claude Code 吗？** 不会。byom 只是带上一个 base URL、一个设置文件和一个会话插件来启动官方的 `claude`。直接运行 `claude` 时，byom 的东西都不会出现。

**会存储哪些数据？** 设置、你添加的凭据、一个本地 bridge key 以及请求元数据，全部放在 `~/.byom`。详见 [SECURITY.md](SECURITY.md)。

**和 claude-code-router 有什么区别？** 两者都能把 Claude Code 的请求转给其他模型。byom 的侧重点有四处不同：它通过 OpenAI 官方的开源应用登录流程接入你的 ChatGPT 套餐，不需要 API key；Claude 和其他模型在同一个 `/model` 列表里；Claude 拥有每个模型对应的 subagent，还能组建多模型团队，可以自己把工作交给其他模型；它是单个 Rust 二进制文件，不需要配置文件就能找到你的模型。

**从 byoclaude 迁移过来？** byom 在 0.4.0 之前叫 byoclaude。首次运行时会把 `~/.byoclaude` 复制到 `~/.byom`；agent 名称从 `byoclaude:` 改为 `byom:`。

## 用量限制

各服务商自己的套餐限制照常适用。ChatGPT 还会对每个已连接的应用单独设上限：如果请求失败并提示 "ChatGPT plan limit reached"，可以在 [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage) 里调高 byom 的上限，或者等额度重置。触发套餐限制的请求不会重试，如果配置了 fallback，会由它接手。`byom models` 会把最近触发过限制的模型标记为 `capped`。

## 故障排查

- **出了问题。** 先运行 `byom doctor`，再看 `byom logs`。
- **提示 "Not signed in" 或者没有模型。** 运行 `byom login`，然后 `byom models --refresh`。
- **`/model` 里没有 Claude。** Claude 模型需要手动开启，见 [Claude 模型](#claude-models)。
- **Ollama 答非所问。** Claude Code 的提示词很长，用 `OLLAMA_CONTEXT_LENGTH=32768 ollama serve` 调大 Ollama 的上下文，并选用支持工具调用的模型。
- **升级后请求失败。** 运行 `byom restart` 换上新的 bridge；已打开的会话会在下一次请求时自动重连。
- **Claude Code 里提示 "Connection refused"。** bridge 没有在运行；`byom restart` 会启动它，会话可以继续。

## 卸载

```sh
byom stop --force
rm "$(command -v byom)"      # or: cargo uninstall byom
rm -rf ~/.byom
```

如果用 ChatGPT 登录过，还需要在 [ChatGPT settings](https://chatgpt.com/settings) 里断开 byom，并到各服务商处撤销你粘贴过的 API key。

## 参与贡献

见 [CONTRIBUTING.md](CONTRIBUTING.md)、[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) 和 [docs/ROADMAP.md](docs/ROADMAP.md)。

## 许可证

[MIT](LICENSE)
