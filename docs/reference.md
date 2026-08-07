---
title: qex reference
description: Every command, option, claim word and configuration field.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

# Reference

## Commands

```
qex submit [--cpu N] [--mem SIZE] [--timeout TIME] [--needs ID,ID]
           [--after ID,ID] [--name NAME] [--job FILE] -- COMMAND...
qex wait   <id>... [--timeout TIME] [--passthrough]
qex list   [--state STATE] [--tag TAG] [--json]
qex status <id> [--json] [--show-env]
qex logs   <id> [--follow] [--tail N] [--stdout|--stderr]
qex kill   <id>...          stop a job that operates
qex cancel <id>...          remove a job from the queue
qex clean  [<id>|completed|done|--state STATE|--older-than 7d|--all]
qex info                    the coordinator: its pid, its budget and its load
qex config show             the values that qex uses now
qex schema job|status       the JSON Schema of each format
qex help <topic>
```

`qex submit` writes the job id to stdout and writes nothing else, so
`ID=$(qex submit ...)` operates correctly. A warning goes to stderr.

Every command that reads data accepts `--json`.

### Exit codes of `qex wait`

| Code | Meaning |
| ---- | ------- |
| 0    | The job succeeded. |
| 1    | The job failed. |
| 124  | Your wait reached its time limit. The job continues. |
| 125  | Something stopped the job: kill, cancel, timeout or out-of-memory. |
| 126  | The job did not run, because a job that it needed failed. |
| 127  | There is no job with that id. |

The code 124 has the same meaning as the code of the `timeout` command. A
timeout on `qex wait` stops your wait only. It does not stop the job.

Add `--passthrough` to exit with the exit code of the job.

### Exit codes of `qex run`

| Code | Meaning |
| ---- | ------- |
| the exit code of the job | The job ran. `qex run -- sh -c 'exit 7'` gives 7. |
| 125  | Something stopped the job: kill, cancel, Ctrl-C, timeout, out-of-memory. |
| 126  | The job did not run, because a job that it needed failed. |
| 127  | There is no job with that id. |

`qex run` writes the output of the job, so it gives the exit code of the job
when the job RAN.

A job of `qex run` is a job like any other, so `qex kill` and `qex cancel` from
a different command can stop it. Such a job gave no exit code of its own, and
`qex run` then gives 125. `qex run` also writes a line to stderr that names the
cause, and that line says when this command did not stop the job.

The code 1 has two causes. Your work ran and it gave the exit code 1, or qex
could not finish its own work: the coordinator stopped while `qex run` waited,
for example. qex writes the second cause on stderr, and the job can then still
operate.

For each state in which the job gave NO exit code of its own, `qex run` gives
the same code as `qex wait`. For a job that RAN, `qex run` gives the exit code
of the job, and `qex wait` gives 0 or 1 unless you add `--passthrough`.

`qex run` never gives 124. The code 124 says that YOUR WAIT reached its limit
while the job continued, and `qex run` waits with no limit of its own. A job
that reaches the time limit of `--timeout` gives 125, because something stopped
that job.

## Resource claims

Give `--cpu` and `--mem`. qex uses these claims to decide how many jobs operate
together.

If you do not know the size of a task, use a word in place of a number:

| Word | Meaning |
| ---- | ------- |
| `half`, `guess` | One half of the budget. Two such jobs operate together. |
| `full`, `max`   | The full budget. The job operates alone. |

qex calculates these words against the budget, and not against the free memory
of the moment. The same command thus always gives the same claim.

### qex learns the size of a task

qex records what each job really used and uses those numbers as the claim for
the next job of the same command:

```sh
qex submit -- cargo test    # run 1: the default claim
qex submit -- cargo test    # run 2: the claim comes from run 1
```

`qex status` says where a claim came from. The record is for the command, not
the name, because `cargo build` and `cargo test` need different sizes. qex uses
the **largest** measurement it holds plus a margin, because a claim that is too
small stops the job while a claim that is a little large costs only capacity.
A job that did not complete is never recorded: it shows the memory it reached,
not the memory it needs.

