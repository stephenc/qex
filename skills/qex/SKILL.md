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

## The two commands

```sh
qex submit --wait --cpu 2 --mem 4GB --id-file .qex-build.id -- make release
qex logs "$(cat .qex-build.id)" --tail 100
```

`--wait` holds the command until the job stops, and it gives you the exit code
of the job. **One command cannot be forgotten.** `qex submit` and then a wait is
two commands, and the second is a thing to remember: an agent that forgets it
never learns that the job stopped, and the result waits for nobody. Discipline
is not the remedy — the agent that reported this fault made the same mistake
twice more in the same session after writing the rule down.

**Every submission joins a wait or gets its own.** A job runs whether or not
anybody watches it.

The output of the job goes to the log file and not to your terminal, so a job of
two hours does not fill your context. `--id-file` writes the id to the disk
**before** the wait begins, so `qex wait "$(cat .qex-build.id)"` attaches to the
job again after any interruption.

`qex submit` without `--wait` writes the id to stdout and nothing else, so
`ID=$(...)` is safe. A warning goes to stderr.

For work that is **short and heavy** and that you wait for right now, put
`qex run` in front instead. The output arrives as it happens and the exit code
is the exit code of the job, or 125 when something stopped the job:

```sh
qex run -- cargo test
```

**`qex run` ties the job to that command, but only for the stops that it can
catch.** Ctrl-C stops the job, and a SIGTERM on `qex run` stops the job. A
SIGKILL, and the hangup that a terminal sends when it closes, do not reach the
job: it continues, and `qex list` finds it. That is right for work you are
waiting for, and wrong for work that outlives your attention — use `qex submit`
for anything you might come back to.

**If your harness stopped you hard, look for the job.** `qex list` shows a job
of `qex run` that continued, and `qex kill <id>` stops it.

## Which command to wait with

| Your situation | Use |
| --- | --- |
| You start the job now, and you want the result | `qex submit --wait -- CMD` |
| You wait now, the work is short, and you want the output on your terminal | `qex run -- CMD` (Ctrl-C or SIGTERM stops the job; a SIGKILL does not) |
| A **different** command started the job, or the id comes from an earlier session | `qex status <id> --wait` |
| A script needs the exit code only | `qex wait <id>` |

All four give the same exit code. `qex submit --wait` and `qex run` differ in
one thing: `qex run` writes the output of the job to your terminal, and
`qex submit --wait` leaves it in the log file.

**One wait for each job.** Give one `qex submit --wait` for each job that you
start, and each notification from your harness names its own job.

**`qex wait --any` returns one time.** The jobs that did not stop then have
**no watcher**, and they finish with nobody to read them:

```sh
qex wait --any "$A" "$B" "$C" "$D"   # one result
qex wait --any "$B" "$C" "$D"        # the rest still need a wait
```

qex names the jobs that stay when it returns.

## The exit codes

**One table.** `qex run`, `qex submit --wait`, `qex wait` and
`qex status --wait` all obey it.

| Code | Meaning |
| --- | --- |
| 0 to 96 | **The job.** qex gives the exit code of the job, unchanged. |
| 97 to 127 | **qex.** The code describes the queue or the wait, never the job. |
| 97 | The job gave a code from 97 to 255. Read the record for it. |
| 98 | A signal stopped the job. Read the record for the signal. |
| 121 | qex could not do what you asked. No job ran. |
| 122 | Your wait stopped, and **the job continues.** Attach to it again. |
| 123 | The job never started. It reached its `--max-queue-time`. |
| 124 | Your wait reached its time limit. **The job continues.** |
| 125 | Something stopped the job: a kill, a cancel, a timeout, or out of memory. |
| 126 | The job did not run, because a job that it needed failed. |
| 127 | There is no job with that id. |
| 128 and up | **qex itself died from a signal.** The job is not described, and it can still operate. Attach to it again. |

**The code answers `pass or fail`. The record answers `why`.** Read
`qex status` whenever you act on the difference between "the job failed" and
"my wait stopped".

A code below 97 comes from the job, and nothing else gives one. A job that exits
124 of its own accord thus gives you **97**, and the record holds the 124; a
wait that reached its time limit gives you **124**. Every other command of qex —
`qex list`, `qex logs`, `qex top` — never speaks for a job, so it uses the usual
0, 1, 2 and 127.

