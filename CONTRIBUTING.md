# How to make a change to qex

## Work on a branch, and merge with a pull request

**Do not push to `main`.** A branch ruleset refuses it. A change goes on a
branch, and a pull request merges it:

```sh
git switch -c feat/the-thing
# ... the work ...
git push -u origin feat/the-thing
gh pr create
```

## The title of your pull request gives the version number

qex has no manual release step. A merge to `main` reads the commit messages
since the last release, calculates the next version number, makes the tag, and
then builds the binaries and publishes the release.

**No pull request changes the version number.** `Cargo.toml` on `main` holds
`0.0.0-dev` for ever. The number of a release lives on the tag, and on the one
commit that the tag names.

**A pull request goes to `main` as a squash**, so it makes ONE commit there. The
title of the pull request becomes the first line of that commit, and the body of
the pull request becomes its body.

**The title of your pull request is therefore part of the build.** It follows
[Conventional Commits](https://www.conventionalcommits.org/):

```
<type>[(scope)][!]: <what the change does>
```

| Type | The version number |
| ---- | ------------------ |
| `feat` | the second number goes up (0.5.3 → 0.6.0) |
| `fix`, `perf`, `revert` | the third number goes up (0.5.3 → 0.5.4) |
| a `!` before the `:`, or a `BREAKING CHANGE:` line | see below |
| `build`, `chore`, `ci`, `docs`, `refactor`, `style`, `test` | no release |

Examples:

```
feat: add a command that shows the space that qex holds
fix(top): show the queue when no coordinator operates
feat!: rename --lock to --exclusive
docs: correct the example in the agent page
```

CI tests the form of the title, so a title with a fault stops the merge and not
the release. A title is not a commit, so you correct it in the pull request and
you need no new commit.

**The messages of the commits ON YOUR BRANCH do not reach `main`**, because the
merge is a squash. Write them for a reader of the pull request. CI does not test
them.

### The body of the pull request is the body of the commit

The squash takes the body of the pull request as the body of the commit, so it
goes into `main` and it stays there. Write it as a commit message:

- Say what the change does and WHY. That is the text a reader of `git log`
  finds in a year, when the pull request is a page that nobody opens.
- Use Simplified Technical English, like every other word of this project.
- Leave out the state of the day: "CI is not green yet", "this waits for the
  review of Sam", "I could not test the Windows target". Those sentences are
  true for an hour and wrong for ever after.
- A `BREAKING CHANGE:` line here asks for a break, because the release reads
  this text.

Correct the body before you merge, not when you open the pull request. A pull
request is a conversation while it is open, and a commit message when it lands.

### A checklist goes in a comment, and never in the body

Work that you must complete before the merge goes in a **comment** on the pull
request, as a list of boxes. Make it the first comment, so that a reader finds
it before the discussion.

```markdown
- [ ] Rebase on `main` after #29 lands
- [ ] CI green after the rebase
- [ ] An independent review of what the rebase changed
```

CI has a check, `task-list-completed`, that reads the comments and refuses the
merge while a box is empty. The checklist is thus a gate and not a note.

Two reasons for a comment and not the body:

- The body becomes the commit message. A list of boxes in `git log` says
  nothing to a reader in a year, and every box in it is empty for ever.
- The state of the work changes while the pull request is open. A comment
  changes with it, and the commit message must not.

**While the first number is 0, a break moves the SECOND number.** A 0.x version
says that the interface can still change, so an automatic move to 1.0.0 would
say something that no person decided.

## How a release happens

The release comes from a TAG, and never from a commit on `main`.

A branch ruleset asks for a pull request on `main`, and the GitHub Actions app
cannot take a bypass on a repository that a person owns. A workflow can
therefore no longer push to `main`. It can still push a tag.

`release.yml` runs on each merge to `main`. When the commit messages since the
last release ask for one, it:

1. writes the number into `Cargo.toml` and `Cargo.lock`,
2. commits that as `chore(release): vX.Y.Z`, **beside `main` and not on it**,
3. makes the annotated tag `vX.Y.Z` on that commit,
4. pushes the tag, and nothing else.

The history thus looks like a fishbone: `main` is the spine, and each release is
a rib of one commit hanging off it.

```
main:  A --- B --- C            the spine, always 0.0.0-dev
              \     \
               b     c          a rib of one commit: chore(release)
               |     |
            v0.7.2  v0.7.3      the tag is on the rib
```

One workflow does all of it, and it then builds the four binaries from the tag
and publishes the release. **A tag is not an ancestor of `main`**, so `git
describe` finds none of them; code that looks for the last release sorts the
tags instead. The rules are in
`.github/actions/compute-version/version-rules.sh`, and
`.github/actions/compute-version/test.sh` holds them.

### A release that failed leaves its tag behind

The tag comes BEFORE the tests and the builds, because the tag is what a build
builds. A run that fails after the tag thus leaves a tag with no release beside
it.

**Use "Re-run failed jobs", and never "Re-run all jobs".**

- **"Re-run failed jobs" is correct, and it is the cheaper of the two.** GitHub
  keeps the outputs of the jobs that already succeeded, so `decide` does not run
  again and the run keeps the tag that it made.
- **"Re-run all jobs" goes green having released nothing.** `decide` runs again,
  the tag exists by then, so the rules take it as the last release, and the
  range of commits is empty.

You can also start the workflow ON THE TAG, which works from any state:

```sh
gh workflow run release.yml --ref v0.8.0
```

The ref is then a tag, so the run takes the number from it, makes no new tag,
and goes to the build.

### To go to 1.0.0, make the tag

A move to 1.0.0 is the decision of a person, so a person makes the tag:

```sh
git fetch origin
git checkout --detach origin/main
.github/scripts/set-version.sh 1.0.0
git commit -am "chore(release): v1.0.0"
git tag -a v1.0.0 -m "qex 1.0.0"
git push origin v1.0.0
```

The push of that tag starts `release.yml`, and every release after it counts up
from 1.0.0.

## The version number that your build reports

**A build that you install on a machine must not report the same number as a
different build.** qex compares the version of the CLI against the version of
the coordinator that operates, and it gives a warning when the two differ. Two
different binaries that report one number make that warning useless, and a user
then meets a fault with no cause.

You need no action for this, and you must not change `Cargo.toml`. `build.rs`
puts the commit into the version of each build:

```
0.0.0-dev+g98513e2          the commit that this build holds
0.0.0-dev+g98513e2.dirty    the same, with changes that are not committed
0.0.0-dev+unknown           a build that could not learn its commit
```

**A development build claims nothing except which commit it is.** That is the
one thing it can say honestly, and it is the thing that a fault report needs.

A release binary reports the release number, and nothing carries that number in
through the environment: the commit that the tag names holds it in `Cargo.toml`,
and `build.rs` reads it from there. A build from crates.io reports it for the
same reason.

`build.rs` reads git ONLY when `Cargo.toml` says `0.0.0-dev`, and only when the
repository that git finds is the one whose root holds the package. git walks up
the directory tree, so a copy of the source inside another repository would
otherwise report a stranger's commit; `cargo install --git` makes exactly that
shape.

A coordinator that reports a development build below the capability floor gives a
WARNING and not a refusal, so a build that you make is usable by its own CLI.
The capability test still refuses each option that such a coordinator cannot
obey, and it names the option. See `src/capabilities.rs`.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --bins
cargo test --test e2e -- --test-threads=2
```

CI runs each of these on Linux and on macOS, and it also builds for the four
release targets and tests the earliest version of Rust that `Cargo.toml` names.

**Use two test threads.** Each end-to-end test starts real processes and waits
for them. With more threads the machine becomes busy, a job starts late, and a
test reports a failure that the program does not have.

## The words

**Every word that a user reads uses Simplified Technical English (ASD-STE100).**
That covers the documentation, the code comments, the help text and the error
messages. Use short sentences, one instruction in each sentence, and the simple
words of the standard.

An error message says three things: what happened, why it matters, and what the
reader must do. A message that gives a state and no remedy is not complete.

## What a comment is for

A comment says WHY the code is as it is, and it does not repeat what the code
says. The valuable comment in this repository is the one that records a fault
that somebody met, so that a later change does not bring the fault back.

## The tests that must not go away

Some tests hold the reason that qex exists. Do not delete them:

- `watchers::tests::the_search_never_finds_itself` — the fault that started the
  project.
- The capability tests — an earlier coordinator must refuse an option that it
  cannot obey, and never accept it in silence.
- The timeout race tests — a job that succeeded must never get the state
  `timeout`.
