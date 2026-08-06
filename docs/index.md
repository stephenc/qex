---
title: qex — a local job queue for long tasks
description: A resource-aware local job queue and CLI job scheduler for long-running builds, tests and jobs. It replaces polling shell scripts with a wait that always answers.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

# qex — Queued EXecutor

**A local job queue for the long tasks that you start from a terminal.** qex is a
resource-aware job scheduler for one machine: it holds your builds, tests and
jobs in a queue, starts each one when the machine has the cores and the memory,
and gives you one command that waits for the result.

- **A wait that always answers.** No polling, and no watch script.
- **Cores and memory.** A local batch scheduler that knows what already operates.
- **The result survives.** The coordinator can stop; your job and its result do not.
- **Linux and macOS.** One binary, and no service to configure.

```sh
cargo install qex
```

## Twenty seconds

Put `qex run` in front of a command. It goes in the queue, this command waits
for it, and the exit code is the exit code of the job. When something stops the
job, `qex run` gives 125 in place of it:

```console
$ qex run -- sh -c 'echo building; sleep 2; echo done'
building
done
```

Use `qex run` for work that you wait for now. Ctrl-C stops the job with it. See
[the page for agents](agents.md) for the stops that do not reach the job.

Or submit it, walk away, and ask later:

```console
$ qex submit --name build --cpu 2 --mem 2GB -- cargo build --release
f8872c97-7b4b-44f6-bacb-975342b5db2a
```

`qex submit` writes the id and nothing else, so `ID=$(qex submit ...)` operates
correctly.

```console
$ qex list
ID        STATE       NAME                CPU       MEM      TIME  NOTE
ba1cb9f6  completed   sh                    1     1.8GB        2s  the job succeeded
f8872c97  running     build                 2       2GB        1s
95d4e1dc  queued      test                  1     1.8GB         -  waits for the job f8872c97 (build), which is running
7f72886b  queued      big                  64      64GB         -  the job claims 64 cores and the budget is 12 cores; the job claims 64GB of memory and the budget is 21.2GB; qex starts this job when no other job operates
```

Each job says why it waits. A queue that does not say why is a queue that you
must debug.

## Why qex exists

Three shell loops that engineers write every day. Each one is broken, and the
second and the third hold no pattern fault at all:

```sh
while pgrep -f solve.py > /dev/null; do sleep 60; done
```

**✗ It finds itself.** The command line of that shell holds the letters
`solve.py`, so the pattern matches the monitor. The task stops, one process
stays, and the count never reaches zero.

```sh
until grep -q DONE run.log; do sleep 60; done
```

**✗ It waits for ever after a crash.** A careful command, until somebody stops
the task that writes that line. The marker will never arrive now.

```sh
while kill -0 "$PID" 2>/dev/null; do sleep 5; done
```

**✗ The machine gives that pid to a different process.** Your job stopped an
hour ago, and you now wait for the work of somebody else.

```sh
qex wait "$ID"
```

**✓ It waits for the process itself.** qex is the parent of your task, and it
uses `waitpid` on that exact process. A process ends or it does not, and there
is no third result.

Four monitors of this kind were measured on one machine in one day. Together
they had slept for **95 hours**, and not one of the conditions could ever become
true. `qex watchers` finds them:

```console
$ qex watchers
1 monitor script(s) wait for a proxy. Together they have waited 63h 12m.

MONITOR  pid 3565469  waiting 63h 12m
  waits for: a count of the processes that match, which another user holds above zero
  command:   bash -c while true; do M=$(ps -Ao args | grep -c solver); K=$(ssh other ...
  This monitor waits until NOTHING matches, and it has no pattern fault. On a
  machine that two agents share, the work of the other agent keeps the count
  above zero for ever, and your own work is already complete.
```

That command removes its own process and the processes that started it before it
reports anything, so it never finds itself.

## One command gives the result and the cause

```console
$ qex status 073a71f8
id:        073a71f8-d655-4b2f-9d4e-87af8922ab01
name:      check
state:     failed
exit code: 2
claim:     1 core(s), 1.8GB  (the default; give --cpu and --mem to change it)
used:      1.8MB of memory, 0.0s of CPU time
           the job used 0% of its memory claim
time:      0s

--- stderr ---
error: expected `;`

--- stdout ---
compiling 42 files
```

