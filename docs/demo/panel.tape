# Records docs/demo/panel.gif: Claude asks a byoclaude panel, the session keeps going, and the
# verdict comes back as a message. Local mock servers stand in for every provider (no account,
# no plan usage). From the repository root:
#   cargo build --release --bin byoclaude --example mock_openai && vhs docs/demo/panel.tape
# then hold the last frame so the answer can be read:
#   ffmpeg -i docs/demo/panel.gif -vf "tpad=stop_mode=clone:stop_duration=5,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" panel.gif && mv panel.gif docs/demo/
Output docs/demo/panel.gif

Set Shell bash
Set FontSize 14
Set Width 1440
Set Height 860
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Hide
Type "export DEMO_PANEL=1; source docs/demo/setup.sh"
Enter
Sleep 1s
Show

Type "byoclaude"
Sleep 300ms
Enter
Sleep 6s
Type "Get a panel's view: is Pool::release in src/pool.rs safe under concurrent callers?"
Sleep 500ms
Enter
Sleep 16s
Sleep 6s

Hide
Ctrl+C
Sleep 300ms
Ctrl+C
Sleep 1s
Type "byoclaude stop; kill %1"
Enter
Sleep 500ms
