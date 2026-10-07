# Roadmap

## Destination

**byoclaude: bring your own Claude.** The official Claude Code becomes a multi-provider workspace. The session's default model is whatever the user chooses, and every signed-in provider is available to `/model`, subagents and workflows. A local router sends each request to the right place by model ID:

```text
claude (official, unmodified; signed in to the user's Claude plan or not)
  │  Authorization: Claude Code's own credential   x-byoclaude-key: bridge key
  ▼
byoclaude router ── model ID ──┬─ claude-*           → api.anthropic.com, relayed untouched
                               ├─ openai-responses   → ChatGPT plan · OpenAI key · xAI      (built)
                               ├─ anthropic-compat   → Z.ai · Kimi · MiniMax · DeepSeek · OpenRouter · Ollama · LM Studio
                               └─ openai-chat        → Groq · Mistral · Gemini key · Together · local OpenAI servers
```

Claude learns the roster from the binary, in the style of herdr: a thin skill tells Claude to load `byoclaude --skill`, and `byoclaude models --json` reports the live roster.

Boundaries: never imitate another client, never read other tools' credentials, never pool accounts, never offer Claude, Copilot, Gemini CLI or Antigravity subscription logins.

## Evidence (2026-10-07)

- A Claude Code signed in to a Claude plan sends its own `Authorization: Bearer sk-ant-oat…` to a custom `ANTHROPIC_BASE_URL`, so the router can relay Claude requests without storing anything.
- A subagent declared with `model: gpt-5.6-sol` sends that model ID to the base URL while the main loop stays on Claude, so routing by model ID works for subagents.
- Claude Code warns "unrecognized model" for unknown IDs; `modelPicker` rows with `behavesAs` remove the warning.
- With a non-Claude main model, Claude Code disables its advisor tool.
- Most providers accept Anthropic Messages natively; OpenAI-family APIs need translation. See [PROVIDER.md](PROVIDER.md) for the ChatGPT route.

## Build order

1. **Router core and Claude relay.** Provider abstraction (`id`, `protocol`, `base_url`, `auth`), routing by model ID, Claude passthrough, bridge key in a custom header, merged `modelPicker` (built-in Claude rows plus provider rows).
2. **Providers.** Anthropic-compatible API-key providers, then an OpenAI Chat Completions adapter, then local servers. Sign-in: API keys and Sign in with ChatGPT.
3. **Claude-facing layer.** Injected skill, `byoclaude --skill`, `models --json` with live status.
4. **CLI, TUI, config.** The command set and tabbed home below; `~/.byoclaude` and migration.
5. **Reliability.** Fallbacks on quota or outage, usage and cost stats from the bridge log.

## Decisions (settled 2026-10-07)

