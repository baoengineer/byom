# ChatGPT plan provider contract

The `openai` provider uses OpenAI's [ChatGPT plan usage in open-source apps](https://developers.openai.com/siwc/token-sharing-open-source) flow ("Sign in with ChatGPT"). It does not use the Codex backend or credentials from other tools. Other providers use API keys or no credential; see [ARCHITECTURE.md](ARCHITECTURE.md) for how each protocol is handled.

## Sign-in

- Dynamic client registration: first sign-in uses `client_id=dynamic_agent_client` with `agent_name_hint=byoclaude`; later sign-ins reuse the issued client ID and a stable host ID.
- Authorization code flow with PKCE (S256), state and nonce, on a `127.0.0.1` callback.
- Scopes `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`, resource `https://api.openai.com/v1`. Sign-in fails without `chatgpt.tokens.use.direct`.
- The ID token is verified against OpenAI's JWKS: issuer, audience, expiry and nonce.
- Tokens are stored in `auth.json` under `openai`, with owner-only permissions and atomic writes. Refresh rotates the refresh token and is serialized across processes with a lock file.

## Models

`GET https://api.openai.com/v1/models` with the access token returns the account's catalog. byoclaude reads `slug`, `display_name`, `description`, `context_window`, `supported_reasoning_levels`, `default_reasoning_level` and `visibility`, and caches them in `models.json`.

## Inference

Requests go to the Responses API with the OAuth access token, `store: false` and `stream: true`.

- **WebSocket mode** (default): `wss://api.openai.com/v1/responses`, one `response.create` message per turn. A later turn on the same connection may set `previous_response_id` and send only new input items. That state lives only on the connection.
- **HTTP**: `POST https://api.openai.com/v1/responses` with the full input every turn. `previous_response_id` is not supported over HTTP.

Route requirements:

- Omit `max_output_tokens`, `temperature`, `top_p`, `metadata`, `truncation`, `user` and `prompt_cache_retention`.
- Use `instructions` or developer messages; `system` role items are rejected.
- Supply function tools in an `additional_tools` input item. The hosted `web_search` tool is accepted in `tools`.
- File search, code interpreter, image generation, hosted MCP and `tool_search` are rejected.
- `include: ["reasoning.encrypted_content"]` returns reasoning that can be replayed in later input with `store: false`.

Observed behavior:

- HTTP streams carry no `Content-Type` header.
- `response.completed` has an empty `output` list when `store` is false; items arrive through `response.output_item.done`.
- `prompt_cache_key` is accepted and reported cache hits appear in `usage.input_tokens_details.cached_tokens`.

## Limits and errors

Each connected app has a usage cap that the user can change in [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage). Exceeding it returns `subscription_sharing_usage_limit_exceeded`, sometimes as an `error` event inside an HTTP 200 stream. On Plus, a five-hour limit is shared across every app using the plan.

byoclaude maps errors to Anthropic error types so Claude Code reacts correctly:

| OpenAI | Claude Code sees |
|---|---|
| usage limit, `rate_limit_exceeded` | 429 `rate_limit_error`, `x-should-retry: false` for plan limits |
| `context_length_exceeded` | 400 `prompt is too long: …` (triggers compaction) |
| 401 | `authentication_error` with a sign-in hint |
| overloaded, 503 | 529 `overloaded_error` |
| other 4xx | `invalid_request_error` |

The route is an OpenAI preview; the contract above can change.
