# Security

## What byoclaude stores

All state lives in `~/.byoclaude` (or `$BYOCLAUDE_HOME`), a directory only you can read:

- `auth.json`: credentials keyed by provider. The ChatGPT sign-in (access, rotating refresh and ID tokens, issued client ID) and API keys you paste. Written atomically with mode `0600`.
- `bridge.key`: a random 256-bit key Claude Code must present to the local bridge. Mode `0600`.
- `config.json`: settings. API keys can be referenced as `$ENV_VAR` or `!command` instead of stored.
- `cache/`, `plugin/`: model catalogs and the generated session plugin.
- `logs/bridge.log`: per-request metadata (model, route, timing, token counts) and, for failures, the first 160 characters of the provider's error message. No prompts, responses or credentials are logged; a provider's error message could quote part of a request.

## Network exposure

The bridge listens only on `127.0.0.1` and rejects requests without the bridge key. Each credential is sent only to its own provider's API.

The Claude relay forwards Claude Code's own Anthropic credential, as Claude Code sent it, to `api.anthropic.com` (or a configured Anthropic base URL) and nowhere else. byoclaude never stores it, never reads Claude Code's credential files or keychain entries, and never reads credentials belonging to other tools. With the relay active, Claude Code keeps its own credential variables; without it, the launcher removes other providers' credentials from the environment it passes to Claude Code.

To revoke access: remove a key with `byoclaude logout <provider>`, and for ChatGPT also disconnect byoclaude in ChatGPT settings.

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub security advisories](https://github.com/baoengineer/byoclaude/security/advisories/new) rather than in a public issue.
