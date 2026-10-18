#!/bin/sh
# bay's gate: the checks a commit must pass before it is pushed to main, recorded as bay
# evidence. It replaces hosted CI (owner's ruling, 2026-10-18).
#
#   scripts/gate.sh                 check the working tree's HEAD, record the evidence, push
#   scripts/gate.sh --no-push       check and record only
#   scripts/gate.sh --verify [REV]  verify the recorded evidence of REV (default HEAD)
#
# Each check runs on the exact tree of HEAD (no tracked file may differ from it, and no
# untracked Rust source is built), and each pass is a producer-signed bay.evidence/1
# record on that tree under
# refs/notes/bay/evidence/<producer>. The evidence is pushed before main, so main never carries
# a tree whose evidence is not on the remote. A failing check records nothing and pushes nothing.
#
# Configuration (git config, per clone): bay.gateProducer (an operator name, e.g. gudjon-mac)
# and bay.gateProducerKey (an SSH private key). Its public key is listed in
# scripts/gate/allowed_signers under that name.
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
allowed="$root/scripts/gate/allowed_signers"
checks="fmt clippy test-sha1-files test-sha256-reftable"
evidence() { cargo run -q --example gate_evidence -- "$@"; }

if [ "${1:-}" = "--verify" ]; then
    evidence verify "$allowed" "${2:-HEAD}" $checks
    exit
fi

producer=$(git config bay.gateProducer) || { echo "gate: set git config bay.gateProducer" >&2; exit 1; }
key=$(git config bay.gateProducerKey) || { echo "gate: set git config bay.gateProducerKey" >&2; exit 1; }
grep -q "^$producer " "$allowed" || { echo "gate: $producer is not in $allowed" >&2; exit 1; }
# The checks read the whole tree, so nothing may differ from HEAD.
git diff --quiet HEAD ||
    { echo "gate: tracked files differ from HEAD; commit or stash them" >&2; exit 1; }
test -z "$(git ls-files --others --exclude-standard -- '*.rs' Cargo.toml Cargo.lock)" ||
    { echo "gate: untracked Rust sources would be built" >&2; exit 1; }

environment="$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)-git$(git --version | awk '{print $3}')"
notes="refs/notes/bay/evidence/$producer"

run() {
    name=$1; shift
    echo "gate: $name" >&2
    "$@"
    evidence record "$producer" "$key" "$name" "$environment" >/dev/null
}

run fmt cargo fmt --check
run clippy sh -c 'test -z "$(cargo clippy -q --all-targets 2>&1 | grep -E "^(warning|error)")"'
run test-sha1-files env BAY_TEST_FORMATS=sha1/files cargo test -q
run test-sha256-reftable env BAY_TEST_FORMATS=sha256/reftable cargo test -q
evidence verify "$allowed" HEAD $checks

if [ "${1:-}" != "--no-push" ]; then
    git push -q origin "$notes:$notes"
    git push -q origin main
fi
