#!/bin/sh
# bay's release: signed builds of a gated commit, published as a GitHub release.
#
#   scripts/release.sh setup              create this box's release key (once) and record it
#   scripts/release.sh build <version>    build, package and sign HEAD into target/release/<version>/
#   scripts/release.sh attach-sig <version> <file>
#                                         take SHA256SUMS.sig made by an external signer
#   scripts/release.sh publish <version>  create the GitHub release v<version> from that directory,
#                                         tagging the commit build recorded in it (COMMIT)
#
# With BAY_RELEASE_EXTERNAL_SIGNER=1, build writes SHA256SUMS and does not sign it: it prints the
# file to sign, and the signature is made elsewhere (a hardware key that signs only with a touch,
# `ssh-keygen -Y sign -n bay-release -f <key> SHA256SUMS`) and brought back with attach-sig, which
# copies it in only when it verifies against a key in scripts/release/allowed_signers. publish
# refuses a release whose SHA256SUMS.sig is absent or does not verify.
#
# A release is only made from a commit whose gate evidence verifies (scripts/gate.sh --verify),
# so every binary is built from a tree that passed every check. The assets are one
# bay-<target>.tar.gz per target (`bay` at its root), VERSION (the version and a newline),
# SHA256SUMS over all of them, and SHA256SUMS.sig: an SSH signature in the namespace bay-release
# by the release key, a key of its own that signs nothing else. A version with a suffix
# (0.1.0-pilot.1), or whose CHANGELOG.md section starts with the line "Pre-release.", is a
# pre-release. Every version needs its "## <version>" section in CHANGELOG.md; that section is
# the release's notes. BAY_RELEASE_TARGETS narrows the target list (a release may ship fewer
# platforms; the changelog says which).
#
# Configuration (git config, per clone): bay.releaseKey (the SSH private key), unless the signer
# is external. scripts/release/allowed_signers lists every release key under the principal
# bay-release, the same keys as RELEASE_KEYS in src/update.rs (a unit test keeps them equal).
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
allowed="$root/scripts/release/allowed_signers"
targets="${BAY_RELEASE_TARGETS:-aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl}"

usage() { echo "usage: scripts/release.sh setup | build <version> | attach-sig <version> <file> | publish <version>" >&2; exit 2; }

# Whether signature file $2 verifies SHA256SUMS in directory $1 against a listed release key.
verifies() {
    [ -f "$1/SHA256SUMS" ] && [ -f "$2" ] &&
        ssh-keygen -Y verify -q -f "$allowed" -I bay-release -n bay-release -s "$2" < "$1/SHA256SUMS"
}

check_version() {
    printf '%s' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' ||
        { echo "release: $1 is not a version (X.Y.Z or X.Y.Z-suffix)" >&2; exit 1; }
    crate=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
    case "$1" in
        "$crate" | "$crate"-*) ;;
        *) echo "release: $1 does not match the crate version $crate" >&2; exit 1 ;;
    esac
    grep -qx "## $1" CHANGELOG.md ||
        { echo "release: CHANGELOG.md has no '## $1' section" >&2; exit 1; }
    check_bump "$1"
}

# A new MINOR or MAJOR over the newest released tag needs the owner's approval (BACKLOG 16e): the owner
# sets BAY_RELEASE_APPROVED=<version>. A PATCH, or the same MAJOR.MINOR, needs none.
check_bump() {
    last=$(git tag -l 'v[0-9]*' --sort=-v:refname | head -1 | sed 's/^v//')
    [ -n "$last" ] || return 0
    new_mm=$(printf '%s' "$1" | cut -d- -f1 | cut -d. -f1,2)
    last_mm=$(printf '%s' "$last" | cut -d- -f1 | cut -d. -f1,2)
    [ "$new_mm" = "$last_mm" ] && return 0
    [ "${BAY_RELEASE_APPROVED:-}" = "$1" ] && return 0
    echo "release: $1 is a new MINOR or MAJOR over v$last; it needs the owner's approval: BAY_RELEASE_APPROVED=$1 (BACKLOG 16e)" >&2
    exit 1
}

# The CHANGELOG.md section of version $1, without its heading.
# The CHANGELOG section of version $1, read at commit $2.
notes() {
    git show "$2:CHANGELOG.md" | awk -v head="## $1" '$0 == head { on = 1; next } on && /^## / { exit } on'
}

key() {
    git config bay.releaseKey || { echo "release: set git config bay.releaseKey (scripts/release.sh setup)" >&2; exit 1; }
}

case "${1:-}" in
setup)
    file="$HOME/.config/bay/release/bay-release"
    if [ ! -e "$file" ]; then
        mkdir -p "$(dirname "$file")"
        chmod 700 "$(dirname "$file")"
        ssh-keygen -q -t ed25519 -N "" -C bay-release -f "$file"
    fi
    git config bay.releaseKey "$file"
    echo "release: key $file; its public line for $allowed:"
    echo "bay-release $(cut -d' ' -f1,2 "$file.pub")"
    ;;
