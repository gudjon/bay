---
name: bay
description: "Use bay for Git work on this repository. Load it to start a change, land, read status, see the fleet, watch inputs, send a talk, or undo a landing. Read the machine-readable result on stdout. An ack means the talk arrived. It is not proof of work."
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
| Read state | `bay status` |
| List machines and actors | `bay status --fleet` |
| See what changed for this change | `bay watch` |
| Send one talk | `bay talk <actor> <text>` |
| Undo one landing | `bay undo <change>` |

`bay status --fleet` reads the local Git cache. It does not search machines by SSH. It does not start a session. If the remote has not been fetched, the result is not success (bay design § 11a).

An ack means the talk arrived and was accepted. It does not mean the work is understood, started, or done (bay design § 11c).

Do not grant a key. Do not move trunk. Do not close a talk. Do not widen policy. Do not run `install` to land work (bay design § 15).

Do not run a command on the deny list (bay design § 11). Do not run `git reset --hard`. Do not push trunk.
