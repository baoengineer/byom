#!/usr/bin/env bash
# Drive the installed Claude Code through `byom run` against a scripted mock of the
# OpenAI Responses WebSocket API (examples/mock_openai.rs). Spends no plan usage.
# Requires: cargo, python3, and Claude Code (`claude`) on PATH.
# Usage: tests/e2e/run.sh
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
(cd "$root" && cargo build --quiet --release --bin byom --example mock_openai)
bin=$root/target/release/byom
mock=$root/target/release/examples/mock_openai
work=$(mktemp -d)
home=$work/home
mkdir -m 700 "$home"
mock_port=$((47500 + RANDOM % 400))
bridge_port=$((mock_port + 1))
anthropic_port=$((mock_port + 2))
cleanup() {
  pkill -f "bridge --port $bridge_port" 2>/dev/null || true
  [ -n "${mock_pid:-}" ] && kill "$mock_pid" 2>/dev/null || true
}
trap cleanup EXIT

far=$(( $(date +%s) + 86400 ))
umask 077
cat > "$home/auth.json" <<JSON
{"openai":{"access_token":"mock-access","refresh_token":"mock-refresh","id_token":"x","client_id":"mock-client",
 "subject":"mock","email":null,"scopes":["chatgpt.tokens.use.direct"],"expires_at":$far,"earliest_refresh_at":0}}
JSON
echo "{\"relay\":true,\"upstream_base_url\":\"http://127.0.0.1:$mock_port/v1\",\"providers\":{\"anthropic\":{\"base_url\":\"http://127.0.0.1:$anthropic_port\"}}}" > "$home/config.json"
mkdir -m 700 "$home/cache"
echo '[]' > "$home/cache/roster.json"
cat > "$home/cache/models.json" <<JSON
[{"slug":"mock-main","display_name":"Mock Main","description":"","context_window":272000,"effort_levels":["low","medium","high"],"default_effort":"low","listed":true},
 {"slug":"mock-luna","display_name":"Mock Luna","description":"","context_window":272000,"effort_levels":["low"],"default_effort":"low","listed":true}]
JSON

MOCK_DROP_ONCE=1 MOCK_SUBAGENT=byom:openai-mock-main MOCK_LOG=$work/mock.log MOCK_ANTHROPIC_LOG=$work/anthropic.log "$mock" "$mock_port" "$anthropic_port" > "$work/mock.out" 2>&1 &
mock_pid=$!
sleep 0.5

mkdir "$work/project" "$work/claude-config"
cd "$work/project"
# Isolate from the user's Claude Code config: no hooks, MCP servers, or session history.
export CLAUDE_CONFIG_DIR=$work/claude-config
BYOM_HOME=$home BYOM_PORT=$bridge_port "$bin" run mock-main -- \
  -p "Run the marker check." --allowedTools Bash --output-format stream-json --verbose \
  < /dev/null > "$work/claude.jsonl" 2> "$work/claude.err" || true

BYOM_HOME=$home BYOM_PORT=$bridge_port "$bin" run mock-main -- \
  -p "SEARCHTEST: look this up." --allowedTools WebSearch --output-format stream-json --verbose \
  < /dev/null > "$work/search.jsonl" 2>> "$work/claude.err" || true

# Relay: Claude Code signed in (a fake token), Claude main model relayed to the mock
# Anthropic endpoint, and the session plugin's agent for an OpenAI model as the subagent.
CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-mock BYOM_HOME=$home BYOM_PORT=$bridge_port "$bin" run -- \
  -p "SUBAGENTTEST: ask another model." \
  --output-format stream-json --verbose < /dev/null > "$work/relay.jsonl" 2>> "$work/claude.err" || true

python3 - "$work" <<'PY'
import json, os, sys
work = sys.argv[1]
events = [json.loads(l) for l in open(f"{work}/claude.jsonl") if l.startswith("{")]
blocks = [b for e in events if e.get("type") == "assistant" for b in e["message"]["content"]]
results = [e for e in events if e.get("type") == "user"]
final = next((e for e in events if e.get("type") == "result"), {})
mock = [json.loads(l) for l in open(f"{work}/mock.log")]
bridge = [json.loads(l) for l in open(f"{work}/home/logs/bridge.log")]
checks = {
    "dropped stream reconnected": any(b.get("retried") and b.get("outcome") == "ok" for b in bridge),
    "thinking shown": any(b["type"] == "thinking" and "Planning" in b.get("thinking", "") for b in blocks),
    "tool ran": any("mock-ok" in json.dumps(e) for e in results),
    "final answer": final.get("result") == "Mock done." and not final.get("is_error"),
    "behavesAs (no unknown-model warning)": "unrecognized_model" not in open(f"{work}/claude.err").read(),
    # Reasoning reaches the model either held server-side (continued) or replayed in full.
    "reasoning carried": all(m["previous"] or "reasoning" in m["types"]
                             for m in mock[1:] if m["session"] == mock[0]["session"]),
    "continued over websocket": any(b.get("continued") for b in bridge),
    "web search reached hosted search": any("web_search" in m.get("hosted", []) for m in mock),
    "web search result returned to model": "mock-source" in open(f"{work}/search.jsonl").read(),
}
anthropic = [json.loads(l) for l in open(f"{work}/anthropic.log")] if os.path.exists(f"{work}/anthropic.log") else []
relayed = [a for a in anthropic if a["path"] == "/v1/messages" and str(a["model"]).startswith("claude")]
relay_events = [json.loads(l) for l in open(f"{work}/relay.jsonl") if l.startswith("{")]
relay_final = next((e for e in relay_events if e.get("type") == "result"), {})
checks.update({
    "relay: Claude model reached Anthropic": bool(relayed),
    "relay: Claude Code's own credential forwarded": all(a["auth_prefix"] == "sk-ant-oat" for a in relayed) and bool(relayed),
    "relay: bridge key not forwarded": not any(a["bridge_key_forwarded"] for a in anthropic),
    "relay: subagent ran on OpenAI model": any(m["model"] == "mock-main" and m["session"] != mock[0]["session"] for m in mock),
    "relay: final answer from Claude": relay_final.get("result") == "Claude mock answer." and not relay_final.get("is_error"),
    "relay: logged as relay": any(b.get("transport") == "relay" for b in bridge),
    "plugin: per-model agent offered to Claude": any(a.get("lists_plugin_agent") for a in relayed),
    "plugin: byom skill offered to Claude": any(a.get("lists_skill") for a in relayed),
})
for name, ok in checks.items():
    print(("PASS " if ok else "FAIL ") + name)
print("bridge:", [{k: b.get(k) for k in ("model", "continued", "sent_items", "miss", "outcome")} for b in bridge])
print("mock:", [{k: m[k] for k in ("conn", "model", "previous", "items", "types")} for m in mock])
sys.exit(0 if all(checks.values()) else 1)
PY
