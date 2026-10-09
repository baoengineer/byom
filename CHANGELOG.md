# Changelog

## 0.4.0

Renamed to byom, and the Claude relay is opt-in.

- byoclaude is now byom ("bring your own model"): the binary, crate, `~/.byom` (copied from `~/.byoclaude` on first run), `BYOM_HOME`, `BYOM_PORT`, the `x-byom-key` header and `byom:` agent names.
- The Claude relay is off by default. Turn it on with `byom config set relay true`; the README explains Anthropic's terms first. A saved Anthropic API key now makes Claude models available without the relay, and a signed-in Claude Code's own credential takes precedence over it.
- Security: API keys are re-read on every request, so `logout` and new keys apply to a running bridge; requests carrying keys do not follow redirects; providers other than the Claude relay receive only an allowlist of headers; pasted keys are stored as typed and never run.
- Chat Completions providers: a stream cut off before finishing is an error instead of a finished turn, and parallel tool calls no longer merge.
- Non-object JSON bodies get a 400 instead of failing the request handler.
- Teams: `byom config set teams true` turns on Claude Code's agent teams, and each teammate can run on a different provider through its byom agent. byom sessions keep teammates in-process so they reach the bridge.
- 0.3.0 was tagged briefly and withdrawn; its panels feature was removed before release.

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
