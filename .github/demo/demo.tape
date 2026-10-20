# The README demo. Record it with vhs (github.com/charmbracelet/vhs) from this directory: vhs demo.tape
Output demo.gif
Set Shell "bash"
Set FontSize 19
Set Width 1180
Set Height 300
Set Padding 26
Set TypingSpeed 38ms
Set Theme "Builtin Dark"
Set WaitPattern /^\$ ?$/
Set WaitTimeout 90s

Hide
Type "R=$PWD; D=$(mktemp -d); sh $R/setup.sh $D >/dev/null 2>&1; export HOME=$D/home; unset CLAUDE_CONFIG_DIR; export PATH=$HOME/.local/bin:$PATH; cd $D/app; export PS1='$ '; clear" Enter
Wait
Show

Type "# open a change: a view of only src/" Enter
Wait
Type 'cd "$(bay start CARD-7 --paths src --json | jq -r .detail.view)"' Enter
Wait
Type 'echo "hello, world" > src/greeting.txt' Enter
Wait
Type "# land it: bay checks the exact result, then lands it" Enter
Wait
Type "bay land --wait --json | jq -c '{decision, change, commit: .detail.commit[0:7]}'" Enter
Wait
Sleep 1s
Type "bay status --json | jq -c '.detail.changes[0].evidence.records[] | {check, verdict}'" Enter
Wait
Sleep 4s