**125 does not say that your work failed.** A job of `qex run` or of
`qex submit --wait` is a job like any other, so another agent on this machine
can run `qex kill` or `qex cancel` on it. Read the line on stderr before you
start the work again.

**Ctrl-C during a wait gives 122**, and qex names the job that continues. A
second Ctrl-C stops the command immediately.

## Your session can stop, and the work continues

This is about `qex submit`. A job of `qex run` differs in one case only: Ctrl-C
or a SIGTERM on the waiting `qex run` stops the job; see above.

The job is not a child of your shell and not a child of you. A person can stop
you at any moment: the job continues, it writes its result, and any later session
attaches to it with the id.

**Keep the id in a file that lasts longer than your session:**

```sh
qex submit --wait --id-file .qex-build.id -- make   # session 1. A person stops you here.
qex status "$(cat .qex-build.id)" --wait            # session 2, and the result is there
```

Put that file in the project or the home directory. **Not** in a scratch
directory that your harness owns, and not in `/tmp`: the job continues when the
session stops, but the file goes with the session, and you then hold no handle
for work that still operates. qex gives a warning when the file goes to such a
place.

**Give each submission a key, and a second run of your script starts nothing:**

```sh
qex submit --wait --dedupe-key train:$(pwd) -- uv run train.py
```

While a job with that key waits or operates, a second submission with the same
key starts **no** job: qex writes the id of that job and exits with the code 0.
Your script does not change, and it cannot start the same four-hour run twice.

Do not read `qex list` and decide for yourself. That test is a proxy, and a
different agent can submit between your read and your decision. The coordinator
makes the test and the submission one step.

The key is free when the job stops. Add `--dedupe-window 1h` to keep the key of
a job that **succeeded** for an hour also; a job that did not succeed never
keeps its key. The window of the command that asks applies, so give the same
window in each command that shares a key. Add `--json` when your script must
know if **it** started the work.

`qex run --dedupe-key` waits for the job that the key gives, and Ctrl-C then
stops your wait only: a different agent can be the owner of that job. `qex run`
then gives **122**, which says that your wait stopped and the job continues. Use
`qex kill <id>` to stop the job itself, or `qex status <id> --wait` to wait
again.

**A key names the work. qex does not compare the command.** A second submission
with the same key gives you the first job whatever command you wrote, so give
each different piece of work its own key.

## Many jobs at one time

Do not ask about each job in a loop. Read one stream:

```sh
qex events --json      # one JSON object on one line for each change of state
```

Each `job` line holds the whole record, the same as `qex status --json`, so you
need no second command for the exit code or the cause of a failure.

Keep **two** values: the `stream_id` of the first line and the largest `seq` you
read. Give both when you start again:

```sh
qex events --json --since "$STREAM_ID:348"
```

The numbers belong to one coordinator, and the next coordinator starts them at 1
again. With the name, qex sees that and gives you a `gap` line; **with a number
alone it cannot**, and you can lose events with no message.

A new coordinator reads the records again, so after that `gap` line it gives you
some lines a second time — a job that stopped while you were away arrives again
as `completed`. **Act on `id` and `state`, and not on the arrival of a line.**

The coordinator keeps the last 512 events and never waits for a reader. If you
fall behind you receive a `gap` line that counts what you lost; qex never hides
a gap. The stream reports what the coordinator saw: a job shorter than half a
second can go from `starting` to `completed` with no `running` line, and
`previous` gives the true sequence. Run `qex help events` for the detail.

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
qex wait "$TEST"    # the code of the test if it failed, 126 if the build failed
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
qex events --json            one line for each change of state, as it happens
qex top                      watch the queue; press q to leave
qex watchers                 find the polling loops that already wait on this machine
qex clean --auto             delete the records that stopped more than an hour ago
qex info                     the coordinator, the budget and the load
```

Every command that reads data accepts `--json`.

## Things that will catch you

- **`qex wait --timeout` limits YOUR WAIT and not the job.** Code 124 means the
  job continues. Use `--timeout` on `qex submit` to limit the job itself, and
  `--wait-timeout` to limit the wait of `qex submit --wait`.
- **`qex wait --any` leaves the other jobs unwatched.** It returns one time.
  Wait again for the rest, or give each job its own `qex submit --wait`.
- **A job waits until the machine has capacity.** Add `--max-queue-time 30m` to
  `qex submit` when you must have an answer inside a time. The job then does not
  start after that wait: its state becomes `expired` and `qex wait` gives 123.
  Nothing ran, so there is no output to read.
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
