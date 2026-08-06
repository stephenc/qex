#!/usr/bin/env bash
#
# Puts one version number into Cargo.toml and Cargo.lock.
#
# Each job of the release workflow calls this before it builds, so that every
# binary of one release reports the same number, and so that no tag exists until
# each build is complete. A `qex` binary compares its own number against the
# number of the coordinator that operates, and it gives a warning when the two
# differ; two builds that report one number make that warning useless.
#
# Usage: set-version.sh 0.6.0

set -euo pipefail

version="${1:?give the version, such as 0.6.0}"

cd "$(dirname "$0")/../.."

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
