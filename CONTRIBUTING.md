# Contributing

Issues and pull requests are welcome.

## Checks

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
tests/e2e/run.sh
```

`tests/e2e/run.sh` runs your installed Claude Code through `byoclaude run` against `examples/mock_openai.rs`, which stands in for the OpenAI Responses WebSocket API and the Anthropic API. It uses an isolated Claude config directory and a fake Claude sign-in, and spends no plan usage. Run it for any change to routing, translation, the relay, the transport, the session plugin or the launcher.

Local servers make good real-provider checks: Ollama serves both the Anthropic and Chat Completions protocols (`ollama/<model>`, or a custom `openai-chat` provider at `http://127.0.0.1:11434/v1`).

## Guidelines

- Keep intake tolerant. A new or unknown field from Claude Code must never fail a request; drop or degrade it and add a test.
- Never log prompt or response content, and never read credentials belonging to other tools.
- Relayed Claude requests must stay byte for byte as Claude Code sent them.
- New built-in providers need a documented API and terms that allow use from third-party coding tools. No client impersonation.
- Behavior of the ChatGPT plan route goes in [docs/PROVIDER.md](docs/PROVIDER.md), with only what was observed or documented by OpenAI.
- When Claude Code changes its request shape, capture a request against a local endpoint with synthetic content and add it under `tests/fixtures/`.
