# bay — start here

For contributors and coding agents working in this repository. Read `README.md` first, then the docs at
https://getbay.io/docs: they describe what bay does today, page by page. Every docs page also reads as
markdown (add `.md` to its address).

Code and tests cite bay's design by name and section, as in `DESIGN` § 6. The design itself is not
published; when a comment and the public docs disagree, the code and its tests are the truth.

## Build and test

```sh
cargo fmt --check
cargo clippy --all-targets
cargo test                                         # sha1 objects, files refs
BAY_TEST_FORMATS=sha256/reftable cargo test        # the same suite on SHA-256 and reftable
```

The integration tests build as one binary under `tests/it/`. A file named `gNN_*.rs` is the failure test
of one promise. Shared helpers are in `tests/it/support/`, and expected outputs in `tests/golden/`.

## When something is done

A promise is done when its failure test passes in bay's gate, and the pass is on record as a signed
`bay.evidence/1` note on that tree. Until then it is proposed.

- A new promise comes with a failure test.
- That test must first fail for a real reason: mutate the guard it protects, and watch it fail.
- Maintainers run `scripts/gate.sh`. It runs fmt, clippy and both test formats on the exact tree of
  `HEAD`, records each pass as evidence, then pushes. `scripts/gate.sh --verify <commit>` checks a record.
- There is no hosted CI. A pull request reaches `main` through a maintainer's gate run.
- Never force-push `main`.

## Do not reopen

- bay is not a code-review tool. Review is an outside producer of evidence, and bay never calls a model.
  No model sits in bay's decision path.
- bay does not carry any one organisation's merge tooling.

## One place, one truth

bay removes complexity, so it never adds a second place to look, a second copy of a fact, or a second concept to
learn.
- People act in the conversation they are already in. There is no dashboard, mirror or extra page.
- Each fact has one home:
  - Git holds the state.
  - `bay.result/1` is what a command says; its `say` sentence is derived from it.
  - The policy at the base holds the rules.
  - The registry holds the keys.
  - The skill holds the agent's guide.
  - The CHANGELOG holds the release notes.
- A new command or flag must take a step away from someone, not add one.
- The surface is budgeted: G312 counts the commands, flags, reason codes and plain words `bay help --json` lists. Growth
  raises the budget in the same commit, with one line on the step it takes away. The words a person never sees live
  in one list (`PLAIN_WORDS`, G311); the skill and the site are held to it.
- When a feature would need a second surface or a copy, extend the one home instead.

## Organisation-neutral

bay is a general tool. Nothing in this repository is specific to one organisation: no organisation's
names, hosts, paths, tools, trunk names, trailers, commands, defaults, examples or test fixtures. What an
organisation needs goes in its own `.bay/config.toml` and `.bay/policy.toml`, in a plugin behind a
public contract, or in its own docs. A feature that only makes sense for one organisation does not go
into bay. Defaults are plain Git ones: `main`, any host.

## Files with one owner

- **The bay skill**, `skills/bay/SKILL.md`, is the one copy for every agent and organisation. `bay install`
  copies it, and `bay doctor` reports `SKILL_MISMATCH` when an installed copy differs. When a command or
  flag changes, change the skill in the same commit.
- **`CHANGELOG.md`**: one `## <version>` section per release, newest first, user-visible changes only, no
  test ids. getbay.io copies it at each release tag, so keep each sentence to 20 words or fewer.
- **Versions**: `MAJOR.MINOR.PATCH`. While bay is at 0.x, a minor release may still change a command or a format. From 1.0, `scripts/release.sh`
  refuses a new MINOR or MAJOR without the maintainer's approval (`BAY_RELEASE_APPROVED=<version>`); a PATCH, and any 0.x
  release, needs none.

## Security

Report a vulnerability privately; see `SECURITY.md`.
