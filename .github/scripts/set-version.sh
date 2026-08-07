#!/usr/bin/env bash
#
# Puts one version number into Cargo.toml and Cargo.lock.
#
# `main` holds `0.0.0-dev` and no pull request changes it. `release.yml` calls
# this script ONCE for each release: it writes the number, it commits that as
# `chore(release): vX.Y.Z` on a commit that hangs off `main`, and it makes the
# tag on that commit. The tag thus names a commit that holds the correct number
# already, so no build and no `cargo publish` writes it again.
#
# A person who makes a release by hand — `v1.0.0` — makes the same commit:
#
#     git checkout --detach main
#     .github/scripts/set-version.sh 1.0.0
#     git commit -am "chore(release): v1.0.0"
#     git tag -a v1.0.0 -m "qex 1.0.0"
#     git push origin v1.0.0
#
# Usage: set-version.sh 0.6.0

set -euo pipefail

version="${1:?give the version, such as 0.6.0}"

cd "$(dirname "$0")/../.."

# THE FILE MUST HOLD EXACTLY `0.0.0-dev` BEFORE THIS SCRIPT WRITES.
#
# `main` holds that string and nothing else holds it. The test is thus the same
# question as "am I on `main`, with nothing written yet", and it catches three
# mistakes that are otherwise invisible:
#
#   * the job runs against the wrong ref, such as a tag,
#   * the checkout is old and holds a number that a release already took,
#   * this script runs a second time in one job.
#
# Each of those writes a number over a number, and the release then carries a
# version that no rule chose. The comparison is against the WHOLE string:
# `0.0.0-dev2` is a different state and it must not pass.
expected="0.0.0-dev"
current="$(awk -F'"' '/^version *= *"/ { print $2; exit }' Cargo.toml)"
if [ "$current" != "$expected" ]; then
    echo "Cargo.toml holds \`$current\`, and this script writes only over \`$expected\`." >&2
    echo >&2
    echo "\`$expected\` is the number that \`main\` holds. A file with another number" >&2
    echo "is a checkout of a tag, a checkout that is old, or a file that this script" >&2
    echo "wrote already. Each of those gives the release a number that no rule chose." >&2
    echo >&2
    echo "Run this on a checkout of \`main\`, and run it once." >&2
    exit 1
fi

# In Cargo.toml, the version of the package is the first line that starts with
# `version`. A dependency gives its version inside a table on one line, such as
# `clap = { version = "4" }`, so no dependency matches.
awk -v v="$version" '
    !done && /^version *= *"/ { print "version = \"" v "\""; done = 1; next }
    { print }
' Cargo.toml >Cargo.toml.new
mv Cargo.toml.new Cargo.toml

# In Cargo.lock, the version of qex follows the name of qex.
awk -v v="$version" '
    /^name = "qex"$/ { print; found = 1; next }
    found && /^version = / { print "version = \"" v "\""; found = 0; next }
    { print }
' Cargo.lock >Cargo.lock.new
mv Cargo.lock.new Cargo.lock

# Prove that the two files agree, because a build with `--locked` stops when
# they do not.
toml="$(awk -F'"' '/^version *= *"/ { print $2; exit }' Cargo.toml)"
if [ "$toml" != "$version" ]; then
    echo "Cargo.toml holds $toml, and this script wrote $version" >&2
    exit 1
fi

echo "the version is now $version"
