# How to make a change to qex

## The title of your pull request gives the version number

qex has no manual release step. A merge to `main` reads the commit messages
since the last tag, calculates the next version number, builds the binaries,
makes the tag and publishes the release.

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
say something that no person decided. To go to 1.0.0, put that number in
`Cargo.toml`: the number in `Cargo.toml` is a floor, and the release is never
below it.

## The version number in Cargo.toml

The release workflow writes the number, so you do not need to change it.

You still can, and there is one case where you must: **a build that you install
on a machine must not report the same number as a different build.** qex
compares the version of the CLI against the version of the coordinator that
operates, and it gives a warning when the two differ. Two different binaries
that report one number make that warning useless, and a user then meets a fault
with no cause.

If you build qex and install it while you work, move the number up first.

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
