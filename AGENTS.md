# bay — start here

Read `README.md` first. bay's design and contracts (`DESIGN`, `INTERFACES`, `ROADMAP`, `NAMES`,
`GIT-FEATURES`, `DECISIONS`, `PRIOR-ART`) and its research record are not in this repository: they
live in the project's private repository (getbay.io, under `docs/`). Code and tests cite them by name
and section, as in `DESIGN` § 6. If you can read them, read `DESIGN` §§ 1–6, then `INTERFACES`, then
`ROADMAP`, and `NAMES` for the one meaning of each name.

## Design state

The design chapters were imported on 2026-10-18; their provenance is kept with the research record.

Rounds 20–22 are in that text:

- one allowed-signers file per key class, a key in exactly one file, RFC 8785 manifests, staleness by manifest digest
- the check adapter runs from the accepted base, and its object ids enter the definition digest
- an ordinary policy or adapter change is checked twice; a registry-key signature is only the last-resort repair of `.bay/policy.toml` and adapter paths; `expected_minimum` is required; the sandbox holds only listed files

Round 23's simplification (every policy or adapter change is a registry-key administrative act,
and an agent can never sign one) is **not** in the design. It was only proposed in a session log.
Do not treat it as the design.

The research unit's chapter 15 states a clean-round count of **0**. Two consecutive clean review
rounds are still owed. Seeding and increment 1 do not wait for them.

A local crate is already in this tree (`src/git.rs`, `src/result.rs`, `bay doctor`, and ignored
G-tests). Keep it. Do not start a second seed.

## Do not reopen

bay is not a code-review tool. Review is an external evidence producer; bay does not call a model.
No model sits in the decision path. Do not put another organisation's landing tooling in this
repository.

The fleet-specific review ruling (decision 13) lives in the research record, chapter 12
(`12-rulings-and-ledger.md`).
Do not re-open it as a bay feature. A pilot at any one organisation lives in that research record, not in
this repository.

## Organisation-neutral

bay is a general tool. Nothing in this repository is specific to one organisation: no
organisation-specific names, hosts, paths, seats, tools, trunk names, trailers, commands, defaults,
examples or test fixtures. What an organisation needs goes in its own `.bay/config.toml` and
`.bay/policy.toml`, in an adapter behind a public contract, or in its own docs. A feature that only
makes sense for one organisation does not go into bay.

## Done, and how code reaches main

A guarantee is done when its G-test passed in bay's gate and that pass is on record: a
producer-signed `bay.evidence/1` record on the tree, under `refs/notes/bay/evidence/<producer>`,
that verifies against `scripts/gate/allowed_signers` (`scripts/gate.sh --verify <commit>`). Until
then it is proposed. There is no hosted CI (owner's ruling, 2026-10-18).

Work on `main`. `scripts/gate.sh` runs `cargo fmt --check`, `cargo clippy --all-targets` and
`cargo test` under sha1/files and under sha256/reftable on the exact tree of HEAD, records each
pass as evidence on that tree, pushes the evidence, then pushes `main` with a plain
`git push origin main`. Never force-push. A failing check records nothing and pushes nothing; that
is a simplification of the gate, not bay's rule (bay records a failure as evidence, G12).

The gate checks the commit at HEAD; it never stages anything, so commit first (with the parent taken
from a fresh HEAD and `main` moved by `git update-ref refs/heads/main <new> <old>`), then run it. Its
push is a plain fast-forward push of the evidence, then of `main`. Every commit goes through the gate.

The producer key is a key of its own (`bay.gateProducerKey`, one per box). It never signs commits or
agent requests, so a record means a producer attested the run, not only that the author ran it.

A guarantee is tested only for the environments its evidence names (`ROADMAP`, top). Open gaps:
the Linux check sandbox (bwrap, G18's sandbox case) and Git's development branch, until a producer on
such a box runs the gate.

## Next action

The seed is on `main`. Increment 1 is being ported from the local `increment-1` branch (tip `924bbda`,
no merge-base with `main`) one G-test at a time, in `ROADMAP` § 2 order. Copy file contents only.
Never merge, cherry-pick or push that branch. Each enabled G-test must first fail for a real reason under a
mutation of the guard it protects. Two clean review rounds are still owed on the research unit; do not wait
for them, and do not rewrite the policy to the round-23 rule.
