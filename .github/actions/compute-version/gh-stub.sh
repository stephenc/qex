#!/usr/bin/env bash
#
# A `gh` that answers from a local git repository, for `test.sh`.
#
# `compute-version.sh` reads the GitHub API, so a test of it needs an API. This
# file is a MODEL of the four endpoints that the driver calls, built from the
# repository in `QEX_TEST_REPO`. The test copies it to a directory called `gh`
# on the PATH, so the driver runs with no change of its own.
#
# The model gives the SHAPES that the driver reads, and the driver applies its
# own `jq` expressions to them, so a change to one of those expressions is
# tested here. The shapes come from the API of GitHub, and a change there is
# what this file cannot see; keep the JSON below beside the documentation.
#
# `QEX_TEST_GH_FAIL=1` makes every call fail, which is how the test proves that
# a call that fails stops the driver and does not go out as an empty answer.

set -euo pipefail

if [ "${QEX_TEST_GH_FAIL:-}" = "1" ]; then
    echo "gh: the API said no (this is the stub, and it fails on purpose)" >&2
    exit 1
fi

repo="${QEX_TEST_REPO:?the stub needs QEX_TEST_REPO}"

[ "${1:-}" = "api" ] || {
    echo "the stub knows \`gh api\` only, and it got \`${1:-}\`" >&2
    exit 2
}
endpoint="$2"
shift 2

jq_expr=""
while [ $# -gt 0 ]; do
    case "$1" in
        --jq)
            jq_expr="$2"
            shift 2
            ;;
        *) shift ;;
    esac
done

# The commits of one range, in the shape that `compare` gives them. `jq -Rs .`
# writes the JSON of one message, because a message holds newlines and quotation
# marks of its own.
commits() {
    local first=1 message
    printf '['
    while IFS= read -r -d '' message; do
        [ "$first" -eq 1 ] || printf ','
        first=0
        printf '{"commit":{"message":%s}}' "$(printf '%s' "$message" | jq -Rs .)"
    done < <(git -C "$repo" log -z --format='%B' "$1")
    printf ']'
}

answer() {
    case "$endpoint" in
        */git/matching-refs/tags/v)
            local first=1 name object type
            printf '['
            while read -r name object type; do
                [ "$first" -eq 1 ] || printf ','
                first=0
                printf '{"ref":"%s","object":{"sha":"%s","type":"%s"}}' \
                    "$name" "$object" "$type"
            done < <(git -C "$repo" for-each-ref \
                --format='%(refname) %(objectname) %(objecttype)' 'refs/tags/*')
            printf ']'
            ;;

        # An ANNOTATED tag points at a tag object. This endpoint follows it
        # through to the commit, and the driver must call it.
        */git/tags/*)
            printf '{"object":{"sha":"%s"}}' \
                "$(git -C "$repo" rev-parse "${endpoint##*/}^{commit}")"
            ;;

        */compare/*)
            local spec base head
            spec="${endpoint##*/compare/}"
            base="${spec%%...*}"
            head="${spec##*...}"
            printf '{"commits":'
            commits "$base..$head"
            printf '}'
            ;;

        # The `?` is escaped, because a `?` in a pattern of `case` matches one
        # character of anything.
        */commits\?sha=*)
            commits "${endpoint##*sha=}"
            ;;

        */commits/*)
            local sha type parent
            sha="${endpoint##*/}"

            # THIS ENDPOINT TAKES A COMMIT, AND IT REFUSES A TAG OBJECT.
            #
            # The real API answers `422 No commit found for SHA` for the sha of
            # an annotated tag, so the driver MUST call `git/tags/` first and
            # follow the tag object through to its commit.
            #
            # `git rev-parse <sha>^1` does NOT refuse it: git peels a tag object
            # for you, in silence. A stub built on `rev-parse` alone therefore
            # answers a tag object correctly, the tests then pass with the
            # dereference REMOVED from the driver, and the one behaviour that
            # this file exists to protect is not protected at all.
            type="$(git -C "$repo" cat-file -t "$sha" 2>/dev/null || true)"
            if [ "$type" != "commit" ]; then
                echo "gh: HTTP 422: No commit found for SHA: $sha" >&2
                exit 1
            fi

            parent="$(git -C "$repo" rev-parse -q --verify "${sha}^1" || true)"
            if [ -n "$parent" ]; then
                printf '{"parents":[{"sha":"%s"}]}' "$parent"
            else
                printf '{"parents":[]}'
            fi
            ;;

        *)
            echo "the stub does not know the endpoint \`$endpoint\`" >&2
            exit 2
            ;;
    esac
}

if [ -n "$jq_expr" ]; then
    answer | jq -r "$jq_expr"
else
    answer
fi
