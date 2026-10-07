# Records docs/demo/demo.gif against the local mock OpenAI server (no account, no plan usage).
# From the repository root:
#   cargo build --release --bin byoclaude --example mock_openai && vhs docs/demo/demo.tape
Output docs/demo/demo.gif

Set Shell bash
Set FontSize 15
Set Width 1240
Set Height 720
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Hide
Type "source docs/demo/setup.sh"
Enter
Sleep 1s
Show

# 1. Pick models from your ChatGPT plan.
Type "byoclaude config"
Sleep 300ms
Enter
Sleep 1.5s
Enter
Sleep 1.2s
Down
Sleep 300ms
Down
Sleep 500ms
Enter
Sleep 800ms
Down
Sleep 300ms
Enter
Sleep 1s
Down
Sleep 200ms
Down
Sleep 200ms
Down
Sleep 200ms
Down
Sleep 500ms
Enter
Sleep 800ms
Type "s"
Sleep 1.5s
Type "q"
Sleep 500ms

# 2. Claude Code, running on GPT.
Type "byoclaude run"
Sleep 300ms
Enter
Sleep 6s
Type "what's in this project?"
Sleep 400ms
Enter
Sleep 5s

# 3. /model lists the plan's GPT models.
Type "/model"
Sleep 600ms
Enter
Sleep 4s
Escape
Sleep 800ms

Hide
Ctrl+C
Sleep 300ms
Ctrl+C
Sleep 1s
Type "byoclaude stop; kill %1"
Enter
Sleep 1s
