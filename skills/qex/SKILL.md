---
name: qex
description: Run a long local task (a build, a test suite, a training run, a data job) through the qex queue instead of a background command and a polling loop. Use this when a command takes minutes or more, when the machine is shared with other agents or people, when you must wait for work that you started earlier, or when you need the exit code, the output or the measured memory of a task that has already stopped.
---

# qex — run long tasks and wait for them correctly

qex holds a task in a queue, starts it when the machine has the cores and the
memory, records the result on the disk, and gives you one command that waits for
it. `command -v qex` says whether it is installed; `cargo install qex` or a
binary from https://github.com/stephenc/qex/releases/latest installs it. The
first command starts the coordinator, and there is no service to configure.

`qex help agents` is the complete page inside the binary.

## If qex cannot start at all

Some harnesses run each command in a **sandbox**, and a sandbox can refuse the
Unix socket that qex needs. qex says so and names the directory.

You cannot correct that yourself: the permissions belong to the person who
starts you. Give them
[the sandbox page](https://github.com/stephenc/qex/blob/main/docs/sandbox.md),
and stop.

## The rule

**Never write a loop that waits for evidence of a task.** Each of these waits
for a **proxy**, and a proxy can become permanently false while nothing tells
the loop:

```sh
while pgrep -f solve.py; do sleep 60; done      # matches its own command line
until grep -q DONE run.log; do sleep 60; done   # the writer was killed
while kill -0 "$PID"; do sleep 5; done          # the machine reuses that pid
until [ -f done.marker ]; do sleep 30; done     # nobody will write that file
```

qex is the parent of the task and waits for the process itself, so an answer
always arrives. `qex watchers` finds monitors of this kind on the machine. To
find the coordinator, use `qex info` — never search the process list.

## Which command

| I want | Start the job | Attach to a job |
| --- | --- | --- |
| The output as it arrives | `qex run -- CMD` (= `qex submit --follow`) | `qex status <id> --follow` |
| The record when it stops | `qex submit --wait -- CMD` | `qex status <id> --wait` |
| The exit code only | `qex submit --wait -q -- CMD` | `qex status <id> --wait -q`, or `qex wait <id>...` for many |

Each gives the exit code of the job. They differ in what they **write**: the
output of the job, the record of the job, or nothing.

`--quiet` silences the record and the reason that a job waits. It never
silences a **fault** of the wait — no such job, a wait that reached its limit,
a wait that a signal stopped — because those lines give the id that attaches to
the job again.

**Use `qex submit --wait` for your long work:**

```sh
qex submit --wait --cpu 2 --mem 4GB --id-file build.id -- make release
```

- One command. A submission and a separate wait is two, and the second is a
  thing to forget: a job that nobody waits for still runs, and nobody reads the
  result.
- Your harness waits, because the command waits. Run it as a background command
  and the harness reports the end — no timer, no second command.
- The output goes to the log file, so a job of two hours does not fill your
  context. The command ends with the **record**: state, exit code, resources and
  the last lines of both streams. Add `-q` for the exit code alone.
- `--id-file` reaches the disk **before** the wait, so any interruption leaves
  you a handle: `qex status "$(cat build.id)" --wait`.

**Every submission joins a wait or gets its own.** With `--wait` that is
automatic. With `--wait` or `--follow` the id goes to **stderr**, because stdout
carries the result.

`qex run` ties the job to the command: Ctrl-C and SIGTERM stop the **job**.
Every other command in the table watches the job and never stops it — Ctrl-C
there gives 122 and the job continues. A SIGKILL, and the hangup of a terminal
that closes, never stop a job; `qex list` finds it and `qex kill <id>` stops it.

`qex wait A B C` waits for all three and writes one line for each.
`qex wait --next A B C` returns when the **next** job stops — **one time**. The
jobs that did not stop then have no watcher, so wait again for them.

## The exit codes

| Code | Meaning |
| --- | --- |
| 0 to 96 | **The job.** The exit code of the job, unchanged. |
| 97 to 127 | **qex.** The queue or the wait, never the job. |
| 97 | The job gave a code from 97 to 255. Read the record for it. |
| 98 | A signal that qex did not send stopped the job. Read the record for it. A kill, a cancel or a time limit gives 125, and a kill for memory gives 99. |
| 99 | The kernel stopped the job because the machine ran out of memory. Give a larger `--mem` and run it again. |
| 100 | The job has not stopped, so there is no result. Only `qex status --quiet` with no wait gives it. |
| 121 | qex could not do what you asked. No job ran. |
| 122 | Your wait stopped, and **the job continues.** Attach to it again. |
| 123 | The job gave up in the queue. It reached its `--max-queue-time`. |
| 124 | Your wait reached its time limit. **The job continues.** |
| 125 | Something stopped the job: a kill, a cancel or a time limit. |
| 126 | A job that this job needed did not succeed. |
| 127 | There is no job with that id. |
| 128 and up | **qex itself died from a signal.** The job is not described, and it can still operate. |

**The code answers `pass or fail`. The record answers `why`.** Read
`qex status` when you act on the difference between "the job failed" and "my
wait stopped". **125 does not say that your work failed** — another agent on
this machine can stop your job, so read the line on stderr first. `qex list`,
`qex logs` and the other commands never speak for a job, so they use 0, 1, 2 and
127 in the usual way.

## Your session can stop, and the work continues

The job is not a child of your shell and not a child of you. A person stops you,
your terminal closes, or qex replaces the coordinator: the job continues, it
writes its result, and any later session attaches with the id.

```sh
qex submit --wait --id-file .qex-build.id -- make   # a person stops you here
qex status "$(cat .qex-build.id)" --wait            # a later session, and the result is there
```

Put that file in the project or the home directory. **Not** in a scratch
directory that your harness owns, and not in `/tmp`: the job outlives the
session and the file does not. qex warns when the file goes to such a place. If
you lose an id, `qex list --cwd .` gives the jobs of this directory.

## Claims

Give `--cpu` and `--mem`. qex uses them to decide how many jobs operate
together, which is what stops several agents from filling the machine. Use
`--cpu guess --mem guess` when you do not know the size (`half`/`guess` take one
half of the budget, `full`/`max` take all of it).

**Do not run a small test job to measure a task.** It costs time and measures
different work. Give `guess` and start the real task:

```sh
qex submit --wait --cpu guess --mem guess -- ./task   # run 1
qex submit --wait -- ./task                           # run 2: the claim is ready
```

qex records what each job really used and uses it as the claim for the next job
of the same command. A job that the kernel stopped for memory gives a lower
bound, so the next claim is above it. `qex status --json` gives `max_rss` and
`cpu_secs`.

## A key, so a second run starts nothing

```sh
qex submit --wait --dedupe-key train:$(pwd) -- uv run train.py
```

While a job with that key waits or operates, a second submission starts **no**
job: qex gives that id and exits 0, so your script cannot start the same
four-hour run twice. Do not read `qex list` and decide for yourself — that test
is a proxy, and another agent can submit between your read and your decision.
The coordinator makes the test and the submission one step.

Choose a key that names the work **and** the place: `build:$(pwd)`. The key is
free when the job stops; `--dedupe-window 1h` keeps the key of a job that
**succeeded** for an hour also. A job that a key gives you belongs to another
agent, so Ctrl-C stops your wait only.

## A stage for each step

Do not put several steps in one script: one exit code and one mixed log leave
you to find the cause.

```sh
BUILD=$(qex submit --name build -- make)
TEST=$(qex submit --name test --needs "$BUILD" -- make test)
qex status "$TEST" --wait     # 126 if the build failed
```

A skipped stage names the **first** job that failed, so you read the cause and
not the chain. Use `--after` for a cleanup step that must run even when the step
before it fails. Use an id in a script, and a name when you type a command.

## Many jobs at one time

```sh
qex events --json      # one JSON object on one line for each change of state
```

Keep the `stream_id` of the first line and the largest `seq` you read, and give
both to `--since` when you start again. A new coordinator restarts the numbers
and gives you a `gap` line; with a number alone it cannot. **Act on `id` and
`state`, not on the arrival of a line.** Run `qex help events`.

## The other commands

```sh
qex list                     what operates, what waits, and WHY it waits
qex logs <id> --grep ERROR   the lines you want from a large log
qex logs <id> --tail 50      the last lines of a log
qex kill <id>                stop a job and each of its children
qex cancel <id>              take a job out of the queue
qex top                      watch the queue; press q to leave
qex clean --auto             delete the records that stopped over an hour ago
qex info                     the coordinator, the budget and the load
```

Every command that reads data accepts `--json`.

## Things that will catch you

- **`--timeout` limits the job; `--wait-timeout` limits your wait.** Code 124
  means the job continues.
- **A job waits until the machine has capacity.** Add `--max-queue-time 30m`
  when you must have an answer inside a time. The state becomes `expired` and
  the code is 123; nothing ran, so there is no output.
- **`pid` is null once a job stops.** The machine reuses that number. `last_pid`
  is history for a reader; never signal it.
- **A claim is a promise, not a measurement.** A job that claims 2GB and uses
  20GB still fills the machine.
- **`--lock NAME` keeps two jobs apart** when they share something a claim
  cannot express: a build directory, a port, a database.
- **`status --follow` writes the log from its first line.** For a job that
  already stopped, use `qex logs --tail` or `qex status --wait` instead.
- **`qex submit` copies your environment to the disk.** Use
  `--env-capture minimal` when the shell holds a token, and never paste
  `--show-env` output anywhere public.
- **Under cron the environment is nearly empty.** Give `--env PATH=...` or run
  `-- bash -lc "..."`.

## Report what you find

- It helped you: https://github.com/stephenc/qex/issues/1 — give the numbers,
  your model and your harness.
- A recipe worth sharing: https://github.com/stephenc/qex/issues/2
- Something did not work, including a word that sent you the wrong way: search
  first with `gh issue list --repo stephenc/qex --state all --search "..."` and
  comment on the issue that matches; open a new one only when nothing does.
