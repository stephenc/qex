#!/usr/bin/env bash
#
# Tests what `build.rs` makes a binary report.
#
# `build.rs` gives a build its version, and a wrong answer there is invisible
# until somebody reads a fault report. Its rules exist because GIT ANSWERS FOR A
# REPOSITORY THAT IS NOT OURS: git walks up the directory tree, so a package
# that is unpacked inside somebody else's repository takes the commit of that
# repository. Read `build.rs` for the cases.
#
# Nothing gives the version to a build through the environment. A release states
# its number in its own Cargo.toml, and these tests hold that.
#
# Each test builds a copy of this package in a directory of its own and reads
# `qex --version`, because that is the only way to see what a build script did.
#
# It needs cargo, so it does not run in the fast job of CI.
#
# Usage: build-version-test.sh

set -uo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# One target directory for every build, so the dependencies compile once.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}/build-version-test"
binary="$CARGO_TARGET_DIR/debug/qex"

pass=0
fail=0

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

# `copy_package <directory> <version>`
#
# The copy holds no `.git` of its own, so each test decides which repository the
# package sits in. `set-version.sh` writes the number, so its guard covers this
# path as well.
copy_package() {
    mkdir -p "$1"
    cp -R "$root/Cargo.toml" "$root/Cargo.lock" "$root/build.rs" \
        "$root/src" "$root/tests" "$1/"
    if [ "$2" != "0.0.0-dev" ]; then
        mkdir -p "$1/.github/scripts"
        cp "$root/.github/scripts/set-version.sh" "$1/.github/scripts/"
        (cd "$1" && ./.github/scripts/set-version.sh "$2" >/dev/null) || return 1
        rm -rf "$1/.github"
    fi
}

# `reports <directory>` — the version that a build in that directory writes.
reports() {
    if ! (cd "$1" && cargo build -q 2>"$work/build.log"); then
        echo "THE BUILD FAILED"
        sed -n '1,5p' "$work/build.log" >&2
        return 0
    fi
    "$binary" --version | awk '{ print $2 }'
}

# A repository that belongs to somebody else, with a commit and a change that
# nobody committed, so that git has every answer to give.
stranger="$work/stranger"
mkdir -p "$stranger"
(
    cd "$stranger"
    git init -q -b main .
    git config user.email t@example.com
    git config user.name Test
    git commit -q --allow-empty -m "the work of somebody else"
    echo "not committed" >a-change
) >/dev/null

echo "== A package inside a repository that is not ours =="

# The shape that a registry gives: `~/.cargo/registry/src/<index>/qex-0.8.0/`,
# under a `$HOME` that somebody made a repository of dotfiles.
registry="$stranger/.cargo/registry/src/index.crates.io-abc/qex-0.8.0"
copy_package "$registry" "0.8.0"
check "a package with a real number reports that number, and never a hash" \
    "0.8.0" "$(reports "$registry")"

# `cargo install --git` unpacks a source tree that still says `0.0.0-dev` into a
# checkout that is ITSELF a git repository. The test of Cargo.toml does not
# cover this one, and the test of the root of the repository does.
foreign="$stranger/checkout/qex"
copy_package "$foreign" "0.0.0-dev"
check "a development package in a repository that is not ours says unknown" \
    "0.0.0-dev+unknown" "$(reports "$foreign")"

echo "== A checkout of qex =="

own="$work/own"
copy_package "$own" "0.0.0-dev"
(
    cd "$own"
    git init -q -b main .
    git config user.email t@example.com
    git config user.name Test
    git add -A
    git commit -q -m "feat: the package"
) >/dev/null
commit="$(git -C "$own" rev-parse --short=7 HEAD)"

check "a checkout of qex reports its own commit" \
    "0.0.0-dev+g$commit" "$(reports "$own")"

# The change goes into a file that `build.rs` names in `rerun-if-changed`, and
# it must still compile. A change anywhere else would leave cargo with no reason
# to run the build script again, and the binary would keep the string that it
# had — which is the correct answer, and not the one that this test measures.
echo "// a change that nobody committed" >>"$own/src/main.rs"
check "a tree with changes that nobody committed is marked" \
    "0.0.0-dev+g$commit.dirty" "$(reports "$own")"
git -C "$own" checkout -q -- src/main.rs

# THE SHAPE OF A RELEASE BUILD, and the one that no other test here reaches.
#
# A release checks out the tag. That tree HAS a repository of its own, its root
# IS the package, and its Cargo.toml holds the real number, because the commit
# that the tag names wrote it. The answer must come from Cargo.toml, and git
# must not be consulted at all.
#
# Every other test here is either "a real number with no repository of ours" or
# "a development number with a repository of ours". Neither separates the two
# rules, because the test of the root of the repository alone gives the right
# answer for both. Without this test, a change that reads git BEFORE Cargo.toml
# passes every other test here, and each release binary then reports a commit
# in place of its version. The release workflow tests the binary as well, and
# it does so after the tag is public.
release="$work/release"
copy_package "$release" "0.8.0"
(
    cd "$release"
    git init -q -b main .
    git config user.email t@example.com
    git config user.name Test
    git add -A
    git commit -q -m "chore(release): v0.8.0"
) >/dev/null
check "a checkout of a tag reports the number in Cargo.toml, and never a hash" \
    "0.8.0" "$(reports "$release")"

echo "== A git worktree =="

# `--show-toplevel` gives the root of the WORKTREE, and our worktrees hold the
# package at their root, so a worktree reports its own commit and not `unknown`.
git -C "$own" worktree add -q -b other "$work/worktree" >/dev/null 2>&1
(cd "$work/worktree" && git commit -q --allow-empty -m "fix: another commit") >/dev/null
worktree_commit="$(git -C "$work/worktree" rev-parse --short=7 HEAD)"
check "a worktree reports its own commit, and never unknown" \
    "0.0.0-dev+g$worktree_commit" "$(reports "$work/worktree")"

echo "== No git at all =="

# The crates.io tarball: no `.git`, and the real number in Cargo.toml.
tarball="$work/tarball"
copy_package "$tarball" "0.8.0"
check "a tarball with no git reports the number in Cargo.toml" \
    "0.8.0" "$(reports "$tarball")"

# A development tree with no repository anywhere above it. `unknown` is the
# same answer as a repository that is not ours, because a reader of a fault
# report does the same thing for both.
bare="$work/bare"
copy_package "$bare" "0.0.0-dev"
check "a development tree with no git says unknown" \
    "0.0.0-dev+unknown" "$(reports "$bare")"

git -C "$own" worktree remove --force "$work/worktree" >/dev/null 2>&1

echo
if [ "$fail" -eq 0 ]; then
    echo "$pass tests passed."
    exit 0
fi
echo "$fail of $((pass + fail)) tests failed."
exit 1
