#!/bin/sh
# Ends the two-agents demo: its tmux server and the executor that start-agents.sh started, nothing else.
R=$(cd "$(dirname "$0")" && pwd); D=$(cat "$R/.demo-dir" 2>/dev/null) || exit 0
tmux -L demo kill-server 2>/dev/null
[ -n "$D" ] && pkill -f "$D/home/.local/bin/bay executor run"
rm -f "$R/.demo-dir"
exit 0
