# byoclaude

**Bring your own Claude.** Keep [Claude Code](https://claude.com/claude-code) as your coding harness, and run OpenAI's GPT models in it on the ChatGPT Plus or Pro plan you already pay for. No API key, no per-token billing.

![byoclaude config choosing a model, then byoclaude run](docs/demo/demo.gif)

Claude Code stays the harness: its tools, subagents, hooks, MCP servers, skills, plan mode and session resume all work as usual. byoclaude runs a small local bridge that translates Claude Code's requests to the OpenAI Responses API and streams the answers back.

```text
claude ──Anthropic Messages──▶ byoclaude bridge (127.0.0.1) ──WebSocket──▶ api.openai.com/v1/responses
```

byoclaude is an independent project, not affiliated with or endorsed by Anthropic or OpenAI. It signs in through OpenAI's documented [ChatGPT plan usage in open-source apps](https://developers.openai.com/siwc/token-sharing-open-source) flow, which OpenAI currently labels a preview.

## Requirements

- macOS or Linux
- Claude Code 2.1.289 or newer (tested through 2.1.290)
- A ChatGPT Plus or Pro plan

## Install

Download a binary for macOS or Linux from [Releases](https://github.com/baoengineer/byoclaude/releases) and put `byoclaude` on your `PATH`, or build from source with Rust 1.89 or newer:

```sh
cargo install --git https://github.com/baoengineer/byoclaude
```

## Quick start

```sh
byoclaude login     # authorize byoclaude in your browser
byoclaude models    # list the models your plan offers
byoclaude run       # start Claude Code on GPT
```

`byoclaude run [model] -- [claude args]` starts the bridge if needed and passes everything after `--` to `claude`, for example `byoclaude run gpt-5.6-sol -- --continue`. Inside Claude Code, `/model` lists your plan's GPT models.

## Features

- **Native model handling.** Every model in your plan appears in `/model`. Claude Code treats each one like a Claude model, so effort levels and thinking work, and compaction uses the model's real context window.
- **Reasoning.** GPT reasoning summaries show as thinking. Encrypted reasoning is carried into the next turn, so multi-step work keeps its chain of thought.
- **Tools.** All Claude Code tools, including MCP tools. The WebSearch tool uses OpenAI's hosted search.
- **Images and PDFs** in prompts and tool results.
- **Low latency.** One WebSocket per Claude Code session. Follow-up turns send only the new messages, and a warm connection is kept ready for new sessions.
- **Clear errors.** Plan limits show as rate-limit errors with a link to your usage settings, and Claude Code does not retry them. A context overflow triggers Claude Code's compaction.

## Configuration

`byoclaude config` opens a terminal UI for choosing the default model, the background model (for titles and summaries), the context window, the transport and the Claude profile. Each setting shows what "auto" resolves to.

Settings are stored in `~/.byoclaude-rs/config.json`, or `$BYOCLAUDE_HOME/config.json` if that is set:

```json
{
  "model": "gpt-5.6-sol",
  "small_model": "gpt-5.6-luna",
  "context_tokens": 0,
  "transport": "auto",
  "behaves_as": "claude-opus-5-5"
}
```

Empty and zero values come from your plan's model catalog. `transport` is `auto` (WebSocket with HTTP fallback) or `http`. `BYOCLAUDE_PORT` changes the bridge port (default `47391`).

Other commands: `byoclaude status`, `byoclaude stop`, `byoclaude auth-status`.

## Usage limits

Your plan's usage applies. ChatGPT also caps each connected app separately: if requests fail with "ChatGPT plan limit reached", open [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage), find byoclaude and raise its limit, or wait for the reset. On Plus, a five-hour limit is shared with every app that uses your plan.

## Troubleshooting

- **"Not signed in".** Run `byoclaude login`.
- **Requests fail right after an upgrade.** `byoclaude run` replaces a bridge from an older version automatically. If that fails, run `byoclaude stop` and retry.
- **Authentication errors.** Run `byoclaude login` again.
- **Slow turns.** `~/.byoclaude-rs/bridge.log` has one line per request, with time to first token, whether the turn continued over the WebSocket, and why it did not. It contains no prompt or response content.
- **A feature errors.** The ChatGPT plan route rejects some OpenAI features (file search, code interpreter, tool search). byoclaude disables Claude Code's tool search for this reason.

## Development

```sh
cargo test
tests/e2e/run.sh          # real Claude Code against a local mock of OpenAI; uses no plan quota
vhs docs/demo/demo.tape   # re-record the demo GIF
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PROVIDER.md](docs/PROVIDER.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
