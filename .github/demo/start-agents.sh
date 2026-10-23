#!/bin/bash
# Opens the two-agents demo: a throwaway repository (setup-two-agents.sh), a runner in the background, one
# view per agent, and a tmux window with one pane per agent. In a pane, `bay` shows bay's own human summary
# (its stderr) without the bay.result/1 line it also prints on stdout. Usage: start-agents.sh (from this directory).
set -eu
R=$(cd "$(dirname "$0")" && pwd); D=$(mktemp -d); echo "$D" > "$R/.demo-dir"
sh "$R/setup-two-agents.sh" "$D" >/dev/null 2>&1
export HOME="$D/home"; unset CLAUDE_CONFIG_DIR; export PATH="$HOME/.local/bin:$PATH"
cd "$D/app"
("$HOME/.local/bin/bay" runner run >/dev/null 2>&1 &)
A=$(bay start CARD-1 --paths services --json | jq -r .detail.view)
B=$(bay start CARD-2 --paths services --json | jq -r .detail.view)
mkdir -p "$D/bin"
printf '#!/bin/sh\nexec "%s" "$@" 2>&1 >/dev/null\n' "$HOME/.local/bin/bay" > "$D/bin/bay"; chmod +x "$D/bin/bay"
export PATH="$D/bin:$PATH" DEMO_DIR="$D"
pane() { printf 'cd %s; PS1=%q bash --norc -i' "$1" "$2"; }
exec tmux -L demo -f "$R/tmux.conf" new-session -s d "$(pane "$A" 'agent A $ ')" \; \
  split-window -v -l 65% "$(pane "$B" 'agent B $ ')" \; \
  select-pane -t 0 -T ' agent A · adds the web service on port 8080 ' \; \
  select-pane -t 1 -T ' agent B · adds the API service, also on port 8080 ' \; \
  select-pane -t 0
