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
dir=\$(mktemp -d) &&
archive="qex-\$(uname -s)-\$(uname -m).tar.gz" &&
curl -fsSL -o "\$dir/\$archive" \\
  "https://github.com/$repository/releases/download/v$version/\$archive" &&
curl -fsSL -o "\$dir/\$archive.sha256" \\
  "https://github.com/$repository/releases/download/v$version/\$archive.sha256" &&
if command -v sha256sum >/dev/null
then (cd "\$dir" && sha256sum -c "\$archive.sha256")
else (cd "\$dir" && shasum -a 256 -c "\$archive.sha256")
fi &&
tar -xzf "\$dir/\$archive" -C "\$dir" qex &&
mkdir -p ~/.local/bin &&
install -m 755 "\$dir/qex" ~/.local/bin/qex
[ -n "\$dir" ] && rm -rf "\$dir"
command -v qex
\`\`\`

These commands write into a temporary directory, and they do not write into
the directory that you are in. The two \`curl\` lines take the archive and the
\`.sha256\` file that the release holds beside it. The check compares the two.
The \`&&\` after each write means a line that fails stops the writes that follow.
The \`rm\` line still runs, and it removes the temporary directory.

\`mkdir\` makes \`~/.local/bin\` when that directory is not there. \`install\` puts
\`qex\` in that directory, and it REPLACES a \`qex\` that is already there.

**\`install\` does not put \`~/.local/bin\` on your path.** \`command -v qex\` must
show a file in \`~/.local/bin\`. If it shows nothing, add \`~/.local/bin\` to the
path of this shell. If it shows a different file, that file is the one the
shell will run.

Then \`qex help agents\` is the page for agents. The first qex command starts
the coordinator. The Linux binaries hold no dynamic library, so they run on
any Linux of that architecture.
EOF

    if [ -n "$previous" ]; then
        printf '\nEach change: https://github.com/%s/compare/%s...v%s\n' \
            "$repository" "$previous" "$version"
    fi
}
