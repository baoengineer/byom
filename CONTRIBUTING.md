# Contributing

Issues and pull requests are welcome.

## Checks

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
tests/e2e/run.sh
```

`tests/e2e/run.sh` runs your installed Claude Code through `byoclaude run` against `examples/mock_openai.rs`, a scripted stand-in for the OpenAI Responses WebSocket API. It uses an isolated Claude config directory and spends no plan usage. Run it for any change to request or stream translation, the transport, or the launcher.

## Guidelines

- Keep intake tolerant. A new or unknown field from Claude Code must never fail a request; drop or degrade it and add a test.
- Never log prompt or response content, and never read credentials belonging to other tools.
- Behavior of the ChatGPT plan route goes in [docs/PROVIDER.md](docs/PROVIDER.md), with only what was observed or documented by OpenAI.
- When Claude Code changes its request shape, capture a request against a local endpoint with synthetic content and add it under `tests/fixtures/`.
