# Security

bay decides what lands on trunk, signs evidence, and updates its own binary. We take reports about any
of that seriously.

## Report a vulnerability

Please do not open a public issue. Report it privately:

- through [GitHub's private vulnerability reporting](https://github.com/gudjon/bay/security/advisories/new), or
- by email to [hello@getbay.io](mailto:hello@getbay.io), with "security" in the subject.

Tell us what you found, how to reproduce it, and which version (`bay --version`) you ran. We reply
within a few working days, agree a fix and a date with you, and credit you in the release notes unless
you ask us not to.

## In scope

Anything that breaks one of bay's promises counts. For example:

- a commit lands that is not the one that was checked
- a check reads outside its sandbox, or reaches the network when its policy says no
- evidence verifies when it should not, or an actor acts with a key that is not theirs
- `bay update` or `install.sh` installs a binary that is not a signed release

## Supported versions

bay is pre-release. Fixes go into the newest release only. Run `bay update` to get it.

## Verify a release yourself

Every release carries `SHA256SUMS` and `SHA256SUMS.sig`, an SSH signature in the namespace
`bay-release`. The trusted keys are in [`scripts/release/allowed_signers`](scripts/release/allowed_signers).

```sh
ssh-keygen -Y verify -f scripts/release/allowed_signers -I bay-release -n bay-release \
  -s SHA256SUMS.sig < SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
```

`install.sh` and `bay update` run the same checks before they replace a binary.
