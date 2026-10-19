#!/bin/sh
# bay's release: signed builds of a gated commit, published as a GitHub release.
#
#   scripts/release.sh setup              create this box's release key (once) and record it
#   scripts/release.sh build <version>    build, package and sign HEAD into target/release/<version>/
#   scripts/release.sh publish <version>  create the GitHub release v<version> from that directory
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
# Configuration (git config, per clone): bay.releaseKey (the SSH private key). Its public key is
# scripts/release/allowed_signers under the principal bay-release.
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
allowed="$root/scripts/release/allowed_signers"
targets="${BAY_RELEASE_TARGETS:-aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl}"

usage() { echo "usage: scripts/release.sh setup | build <version> | publish <version>" >&2; exit 2; }

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
notes() {
    awk -v head="## $1" '$0 == head { on = 1; next } on && /^## / { exit } on' CHANGELOG.md
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
    signing_key=$(key)
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
    (cd "$out" && shasum -a 256 VERSION bay-*.tar.gz > SHA256SUMS)
    ssh-keygen -Y sign -q -f "$signing_key" -n bay-release "$out/SHA256SUMS"
    ssh-keygen -Y verify -q -f "$allowed" -I bay-release -n bay-release -s "$out/SHA256SUMS.sig" < "$out/SHA256SUMS"
    echo "release: $version built from $(git rev-parse --short HEAD) in $out"
    ;;
publish)
    [ $# -eq 2 ] || usage
    version=$2
    check_version "$version"
    out="$root/target/release/$version"
    ssh-keygen -Y verify -q -f "$allowed" -I bay-release -n bay-release -s "$out/SHA256SUMS.sig" < "$out/SHA256SUMS"
    (cd "$out" && shasum -a 256 -c -q SHA256SUMS)
    commit=$(git rev-parse HEAD)
    git fetch -q origin
    git merge-base --is-ancestor "$commit" origin/main || { echo "release: HEAD is not on origin/main" >&2; exit 1; }
    pre=""
    case "$version" in *-*) pre="--prerelease" ;; esac
    notes "$version" | sed '/./,$!d' | head -1 | grep -qx 'Pre-release.' && pre="--prerelease"
    notes_file="$out/NOTES.md"
    { notes "$version"; printf '\nBuilt from %s. Verify SHA256SUMS with SHA256SUMS.sig (ssh-keygen -Y verify, namespace bay-release).\n' "$commit"; } > "$notes_file"
    gh release create "v$version" --target "$commit" $pre --title "bay $version" \
        --notes-file "$notes_file" \
        "$out/VERSION" "$out/SHA256SUMS" "$out/SHA256SUMS.sig" "$out"/bay-*.tar.gz
    ;;
*) usage ;;
esac
