# Changelog

## 0.3.0

Panels.

- `byoclaude panel "<question>"` puts one question to several models at once, each a headless Claude Code agent that can read the repository and run tests; a judge from another provider compares the anonymized reports and prints a verdict: agreement, conflicts, unique findings, blind spots, recommendation, confidence.
- `--attempt`: each panelist makes the change in its own git worktree; byoclaude collects the patches and runs the test command when one is set, and the verdict ranks the attempts and names a winner. `byoclaude panel apply <id> [panelist]` applies one with `git apply`, falling back to a three-way merge when the tree has moved on.
- Automatic selection of ready models from distinct providers and of a judge from a provider not on the panel, or `--models` and `--judge`.
- Ledger in `~/.byoclaude/panels/<id>/` with each member's status, time, tokens and estimated cost (plan usage marked); `panel list`, `panel show <id>`, `--json`.
- Settings: `panel.models`, `panel.judge`, `panel.size`, `panel.test_command`, `panel.timeout_secs`.
- The guide gains a panels section, and the session plugin a `/byoclaude:panel` command.
- A Claude Code mod in each session (Claude Code 2.1.287 or later) makes panels native: an `mcp__byoclaude__panel` tool that returns at once and brings the verdict back as a message, a progress line above the prompt, a panel pane with Apply buttons, `/panel` that works while Claude is busy, and an opt-in gate before risky shell commands (`panel.gate`).

## 0.2.0

Multi-provider sessions.

- Routes by model ID: `claude-*` to Anthropic, `provider/model` to that provider.
- Claude relay: with Claude Code signed in, Claude models keep working unmodified on Claude Code's own sign-in, and every other model is added alongside (`relay` setting).
- Providers: Z.ai, Kimi For Coding, Moonshot, MiniMax, DeepSeek, OpenRouter, Ollama, LM Studio (Anthropic protocol); Groq, Mistral, Gemini, Cerebras, Together, xAI (Chat Completions); custom Anthropic- or OpenAI-compatible providers.
- Session plugin: a byoclaude skill and one subagent per model, so Claude can delegate to other models; `byoclaude --skill`.
- Roles: main, background and subagent models, and `opus`/`sonnet`/`haiku` alias remapping.
- Fallback chains when a model fails before answering.
- Commands: `login [provider]`, `logout`, `auth`, `models --json`, `doctor`, `logs`, `config get/set/unset/path`; bare `byoclaude` runs.
- `byoclaude config` is a tabbed home: Models, Providers, Roles, Usage.
- State moves to `~/.byoclaude` with a provider-keyed `auth.json`; 0.1.0 state migrates automatically.

## 0.1.0

First public release.

- Sign in with ChatGPT (OAuth with PKCE, dynamic client registration, rotating refresh tokens); no API keys.
- Local bridge serving the Anthropic Messages API from the OpenAI Responses API, tolerant of unknown request fields.
- Claude Code integration: model picker rows with `behavesAs`, real context windows, separate background model.
- Reasoning shown as thinking and carried across turns as encrypted reasoning.
- WebSocket transport with per-session continuation, a warm spare connection, and HTTP fallback.
- Web search, images, PDFs, non-streaming requests, token counting and `/v1/models`.
- Errors mapped to Anthropic error types; plan limits are not retried.
- `byoclaude config` terminal UI; `run`, `login`, `models`, `status`, `stop`, `auth-status` commands.
- End-to-end harness driving the real Claude Code against a local mock of OpenAI.
