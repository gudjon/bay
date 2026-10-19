# Changelog

## 0.1.1
Pre-release.

Linux builds only (x86_64 and aarch64, musl). macOS builds come in a later release.

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
- Fixed release archives: they carry no extended attributes, so Linux tar unpacks them without warnings.

## 0.1.0-pilot.1
Pre-release.

- Added the first build: `bay start`, `land`, `status`, `doctor`, `install`, `share`, `host init` and `executor run`.
- Added signed release assets: one archive per platform, `VERSION`, `SHA256SUMS` and `SHA256SUMS.sig`.
- Added macOS and Linux builds for ARM and Intel.
