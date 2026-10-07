# Sourced by docs/demo/demo.tape: an isolated demo home with a mock ChatGPT sign-in, the
# mock OpenAI server, a sample project, and Claude Code onboarding already completed.
# Uses no real account and no plan usage.
REPO=$PWD
# Drop variables inherited from any Claude Code session the recorder runs under.
for name in $(env | grep -E '^(CLAUDE|ANTHROPIC)' | cut -d= -f1); do unset "$name"; done
DEMO=$(cd "$(mktemp -d)" && pwd -P)
export HOME=$DEMO CLAUDE_CONFIG_DIR=$DEMO/.claude BYOCLAUDE_PORT=47811 PATH=$REPO/target/release:$PATH
mkdir -m 700 "$HOME/.byoclaude-rs" "$CLAUDE_CONFIG_DIR"
mkdir -p "$HOME/myapp/src"
printf '[package]\nname = "myapp"\nversion = "0.1.0"\nedition = "2024"\n' > "$HOME/myapp/Cargo.toml"
printf 'fn main() {\n    println!("hello");\n}\n' > "$HOME/myapp/src/main.rs"
printf '# myapp\n' > "$HOME/myapp/README.md"

cp "$REPO/docs/demo/models.json" "$HOME/.byoclaude-rs/"
(
  umask 077
  printf '{"access_token":"demo","refresh_token":"demo","id_token":"demo","client_id":"demo","subject":"demo","email":null,"scopes":["chatgpt.tokens.use.direct"],"expires_at":%s,"earliest_refresh_at":0}\n' \
    "$(( $(date +%s) + 86400 ))" > "$HOME/.byoclaude-rs/chatgpt.json"
  printf '{"upstream_base_url":"http://127.0.0.1:47810/v1"}\n' > "$HOME/.byoclaude-rs/config.json"
)
version=$(claude --version | cut -d' ' -f1)
printf '{"hasCompletedOnboarding":true,"hasResetAutoModeOptInForDefaultOffer":true,"hasSeenAutoDefaultNudge":true,"officialMarketplaceAutoInstallAttempted":true,"lastOnboardingVersion":"%s","projects":{"%s":{"hasTrustDialogAccepted":true}}}\n' \
  "$version" "$HOME/myapp" > "$CLAUDE_CONFIG_DIR/.claude.json"
printf '{"theme":"dark","permissions":{"defaultMode":"default","allow":["Bash(ls:*)"]}}\n' > "$CLAUDE_CONFIG_DIR/settings.json"

MOCK_THINKING="I'll list the project files first." \
MOCK_COMMAND="ls -R" \
MOCK_ANSWER="This is a small Rust binary crate, **myapp**: \`src/main.rs\` prints a greeting, and \`Cargo.toml\` targets edition 2024." \
MOCK_LOG=$DEMO/mock.log "$REPO/target/release/examples/mock_openai" 47810 > /dev/null 2>&1 &
cd "$HOME/myapp"
PS1='$ '
clear