Turn it off with `[learn] enabled = false`.

Do not run a small test job to measure a task. Give `guess` and start the real
task. qex measures each job, and you can read the true use later:

```sh
qex status $ID --json      # the usage field gives max_rss and cpu_secs
```

Read those numbers only when you run the same kind of task many times and the
queue is slow. For one task, `guess` is sufficient.

### A claim that is larger than the budget

Such a job can never meet the usual rule. qex starts it alone when no other job
operates. The job can then cause swap operations, use every core, or stop with
an out-of-memory error.

Each of these results is data for you. A job that waits for ever gives no data.
The status field `forced` is `true` for such a job, and `qex submit` writes a
warning at the time of the submission.

## A pipeline of stages

Do not put the stages of a pipeline in one script. If stage 3 of that script
fails, you get one exit code and one log file with every stage mixed together,
and you must find the cause yourself.

Give each stage its own job:

```sh
BUILD=$(qex submit --name build -- make)
TEST=$(qex submit --name test --needs $BUILD -- make test)
SHIP=$(qex submit --name ship --needs $TEST -- ./deploy.sh)
qex wait $SHIP
```

Keep the id of each stage and give it to the next stage.

Each stage has its own log file, its own exit code and its own claim. If `build`
fails, `test` and `ship` do not start:

```
ID        STATE     NAME   ...  NOTE
a1b2c3d4  failed    build  ...  the job stopped with the exit code 2
b2c3d4e5  skipped   test   ...  the job a1b2c3d4 (build) is failed, ...
c3d4e5f6  skipped   ship   ...  the job a1b2c3d4 (build) is failed, ...
```

There is one failure only, and it is the cause. `qex logs a1b2c3d4` gives the
output of that stage, and no other output.

Each skipped job names the **first** job that failed, and not the job before it.
A read of the last stage thus gives the cause immediately, and you do not follow
the chain.

| Option | Meaning |
| ------ | ------- |
| `--needs ID,ID` | Wait for these jobs. Do not run if one does not succeed. |
| `--after ID,ID` | Wait for these jobs, whatever their result. |

Use `--after` for a cleanup step that must run also when the build fails.

`qex wait` gives 126 for a skipped job and 1 for a job that failed, so a script
can separate a failure of its own stage from a failure of an earlier stage.

Each option accepts an id or a name, and the two have different rules.

An **id** must exist. That is the only rule, so a script can submit its last
stage even when the first stage already failed; the last stage then becomes
`skipped` with the correct cause.

A **name** must give a job that is in the queue or operates. A name can give a
job of an earlier run — you write `--needs test`, you forgot to start a new test
job, and the name gives yesterday's test job, which already succeeded. Your
stage would then start immediately and wait for nothing. qex refuses that.

Use an id in a script. Use a name when you type a command yourself.

A job can name only the jobs that you started before it, so a circle of
dependencies is not possible.

## Job files

```sh
qex submit --job train.toml
```

```toml
name = "train-model"
command = ["uv", "run", "train.py", "--epochs", "50"]
timeout = "4h"
tags = ["ml"]

[resources]
cpu = 3          # or "guess", or "full"
mem = "8GB"

[env]
CUDA_VISIBLE_DEVICES = "0"
```

A job file also accepts `needs` and `after`:

```toml
command = ["make", "test"]
name = "test"
needs = ["build"]
```

qex reads TOML, YAML and JSON. The file extension selects the format.

`command` is a list of arguments, and it is not a shell command line. qex starts
no shell, so you need no quotation marks and no escape characters. To use a
shell feature, name the shell: `["bash", "-lc", "a | b > c.txt"]`.

A field name with a spelling error gives an error. qex does not ignore it.

## The environment and the directory

`qex submit` copies your environment and your current directory. Your job thus
operates in the same way as a command that you type now.

A later source replaces an earlier source:

