#!/usr/bin/env bash
#
# Tests the rules that give the version number, and the driver that feeds them.
#
# `version-rules.sh` decides the number of every release. This file holds it to
# the rules that are easy to break in silence:
#
#   * A break before 1.0.0 moves the SECOND number. `feat!` on `v0.10.0` gives
#     `0.11.0`, and never `1.0.0`.
#   * The tags sort by VERSION and never as text. As text `v0.9.0` comes after
#     `v0.10.0`, and the next release then goes backwards.
#   * A tag hangs OFF `main` and is never ON it (fishbone tagging), so the last
#     release is the highest tag and NOT the nearest tag in the history. A test
#     that only tags commits on the trunk passes while the real thing is broken.
#   * A call to the API that fails must STOP the driver. A driver that writes an
#     empty answer makes a run that goes green and releases nothing.
#
# It also tests the guard in `set-version.sh`, which refuses to write over any
# number except the one that `main` holds.
#
# It needs bash, git and jq. It needs no network and no cargo, so it runs in a
# few seconds in a fast job of CI.
#
# Usage: test.sh

set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
rules="$here/version-rules.sh"
driver="$here/compute-version.sh"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The stub of `gh`, on the PATH under its own name.
mkdir -p "$work/bin"
cp "$here/gh-stub.sh" "$work/bin/gh"
chmod +x "$work/bin/gh"
export PATH="$work/bin:$PATH"

pass=0
fail=0

# Reads one value from the output of the rules.
value_of() {
    sed -n "s/^$1=//p" <<<"$2"
}

# `check <name> <expected> <got>`
check() {
    if [ "$2" = "$3" ]; then
        pass=$((pass + 1))
    else
        fail=$((fail + 1))
        echo "FAIL: $1"
        echo "      expected: \`$2\`"
        echo "      got:      \`$3\`"
    fi
}

# `rules_next <name> <previous tag> <field> <expected> <message>...`
#
# Gives the messages to the rules with a zero byte after each one, which is what
# `git log -z` writes.
rules_next() {
    local name="$1" previous="$2" field="$3" expected="$4"
    shift 4
    local out
    out="$(printf '%s\0' "$@" | "$rules" next "$previous" "")"
    check "$name" "$expected" "$(value_of "$field" "$out")"
}

echo "== The rules =="

# The table of https://github.com/stephenc/qex/issues/30.
rules_next "feat! on v0.10.0 gives 0.11.0 and never 1.0.0" \
    v0.10.0 version 0.11.0 "feat!: a break"
rules_next "feat! on v0.9.0 gives 0.10.0" \
    v0.9.0 version 0.10.0 "feat!: a break"
rules_next "feat! on v1.2.3 gives 2.0.0" \
    v1.2.3 version 2.0.0 "feat!: a break"
rules_next "feat on v0.10.0 gives 0.11.0" \
    v0.10.0 version 0.11.0 "feat: a thing"
rules_next "fix on v0.10.0 gives 0.10.1" \
    v0.10.0 version 0.10.1 "fix: a thing"
rules_next "perf on v0.10.0 gives 0.10.1" \
    v0.10.0 version 0.10.1 "perf: a thing"
rules_next "revert on v0.10.0 gives 0.10.1" \
    v0.10.0 version 0.10.1 "revert: a thing"
rules_next "a scope does not change the type" \
    v0.10.0 version 0.11.0 "feat(top): a thing"
rules_next "a break with a scope moves the second number" \
    v0.10.0 version 0.11.0 "feat(top)!: a break"
rules_next "the bump of a release is named" \
    v0.10.0 bump patch "fix: a thing"
rules_next "the tag to make is the version with a v" \
    v0.10.0 tag v0.10.1 "fix: a thing"

# `docs:` asks for no release, so `version` and `tag` and `bump` are empty.
rules_next "docs gives no release" v0.10.0 version "" "docs: a word"
rules_next "docs gives no tag" v0.10.0 tag "" "docs: a word"
rules_next "docs gives no bump" v0.10.0 bump "" "docs: a word"
rules_next "chore gives no release" v0.10.0 version "" "chore: a thing"
rules_next "the release commit itself asks for no release" \
    v0.10.0 version "" "chore(release): v0.10.0"

# A break in the BODY, in both of the forms that Conventional Commits gives.
rules_next "BREAKING CHANGE in the body is a break" \
    v0.10.0 version 0.11.0 "feat: a thing

BREAKING CHANGE: the option changed its name"
rules_next "BREAKING-CHANGE in the body is a break" \
    v0.10.0 version 0.11.0 "feat: a thing

BREAKING-CHANGE: the option changed its name"
rules_next "a break in the body of a fix is still a break" \
    v0.10.0 version 0.11.0 "fix: a thing

BREAKING-CHANGE: the option changed its name"
rules_next "the words BREAKING CHANGE inside a sentence are not a break" \
    v0.10.0 version 0.10.1 "fix: a thing

This is not a BREAKING CHANGE: it corrects a fault."

