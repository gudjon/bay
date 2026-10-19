---
name: bay
description: "Use bay for Git work on this repository. Load it to start a change, land it, give it up, or read status. Read the machine-readable result on stdout. Unknown is not success."
audit_gate: advisory — no deterministic artifact
verifiable_output_shape: advisory — knowledge skill, no artifact
allowed-tools:
  - Bash(bay:*)
  - Read
---

# bay

Run `bay`. Read stdout as one `bay.result/1` document (bay interfaces § 1). Text on stderr is not the result. There is no prompt. Unknown is not success.

Use the verb in bay design § 4.

| When | Run |
| --- | --- |
| Open one change | `bay start <intent> --paths <paths> [--input <kind>:<name>]` |
| Read what trunk now requires (after `INPUT_UNOBSERVED`) | `bay start --resume <change>` |
| Publish the checked result | `bay land` (returns at once; an executor checks and lands it) |
| Wait for the landing's outcome here | `bay land --wait` |
| Give the change up, so another actor may adopt it | `bay land --abandon` |
| Read state | `bay status` |
| See every machine and session the local cache knows | `bay status --fleet` |
| Read landings and events that touch your change's inputs | `bay watch [--since <cursor>]` (pass the returned cursor next time) |
| Ask another agent for one thing, with a reply time | `bay talk <agent> <what> --kind delivery\|discovery\|reliability … --follow-up <time>` (run `bay talk --help`) |
| Answer a talk addressed to you | `bay talk --answer <id> --value <v>` |
| Undo a landed change with a new change | `bay undo <change>` |

Run `bay <command> --help` for every flag this binary has.

Do not grant a key. Do not move trunk. Do not widen policy. Do not run `install` to land work (bay design § 15).

Do not run a command on the deny list (bay design § 11). Do not run `git reset --hard`. Do not push trunk.
