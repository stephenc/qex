---
title: qex for agents
description: The one page that an agent needs, and the property that makes qex safe to interrupt.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

# qex for agents

This page is also in the binary. Run `qex help agents` to read it with no
network.

## The three commands

```sh
ID=$(qex submit --cpu 2 --mem 4GB -- uv run train.py)
qex wait $ID
qex logs $ID
```

`qex submit` writes the id to stdout and writes nothing else, so `ID=$(qex
submit ...)` is correct. A warning goes to stderr.

For work that you wait for now, put `qex run` in front of the command instead:

```sh
qex run -- make test
```

The output arrives as it happens, and the exit code is the exit code of the job.

## Your session can stop, and the work continues

**This is the property that makes qex safe for an agent that a person can
stop.**

The job is not a child of your shell, and it is not a child of your agent. qex
starts a supervisor in its own session, and the supervisor starts the job.

| What happens | The job |
| ------------ | ------- |
| Somebody stops your agent | continues |
| Your terminal closes | continues |
| The coordinator stops, or a new version replaces it | continues, and it still writes its result |

Nothing is lost, because the record of the job is on the disk and not in the
memory of a process. Your wait is the only thing that stops.

You can therefore attach the wait again, at any time, from any session:

```sh
qex submit --id-file build.id -- make    # session 1
# a person stops the agent here. `make` continues.
qex status "$(cat build.id)" --wait      # session 2, and the result is there
```

The id is the handle. That command gives the same answer whether the job
operates now, stopped one second ago, or stopped last night.

**Put the id file where it lasts longer than your session.** Your project
directory or your home directory is correct. A scratch directory that your
harness owns is not correct, and neither is `/tmp`. The job continues when the
session stops, but the file goes with the session, and you then have no handle
for a job that still operates. qex gives a warning when the file goes to such a
directory.

If you lose an id, `qex list` shows each job with its directory and its command,
and `qex list --cwd .` shows the jobs of this directory only.

A monitor script cannot do this. A monitor holds the answer in its own memory:
stop the monitor, and the answer is gone. Keep the id in a file, and the answer
waits for you instead.

**Before you start work again in a new session, ask first.** `qex list` shows
what already operates, and `qex status <id>` gives the result of what stopped.
A person who stops you pays nothing, and a person who stops you and then gets
the same build twice pays for it.

## Do not write a monitor script

Every monitor that you write waits for a **proxy**: a pattern in the process
list, a line in a log file, a file that appears. A proxy can become permanently
false, and nothing tells the monitor. It then waits for ever.

These waits were measured on one machine in one day. Together they slept for 95
hours, and not one of the conditions could ever become true:

```sh
while pgrep -f "solve.py"; do sleep 60; done      # matches its own command line
until grep -q "DONE" run.log; do sleep 60; done   # the writer was killed
until grep -q "READY" ~/other.log; do sleep 60; done  # that file never existed
```

The second and the third hold no pattern fault. They are careful commands. The
fault is the proxy: a log line is evidence of the work, and evidence stops when
the work stops, in a way that the monitor cannot see.

qex waits for the process, and not for a proxy of the process.

```sh
qex watchers
```

That command finds the monitors of this kind on your machine. It removes its own
process and the processes that started it before it reports anything, so it
never finds itself.

## If you operate inside a harness

`qex wait` blocks, and your harness — not qex — reports when a background
command ends. Put the two together:

```sh
ID=$(qex submit -- make test)   # gives the id at once
qex status $ID --wait           # run THIS in the background of your harness
```

Use `qex status --wait` and not `qex wait` here. It blocks in the same way and
gives the same exit code, and its output also holds the state, the exit code and
the last lines of the error output. One command gives everything.

## The exit codes

| Code | Meaning |
| ---- | ------- |
| 0    | The job succeeded. |
| 1    | The job failed. |
| 124  | Your wait reached its time limit. The job continues. |
| 125  | Something stopped the job: kill, timeout or out-of-memory. |
| 126  | The job did not run, because a job that it needed failed. |
| 127  | There is no job with that id. |

The code 124 has the same meaning as the code of the `timeout` command. A
timeout on `qex wait` stops your wait only. It does not stop the job.

## Two fields that need care

**`pid` is null after the job stops.** While the job operates, `pid` gives the
process. After the job stops, the machine can give that number to a different
process, so qex removes it. The historical value is in `last_pid`, and it is for
a reader only: never send a signal to it. To stop a job, use `qex kill <id>`,
which is correct at each moment.

**A claim is a promise, and not a measurement.** Give `guess` when you do not
know the size, and read `usage` in `qex status --json` after the job to learn
the true size. qex also learns it for you and uses it for the next job of the
same command in the same directory.

## Everything else

`qex help <topic>` covers `job-file`, `resources`, `states`, `exit-codes` and
`config`. `qex schema job` and `qex schema status` give the JSON Schema of each
format. See the [reference](reference.md) for the full command list.
