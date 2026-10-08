# Architecture

```text
byoclaude run ──exec──▶ claude (unmodified) ──Anthropic Messages──▶ bridge (127.0.0.1) ──▶ providers
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

When Claude Code is signed in (`claude auth status --json` reports `loggedIn`) and `relay` is on, the launcher leaves Claude Code's own credential in place and sends the bridge key in `x-byoclaude-key` through `ANTHROPIC_CUSTOM_HEADERS`. Claude requests reach Anthropic with the body unchanged and Claude Code's headers, including its `Authorization`; only hop-by-hop and compression headers and `x-byoclaude-key` are dropped. Other Anthropic API paths Claude Code calls are passed through the same way. With the relay off or no Claude sign-in, the launcher sets `ANTHROPIC_AUTH_TOKEN` to the bridge key, hides Claude rows, and points every slot at other providers.

## Launcher (`launch.rs`)

- Builds a plan: main, background (`haiku` slot), `opus`/`sonnet` aliases, subagent default, context window. With the relay, unset slots keep Claude Code's defaults.
- Passes `--settings` with a `modelPicker` whose rows are every usable non-Claude model, each with `behavesAs` (default `claude-opus-5-5`) so Claude Code applies effort levels and thinking and accepts the ID. With the relay, rows are added to Claude Code's built-in list; without it they replace it.
- Generates the session plugin (`skill.rs`) under `~/.byoclaude/plugin` and passes `--plugin-dir`.
- Disables tool search, since non-Claude models never receive deferred tools.

## Claude-facing layer (`skill.rs`, `roster.rs`)

The Agent tool's `model` field accepts only Claude aliases, but agent definitions accept any model ID. The plugin therefore contains one general-purpose agent per model (`byoclaude:openai-gpt-5-6-sol`), a pointer skill and a `/byoclaude:panel` command. `byoclaude --skill` prints the guide for the installed version; `byoclaude models --json` returns the roster with each model's agent type, limits, price and live status (`capped` comes from recent limit errors in the log).

## Panels (`panel.rs`)

`byoclaude panel` runs one question, or one change with `--attempt`, on several models and has a judge compare the results.

```text
byoclaude panel ──▶ select members + judge ──▶ claude -p --model <id>, one per member, in parallel ──▶ bridge
                                                  │ opinion: repo cwd, read-only tools
                                                  │ attempt: own worktree, Edit/Write; patch + tests collected
                                                  ▼
                    judge: claude -p, read-only tools, reports as Panelist A, B, C ──▶ verdict + ledger
```

- **Selection.** `--models` or `panel.models`, canonicalized. Otherwise `panel.size` ready models from distinct providers, the highest output price of each provider first, local providers last; Claude counts as `anthropic` and is ready only with the relay on and Claude Code signed in. Candidates are the roster's model picker entries plus every Claude model, and readiness matches the roster: `capped`, keyless and signed-out models are skipped and reported. Local providers count as ready without a reachability check. The judge is `--judge` or `panel.judge`, else the highest-priced ready model from a provider not on the panel (Claude on the relay first when `anthropic` is off it), else the highest-priced panel model with a warning. Selection is a pure function of config and roster.
- **Panelists.** Each member is a headless `claude -p --model <id> --output-format json --no-session-persistence --strict-mcp-config --json-schema <report>`, built by the launcher's shared command builder (same environment, bridge key and model picker rows) without the session plugin. Nested-session variables are removed and `BYOCLAUDE_PANEL=1` is set; `byoclaude panel` refuses to run when it is set. Every agent runs with `--restricted`, so user, project and local settings files do not apply and file tools stay inside the working directories: the cwd plus the repository root, or for an attempt its worktree. Opinion members use `--permission-mode dontAsk` with Read, Grep, Glob, WebSearch and read-only Bash plus the test command, and the judge the same without WebSearch or the test command; deny rules block the write and exec flags of those commands (`git diff --output`, `rg --pre`, `find -exec`, `-delete`, `-fprint`). WebFetch is left out. Attempt members also get Edit and Write under `acceptEdits`, which confines edits to the worktree. Members run concurrently, each in its own process group; after `panel.timeout_secs`, or on Ctrl-C, byoclaude kills the member with every process it started, including Claude Code's shell commands, which run in groups of their own. A failed or timed-out member is reported and the rest continue.
- **Attempts.** Each member works in a detached worktree under the panel's ledger directory, created from HEAD with the repository's uncommitted tracked diff applied. Afterwards byoclaude collects the worktree's changes as a patch against that starting state, runs `panel.test_command` (or `--test`) there with the timeout, keeps the exit code and the last 60 lines of output, and removes the worktree. Patches are made with explicit `git diff` format options, so user git config cannot change them. `panel apply` applies a patch with `git apply`, falling back to `git apply --3way` (which stages the result and may leave conflicts) when the tree has moved on; nothing is applied automatically.
- **Judge.** With at least two reports, a headless `claude -p` in the repository with read-only tools and a verdict schema compares the reports, labeled Panelist A, B, C without model names. Opinion verdict: summary, agreement, conflicts, unique findings, blind spots, recommendation, confidence. Attempt verdict adds a ranking, a winner (or none) and what to merge. Labels are mapped back to model IDs in the output and the ledger.
- **Ledger.** `~/.byoclaude/panels/<id>/` (id: UTC timestamp plus a short suffix): `panel.json` with the question, mode, members (status, time, tokens, cost estimated from roster prices, or plan usage for the relay and the ChatGPT plan), judge, verdict and whether a patch was applied, plus raw outputs and patches. The verdict goes to stdout as Markdown with a ledger table; progress goes to stderr.

## Catalog (`catalog.rs`)

The roster combines each usable provider's model list (`/v1/models` or `/models`, by protocol), the ChatGPT account catalog, and models.dev metadata (context window, max output, reasoning, tools, images, price). Providers without a model list fall back to models.dev; providers listing more than 25 models offer only those named in `providers.<id>.models`. The roster is cached in `cache/roster.json`; `models --refresh`, `login` and `r` in `byoclaude config` rebuild it.

## State (`store.rs`)

`~/.byoclaude` (or `$BYOCLAUDE_HOME`): `config.json`, `auth.json` (credentials keyed by provider, owner-only, written atomically under `auth.lock`), `bridge.key`, `host-id`, `cache/`, `logs/bridge.log` (metadata only), `plugin/`, `panels/`. A 0.1.0 `~/.byoclaude-rs` is copied over on first run.

## Other modules

| Module | Role |
|---|---|
| `bridge.rs` | HTTP server, authentication, fallback loop, per-route logging including usage read from forwarded streams |
| `relay.rs` | Header and body preparation for forwarded routes |
| `request.rs`, `response.rs`, `upstream.rs` | Responses API translation and WebSocket transport |
| `chat.rs` | Chat Completions translation |
| `accounts.rs` | `login`, `logout`, `auth` |
| `doctor.rs` | `doctor` |
| `tui.rs` | `byoclaude config` |
| `panel.rs` | `byoclaude panel`: selection, panelists, attempt worktrees, judge, ledger |
| `auth.rs` | Sign in with ChatGPT |

## Testing

- `cargo test`: translation, routing, relay header handling, launch plans, the config UI (ratatui test backend), and a launcher process test with a fake `claude`.
- `tests/e2e/run.sh`: the installed Claude Code through `byoclaude run` against `examples/mock_openai.rs` (a Responses WebSocket mock plus an Anthropic HTTP mock). It covers thinking, tool execution, reasoning carried across turns, WebSocket continuation, web search, the Claude relay with Claude Code's credential, and a subagent running on another provider through the plugin agent.