```
environment from the shell  ->  job file [env]  ->  --env K=V
directory from the shell    ->  job file cwd    ->  --cwd D
config file defaults        ->  job file        ->  command line options
```

Use `--env-capture minimal` if your shell holds secrets. That mode copies
`PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG` and `TZ` only. Use
`--no-env-capture` to copy nothing.

qex writes the captured environment to `spec.json` with mode 0600, and the job
directory has mode 0700. `qex status` hides the environment. Add `--show-env` to
see it.

## Configuration

The config file is `~/.config/qex.toml`. Every field is optional. Run
`qex config show` to see the values that qex uses now.

```toml
[budget]
cpu = "75%"           # cores that qex can use
mem = "75%"           # memory that qex can use

[system]
reserve_mem  = "2GB"  # memory to keep free for other programs
max_pressure = 20     # maximum PSI memory pressure (Linux only)

[queue]
oversized = "run-when-idle"   # run-when-idle, reject or queue

[defaults]
cpu = 1               # the default is 1 core
mem = "2GB"           # the default is the machine memory / the core count
timeout = "0"         # the default is no limit
```

With no `[defaults]` section, a job gets 1 core and an equal part of the machine
memory. The default job size thus scales with the machine.

A field that takes a number, a size, a time or a percentage accepts the value
with quotation marks and without them. `cpu = 2` and `cpu = "2"` give the same
budget, and `margin = 1.5` and `margin = "1.5"` give the same margin. A size
with no unit is bytes, and a time with no unit is seconds.

The quotation marks do not change **which** values a field takes. `[budget] cpu`
takes a percentage, because it gives a part of the machine to all the jobs
together. `[defaults] cpu` gives the cores for one job, so it takes a whole
number only, and a percentage there gives an error.

### The coordinator reads this file again when it changes

A coordinator operates for hours. It reads the configuration file again when the
content of the file changes, so `qex config show` and `qex info` no longer
disagree about the budget of qex. Measured: the new values arrive in 0.52s to
0.56s on a busy coordinator, and in 0.68s to 1.01s on one with nothing to do.

The new values apply to the jobs that START after the change. A job that
operates keeps the claim that it made, and the coordinator keeps that claim
against the budget until the job stops.

**qex compares the CONTENT of the file, and not its time.** Linux takes the time
of a file from a coarse clock with the granularity of one tick, which is 4
milliseconds on a usual machine. Two writes inside one tick give a file the same
time, so a test of the time misses the second write, and it misses it for ever.

**qex looks at the file about ten times in half a second, and it takes the
content when every look gave the same content.** A shell `>` and a redirect, and
every program that writes one line at a time, leave a file that stops in the
middle for a moment, and a file that stops in the middle is still valid TOML. It
parses, and it is wrong in two ways:

- Every key that the writer did not reach yet takes its DEFAULT value. That is
  the one road by which a budget of 2 cores CAN become the default budget of 12.
- A stop in the MIDDLE OF A LINE gives a wrong value that is not a default
  value. A file that was becoming `cpu = 16` reads as `cpu = 1`, and `qex config
  show` then reports `budget: 1 cores`.

qex says nothing in either case, because it CAN read such a file. This is why
qex looks more than one time, and why the new values take about one second and
not half a second.

**The wait is a TIME, and not a count of turns of the scheduler.** The scheduler
waits 500 milliseconds for a change, but every request wakes it, so a
coordinator with work in the queue turns much faster. Measured with a mark on
each turn: the median gap was 500.7ms with nothing to do, and 17.0ms with a loop
of `qex submit` running. While the file settles, qex looks at it every 50
milliseconds.

**A file that changes back and forth in step with those looks can still be
taken.** qex LOOKS at the file; it does not get a message when the file changes.
A writer that puts two different whole files at the path in turn, at a period
near the period of the looks, gives every look the same content while the file
was never that content for longer than one period. Measured: a writer that
changed the file every 25 milliseconds made the coordinator take a half-written
file in 3 trials of 5.