# A CAPITAL LETTER IN THE TYPE GIVES AN ANSWER THAT DEPENDS ON THE `!`.
#
# The test for the type reads the type and compares it in small letters, so
# `Feat:` asks for no release. The test for a break does not read the type at
# all, so `Feat!:` gives a break. `check-title.sh` refuses both, and its message
# says which of the two happens. These two files hold one rule between them.
rules_next "a capital letter in the type gives no release" \
    v0.10.0 version "" "Feat: a capital letter"
rules_next "a capital letter with a break still gives a break" \
    v0.10.0 version 0.11.0 "Feat!: a capital letter"
"$root/.github/scripts/check-title.sh" "Feat: a capital letter" >/dev/null 2>&1
check "check-title.sh refuses a capital type" "1" "$?"
"$root/.github/scripts/check-title.sh" "Feat!: a capital letter" >/dev/null 2>&1
check "check-title.sh refuses a capital type with a break" "1" "$?"

# The highest bump of the range wins, whatever the order of the commits.
rules_next "a break after a fix still gives a break" \
    v0.10.0 version 0.11.0 "fix: a thing" "feat!: a break"
rules_next "a fix after a break still gives a break" \
    v0.10.0 version 0.11.0 "feat!: a break" "fix: a thing"
rules_next "a feat after a fix gives a feat" \
    v0.10.0 version 0.11.0 "fix: a thing" "feat: a thing"

# With no release at all, start from 0.0.0.
rules_next "the first release of a feat is 0.1.0" "" version 0.1.0 "feat: the first"
rules_next "the first release of a fix is 0.0.1" "" version 0.0.1 "fix: the first"

echo "== A commit that carries a tag =="

out="$("$rules" next v0.10.0 v0.11.0 </dev/null)"
check "the version of a tagged commit is the tag" "0.11.0" "$(value_of version "$out")"
check "a tagged commit is a release" "true" "$(value_of is-release "$out")"
check "a tagged commit needs no new tag" "" "$(value_of tag "$out")"
check "a tagged commit names the release before it" "v0.10.0" "$(value_of previous "$out")"

# A tag of another form names something that is not a release, and the rules
# must refuse to take a number from it.
for bad in v1.0 v1.0.0-rc1 latest; do
    "$rules" next "" "$bad" </dev/null >/dev/null 2>&1
    check "\`$bad\` is not a release tag" "2" "$?"
done

echo "== The order of the tags =="

# THE ORDER MUST BE A VERSION ORDER. As text, `v0.9.0` comes after `v0.10.0`.
out="$(printf 'v0.9.0\nv0.10.0\nv0.8.1\n' | "$rules" previous "")"
check "the last release is the highest version, and not the highest text" \
    "v0.10.0" "$(value_of previous "$out")"
out="$(printf 'v0.10.0\nv0.9.0\nv0.8.0\n' | "$rules" previous v0.9.0)"
check "the release before an earlier tag is the one below it, and not the highest" \
    "v0.8.0" "$(value_of previous "$out")"
out="$(printf 'v0.10.0\n' | "$rules" previous "")"
check "one tag is its own answer" "v0.10.0" "$(value_of previous "$out")"
out="$(printf '' | "$rules" previous "")"
check "no tag gives no release before" "" "$(value_of previous "$out")"
out="$(printf 'v1.0.0-rc1\nv1.0\nv0.9.0\n' | "$rules" previous "")"
check "a tag that is not a release number is not a release" \
    "v0.9.0" "$(value_of previous "$out")"

echo "== The driver, against a repository with fishbone tags =="

# The tag hangs OFF the trunk and is never ON it. `git describe` finds only the
# tags that HEAD reaches, so it finds NONE of these. A test that tagged the
# trunk would pass while the real thing is broken.
#
# The tags are ANNOTATED, like the tags of the repository. An annotated tag
# points at a tag object and not at a commit, so the driver must follow it
# through before it asks for the parents of the commit.
repo="$work/repo"
mkdir -p "$repo"
export QEX_TEST_REPO="$repo"
(
    cd "$repo"
    git init -q -b main .
    git config user.email t@example.com
    git config user.name Test
    git config commit.gpgsign false
    git config tag.gpgsign false

    git commit -q --allow-empty -m "feat: the first thing"

    # A rib: the release commit hangs off the trunk, and the tag goes on the
    # rib. `main` never moves.
    rib() {
        local number="$1"
        git checkout -q --detach HEAD
        echo "$number" >version
        git add version
        git commit -q -m "chore(release): v$number"
        git tag -a "v$number" -m "qex $number"
        git checkout -q main
    }

    rib 0.9.0
    git commit -q --allow-empty -m "fix: a fault"
    rib 0.9.1
    git commit -q --allow-empty -m "feat: another thing"
    rib 0.10.0
    git commit -q --allow-empty -m "docs: a word"
    git commit -q --allow-empty -m "fix: one more fault"
) >/dev/null

main_sha="$(git -C "$repo" rev-parse main)"
out="$("$driver" owner/name "$main_sha" "")"
check "the fishbone tag is found from main" "v0.10.0" "$(value_of previous "$out")"
check "the number comes from the commits after that tag" "0.10.1" "$(value_of version "$out")"
check "the tag to make is v0.10.1" "v0.10.1" "$(value_of tag "$out")"
check "main is not a release" "false" "$(value_of is-release "$out")"
check "the bump is a patch" "patch" "$(value_of bump "$out")"

