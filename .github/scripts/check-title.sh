#!/usr/bin/env bash
#
# Tests that the title of a pull request follows Conventional Commits.
#
# THE TITLE IS THE COMMIT MESSAGE. This repository accepts a squash only, and
# the squash takes the title of the pull request as the subject of the commit
# and the body of the pull request as the body. One commit thus reaches `main`
# for each pull request, and the title is its first line.
#
# The version number of a release comes from the messages on `main`, so a title
# with the wrong form makes a release with the wrong number, or no release at
# all.
#
# An earlier version of this test read the commits of the branch instead. Those
# messages never reach `main`, so the one message that decides the release was
# the one message that nothing tested.
#
# The body of the pull request reaches `main` as well, so a `BREAKING CHANGE:`
# line in the body still asks for a break. `next-version.sh` reads it.
#
# Usage: check-title.sh <title>
#
# Give the title as an argument, and never inside the text of a command: a title
# is text that a person outside this project can write.

set -euo pipefail

title="${1-}"

if [ -z "$title" ]; then
    echo "give the title of the pull request" >&2
    exit 2
fi

types="build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test"
pattern="^($types)(\([a-z0-9._/-]+\))?!?: .+"

bad=0

if [[ ! "$title" =~ $pattern ]]; then
    echo "the title does not follow Conventional Commits:"
    echo "    $title"
    bad=1
fi

# 72 characters keeps the subject readable in `git log` and in the list of
# releases. GitHub accepts a longer title, so this test is the only limit.
if [ "${#title}" -gt 72 ]; then
    echo "the title is ${#title} characters, and the limit is 72:"
    echo "    $title"
    bad=1
fi

# A subject is a line, not a sentence, so it takes no full stop at the end.
if [[ "$title" =~ \.$ ]]; then
    echo "the title ends with a full stop, and a subject takes none:"
    echo "    $title"
    bad=1
fi

if [ "$bad" -eq 0 ]; then
    echo "the title has the correct form."
    exit 0
fi

cat <<EOF

The title of the pull request becomes the commit message on \`main\`, so it
gives the next version number.

A title starts with a type, and then a colon and a space:

    feat: add a command that shows the space that qex holds
    fix(top): show the queue when no coordinator operates
    feat!: rename --lock to --exclusive

The type gives the next version number:

    feat            the second number goes up   (0.5.3 -> 0.6.0)
    fix, perf       the third number goes up    (0.5.3 -> 0.5.4)
    feat!           a break. WHILE THE FIRST NUMBER IS 0, a break moves the
                    SECOND number (0.5.3 -> 0.6.0). It does not go to 1.0.0.
    everything else no release

A move to 1.0.0 is the decision of a person: put that number in Cargo.toml.

The other types are: $(echo "$types" | tr '|' ' ').

Change the title of the pull request. You need no new commit.
EOF
exit 1
