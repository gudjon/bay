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
# (0.1.0-pilot.1) is a pre-release.
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
        case "$target" in
            *-linux-*) build="zigbuild" ;;
            *) build="build" ;;
        esac
        BAY_RELEASE_VERSION=$version cargo "$build" -q --release --locked --target "$target"
        stage=$(mktemp -d)
        cp "target/$target/release/bay" "$stage/bay"
        tar -C "$stage" -czf "$out/bay-$target.tar.gz" bay
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
    gh release create "v$version" --target "$commit" $pre --title "bay $version" \
        --notes "bay $version, built from $commit. Verify SHA256SUMS with SHA256SUMS.sig (ssh-keygen -Y verify, namespace bay-release)." \
        "$out/VERSION" "$out/SHA256SUMS" "$out/SHA256SUMS.sig" "$out"/bay-*.tar.gz
    ;;
*) usage ;;
esac
