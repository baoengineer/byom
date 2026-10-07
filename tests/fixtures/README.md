# Protocol fixtures

`claude-2.1.289-request.json` preserves a request structure captured from Claude Code v2.1.289 using `--bare`, a temporary HOME/config directory, a synthetic prompt, Read-only tools, disabled MCP, and a loopback capture endpoint with dummy credentials. Generated metadata and prompt text were replaced with generic values. No real user session or provider credentials were captured.

The RSA PEM and JWKS are generated test-only keys for OIDC verification. They are intentionally public fixtures, not OpenAI or user credentials.
