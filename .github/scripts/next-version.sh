#!/usr/bin/env bash
#
# Gives the next version number, from the commit messages since the last tag.
#
# The rules are the Conventional Commits rules:
#
#   fix:            the third number goes up      (0.5.3 -> 0.5.4)
#   feat:           the second number goes up     (0.5.3 -> 0.6.0)
#   a break         see below
#   everything else no release
#
# A break is a `!` before the `:` in the first line, or a `BREAKING CHANGE:`
# line in the body.
#
# WHILE THE FIRST NUMBER IS 0, A BREAK MOVES THE SECOND NUMBER, NOT THE FIRST.
# A 0.x version says that the interface can still change, so an automatic move
# to 1.0.0 would say something that the author did not intend. The move to 1.0.0
# is a decision of a person: put the number in Cargo.toml, because this script
# never gives a number below the number in Cargo.toml.
#
# The number in Cargo.toml is a floor. A person who puts 0.7.0 in Cargo.toml
# gets 0.7.0, even when the commits since the last tag ask for 0.6.1 only. This
# keeps the local rule (bump the version with each change, so two builds never
# report the same number) and the automatic rule in agreement.
#
# Writes to stdout:
#   version=<the number>      the version to release
#   bump=<major|minor|patch|none>
#   previous=<the last tag, or nothing>
#
# An exit of 0 with `bump=none` means that no commit asks for a release.

set -euo pipefail

cd "$(dirname "$0")/../.."

# The version in Cargo.toml, which is the floor.
floor="$(awk -F'"' '/^version *= *"/ { print $2; exit }' Cargo.toml)"
if [ -z "$floor" ]; then
    echo "no version in Cargo.toml" >&2
    exit 1
fi

previous="$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)"

if [ -z "$previous" ]; then
    # The first release. Take the number in Cargo.toml, and read no commits: the
    # history before the first tag says nothing about a step from a version that
    # does not exist.
    echo "version=$floor"
    echo "bump=initial"
    echo "previous="
    exit 0
fi

base="${previous#v}"
IFS='.' read -r major minor patch <<<"$base"
major="${major:-0}"
minor="${minor:-0}"
patch="${patch:-0}"

# Read each commit message since the tag. `-z` puts a zero byte between the
# messages, because a message holds newlines of its own. Without `-z`, git puts
# a newline between the messages as well, and then every message except the
# first starts with an empty line.
bump="none"
while IFS= read -r -d '' message; do
    subject="${message%%$'\n'*}"

    # A break: `feat!:` or `feat(scope)!:` in the first line, or a
    # `BREAKING CHANGE:` line in the body.
    if [[ "$subject" =~ ^[a-zA-Z]+(\([^\)]*\))?!: ]] \
        || grep -qE '^BREAKING[ -]CHANGE:' <<<"$message"; then
        bump="major"
        break
    fi

    if [[ ! "$subject" =~ ^([a-zA-Z]+)(\([^\)]*\))?!?:\  ]]; then
        continue
    fi
    type="${BASH_REMATCH[1]}"

    case "$type" in
        feat)
            bump="minor"
            ;;
        fix | perf | revert)
            if [ "$bump" = "none" ]; then bump="patch"; fi
            ;;
        *) ;;
    esac
done < <(git log -z --format='%B' "$previous..HEAD")

case "$bump" in
    major)
        # See the note at the top: a 0.x version moves the second number.
        if [ "$major" -eq 0 ]; then
            minor=$((minor + 1))
            patch=0
        else
            major=$((major + 1))
            minor=0
            patch=0
        fi
        ;;
    minor)
        minor=$((minor + 1))
        patch=0
        ;;
    patch)
        patch=$((patch + 1))
        ;;
    none)
        echo "version=$base"
        echo "bump=none"
        echo "previous=$previous"
        exit 0
        ;;
esac

version="$major.$minor.$patch"

# The number in Cargo.toml is a floor. Take whichever is higher.
higher="$(printf '%s\n%s\n' "$version" "$floor" | sort -V | tail -1)"
if [ "$higher" != "$version" ]; then
    version="$higher"
fi

# A tag that exists already means that something went wrong: two runs of the
# workflow on one commit, or a hand-made tag. Stop, and say so.
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
    echo "the tag v$version exists already" >&2
    exit 1
fi

echo "version=$version"
echo "bump=$bump"
echo "previous=$previous"
