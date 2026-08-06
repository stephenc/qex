# How to make a change to qex

## The commit message gives the version number

qex has no manual release step. A merge to `main` reads the commit messages
since the last tag, calculates the next version number, builds the binaries,
makes the tag and publishes the release.

**The first line of your commit message is therefore part of the build.** It
follows [Conventional Commits](https://www.conventionalcommits.org/):

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

CI tests the form of each message in a pull request, so a message with a fault
stops the merge and not the release.

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
