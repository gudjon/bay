# Changelog

## 0.1.3
Pre-release.

Builds for Linux (x86_64 and aarch64, musl) and macOS. Apple silicon is tested; Intel (x86_64) is built but not yet run on an Intel Mac.

- Changed where bay reads `.bay/config.toml`: only from the trunk's committed copy, never from a work tree or a change.
- Added `bay install --trunk <name> --remote <name>`, which records where bay finds the trunk on this clone.
- Added `CONFIG_MISSING`: a repository with no config at its trunk is refused, naming where bay looked.
- Added `POLICY_MISSING`: a change never lands without a policy at its base, unless that policy says `unchecked = true`.
- Changed checks to materialize the candidate without a tar of the whole tree, so large repositories check quickly.
- Added `paths` and `materialize_timeout_s` to `[[check]]`; a materialization that runs over is `infra_failure`.
- Added the bay skill under `CLAUDE_CONFIG_DIR` when it is set, and doctor checks it there.
- Changed `bay talk --help` to say agent, not actor.
- Changed `bay update` to take the newest release that has a build for this machine; releases are no longer split into stable and latest.
- Fixed the bay skill: it lists watch, talk, status --fleet and land --wait, which 0.1.2 already had but its skill told agents not to run.
- Removed update channels: `--channel`, `BAY_UPDATE_CHANNEL` and `bay.updateChannel` are no longer read, and doctor names one that is still set.

## 0.1.2
Pre-release.

Builds for Linux (x86_64 and aarch64, musl) and macOS. The macOS builds were added on 19 October 2026, from the same commit. Apple silicon is tested; Intel (x86_64) is built but not yet run on an Intel Mac.

- Added `bay status --fleet`: every session on every machine, from signed live summaries, with its change, state and age.
- Added `bay watch [--since <cursor>]`: landings, events and invalidations since a cursor, so a missed notice never hides one.
- Added `bay talk`: a structured ask to another actor, acked by the recipient's own key, with supersede and answers.
- Added named inputs: `bay start --input <kind>:<name>`, and land refuses work whose inputs moved until it is repaired.
- Added the stale-write refusal: the Claude Code hook refuses a write that a stale input covers, from local data only.
- Added the Stop refusal: a session is asked once to checkpoint before it stops with unsaved edits in its view.
- Added hook notices: a refusal or failed check reaches the session once, as one line, within a byte budget.
- Added evidence reuse with `evidence_reuse = "exact-inputs"`: a check on identical inputs is not run again.
- Added runtime evidence and completion profiles, so status never calls a change done while a required part is missing.
- Added `BAY_UPDATE_VERSION` and `BAY_UPDATE_CHANNEL`, which pin one process's updates on a shared login.
- Added a session work record per agent that lists the files each step wrote.
- Changed checks to fail as `infra_failure` on a box with no sandbox, unless the box sets `bay.executorSandbox none`.
- Changed bay to compile for Windows; Windows builds come in a later release.
- Fixed the release script: a new MINOR or MAJOR needs the owner's approval through `BAY_RELEASE_APPROVED`.

## 0.1.1
Pre-release.

Builds for Linux (x86_64 and aarch64, musl) and macOS. The macOS builds were added on 19 October 2026, from the same commit. Apple silicon is tested; Intel (x86_64) is built but not yet run on an Intel Mac.

- Added `bay undo <change>`: it lands a new, checked change that reverses one landing; history stays.
- Added check cost: each evidence record keeps the check's seconds, and `bay status` sums them per operation and landing.
- Added `bay update`: it replaces bay with a signed, verified release and never with an unverified download.
- Added a daily background update check from `bay status` and `bay doctor`, off with `BAY_DISABLE_AUTOUPDATE=1`.
- Added box-level update settings in global Git config: `bay.updateVersion`, `bay.updateChannel` and `bay.updateAuto`.
- Added `UPDATE_IN_USE`: an update waits while an executor on the box holds an operation.
- Added `bay install --machine`, which installs only the binary and the bay skill.
- Added the bay skill to `bay install`, and a doctor row that reports a skill that does not match.
- Added `bay status --fleet`, which fails closed until the fleet refs have been fetched.
- Added a `Bay-Session` trailer on landed commits; the actor trailer now carries the bare actor name.
- Added `changed_paths` to the check request, so a check can judge only what a change touched.
- Added `POLICY_INVALID` for a policy bay cannot judge by, and `declared_empty` for a check that selects nothing.
- Added `[[quarantine]]` to set aside a check that is already red on trunk.
- Changed checks to start from a fixed environment, which is part of each check's definition.
- Added `CHECK_UNAVAILABLE`: a queue head with no capable worker waits outside the queue.
- Added `REGISTRY_CHANGED` handling: a revoked key re-signs the same tree and reruns only commit-keyed checks.
- Changed the bay skill to list only the commands this version has.
- Changed `bay doctor` to say "Claude adapter not configured" when the adapter is absent.
- Changed `bay doctor` outside a repository to report the box's own rows instead of an error.
- Changed `.bay/config.toml` loading: an unknown key is an error that names it.
- Changed session start to report the change's state and open refusals from local data only.
- Fixed `bay host init` on Git below 2.54: it refuses with `GIT_BELOW_FLOOR` and creates nothing.
- Fixed a cancel or newer revision that races a landing: the losing operation says it was superseded.
- Fixed a request signed under a registry version that is not on the registry chain: it is refused.
- Changed evidence reading: fields a later version adds are kept and signed over, so newer records still verify.
- Fixed release archives: they carry no extended attributes, so Linux tar unpacks them without warnings.

## 0.1.0-pilot.1
Pre-release.

- Added the first build: `bay start`, `land`, `status`, `doctor`, `install`, `share`, `host init` and `executor run`.
- Added signed release assets: one archive per platform, `VERSION`, `SHA256SUMS` and `SHA256SUMS.sig`.
- Added macOS and Linux builds for ARM and Intel.
