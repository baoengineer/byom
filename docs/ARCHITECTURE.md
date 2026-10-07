# Architecture

```text
byoclaude run ──exec──▶ claude ──Anthropic Messages (HTTP/SSE)──▶ bridge ──Responses (WebSocket)──▶ OpenAI
      │                                                               │
      └── starts the bridge, passes --settings and environment        └── one process, 127.0.0.1 only
```

Claude Code is unmodified. The launcher points it at the local bridge with `ANTHROPIC_BASE_URL` and a per-install bearer key, and the bridge translates each request to the OpenAI Responses API.

## Launcher (`launch.rs`)

- Reads `config.json` and the cached model catalog, then builds a launch plan: main model, background model and context window.
- Passes `--settings` with a `modelPicker` that lists the plan's models, each with `behavesAs` set to a Claude model this Claude Code release knows. Claude Code then applies that model's client-side handling (effort levels, adaptive thinking, prompt profile) and accepts the GPT model IDs without an "unrecognized model" warning.
- Sets `ANTHROPIC_MODEL`, the default Opus/Sonnet/Haiku models, `CLAUDE_CODE_MAX_CONTEXT_TOKENS` and `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, disables tool search (unsupported upstream), and removes other providers' credentials from the environment.
- Starts the bridge if needed. `/health` reports the bridge version; a bridge from another version is replaced through `/shutdown`.

## Bridge (`bridge.rs`)

Routes: `POST /v1/messages` (streaming and non-streaming), `POST /v1/messages/count_tokens` (local estimate), `GET /v1/models`, `GET /health`, `POST /shutdown`. Every route requires the bridge key.

Response headers are held until the first content event arrives. A failure before any output therefore returns a real HTTP status, so Claude Code shows the reason instead of falling back to a non-streaming retry. The bridge appends one line per request to `bridge.log`: model, transport, continuation, items sent, time to first token and usage. It never logs content.

## Request translation (`request.rs`)

- Intake is tolerant: unknown fields and block types are dropped, never fatal, so newer Claude Code releases keep working.
- `system` blocks become `instructions`; mid-conversation `system` messages become developer messages.
- Text, images (`input_image`), PDFs (`input_file`), tool calls (`function_call`) and tool results (`function_call_output`) map directly.
- Tools go into an `additional_tools` item; `web_search_*` server tools become the hosted `web_search` tool.
- `output_config.effort` becomes `reasoning.effort`, clamped to the levels the model supports. Adaptive or enabled thinking turns on reasoning summaries and encrypted reasoning.
- Consecutive same-role messages are merged and cache annotations are stripped, so a conversation compares equal across turns.

## Stream translation (`response.rs`)

- Output items are emitted strictly in `output_index` order, buffering later items, so Anthropic content blocks never interleave.
- A reasoning item becomes a `thinking` block (summary text, then a signature carrying `byoc1.<model>.<encrypted_content>`), or a `redacted_thinking` block when there is no summary. On the next turn the signature is decoded and the reasoning replayed, but only to the same model.
- `web_search_call` becomes `server_tool_use` plus `web_search_tool_result`.
- `response.completed` provides the stop reason and usage, with cached tokens reported as `cache_read_input_tokens`.

## Transport (`upstream.rs`)

- A pool of WebSocket connections keyed by Claude Code's session ID (`X-Claude-Code-Session-Id`). Each connection remembers the conversation it last served: request settings, tools and normalized messages.
- When the next request extends that conversation, only the new messages (and any newly added tools) are sent, with `previous_response_id`. Otherwise the full input is sent. Background requests and subagents in the same session use separate connections so they do not evict the main conversation.
- One spare connection is kept warm for new sessions. Idle connections older than four minutes are not reused.
- A failure before the first event, including a rejected continuation, is retried once on a fresh connection with the full input. Nothing is retried after output starts. If WebSocket connection fails, the request goes over HTTP.

## Other modules

- `auth.rs`: sign-in and token refresh ([PROVIDER.md](PROVIDER.md)).
- `catalog.rs`: account model catalog and its cache.
- `config.rs`: `config.json` and the state directory (`$BYOCLAUDE_HOME`, default `~/.byoclaude-rs`).
- `settings.rs`: the `byoclaude config` terminal UI.
- `sse.rs`: incremental SSE decoder for the HTTP transport.

## Testing

- `cargo test`: unit tests for translation, transport planning, the config UI (rendered with ratatui's test backend), and a launcher process test that runs a fake `claude` against a real bridge.
- `tests/e2e/run.sh`: the installed Claude Code driven through `byoclaude run` against `examples/mock_openai.rs`, a scripted Responses WebSocket server. It uses an isolated Claude config directory and no plan usage, and checks thinking display, tool execution, reasoning carried across turns, WebSocket continuation and the WebSearch tool.
