# Security

## What byoclaude stores

All state lives in `~/.byoclaude-rs` (or `$BYOCLAUDE_HOME`), a directory readable only by you:

- `chatgpt.json`: your ChatGPT OAuth tokens (access, rotating refresh, ID token) and the issued client ID. Written atomically with mode `0600`.
- `bridge.key`: a random 256-bit key that Claude Code must present to the local bridge. Mode `0600`.
- `config.json`, `models.json`: settings and the cached model list.
- `bridge.log`: per-request metadata (model, timing, token counts). No prompt or response content.

## Network exposure

The bridge listens only on `127.0.0.1` and rejects requests without the bridge key. Tokens are sent only to `auth.openai.com` and `api.openai.com`. byoclaude never reads credentials belonging to Claude Code, Codex or other tools, and removes other providers' credentials from the environment it passes to Claude Code.

To revoke access, disconnect byoclaude in ChatGPT settings and delete `chatgpt.json`.

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub security advisories](https://github.com/baoengineer/byoclaude/security/advisories/new) rather than in a public issue.