No number of looks removes that. A sampler always has a frequency that walks
past it; more looks only move which frequency. A writer with that regularity is
not a shell `>` and not an editor, because those write the file one time: a
shell loop with its usual jitter could not do it in 5 trials of 5, and only a
writer with an exact period could. Write the file in one step — write a
temporary file and rename it over this one — and none of this applies.

**The path must be a regular file, or a link to one.** qex opens this path on
every turn of the scheduler. A FIFO stops that open until somebody writes to
the FIFO, and the coordinator would then answer nothing at all. Every command
that reads this file applies the same rule, so `qex config show` and `qex
submit` give an error at once in place of a wait with no end.

**A file that qex cannot read does not become the default values.** The
coordinator keeps the values that it had and says so. The same holds for a file
that is empty, for a file that is gone, and for a path that is not a regular
file:

```
qex: WARNING: the configuration file changed, and qex cannot read it:
qex:   config [budget] cpu: invalid core count `two`; expected an integer or a percentage
qex:   The coordinator keeps the values that it had, and they are the values below.
qex:   Correct the file. The coordinator reads it again by itself. Run `qex config show` for the full message.
```

`qex info` gives that warning, and `qex info --json` gives the same text in the
field `config_error`. Correct the file, and the coordinator reads it again with
no other step: the warning then goes away.

The reload tests the values in the same way as the start of a coordinator, so a
value that stops qex from starting cannot arrive by an edit.

### Update the coordinator before you use a new option

qex refuses a field that it does not know. That rule finds a name with a
spelling fault, and a name with a spelling fault must not be ignored in silence.

It has a second cause. A new option belongs in the config file only **after the
coordinator is the new build**:

```sh
qex info                # the version and the pid of the coordinator
# install the new qex
kill <pid>              # the jobs that operate continue
qex info                # the new version now
# NOW put the new option in ~/.config/qex.toml
```

**The program on the disk is not sufficient.** A coordinator operates for hours,
and it holds the code that started it. It reads the config file again when the
file changes, but it reads that file with the code that it holds, and that code
does not know the new option. The coordinator therefore refuses the file, keeps
the values that it had, and `qex info` reports the fault. The new option has no
effect until a NEW coordinator reads it. The coordinator stops by itself when no
job operates, and `kill <pid>` changes it at once.

**Install the new qex before you kill the coordinator.** While the old qex is
the program on the disk, no coordinator can start from a file that holds the new
option, and the commands in the next paragraph that need a coordinator go with
it.

In the other order, `qex submit`, `qex run`, `qex pipeline`, `qex gc`, `qex du`
and `qex config show` stop, and qex cannot start a coordinator — so a queue whose
coordinator retires stays where it is. The jobs that operate continue. These are
the commands you keep, and the second group is the larger one:

| Continue in every state | Continue while a coordinator operates |
| --- | --- |
| `qex wait`, `qex top`, `qex logs`, `qex version` | `qex info`, `qex list`, `qex status`, `qex kill`, `qex cancel`, `qex clean`, `qex rerun` |

`qex rerun` is in the second group, so you can still start work even though `qex
submit` stops: it asks the coordinator for a job that the records already hold,
and it needs no config file. With no coordinator, each command in the second
group waits 10 seconds and then reports that the coordinator did not start. That
message names no cause, which is the second reason to install the new qex first.

A job that starts in this state uses the default values, and `qex status` says
so. Remove the section from the file to go back.

Two people or two agents that share a machine each run their own coordinator, so
each must make this change for itself.

Run `qex help config` for every field.

## More help inside the tool

Each topic below is also in the binary, so an agent needs no network:

```sh
qex help agents      the one page for an agent
qex help job-file    the fields of a job file
qex help resources   claims, the budget and the several-user accounting
qex help states      each job state and what causes it
qex help exit-codes  the exit code of each command
qex help config      each configuration field
qex schema job       the JSON Schema of a job file
qex schema status    the JSON Schema of status.json
```
