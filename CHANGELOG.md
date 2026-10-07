# Changelog

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
