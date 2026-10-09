<p><picture><source media="(prefers-color-scheme: dark)" srcset="docs/brand/mark-dark.svg"><img src="docs/brand/mark-light.svg" width="56" height="56" alt=""></picture></p>

# byom

[![CI](https://github.com/baoengineer/byom/actions/workflows/ci.yml/badge.svg)](https://github.com/baoengineer/byom/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/baoengineer/byom)](https://github.com/baoengineer/byom/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**Bring your own model to Claude Code.** Keep using [Claude Code](https://claude.com/claude-code), and use every model you can sign in to in the same session: GPT on your ChatGPT plan, GLM, Kimi, MiniMax, DeepSeek, Gemini, Groq, OpenRouter, and local models through Ollama or LM Studio. Claude stays available too.

![byom config, then Claude Code on GPT-5.6-Sol, then /model with Claude and GPT side by side](docs/demo/demo.gif)

Claude Code stays unmodified. Its tools, subagents, hooks, MCP servers, skills, plan mode and session resume work as usual. byom runs a small local router that sends each request to the right provider by model ID:

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

Claude can use the other models too. Each session loads a byom skill and one subagent per model, so you can say "get a second opinion from GPT" or "have a fast model scan these files", and Claude picks the right one.

byom is an independent project, not affiliated with or endorsed by Anthropic, OpenAI or any other provider.

## Install

Requires macOS or Linux, [Claude Code](https://code.claude.com/docs/en/setup) (tested with 2.1.295), and at least one model: a ChatGPT Plus or Pro plan, an API key for a supported provider, a local Ollama or LM Studio server, or Claude (see [Claude models](#claude-models)).

With Homebrew:

```sh
brew install baoengineer/tap/byom
```

Or download a binary from [Releases](https://github.com/baoengineer/byom/releases), for example on an Apple silicon Mac:

```sh
curl -LO https://github.com/baoengineer/byom/releases/latest/download/byom-v0.4.1-aarch64-apple-darwin.tar.gz
tar xzf byom-v0.4.1-aarch64-apple-darwin.tar.gz
mv byom-v0.4.1-aarch64-apple-darwin/byom ~/.local/bin/    # any directory on your PATH
xattr -d com.apple.quarantine ~/.local/bin/byom             # macOS: the binary is not notarized
```

Each archive has a `.sha256` file to check it against. Or build from source with Rust 1.89 or newer:

```sh
cargo install --git https://github.com/baoengineer/byom
```

## Quick start

```sh
byom login        # pick a provider: ChatGPT sign-in, or paste an API key
byom              # start Claude Code with every signed-in model
```

Inside Claude Code, `/model` lists every model you can use. Claude Code's own flags work as usual: `byom --resume <id>`, `byom -c`, `byom -p "..."`. Start on a specific model with `byom run openai/gpt-5.6-sol`, and add flags after it, as in `byom run kimi/k3 --continue`.

`byom config` opens a home screen with four tabs: **Models** (the roster; set the main, background and subagent models), **Providers** (sign-ins and keys), **Roles** (model slots, aliases, relay, context, transport) and **Usage** (requests, tokens, estimated cost).

## Providers

| Provider | ID | Sign-in | Notes |
|---|---|---|---|
| OpenAI ChatGPT plan | `openai` | Sign in with ChatGPT | Plus or Pro; per-app usage cap |
| Z.ai GLM Coding Plan | `zai` | API key | For supported coding tools such as Claude Code |
| Kimi For Coding | `kimi` | API key | Kimi Code membership |
| Moonshot AI | `moonshot` | API key | |
| MiniMax | `minimax` | API key | Token Plan allows third-party tools |
| DeepSeek | `deepseek` | API key | |
| OpenRouter | `openrouter` | API key | Name the models you want in `providers.openrouter.models` |
| Groq, Mistral, Gemini, Cerebras, Together, xAI | `groq` … `xai` | API key | Gemini uses an AI Studio key |
| Ollama, LM Studio | `ollama`, `lmstudio` | none | Ollama 0.14+, LM Studio 0.4.1+ |
| Anthropic (Claude) | `anthropic` | API key, or Claude Code's sign-in | See [Claude models](#claude-models) |

Any other Anthropic- or OpenAI-compatible endpoint works as a custom provider; see [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Claude models

Claude models are off in byom until you choose how to reach them:

- **An Anthropic API key**: `byom login anthropic`. Requests go to Anthropic with that key.
- **Your Claude plan, through the relay**: `byom config set relay true`. byom then forwards Claude requests to Anthropic exactly as Claude Code sent them, with Claude Code's own sign-in, and stores nothing from it. If you also saved an API key, Claude Code's sign-in wins.

Read this before turning the relay on. Anthropic [documents local gateways](https://code.claude.com/docs/en/llm-gateway) between Claude Code and its API, but its [terms for Claude Code](https://code.claude.com/docs/en/legal-and-compliance) also say third-party developers may not route requests through Free, Pro or Max plan credentials on their users' behalf. Whether a tool you run yourself falls under that is Anthropic's call, so the relay is your choice and off by default. Without it, Claude Code's Claude sign-in is not used at all, and claude.ai connectors are off in byom sessions.

## Using other models from Claude

Each `byom` session loads a small plugin, and nothing is written to `~/.claude`:

- A **byom skill** tells Claude when another model helps (second opinions, cheap bulk work, long inputs) and how to delegate. Claude loads the full guide with `byom --skill`.
- **One subagent per model**, named like `byom:openai-gpt-5-6-sol`. Claude's Agent tool can only name Claude models directly, so these agents are how subagents and workflows run on other models.
- `byom models --json` gives Claude the live roster: each model's agent, context window, effort levels, price and status (`ready`, `capped`, `no-key`, `signed-out`, `offline`). Local servers that aren't running are left out of the session.

## A team of models

Claude Code's [agent teams](https://code.claude.com/docs/en/agent-teams) let a lead session spawn teammates that share a task list and message each other. With byom, each teammate can run on a different provider:

```sh
byom config set teams true
```

Then ask for a team, for example: "spawn a reviewer on GPT-5.6-Sol, an implementer on GLM-5.3 and a test writer on Kimi K3". Claude picks each teammate's model from its byom agent (`byom:openai-gpt-5-6-sol`, …) and coordinates the work. Teammates run inside the lead's terminal, so they share its connection to byom. Agent teams are experimental in Claude Code and every teammate uses its own tokens.

## Commands

```text
byom [claude flags]                   start Claude Code, as in byom --resume <id>
byom run <model> [claude flags]       start on a specific model
byom login [provider]                 sign in, or save an API key
byom models [--json] [--refresh]      the model roster
byom config [get|set|unset|path]      settings (no argument: the config screen)
byom doctor                           check Claude Code, sign-ins, the bridge and each provider
byom logs [-n N]                      recent requests: model, route, latency, tokens
```

`byom --help` lists the rest (`logout`, `auth`, `status`, `restart`, `stop`, `--skill`). Settings and credentials are described in [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## FAQ

**Is this allowed?** byom never imitates another client, reads another tool's credentials, pools accounts, or offers sign-ins that providers forbid for third-party tools (Claude, Copilot, Gemini CLI and Antigravity subscriptions). ChatGPT uses OpenAI's [plan usage in open-source apps](https://developers.openai.com/siwc/token-sharing-open-source) flow under byom's own app name. Z.ai, Kimi and MiniMax plans name Claude Code as a supported client, and requests keep Claude Code's own client identity. For Claude, see [Claude models](#claude-models). Each provider's own plan limits apply.

**Does it change Claude Code?** No. byom starts the official `claude` with a base URL, a settings file and a session plugin. Run `claude` directly and nothing from byom is there.

**What does it store?** Settings, credentials you add, a local bridge key and request metadata, all in `~/.byom`. See [SECURITY.md](SECURITY.md).

**How is it different from claude-code-router?** Both route Claude Code's requests to other models. byom's focus is different in four ways: it signs in to your ChatGPT plan through OpenAI's official open-source app flow, so no API key; Claude stays in the same `/model` list as the other models; Claude gets one subagent per model and can run model teams, so it can hand work to other models itself; and it ships as one Rust binary that finds your models without a config file.

**Coming from byoclaude?** byom was called byoclaude until 0.4.0. The first run copies `~/.byoclaude` to `~/.byom`; agent names change from `byoclaude:` to `byom:`.

## Usage limits

Each provider's plan limits apply. ChatGPT also caps each connected app: if requests fail with "ChatGPT plan limit reached", raise byom's limit in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage) or wait for the reset. Plan limits are not retried; a configured fallback takes over instead. `byom models` marks recently limited models as `capped`.

## Troubleshooting

- **Something doesn't work.** Run `byom doctor`, then `byom logs`.
- **"Not signed in" or no models.** `byom login`, then `byom models --refresh`.
- **No Claude in `/model`.** Claude models are opt-in; see [Claude models](#claude-models).
- **Ollama answers off-topic.** Claude Code's prompt is large; raise Ollama's context with `OLLAMA_CONTEXT_LENGTH=32768 ollama serve`, and use a model that supports tools.
- **Requests fail after an upgrade.** `byom restart` swaps in the new bridge; open sessions reconnect on their next request.
- **"Connection refused" in Claude Code.** The bridge isn't running; `byom restart` starts it, and the session continues.

## Uninstall

```sh
byom stop --force
rm "$(command -v byom)"      # or: cargo uninstall byom
rm -rf ~/.byom
```

Also disconnect byom in [ChatGPT settings](https://chatgpt.com/settings) if you signed in, and revoke API keys you pasted at each provider.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [docs/ROADMAP.md](docs/ROADMAP.md).

## License

[MIT](LICENSE)
