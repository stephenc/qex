#!/usr/bin/env bash
#
# Writes the notes of one release, from the commit messages since the last tag.
#
# Usage: release-notes.sh <version> [<previous tag>]

set -euo pipefail

version="${1:?give the version}"
previous="${2:-}"
repository="${GITHUB_REPOSITORY:-OWNER/qex}"

cd "$(dirname "$0")/../.."

if [ -n "$previous" ]; then
    range="$previous..HEAD"
else
    range="HEAD"
fi

# Gives the lines of one group. A group with no line writes nothing at all, so
# no release holds an empty heading.
group() {
    local title="$1" pattern="$2" lines
    lines="$(git log --no-merges --format='%h %s' "$range" |
        while read -r hash subject; do
            [[ "$subject" =~ $pattern ]] || continue
            # Remove the type and the scope, and keep the words of the change.
            echo "- ${subject#*: } (\`$hash\`)"
        done)"
    [ -n "$lines" ] || return 0
    printf '### %s\n\n%s\n\n' "$title" "$lines"
}

{
    group "Changes that break the earlier behaviour" '^[a-zA-Z]+(\([^)]*\))?!:'
    group "New" '^feat(\([^)]*\))?:'
    group "Corrected" '^(fix|perf|revert)(\([^)]*\))?:'

    cat <<EOF
### Install

Take the file for your machine, and put it where your shell finds it:

\`\`\`sh
curl -fsSL "https://github.com/$repository/releases/download/v$version/qex-\$(uname -s)-\$(uname -m).tar.gz" | tar xz
install -m 755 qex ~/.local/bin/qex
qex help agents
\`\`\`

Each file has a \`.sha256\` file beside it. The Linux binaries hold no dynamic
library, so they run on any Linux of that architecture.
EOF

    if [ -n "$previous" ]; then
        printf '\nEach change: https://github.com/%s/compare/%s...v%s\n' \
            "$repository" "$previous" "$version"
    fi
}
