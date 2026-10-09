# Records docs/demo/demo.gif against local mock OpenAI and Anthropic servers (no account, no
# plan usage). From the repository root:
#   cargo build --release --bin byom --example mock_openai && vhs docs/demo/demo.tape
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

# 1. Every model you can sign in to, in one place.
Type "byom config"
Sleep 300ms
Enter
Sleep 2.2s
Down
Sleep 400ms
Enter
Sleep 1.2s
Tab
Sleep 1.8s
Type "s"
Sleep 1s
Type "q"
Sleep 400ms

# 2. Claude Code, running on GPT.
Type "byom"
Sleep 300ms
Enter
Sleep 6s
Type "what's in this project?"
Sleep 400ms
Enter
Sleep 5s

# 3. Claude and GPT side by side in /model.
Type "/model"
Sleep 600ms
Enter
Sleep 4.5s
Escape
Sleep 800ms

Hide
Ctrl+C
Sleep 300ms
Ctrl+C
Sleep 1s
Type "byom stop; kill %1"
Enter
Sleep 1s