# `git describe` is the fault that this shape exists to stop. Prove that it
# cannot see the tags, so that a later reader does not "simplify" the code back
# to it.
describe="$(git -C "$repo" describe --tags --abbrev=0 2>&1)"
case "$describe" in
    v*) check "git describe must NOT find a fishbone tag" "no tag" "$describe" ;;
    *) pass=$((pass + 1)) ;;
esac

tag_sha="$(git -C "$repo" rev-parse 'v0.10.0^{commit}')"
out="$("$driver" owner/name "$tag_sha" v0.10.0)"
check "a tagged commit gives the number of its tag" "0.10.0" "$(value_of version "$out")"
check "a tagged commit is a release" "true" "$(value_of is-release "$out")"
check "it names the release before it" "v0.9.1" "$(value_of previous "$out")"
check "it makes no new tag" "" "$(value_of tag "$out")"

# A call to the API that fails MUST stop the driver.
#
# An earlier action piped this driver into `tee`, so the exit value of the step
# was the exit value of `tee`, which is always zero. A call that failed then
# gave an empty answer, the step reported success, and the run went green having
# released nothing.
out="$(QEX_TEST_GH_FAIL=1 "$driver" owner/name "$main_sha" "" 2>/dev/null)"
check "a call to the API that fails stops the driver" "1" "$?"
check "a call to the API that fails writes no answer" "" "$out"

echo "== The driver, before the first release =="

first="$work/first"
mkdir -p "$first"
(
    cd "$first"
    git init -q -b main .
    git config user.email t@example.com
    git config user.name Test
    git commit -q --allow-empty -m "feat: the first thing"
    git commit -q --allow-empty -m "docs: a word"
) >/dev/null

QEX_TEST_REPO="$first" out="$("$driver" owner/name "$(git -C "$first" rev-parse main)" "")"
check "the first release starts from 0.0.0" "0.1.0" "$(value_of version "$out")"
check "the first release has no release before it" "" "$(value_of previous "$out")"

echo "== The guard of set-version.sh =="

# `set-version.sh` writes over the number that `main` holds, and over no other
# number. The test needs a tree of its own, because the script goes to the top
# of the repository that holds it.
new_tree() {
    local tree="$1" number="$2"
    rm -rf "$tree"
    mkdir -p "$tree/.github/scripts"
    cp "$root/.github/scripts/set-version.sh" "$tree/.github/scripts/"
    printf '[package]\nname = "qex"\nversion = "%s"\nedition = "2021"\n' "$number" \
        >"$tree/Cargo.toml"
    printf '[[package]]\nname = "qex"\nversion = "%s"\n' "$number" >"$tree/Cargo.lock"
}

tree="$work/tree"
new_tree "$tree" "0.0.0-dev"
"$tree/.github/scripts/set-version.sh" 0.8.0 >/dev/null 2>&1
check "it writes over the number that main holds" "0" "$?"
check "Cargo.toml holds the new number" "0.8.0" \
    "$(awk -F'"' '/^version *= *"/ { print $2; exit }' "$tree/Cargo.toml")"
check "Cargo.lock holds the new number" "0.8.0" \
    "$(awk -F'"' '/^version *= *"/ { print $2; exit }' "$tree/Cargo.lock")"

# A second run in one job. The number is 0.8.0 now, so it must stop.
message="$("$tree/.github/scripts/set-version.sh" 0.9.0 2>&1 >/dev/null)"
check "it refuses a second run" "1" "$?"
check "Cargo.toml did not change" "0.8.0" \
    "$(awk -F'"' '/^version *= *"/ { print $2; exit }' "$tree/Cargo.toml")"
case "$message" in
    *'0.8.0'*) pass=$((pass + 1)) ;;
    *)
        fail=$((fail + 1))
        echo "FAIL: the message must name what it found"
        echo "      got: $message"
        ;;
esac
case "$message" in
    *'0.0.0-dev'*) pass=$((pass + 1)) ;;
    *)
        fail=$((fail + 1))
        echo "FAIL: the message must name what it expected"
        echo "      got: $message"
        ;;
esac

# THE COMPARISON IS AGAINST THE WHOLE STRING. A number that starts with the same
# text is a different state, and it must not pass.
for number in 0.0.0-dev2 0.0.0 0.7.2 dev "" 0.0.0-DEV; do
    new_tree "$tree" "$number"
    "$tree/.github/scripts/set-version.sh" 0.8.0 >/dev/null 2>&1
    check "it refuses to write over \`$number\`" "1" "$?"
    check "\`$number\` did not change" "$number" \
        "$(awk -F'"' '/^version *= *"/ { print $2; exit }' "$tree/Cargo.toml")"
done

echo
if [ "$fail" -eq 0 ]; then
    echo "$pass tests passed."
    exit 0
fi
echo "$fail of $((pass + fail)) tests failed."
exit 1
