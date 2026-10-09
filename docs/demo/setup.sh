# Sourced by docs/demo/demo.tape: an isolated demo home with a mock ChatGPT sign-in, a fake
# Claude sign-in, mock OpenAI and Anthropic servers, a sample project, and Claude Code
# onboarding already completed. Uses no real account and no plan usage.
REPO=$PWD
# Drop variables inherited from any Claude Code session the recorder runs under.
for name in $(env | grep -E '^(CLAUDE|ANTHROPIC)' | cut -d= -f1); do unset "$name"; done
DEMO=$(cd "$(mktemp -d)" && pwd -P)
export HOME=$DEMO CLAUDE_CONFIG_DIR=$DEMO/.claude BYOCLAUDE_PORT=47811 PATH=$REPO/target/release:$PATH
mkdir -m 700 "$HOME/.byoclaude" "$CLAUDE_CONFIG_DIR"
mkdir -p "$HOME/myapp/src"
printf '[package]\nname = "myapp"\nversion = "0.1.0"\nedition = "2024"\n' > "$HOME/myapp/Cargo.toml"
printf 'fn main() {\n    println!("hello");\n}\n' > "$HOME/myapp/src/main.rs"
printf '# myapp\n' > "$HOME/myapp/README.md"

mkdir -m 700 "$HOME/.byoclaude/cache"
cp "$REPO/docs/demo/models.json" "$HOME/.byoclaude/cache/"
python3 - "$HOME/.byoclaude/cache" <<'PY'
import json, sys
cache = sys.argv[1]
models = json.load(open(f"{cache}/models.json"))
roster = [{"id": "openai/" + m["slug"], "provider": "openai", "name": m["display_name"],
           "context_window": m["context_window"], "effort_levels": m["effort_levels"],
           "reasoning": True, "tools": True, "images": True, "listed": m["listed"]} for m in models]
roster += [{"id": i, "provider": "anthropic", "name": n, "context_window": c, "reasoning": True,
            "tools": True, "images": True, "cost_input": p[0], "cost_output": p[1], "listed": False}
           for i, n, c, p in [("claude-opus-5-5", "Claude Opus 5.5", 1000000, (5, 25)),
                              ("claude-sonnet-5-5", "Claude Sonnet 5.5", 1000000, (3, 15)),
                              ("claude-haiku-4-5", "Claude Haiku 4.5", 200000, (1, 5))]]
json.dump(roster, open(f"{cache}/roster.json", "w"))
PY
touch "$HOME/.byoclaude/.relay-note-shown"
(
  umask 077
  printf '{"openai":{"access_token":"demo","refresh_token":"demo","id_token":"demo","client_id":"demo","subject":"demo","email":null,"scopes":["chatgpt.tokens.use.direct"],"expires_at":%s,"earliest_refresh_at":0}}\n' \
    "$(( $(date +%s) + 86400 ))" > "$HOME/.byoclaude/auth.json"
  printf '{"upstream_base_url":"http://127.0.0.1:47810/v1","providers":{"anthropic":{"base_url":"http://127.0.0.1:47812"}}}\n' > "$HOME/.byoclaude/config.json"
)
version=$(claude --version | cut -d' ' -f1)
printf '{"hasCompletedOnboarding":true,"hasResetAutoModeOptInForDefaultOffer":true,"hasSeenAutoDefaultNudge":true,"officialMarketplaceAutoInstallAttempted":true,"lastOnboardingVersion":"%s","projects":{"%s":{"hasTrustDialogAccepted":true}}}\n' \
  "$version" "$HOME/myapp" > "$CLAUDE_CONFIG_DIR/.claude.json"
printf '{"theme":"dark","permissions":{"defaultMode":"default","allow":["Bash(ls:*)"]}}\n' > "$CLAUDE_CONFIG_DIR/settings.json"

MOCK_THINKING="I'll list the project files first." \
MOCK_COMMAND="ls -R" \
MOCK_ANSWER="This is a small Rust binary crate, **myapp**: \`src/main.rs\` prints a greeting, and \`Cargo.toml\` targets edition 2024." \
MOCK_LOG=$DEMO/mock.log MOCK_ANTHROPIC_LOG=$DEMO/anthropic.log "$REPO/target/release/examples/mock_openai" 47810 47812 > /dev/null 2>&1 &
# Claude Code signed in (fake token): Claude models relay to the mock Anthropic server.
export CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-demo
cd "$HOME/myapp"
PS1='$ '
clear
