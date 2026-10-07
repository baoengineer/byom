# byoclaude

**Bring your own Claude.** Keep [Claude Code](https://claude.com/claude-code) as your harness and use every model you can sign in to, in the same session: Claude on your Claude plan, GPT on your ChatGPT plan, GLM, Kimi, MiniMax, DeepSeek, Gemini, Groq, OpenRouter, and local models through Ollama or LM Studio.

![byoclaude config, then Claude Code with GPT and Claude models in /model](docs/demo/demo.gif)

Claude Code stays unmodified. Its tools, subagents, hooks, MCP servers, skills, plan mode and session resume all work as usual. byoclaude runs a small local router that sends each request to the right provider by model ID:

```text
claude (official, signed in as usual)
  ▼
byoclaude router (127.0.0.1) ─┬─ claude-*        → Anthropic, relayed on Claude Code's own sign-in
                              ├─ openai/*        → ChatGPT plan (Sign in with ChatGPT)
                              ├─ zai/*, kimi/*, minimax/*, deepseek/*, openrouter/*, ollama/*, lmstudio/*
                              │                  → Anthropic-compatible APIs, forwarded
                              └─ groq/*, gemini/*, mistral/*, cerebras/*, together/*, xai/*
                                                 → OpenAI-compatible APIs, translated
```

Claude can use the other models too. Each session loads a byoclaude skill and one subagent per model, so you can say "get a second opinion from GPT" or "have a fast model scan these files", and Claude picks the right one.

byoclaude is an independent project, not affiliated with or endorsed by Anthropic, OpenAI or any other provider.

## Requirements

- macOS or Linux
- Claude Code 2.1.289 or newer
- At least one of: a Claude plan (Claude Code signed in), a ChatGPT Plus or Pro plan, an API key for a supported provider, or a local Ollama or LM Studio server

## Install

Download a binary for macOS or Linux from [Releases](https://github.com/baoengineer/byoclaude/releases) and put `byoclaude` on your `PATH`, or build from source with Rust 1.89 or newer:

```sh
cargo install --git https://github.com/baoengineer/byoclaude
```

## Quick start

```sh
byoclaude login           # pick a provider: ChatGPT sign-in, or paste an API key
byoclaude                 # start Claude Code with every signed-in model
```

Inside Claude Code, `/model` lists every model you can use. Start on a specific model with `byoclaude run openai/gpt-5.6-sol`; arguments after `--` go to `claude`, as in `byoclaude run -- --continue`.

`byoclaude config` opens a home screen with four tabs: **Models** (the roster, and setting the main, background and subagent models), **Providers** (sign-ins and keys), **Roles** (every setting) and **Usage** (requests, tokens and estimated cost).

## Providers

| Provider | ID | Sign-in | Notes |
|---|---|---|---|
| Anthropic (Claude) | `anthropic` | Claude Code's own sign-in, or an API key | Relayed as sent; see below |
| OpenAI ChatGPT plan | `openai` | Sign in with ChatGPT | Plus or Pro; per-app usage cap |
| Z.ai GLM Coding Plan | `zai` | API key | Coding Plan is for supported coding tools such as Claude Code |
| Kimi For Coding | `kimi` | API key | Kimi Code membership |
| Moonshot AI | `moonshot` | API key | |
| MiniMax | `minimax` | API key | Token Plan allows third-party tools |
| DeepSeek | `deepseek` | API key | |
| OpenRouter | `openrouter` | API key | Name the models you want in `providers.openrouter.models` |
| Groq, Mistral, Gemini, Cerebras, Together, xAI | `groq` … `xai` | API key | Gemini uses an AI Studio key |
| Ollama, LM Studio | `ollama`, `lmstudio` | none | Local servers |

Any other Anthropic- or OpenAI-compatible endpoint works as a custom provider:

```sh
byoclaude config set providers.mybox.protocol openai-chat     # or anthropic
byoclaude config set providers.mybox.base_url http://10.0.0.5:8000/v1
byoclaude config set providers.mybox.api_key '$MYBOX_KEY'      # or a key, or '!pass show mybox'
byoclaude config set providers.mybox.models "qwen3-coder, glm-5"
```

byoclaude never imitates another client, reads another tool's credentials, pools accounts, or offers sign-ins that providers forbid for third-party tools (Claude, Copilot, Gemini CLI and Antigravity subscriptions).

## Claude and other models together

When Claude Code is signed in to Claude, byoclaude relays Claude requests to Anthropic as Claude Code sent them: the same body and headers, with Claude Code's own credential, minus only transport headers (connection and compression) and byoclaude's own key, so Claude keeps working as before and the other models are added alongside. byoclaude stores nothing from that sign-in. Anthropic has not explicitly approved relaying subscription traffic through a local proxy; the first run says so. To keep Claude Code talking to Anthropic directly and use byoclaude only for other models, turn the relay off:

```sh
byoclaude config set relay false
```

Without the relay (or without a Claude sign-in), Claude models are hidden and every slot uses your other providers.

## Using other models from Claude

Each `byoclaude` session loads a small plugin:

- A **byoclaude skill** tells Claude when another model helps (second opinions, cheap bulk work, long inputs) and how to delegate well. Claude loads the full guide with `byoclaude --skill`.
- **One subagent per model**, named like `byoclaude:openai-gpt-5-6-sol`. Claude's Agent tool can only name Claude models directly, so these agents are how subagents and workflows run on other models.
- `byoclaude models --json` gives Claude the live roster: each model's agent, context window, effort levels, price and status (`ready`, `capped`, `no-key`).

## Configuration

Settings live in `~/.byoclaude/config.json` (or `$BYOCLAUDE_HOME`). Edit them in `byoclaude config`, with `byoclaude config set/get/unset`, or by hand:

```json
{
  "model": "openai/gpt-5.6-sol",
  "background": "groq/llama-3.3-70b-versatile",
  "subagent": "",
  "aliases": { "haiku": "zai/glm-5.3-flash" },
  "relay": true,
  "fallbacks": { "openai/gpt-6-astra": ["openai/gpt-5.6-sol", "kimi/k3"] },
  "providers": { "openrouter": { "models": ["qwen/qwen3-coder"] } }
}
```

| Setting | Meaning |
|---|---|
| `model` | Main model; empty keeps Claude Code's default (with the relay) or picks the first available model |
| `background` | Model for titles and summaries |
| `subagent` | Model for subagents that don't name one |
| `aliases` | What agents asking for `opus`, `sonnet` or `haiku` get |
| `relay` | Relay Claude models on Claude Code's sign-in (default `true`) |
| `fallbacks` | Models to try, in order, when a model fails before answering (plan cap, outage, missing key) |
| `providers.<id>` | `api_key`, `base_url`, `protocol`, `models`, `disabled` |
| `context_tokens` | Compaction window for non-Claude main models; `0` uses the catalog value |
| `transport` | ChatGPT plan transport: `auto` (WebSocket) or `http` |
| `behaves_as` | Claude model whose handling Claude Code applies to other models |

Credentials are stored in `~/.byoclaude/auth.json` (owner-only), or referenced from config as `$ENV_VAR` or `!command`.

## Commands

| Command | Does |
|---|---|
| `byoclaude`, `byoclaude run [model] [-- claude args]` | Start Claude Code |
| `byoclaude login [provider]`, `logout <provider>` | Sign in, save or remove a key |
| `byoclaude auth` | Each provider's sign-in status |
| `byoclaude models [--json] [--provider id] [--refresh] [--all]` | The model roster |
| `byoclaude config`, `config get/set/unset/path` | Settings |
| `byoclaude doctor` | Check Claude Code, sign-ins, the relay, the bridge and each provider |
| `byoclaude logs [-n N]` | Recent requests: model, route, latency, tokens |
| `byoclaude status`, `stop` | The local bridge |
| `byoclaude --skill` | The guide Claude reads |

## Usage limits

Each provider's own plan limits apply. ChatGPT also caps each connected app separately: if requests fail with "ChatGPT plan limit reached", raise byoclaude's limit in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage) or wait for the reset. Plan limits are not retried; a configured fallback takes over instead. `byoclaude models` marks recently limited models as `capped`.

## Troubleshooting

- **Something doesn't work.** Run `byoclaude doctor`, then `byoclaude logs`.
- **"Not signed in" or no models.** `byoclaude login`, then `byoclaude models --refresh`.
- **Ollama answers off-topic.** Claude Code's prompt is large; raise Ollama's context with `OLLAMA_CONTEXT_LENGTH=32768 ollama serve`, and use a model that supports tools.
- **Requests fail after an upgrade.** `byoclaude run` replaces an older bridge automatically; otherwise `byoclaude stop` and retry.
- **Slow turns.** `byoclaude logs` shows time to first token per request; ChatGPT-plan turns show `ws+cont` when only new messages were sent.

## Development

```sh
cargo test
tests/e2e/run.sh          # real Claude Code against local mocks of OpenAI and Anthropic; no plan usage
vhs docs/demo/demo.tape   # re-record the demo GIF
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PROVIDER.md](docs/PROVIDER.md), [docs/ROADMAP.md](docs/ROADMAP.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
