# agent-walker demo recording (synthetic demo data — no real usage).
# Regenerate:  cargo build && vhs docs/demo.tape
Output docs/demo.gif

Set FontSize 13
Set Width 1280
Set Height 820
Set Padding 16
Set Theme "Catppuccin Mocha"
Env AGENT_WALKER_DEMO "1"

# Launch the dashboard off-camera, start recording on the rendered TUI.
Hide
Type "./target/debug/agent-walker"
Enter
Sleep 3s
Show

Sleep 2s
Tab
Sleep 1800ms
Tab
Sleep 1800ms
PageDown
Sleep 1500ms
PageUp
Sleep 1s
# Open the share modal (the codename stats card).
Type "s"
Sleep 2500ms
Down
Sleep 1200ms
Up
Sleep 1200ms
Escape
Sleep 1s
Type "q"
Sleep 800ms
