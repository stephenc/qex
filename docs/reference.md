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
qex events [--json] [--since SEQ|start|now] [--count N] [--timeout TIME]
qex info                    the coordinator: its pid, its budget and its load
qex config show             the values that qex uses now
qex schema job|status|pipeline|event    the JSON Schema of each format
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
| 125  | Something stopped the job: kill, timeout or out-of-memory. |
| 126  | The job did not run, because a job that it needed failed. |
| 127  | There is no job with that id. |

The code 124 has the same meaning as the code of the `timeout` command. A
timeout on `qex wait` stops your wait only. It does not stop the job.

Add `--passthrough` to exit with the exit code of the job.

## The event stream

```sh
qex events --json
```

One JSON object on one line for each change of state, at the moment of the
change. Read this stream in place of a loop that asks about each job. An agent
that drives twenty jobs reads one stream, and it learns of each result when the
result happens.

```json
{"event":"job","seq":12,"time":1770000000,"id":"a1b2...","name":"build",
 "state":"failed","previous":"running","change":"state","job":{ ... }}
```

| Field | Meaning |
| ----- | ------- |
| `event` | `stream`, `job`, `gap` or `bye`. Ignore a value that you do not know. |
| `seq` | The number of this event. Keep the largest number that you read. |
| `state`, `previous` | The state now, and the state before. `previous` is `null` for the first line of a job, which says that qex accepted it. |
| `change` | `state`, or `reason` for a job that stays in the queue and whose reason to wait changed. |
| `job` | The whole record, the same as `qex status --json`. |

`qex schema event` gives the full schema.

The field `job` holds everything, so a reader needs no second command to learn
the exit code, the measured use or the cause of a failure.

### Read the stream again after a stop

```sh
qex events --json --since 348      # the events after the number 348
qex events --json --since start    # everything that the coordinator holds
qex events --json --since now      # the new events only
```

The default is `start`. A program that keeps the largest `seq` and gives it to
`--since` loses nothing when it stops and starts again. **This is the reason for
the numbers**: a stream that begins at "now" makes a reader that restarts lose
the results that arrived while it was away.

The numbers belong to one coordinator. The coordinator stops when no job
operates, and the next command starts a new one, which begins at 1 again. The
first line of the stream gives `coordinator_started_at`, so a reader can see
that this happened, and a number from an earlier coordinator gives a `gap` line
that says so.

### A reader that is slow

The coordinator keeps the last 512 events. **It never waits for a reader**, and
its memory does not grow for one. A reader that falls behind receives a `gap`
line that COUNTS the events that it lost:

```json
{"event":"gap","time":1770000000,"missed":37,"next_seq":420,"reason":"..."}
```

qex reports a gap and never hides one. A reader that loses the line `failed` and
hears nothing waits for a result that will never arrive.

### The end of the stream

A reader does **not** hold the coordinator open: a stream that keeps a
coordinator alive for ever is a leak. The coordinator stops when no job operates
and no command arrives for the idle time, and it writes a `bye` line first. The
command then exits with the code 0.

A stream that ends with **no** `bye` line means that something stopped the
coordinator. `qex events` writes a message to stderr and exits with the code 1.
The records of the jobs are on the disk and they are correct.

### An earlier coordinator

A coordinator that operates can be older than your command. Such a coordinator
does not know this request, so `qex events` refuses to run, names the
coordinator and gives the command that stops it. It never gives an empty stream,
because an empty stream and a stream with no events look the same.

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

Run `qex help config` for every field.

## More help inside the tool

Each topic below is also in the binary, so an agent needs no network:

```sh
qex help agents      the one page for an agent
qex help job-file    the fields of a job file
qex help resources   claims, the budget and the several-user accounting
qex help states      each job state and what causes it
qex help events      the event stream, its numbers and its gaps
qex help exit-codes  the exit code of each command
qex help config      each configuration field
qex schema job       the JSON Schema of a job file
qex schema status    the JSON Schema of status.json
qex schema event     the JSON Schema of one line of `qex events`
```
