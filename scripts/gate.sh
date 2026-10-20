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

# The check sandbox (G18, G62). On Linux with bwrap on PATH the gate exports
# BAY_REQUIRE_SANDBOX=1, so every sandbox case runs: a bwrap that cannot run bay's sandbox fails
# those cases instead of skipping them. The test records then name the sandbox they ran in.
# On macOS the tests always run under sandbox-exec. Elsewhere the records name no sandbox.
sandbox=
if [ "$(uname -s)" = Linux ] && command -v bwrap >/dev/null 2>&1; then
    export BAY_REQUIRE_SANDBOX=1
    sandbox=bwrap
elif [ "$(uname -s)" = Darwin ] && [ -x /usr/bin/sandbox-exec ]; then
    sandbox=sandbox-exec
fi

run() {
    name=$1; shift
    echo "gate: $name" >&2
    "$@"
    case $name in
        test-*) evidence record "$producer" "$key" "$name" "$environment" ${sandbox:+"$sandbox"} >/dev/null ;;
        *) evidence record "$producer" "$key" "$name" "$environment" >/dev/null ;;
    esac
}

run fmt cargo fmt --check
run clippy sh -c 'test -z "$(cargo clippy -q --all-targets 2>&1 | grep -E "^(warning|error)")"'
# The Windows target keeps compiling. Where that target is not installed the check does not
# run, and nothing is recorded for it.
windows=
if rustup target list --installed 2>/dev/null | grep -qx x86_64-pc-windows-msvc; then
    run check-windows cargo check -q --target x86_64-pc-windows-msvc --all-targets
    windows=check-windows
else
    echo "gate: check-windows skipped, not recorded: the x86_64-pc-windows-msvc target is not installed (rustup target add x86_64-pc-windows-msvc)" >&2
fi
# The two repository formats run at once by default: the tests are built once, then each
# format runs the built test binaries on its own half of the CPUs this gate may use (its
# affinity list, where taskset is present). BAY_GATE_SERIAL=1 runs them one after the other.
# Either way each format records its pass only when every one of its tests passed.
formats="sha1/files sha256/reftable"
if [ "${BAY_GATE_SERIAL:-}" = 1 ]; then
    for format in $formats; do
        run "test-$(echo "$format" | tr / -)" env BAY_TEST_FORMATS="$format" cargo test -q
    done
else
    echo "gate: build tests" >&2
    # The gate's own scratch, under the target directory cargo already ignores.
    logs="$root/target/gate"
    mkdir -p "$logs"
    cargo test -q --no-run --message-format=json >"$logs/build.json"
    # The test harnesses only (built with the test profile): cargo also lists the binary and
    # the examples it built.
    binaries=$(grep -o '"profile":{[^}]*"test":true[^}]*}.*"executable":"[^"]*"' "$logs/build.json" |
        sed 's/.*"executable":"//; s/"$//')
    test -n "$binaries" || { echo "gate: no test binaries were built" >&2; exit 1; }
    # This gate's CPUs, split in two; empty where taskset is absent.
    cpus=$(taskset -cp $$ 2>/dev/null | sed 's/.*: //' | awk -F, '{
        for (i = 1; i <= NF; i++) { n = split($i, r, "-"); for (c = r[1]; c <= r[n]; c++) l[k++] = c }
        if (k < 2) exit
        h = int(k / 2); a = l[0]; for (i = 1; i < h; i++) a = a "," l[i]
        b = l[h]; for (i = h + 1; i < k; i++) b = b "," l[i]
        print a " " b }')
    # The script's own arguments stay as they are: the halves are read by position here.
    half=1
    pids=
    for format in $formats; do
        name="test-$(echo "$format" | tr / -)"
        pin=$(echo "$cpus" | cut -s -d' ' -f"$half")
        half=$((half + 1))
        echo "gate: $name${pin:+ on CPUs $pin}" >&2
        (
            export BAY_TEST_FORMATS="$format"
            for binary in $binaries; do
                ${pin:+taskset -c "$pin"} "$binary" -q || exit 1
            done
            cargo test -q --doc
        ) >"$logs/$name.log" 2>&1 &
        pids="$pids $!:$name"
    done
    failed=
    for entry in $pids; do
        wait "${entry%%:*}" || failed="$failed ${entry#*:}"
    done
    for format in $formats; do
        name="test-$(echo "$format" | tr / -)"
        echo "gate: $name output" >&2
        cat "$logs/$name.log" >&2
    done
    test -z "$failed" || { echo "gate: failed:$failed" >&2; exit 1; }
    for format in $formats; do
        run "test-$(echo "$format" | tr / -)" true
    done
fi
evidence verify "$allowed" HEAD $checks $windows

if [ "${1:-}" != "--no-push" ]; then
    git push -q origin "$notes:$notes"
    git push -q origin main
fi
