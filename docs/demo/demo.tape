# Records docs/demo/rsi-demo.gif with charmbracelet/vhs: `vhs docs/demo/demo.tape`
#
# Before recording: run `rsi` once, add a project (<Space>p), and have two or
# three sessions already running in it so the list has live activity. Adjust
# the objectives and sleeps below to match your machine.

Output docs/demo/rsi-demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1600
Set Height 900
Set Padding 12
Set TypingSpeed 60ms
Set Theme "Catppuccin Mocha"

Hide
Type "rsi" Enter
Sleep 3s
Show

# The session list: every agent, its status and cost.
Sleep 3s

# Start a new sandboxed session from a one-line objective.
Type ":blank add a --json flag to the status command" Enter
Sleep 4s

# Move through the list and open a session's live transcript.
Type "j" Sleep 600ms Type "j" Sleep 600ms
Enter
Sleep 5s

# Contextual help, then back out.
Type "?" Sleep 3s
Escape Sleep 1s
Escape Sleep 2s
