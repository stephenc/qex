#!/usr/bin/env bash
#
# Tests that each commit message follows Conventional Commits.
#
# The version number of a release comes from these messages, so a message with
# the wrong form does more than look untidy: it makes a release with the wrong
# number, or no release at all.
#
# Usage: check-commits.sh <base sha> <head sha>

set -euo pipefail

base="${1:?give the base commit}"
head="${2:?give the head commit}"

types="build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test"
pattern="^($types)(\([a-z0-9._/-]+\))?!?: .+"

bad=0
count=0

while IFS= read -r -d '' message; do
    subject="${message%%$'\n'*}"
    count=$((count + 1))

    # A merge commit comes from GitHub, and no person writes it.
    if [[ "$subject" =~ ^Merge\  ]]; then
        continue
    fi

    if [[ ! "$subject" =~ $pattern ]]; then
        echo "this message does not follow Conventional Commits:"
        echo "    $subject"
        bad=$((bad + 1))
        continue
    fi

    if [ "${#subject}" -gt 72 ]; then
        echo "this first line is longer than 72 characters:"
        echo "    $subject"
        bad=$((bad + 1))
    fi
done < <(git log -z --no-merges --format='%B' "$base..$head")

if [ "$bad" -gt 0 ]; then
    cat <<EOF

$bad message(s) of $count have the wrong form.

A message starts with a type, and then a colon and a space:

    feat: add a command that shows the space that qex holds
    fix(top): show the queue when no coordinator operates
    feat!: rename --lock to --exclusive

The type gives the next version number:

    feat            the second number goes up   (0.5.3 -> 0.6.0)
    fix, perf       the third number goes up     (0.5.3 -> 0.5.4)
    a ! before the : while the first number is 0, the second number goes up
    everything else no release

The other types are: $(echo "$types" | tr '|' ' ').

Correct the messages with \`git rebase -i $base\`, and push again.
EOF
    exit 1
fi

echo "$count message(s) have the correct form."