The state, the exit code, the resources that the job really used, and the last
lines of BOTH streams. A program frequently writes its result to one stream and
its failure to the other, so the error alone reads as a complete failure.

`qex wait` gives that result as an exit code, and your script can then make a
decision: `0` the job succeeded, `1` it failed, `123` it never started and it
reached its `--max-queue-time`, `124` your wait reached its time limit, `125`
something stopped the job, `126` a job that it needed failed, `127` there is no
job with that id.

## How qex compares

| | Waits for the process | Cores and memory | The result survives a stop | Says why a job waits |
| --- | --- | --- | --- | --- |
| `nohup` or `&` | ✗ no handle after the shell ends | ✗ | ✗ no record | ✗ |
| a `while` and `sleep` script | ✗ it waits for a proxy | ✗ | ✗ the answer is in the script | ✗ |
| GNU parallel | ✓ | cores only | partly, with `--joblog` | ✗ |
| `make -j` | ✓ | cores only | ✗ | ✗ |
| **qex** | **✓** | **✓ both** | **✓ on the disk** | **✓** |

To be fair to the other three: `make -j` and GNU parallel control the work of ONE
command, and they do it well. GNU parallel with `--joblog` and `--resume` keeps
the result of each job and continues a queue that stopped, which is close to what
qex does — for the length of that one run.

qex is for a different shape of work: many commands, from several terminals and
several agents, over hours, on a machine that they all share. Nothing above
knows what the others already started.

## What survives what

Your job is not a child of your shell, and it is not a child of your agent:

```
  qex submit
      │
      ▼
  Coordinator ── starts ──▶ Supervisor ── starts ──▶ your job
   (the queue)              (one for each job)      (its own session
                                                     and process group)
```

Stop the coordinator, close the terminal, or replace the qex program:

```
  The coordinator stops
      │
      ▼
  The supervisor continues ── writes ──▶ status.json   (the record, on the disk)
                                              │
                                              ▼
                                   qex status <id> --wait   (a later session)
```

**This is the reason that an agent which uses qex is safe to interrupt.** A
person can stop it at any moment: the build continues, and the next session
attaches to the same job with the id. A watch script holds the answer in its own
memory, and a stop loses that answer.

That is a property of `qex submit`. A job of `qex run` also stops when Ctrl-C or
a SIGTERM stops the command that waits for it.

## Install

```sh
cargo install qex
```

Or take the binary for your machine from
[the latest release](https://github.com/stephenc/qex/releases/latest):

```sh
curl -fsSL "https://github.com/stephenc/qex/releases/latest/download/qex-$(uname -s)-$(uname -m).tar.gz" | tar xz
install -m 755 qex ~/.local/bin/qex
```

Linux and macOS, x86-64 and arm64. The Linux binaries hold no dynamic library.
The first command starts the coordinator for you, and there is no service to
configure. On Windows, use WSL2.

## For coding agents

Several agents on one machine each start work, no agent sees the load of the
others, and the out-of-memory killer selects who loses. qex gives those agents
one queue and one budget.

`qex help agents` is a complete page inside the binary, so an agent needs no
network for it. It covers the submit, wait and logs loop, the exit codes, and
the property above: **your session can stop, and the work continues.**

An agent that drives many jobs reads one stream in place of a loop that asks
about each job:

```sh
qex events --json      # one JSON object on one line for each change of state
```

[Read the agent page](agents.md).

**Used qex? Say so.** The
[testimonials issue](https://github.com/stephenc/qex/issues/1) takes a comment
from any agent or person, and a
[new issue](https://github.com/stephenc/qex/issues/new) takes the small faults —
a word that sent you the wrong way counts. qex exists because somebody measured
their own monitor scripts and gave the numbers.

## The pages

| Page | What it holds |
| ---- | ------------- |
| [Agents](agents.md) | The page for a coding agent. Start here if you are one. |
| [Reference](reference.md) | Each command, option and configuration field. |
| [Design](design.md) | The coordinator, the supervisor, the files, and the limits. |
| [Security](security.md) | What qex writes, and who can read it. |

The source is at [github.com/stephenc/qex](https://github.com/stephenc/qex),
under the Apache 2.0 licence.
