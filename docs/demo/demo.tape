# Records docs/demo/demo.gif. Run from the repository root after `cargo build --release`:
#   vhs docs/demo/demo.tape
Output docs/demo/demo.gif

Set Shell bash
Set FontSize 15
Set Width 1080
Set Height 600
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms

Hide
Type "export REPO=$PWD PATH=$PWD/target/release:$PATH HOME=$(mktemp -d) PS1='$ ' && cd $HOME"
Enter
Type "mkdir -m 700 .byoclaude-rs && cp $REPO/docs/demo/models.json .byoclaude-rs/ && clear"
Enter
Show

Type "byoclaude config"
Sleep 400ms
Enter
Sleep 1.8s

# Pick the main model.
Enter
Sleep 1.4s
Down
Sleep 350ms
Down
Sleep 600ms
Enter
Sleep 1s

# Pick the background model.
Down
Sleep 400ms
Enter
Sleep 1.2s
Down
Sleep 250ms
Down
Sleep 250ms
Down
Sleep 250ms
Down
Sleep 600ms
Enter
Sleep 1s

# Context window.
Down
Sleep 400ms
Enter
Sleep 1.2s
Down
Sleep 250ms
Down
Sleep 600ms
Enter
Sleep 1s

Type "s"
Sleep 2s
Type "q"
Sleep 600ms

Type "cat ~/.byoclaude-rs/config.json"
Sleep 300ms
Enter
Sleep 1.5s
Type "byoclaude run   # Claude Code now runs on gpt-5.6-sol"
Sleep 2.5s
