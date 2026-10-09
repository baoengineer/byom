# Configuration

Settings live in `~/.byom/config.json` (or `$BYOM_HOME/config.json`). Edit them in `byom config`, with `byom config set/get/unset <key> [value]`, or by hand:

```json
{
  "model": "openai/gpt-5.6-sol",
  "background": "groq/llama-3.3-70b-versatile",
  "subagent": "",
  "aliases": { "opus": "openai/gpt-6-astra" },
  "relay": false,
  "fallbacks": { "openai/gpt-6-astra": ["openai/gpt-5.6-sol", "kimi/k3"] },
  "providers": { "openrouter": { "models": ["qwen/qwen3-coder"] } }
}
```

| Setting | Meaning |
|---|---|
| `model` | Main model. Empty keeps Claude Code's default when Claude models are available, or picks the first available model |
| `background` | Model for titles and summaries; overrides `aliases.haiku` |
| `subagent` | Model for subagents that don't name one |
| `aliases` | What agents asking for `opus`, `sonnet` or `haiku` get |
| `relay` | Relay Claude models on Claude Code's own sign-in (default `false`; see [Claude models](../README.md#claude-models)) |
| `fallbacks` | Models to try, in order, when a model fails before answering (plan cap, outage, missing key) |
| `providers.<id>` | `name`, `api_key`, `base_url`, `protocol` (`anthropic` or `openai-chat`), `models`, `disabled` |
| `context_tokens` | Compaction window for the whole session. `0` uses the main model's catalog value when no Claude model is available, and Claude Code's own per-model windows when one is |
| `transport` | ChatGPT plan transport: `auto` (WebSocket) or `http` |
| `teams` | Turn on Claude Code's agent teams, so teammates can run on any model (default `false`) |
| `behaves_as` | Claude model whose handling Claude Code applies to other models (default `claude-opus-5-5`) |

## Credentials

`byom login <provider>` stores credentials in `~/.byom/auth.json`, readable only by you. In config, a provider's `api_key` can instead name an environment variable (`$MY_KEY`) or a command whose output is the key (`!pass show my-key`). A command runs through `sh -c` when byom first needs that key, when it refreshes the model catalog, and in `byom doctor`.

## Custom providers

Any Anthropic- or OpenAI-compatible endpoint works:

```sh
byom config set providers.mybox.protocol openai-chat     # or anthropic
byom config set providers.mybox.base_url http://10.0.0.5:8000/v1
byom config set providers.mybox.api_key '$MYBOX_KEY'
byom config set providers.mybox.models "qwen3-coder, glm-5"
```

Models then appear as `mybox/<model>`.
