# Architecture

```text
byom run ──exec──▶ claude (unmodified) ──Anthropic Messages──▶ bridge (127.0.0.1) ──▶ providers
      │                    --settings: model picker rows                │
      │                    --plugin-dir: skill + one agent per model    ├─ route by model ID
      └─ starts or replaces the bridge                                   └─ forward, translate or relay
```

## Request routing

Model IDs decide everything. `claude-*` goes to the `anthropic` provider; `provider/model` goes to that provider (`openai/gpt-5.6-sol`, `kimi/k3`, `ollama/qwen3:1.7b`). A bare non-Claude ID from a 0.1.0 config is treated as `openai/<id>`.

Each provider has a protocol:

| Protocol | Providers | What the bridge does |
|---|---|---|
| `anthropic` | anthropic (relay), zai, kimi, moonshot, minimax, deepseek, openrouter, ollama, lmstudio, custom | Forwards the request: base URL swap, model rewritten to the provider's name, credential swap (except the relay), Claude-only `anthropic-beta` dropped. Response status, headers and stream pass through. |
| `openai-responses` | openai (ChatGPT plan) | Translates to the Responses API over a per-session WebSocket ([PROVIDER.md](PROVIDER.md)). |
| `openai-chat` | groq, mistral, gemini, cerebras, together, xai, custom | Translates to Chat Completions and back: text, images, tool calls, reasoning as thinking, usage. |

If a model fails before producing output and `fallbacks` names alternatives, the bridge tries them in order.

## The Claude relay

The relay is opt-in (`relay`, default `false`). When it is on and Claude Code is signed in (`claude auth status --json` reports `loggedIn`), the launcher leaves Claude Code's own credential in place and sends the bridge key in `x-byom-key` through `ANTHROPIC_CUSTOM_HEADERS`. Claude requests reach Anthropic with the body unchanged and Claude Code's headers, including its `Authorization`; only hop-by-hop and compression headers and `x-byom-key` are dropped. Other Anthropic API paths Claude Code calls are passed through the same way. With the relay off or no Claude sign-in, the launcher sets `ANTHROPIC_AUTH_TOKEN` to the bridge key. If an Anthropic API key is saved (`byom login anthropic`), Claude requests use it and Claude models stay available; otherwise Claude rows are hidden and every slot points at other providers. A signed-in Claude Code's own credential always takes precedence over a saved key.

## Launcher (`launch.rs`)

- Builds a plan: main, background (`haiku` slot), `opus`/`sonnet` aliases, subagent default, and a compaction window only when no Claude model is available (the window applies to every model in the session). With Claude models available, unset slots keep Claude Code's defaults.
- Passes `--settings` with a `modelPicker` whose rows are every usable non-Claude model, each with `behavesAs` (default `claude-opus-5-5`) so Claude Code applies effort levels and thinking and accepts the ID. With the relay, rows are added to Claude Code's built-in list; without it they replace it.
- Generates the session plugin (`skill.rs`) under `~/.byom/plugin` and passes `--plugin-dir`.
- Disables tool search, since non-Claude models never receive deferred tools.
- Sets `teammateMode` to `in-process`: teammates in their own panes would not inherit the bridge key header. With `teams`, sets `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`; each teammate takes its model from its byom agent definition.

## Claude-facing layer (`skill.rs`, `roster.rs`)

The Agent tool's `model` field accepts only Claude aliases, but agent definitions accept any model ID. The plugin therefore contains one general-purpose agent per model (`byom:openai-gpt-5-6-sol`) and a pointer skill. `byom --skill` prints the guide for the installed version; `byom models --json` returns the roster with each model's agent type, limits, price and live status (`capped` comes from recent limit errors in the log).

## Catalog (`catalog.rs`)

The roster combines each usable provider's model list (`/v1/models` or `/models`, by protocol), the ChatGPT account catalog, and models.dev metadata (context window, max output, reasoning, tools, images, price). Providers without a model list fall back to models.dev; providers listing more than 25 models offer only those named in `providers.<id>.models`. The roster is cached in `cache/roster.json`; `models --refresh`, `login` and `r` in `byom config` rebuild it.

## State (`store.rs`)

`~/.byom` (or `$BYOM_HOME`): `config.json`, `auth.json` (credentials keyed by provider, owner-only, written atomically under `auth.lock`), `bridge.key`, `host-id`, `cache/`, `logs/bridge.log` (metadata only), `logs/bridge.err` (the bridge's stderr), `plugin/`, `.relay-note-shown`. On first run, an existing `~/.byoclaude` (0.2.0) or `~/.byoclaude-rs` (0.1.0) is copied over.

## Other modules

| Module | Role |
|---|---|
| `bridge.rs` | HTTP server, authentication, fallback loop, per-route logging including usage read from forwarded streams |
| `relay.rs` | Header and body preparation for forwarded routes |
| `request.rs`, `response.rs`, `upstream.rs` | Responses API translation and WebSocket transport |
| `chat.rs` | Chat Completions translation |
| `accounts.rs` | `login`, `logout`, `auth` |
| `doctor.rs` | `doctor` |
| `tui.rs` | `byom config` |
| `auth.rs` | Sign in with ChatGPT |

## Testing

- `cargo test`: translation, routing, relay header handling, launch plans, the config UI (ratatui test backend), and a launcher process test with a fake `claude`.
- `tests/e2e/run.sh`: the installed Claude Code through `byom run` against `examples/mock_openai.rs` (a Responses WebSocket mock plus an Anthropic HTTP mock). It covers thinking, tool execution, reasoning carried across turns, WebSocket continuation, web search, the Claude relay with Claude Code's credential, and a subagent running on another provider through the plugin agent.
