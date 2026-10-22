# Contributing to bay

Thanks for helping. bay is built in the open, and issues, ideas and pull requests are all welcome.

## Before you start

- **A bug?** Open an [issue](https://github.com/gudjon/bay/issues/new/choose) with the command you
  ran, the `bay.result/1` it printed, and the output of `bay doctor`.
- **A feature or a new promise?** Open an issue first, so we can agree on the contract before you
  write code. bay's contracts change slowly on purpose.
- **A security problem?** Do not open an issue. See [SECURITY.md](SECURITY.md).

## Build and test

You need the Rust toolchain named in `rust-toolchain.toml` (rustup installs it for you) and Git 2.54
or newer.

```sh
git clone https://github.com/gudjon/bay
cd bay
cargo build
cargo fmt --check
cargo clippy --all-targets
cargo test
```

The integration tests live in `tests/it/`. Most files test one promise, and each is named for what it proves.

## What a good pull request looks like

- **One change, one reason.** Small pull requests merge faster.
- **A new promise comes with a failure test** that fails without your change and passes with it.
- **A bug fix comes with a test** that reproduces the bug.
- **Every refusal says what to do next.** A new refusal has a stable reason, what the caller can still
  do, and when to retry.
- **Plain words.** Messages, docs and the changelog use short, plain sentences. Add a line to
  `CHANGELOG.md` under the next version for anything a user would notice.
- **No new dependency** without a reason in the pull request.
- `unsafe` is forbidden, and clippy's pedantic lints stay on.

## Organisation-neutral

bay is a general tool. Nothing in this repository is specific to one organisation: no
organisation-specific names, hosts, paths, branch names, trailers, commands, defaults, examples or test
fixtures. What one organisation needs goes in its own `.bay/config.toml` and `.bay/policy.toml`, or in
a plugin behind a public contract.

## How code reaches `main`

bay checks its own code the way it asks you to check yours. There is no hosted CI. A maintainer runs
`scripts/gate.sh`, which runs fmt, clippy and the tests on the exact tree of the commit, records each
pass as signed `bay.evidence/1` in Git notes, and only then pushes `main`. You can verify the commit
at the tip of any push (this needs `ssh-keygen`):

```sh
git fetch origin 'refs/notes/bay/*:refs/notes/bay/*'
scripts/gate.sh --verify <commit>
```

So your pull request is reviewed here, then merged through the gate by a maintainer. You do not need a
signing key to contribute.

## Licence

By contributing, you agree that your contribution is licensed under the [MIT licence](LICENSE).