| # | Topic | Decision |
|---|---|---|
| 1 | Claude relay | On when Claude Code is signed in to a Claude plan. First run prints a one-time note explaining the relay; `"relay": false` turns it off. Requests to Anthropic are forwarded untouched. |
| 2 | Model IDs | `provider/model` (`openai/gpt-5.6-sol`, `kimi/kimi-k3`, `ollama/qwen3`); Claude models keep native IDs (`claude-opus-5-5`). Verified: Claude Code accepts `/` in picker rows and subagent `model:` fields. |
| 3 | Bridge auth | Bridge key in `x-byoclaude-key`, sent through `ANTHROPIC_CUSTOM_HEADERS`; `Authorization` carries Claude Code's own credential. Verified. |
| 4 | No Claude sign-in | Claude rows are hidden unless Claude Code is signed in or an Anthropic API key is added as a provider. |
| 5 | Catalog | A curated provider list ships in the binary (base URL, protocol, sign-in kind, terms notes). Model lists come live from each provider's `/models` where available, enriched with a models.dev snapshot (context, cost, reasoning). Users add custom providers in config. |
| 6 | Roles | Config slots `model` (main), `background` (titles, summaries), `subagent` (default for agents without a model), and `aliases` mapping `opus`/`sonnet`/`haiku` to any model ID. With the relay active, aliases default to native Claude; otherwise they follow `model`. No content-based routing. |
| 7 | Credentials | `~/.byoclaude/auth.json`, mode 0600, keyed by provider (OAuth tokens and pasted keys). Config may reference `$ENV_VAR` or `!command` instead of storing a key. |
| 8 | Config | JSON at `~/.byoclaude/config.json` (with `auth.json`, `cache/`, `logs/`). 0.1.0's `~/.byoclaude-rs` migrates automatically on first run. |
| 9 | CLI | `byoclaude` (same as `run`), `run [model] [-- claude args]`, `login [provider]`, `logout [provider]`, `auth`, `models [--json] [--provider] [--refresh]`, `config` (TUI) and `config get/set/path`, `doctor`, `status`, `stop`, `logs`, `--skill`. First run with nothing signed in goes to `login`. |
| 10 | TUI | `byoclaude config` becomes a tabbed home: Models (searchable roster with badges), Providers (sign in/out, keys, status), Roles, Usage. |
| 11 | Claude-facing skill | Injected per session as a plugin by `byoclaude run`; nothing written to `~/.claude`. The skill tells Claude to load `byoclaude --skill` once and use `byoclaude models --json`. |
| 12 | Headless jobs | Deferred. `ask`/`review`/`delegate` come later if subagents leave a gap. |
| 13 | Role agents | None for now. The skill teaches Claude to pass `model: <id>` to subagents. |
| 14 | Fallback | Opt-in chains in config (`"fallbacks": {"openai/gpt-6-astra": ["openai/gpt-5.6-sol"]}`), applied only before any output streams, logged. Capped models are flagged in `models --json` until reset. |
| 15 | Usage | Summed from the bridge log by model, provider and day; cost estimated from models.dev prices and labeled as an API-price equivalent. |
| 16 | Release | 0.2.0, with automatic migration from 0.1.0. |

## Verified during build

- `claude --plugin-dir` loads the session plugin for one session; plugin agents with any model ID work as subagents.
- `claude auth status --json` reports whether Claude Code is signed in; the launcher uses it to enable the relay.
- The Agent tool's `model` field accepts only Claude aliases, so the plugin carries one agent per model (plumbing for decision 13, not role agents).

## Out of scope

- Account pooling, client impersonation, subscription logins that providers forbid for third-party tools.
- A hosted service or paid gateway.

## Status

- [x] Step 1: `~/.byoclaude` layout, provider-keyed `auth.json`, config schema (roles, relay, providers, fallbacks), migration from `~/.byoclaude-rs`.
- [x] Step 2: router core, Claude relay, launcher relay env, harness coverage (mixed session verified end to end against mocks).
- [x] Step 3: Anthropic-compatible providers (Z.ai, Kimi, Moonshot, MiniMax, DeepSeek, OpenRouter, Ollama, LM Studio), API-key sign-in, cross-provider roster from provider model lists and models.dev. Verified with real Claude Code against a local Ollama.
- [x] Step 4: CLI verbs (`login`, `logout`, `auth`, `models --json`, `doctor`, `logs`, `config get/set/unset/path`); bare `byoclaude` runs.
- [x] Step 5: session plugin with the byoclaude skill and one agent per model (`byoclaude:<model>`), since the Agent tool's `model` field accepts only Claude aliases; `byoclaude --skill`.
- [x] Step 6: OpenAI Chat Completions adapter and built-ins (Groq, Mistral, Gemini, Cerebras, Together, xAI); custom `openai-chat` providers. Verified with real Claude Code tool round trips against Ollama on both the Anthropic and Chat protocols. Fallback chains applied before output.
- [x] Step 7: tabbed `byoclaude config` (Models, Providers, Roles, Usage) and token usage for every route, including forwarded streams.
