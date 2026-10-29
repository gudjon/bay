#!/bin/sh
# Each line of the newest release in CHANGELOG.md is 20 words or fewer: getbay.io copies the
# file at each release tag. This script is the rule's one home; scripts/gate.sh runs it before
# any compile, and the test the_newest_changelog_lines_fit_the_site runs it too.
#
#   scripts/changelog-fits.sh [FILE]   check FILE (default: CHANGELOG.md at the repository root)
#
# Words are counted as the site shows them: backticks removed, split on white space.
set -eu

file=${1:-$(dirname "$0")/../CHANGELOG.md}
awk -v most=20 '
    /^## / { n++; next }
    n == 1 && /^- / {
        line = substr($0, 3); gsub(/`/, "", line)
        lines++
        words = split(line, w, " ")
        if (words > most) { print "changelog: " words " words: " $0 > "/dev/stderr"; long++ }
    }
    END {
        if (lines == 0) { print "changelog: the newest release names no change" > "/dev/stderr"; exit 1 }
        exit long > 0
    }' "$file"
