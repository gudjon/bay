# The README demo: two agents in one codebase, one tmux pane each, each in its own view. Both add a service on
# port 8080. A merges; bay checks B's change on the new main and refuses it (CHECK_FAILED); B moves to 8081 and
# merges. Record it with vhs (github.com/charmbracelet/vhs) and tmux, from this directory: vhs demo.tape
Output demo.gif
Set Shell "bash"
Set FontSize 18
Set Width 1000
Set Height 660
Set Padding 18
Set TypingSpeed 35ms
Set Theme "Builtin Dark"

Hide
Type "bash start-agents.sh" Enter
Sleep 5s
Show
Sleep 1s

Type 'echo "port = 8080" > services/web.conf'
Enter
Sleep 600ms
Ctrl+B
Down
Type 'echo "port = 8080" > services/api.conf'
Enter
Sleep 800ms
Ctrl+B
Up
Type "bay land --wait"
Enter
Sleep 3.5s
Ctrl+B
Down
Type "bay land --wait"
Enter
Sleep 6s
Type 'echo "port = 8081" > services/api.conf'
Enter
Sleep 500ms
Type "bay land --wait"
Enter
Sleep 4s
Ctrl+B
Up
Type "git fetch -q && git log --oneline origin/main"
Enter
Sleep 5s

Hide
Ctrl+B
Type "d"
Sleep 500ms
Type "sh stop-agents.sh" Enter
Sleep 1s
