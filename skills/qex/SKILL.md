---
name: qex
description: Run a long local task (a build, a test suite, a training run, a data job) through the qex queue instead of a background command and a polling loop. Use this when a command takes minutes or more, when the machine is shared with other agents or people, when you must wait for work that you started earlier, or when you need the exit code, the output or the measured memory of a task that has already stopped.
---

# qex — run long tasks and wait for them correctly

qex is a job queue for one machine. It holds a task, starts it when the machine
has the cores and the memory, records the result on the disk, and gives you one
command that waits for it.

## Before anything else

```sh
command -v qex || echo "qex is not installed"
```

Install it with `cargo install qex`, or take a binary from
https://github.com/stephenc/qex/releases/latest . qex needs Linux or macOS. The
first command starts the coordinator; there is no service to configure.

`qex help agents` is a complete page inside the binary. Read it when this file
does not answer your question.

## The rule

**Never write a loop that waits for evidence of a task.** Each of these waits
for a PROXY, and a proxy can become permanently false while nothing tells the
loop:

```sh
while pgrep -f solve.py; do sleep 60; done          # matches its own command line
until grep -q DONE run.log; do sleep 60; done       # the writer was killed
while kill -0 "$PID"; do sleep 5; done              # the machine reuses that pid
until [ -f done.marker ]; do sleep 30; done         # nobody will write that file
```

Four monitors of this kind on one machine slept for 95 hours between them. Use
qex, which is the parent of the task and waits for the process itself.

## The three commands

```sh
ID=$(qex submit --cpu 2 --mem 4GB -- make release)   # gives the id at once
qex status "$ID" --wait                              # blocks; state, code and logs
qex logs "$ID" --tail 100
```

`qex submit` writes the id to stdout and nothing else, so `ID=$(...)` is safe. A
warning goes to stderr.

For work that you wait for right now, put `qex run` in front instead. The output
arrives as it happens and the exit code is the exit code of the job:

```sh
qex run -- cargo test
```

## Which command to wait with

| Your situation | Use |
| --- | --- |
| You wait now, in this command | `qex run -- CMD` |
| Your harness reports background commands | `qex status <id> --wait` in the background |
| A script needs the exit code only | `qex wait <id>` |

`qex status --wait` blocks in the same way as `qex wait` and gives the same exit
code, and its output also holds the state, the exit code and the last lines of
BOTH streams. Prefer it: one command gives everything.

## The exit codes

| Code | Meaning |
| --- | --- |
| 0 | The job succeeded. |
| 1 | The job failed. |
| 124 | Your wait reached its time limit. **The job continues.** |
| 125 | Something stopped the job: a kill, a timeout, or out of memory. |
| 126 | The job did not run, because a job that it needed failed. |
| 127 | There is no job with that id. |

## Your session can stop, and the work continues

The job is not a child of your shell and not a child of you. A person can stop
you at any moment: the job continues, it writes its result, and any later session
attaches to it with the id.

**Keep the id in a file that lasts longer than your session:**

```sh
qex submit --id-file .qex-build.id -- make       # in the project, not in a scratch directory
qex status "$(cat .qex-build.id)" --wait         # a later session, and the result is there
```

Put that file in the project or the home directory. **Not** in a scratch
directory that your harness owns, and not in `/tmp`: the job continues when the
session stops, but the file goes with the session, and you then hold no handle
for work that still operates. qex gives a warning when the file goes to such a
place.

**Before you start work again in a new session, ask first.** `qex list` shows
what already operates and `qex status <id>` gives the result of what stopped.
Starting the same build twice is the cost of not asking.

## Claims

Give `--cpu` and `--mem`. qex uses them to decide how many jobs operate together,
which is what stops several agents from filling the machine.

If you do not know the size, use a word: `--cpu guess --mem guess` takes one half
of the budget, `full` takes all of it. Do not run a small test job to measure a
task; give `guess`, start the real task, and read the truth afterwards:

```sh
qex status "$ID" --json     # the usage field gives max_rss and cpu_secs
```

qex records what each job used and uses it as the claim for the next job of the
same command in the same directory.

## Dependencies, and a stage for each step

Do not put several steps in one script: one exit code and one mixed log leave you
to find the cause. Give each step its own job.

```sh
BUILD=$(qex submit --name build -- make)
TEST=$(qex submit --name test --needs "$BUILD" -- make test)
qex wait "$TEST"    # 1 if the test failed, 126 if the build failed
```

A job that is skipped names the FIRST job that failed, so you read the cause and
not the chain. Use `--after` in place of `--needs` for a step that must run even
when the step before it fails.

Use an id in a script, and a name when you type a command yourself.

## The other commands

```sh
qex list                     what operates, what waits, and WHY it waits
qex list --cwd .             the jobs of this directory
qex kill <id>                stop a job that operates, and each of its children
qex cancel <id>              take a job out of the queue
qex top                      watch the queue; press q to leave
qex watchers                 find the polling loops that already wait on this machine
qex clean --auto             delete the records that stopped more than an hour ago
qex info                     the coordinator, the budget and the load
```

Every command that reads data accepts `--json`.

## Things that will catch you

- **`qex wait --timeout` limits YOUR WAIT and not the job.** Code 124 means the
  job continues. Use `--timeout` on `qex submit` to limit the job itself.
- **`pid` is null once a job stops.** The machine gives that number to another
  process. `last_pid` is history for a reader; never signal it. Use
  `qex kill <id>`, which is correct at every moment.
- **A claim is a promise, not a measurement.** A job that claims 2GB and uses
  20GB still fills the machine.
- **`qex submit` copies your environment to the disk** so the job behaves like a
  command you typed. Use `--env-capture minimal` when the shell holds a token,
  and never paste `--show-env` output anywhere public.
- **Under cron the environment is nearly empty.** Give `--env PATH=...` or run
  `-- bash -lc "..."`.

## Report what you find

- It helped you: comment on https://github.com/stephenc/qex/issues/1 with the
  number, your model and your harness.
- A recipe worth sharing: https://github.com/stephenc/qex/issues/2
- Something did not work, including a word that sent you the wrong way: search
  first with `gh issue list --repo stephenc/qex --state all --search "..."` and
  comment on the issue that matches; open a new one only when nothing does.
