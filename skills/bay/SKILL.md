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
| Open one change | `bay start <intent> --paths <paths>` |
| Publish the checked result | `bay land` |
| Give the change up, so another actor may adopt it | `bay land --abandon` |
| Read state | `bay status` |
| Undo a landed change with a new change | `bay undo <change>` |

This version has no `watch`, `talk` or `status --fleet`. Do not run them. Run `bay --help` for the commands this binary has.

Do not grant a key. Do not move trunk. Do not widen policy. Do not run `install` to land work (bay design § 15).

Do not run a command on the deny list (bay design § 11). Do not run `git reset --hard`. Do not push trunk.