build)
    [ $# -eq 2 ] || usage
    version=$2
    check_version "$version"
    external="${BAY_RELEASE_EXTERNAL_SIGNER:-}"
    [ "$external" = 1 ] || signing_key=$(key)
    git diff --quiet HEAD || { echo "release: tracked files differ from HEAD" >&2; exit 1; }
    test -z "$(git ls-files --others --exclude-standard -- '*.rs' Cargo.toml Cargo.lock)" ||
        { echo "release: untracked Rust sources would be built" >&2; exit 1; }
    scripts/gate.sh --verify HEAD
    out="$root/target/release/$version"
    rm -rf "$out"
    mkdir -p "$out"
    for target in $targets; do
        # A macOS host builds its own targets natively; every other target, and macOS targets
        # from a Linux host, build with cargo-zigbuild.
        case "$(uname -s):$target" in
            Darwin:*-apple-darwin) build="build" ;;
            *) build="zigbuild" ;;
        esac
        BAY_RELEASE_VERSION=$version cargo "$build" -q --release --locked --target "$target"
        stage=$(mktemp -d)
        cp "target/$target/release/bay" "$stage/bay"
        # No extended attributes: macOS tar adds com.apple.provenance headers, which GNU tar
        # on Linux warns about while unpacking.
        COPYFILE_DISABLE=1 tar --no-xattrs -C "$stage" -czf "$out/bay-$target.tar.gz" bay
        rm -rf "$stage"
    done
    printf '%s\n' "$version" > "$out/VERSION"
    # The built commit: publish tags it and reads its notes there, whatever HEAD is by then.
    git rev-parse HEAD > "$out/COMMIT"
    (cd "$out" && shasum -a 256 VERSION bay-*.tar.gz > SHA256SUMS)
    if [ "$external" = 1 ]; then
        echo "release: $version built from $(git rev-parse --short HEAD) in $out, unsigned"
        echo "release: sign $out/SHA256SUMS (ssh-keygen -Y sign -n bay-release -f <key> SHA256SUMS), then: scripts/release.sh attach-sig $version <SHA256SUMS.sig>"
        exit 0
    fi
    ssh-keygen -Y sign -q -f "$signing_key" -n bay-release "$out/SHA256SUMS"
    verifies "$out" "$out/SHA256SUMS.sig"
    echo "release: $version built from $(git rev-parse --short HEAD) in $out"
    ;;
attach-sig)
    [ $# -eq 3 ] || usage
    version=$2
    check_version "$version"
    out="$root/target/release/$version"
    [ -f "$out/SHA256SUMS" ] || { echo "release: $out/SHA256SUMS is absent; run build first" >&2; exit 1; }
    verifies "$out" "$3" ||
        { echo "release: $3 does not verify SHA256SUMS against a key in $allowed; nothing attached" >&2; exit 1; }
    cp "$3" "$out/SHA256SUMS.sig"
    echo "release: SHA256SUMS.sig attached to $version"
    ;;
publish)
    [ $# -eq 2 ] || usage
    version=$2
    out="$root/target/release/$version"
    # build checked the version against the commit it built; publish reads that commit, not HEAD,
    # so trunk may move while an external signature is made.
    [ -s "$out/COMMIT" ] ||
        { echo "release: $out/COMMIT is absent; build the release again" >&2; exit 1; }
    [ "$(cat "$out/VERSION" 2>/dev/null)" = "$version" ] ||
        { echo "release: $out/VERSION is not $version" >&2; exit 1; }
    [ -f "$out/SHA256SUMS.sig" ] ||
        { echo "release: $out/SHA256SUMS.sig is absent; sign the release (build, or attach-sig)" >&2; exit 1; }
    verifies "$out" "$out/SHA256SUMS.sig" ||
        { echo "release: SHA256SUMS.sig does not verify against a key in $allowed" >&2; exit 1; }
    (cd "$out" && shasum -a 256 -c -q SHA256SUMS)
    commit=$(cat "$out/COMMIT")
    git fetch -q origin
    # A patch of an older minor is built on its own line, origin/release-<MAJOR>.<MINOR> (0.3.8 on
    # release-0.3, while main holds 0.4); every other release is built on main.
    minor=${version%%-*}
    minor=${minor%.*}
    git merge-base --is-ancestor "$commit" origin/main 2>/dev/null ||
        git merge-base --is-ancestor "$commit" "origin/release-$minor" 2>/dev/null ||
        { echo "release: the built commit $commit is on neither origin/main nor origin/release-$minor" >&2; exit 1; }
    pre=""
    case "$version" in *-*) pre="--prerelease" ;; esac
    notes "$version" "$commit" | sed '/./,$!d' | head -1 | grep -qx 'Pre-release.' && pre="--prerelease"
    notes_file="$out/NOTES.md"
    { notes "$version" "$commit"; printf '\nBuilt from %s. Verify SHA256SUMS with SHA256SUMS.sig (ssh-keygen -Y verify, namespace bay-release).\n' "$commit"; } > "$notes_file"
    gh release create "v$version" --target "$commit" $pre --title "bay $version" \
        --notes-file "$notes_file" \
        "$out/VERSION" "$out/SHA256SUMS" "$out/SHA256SUMS.sig" "$out"/bay-*.tar.gz
    ;;
*) usage ;;
esac
