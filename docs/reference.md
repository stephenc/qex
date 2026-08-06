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
| 125  | Something stopped the job: kill, timeout or out-of-memory. |
| 126  | The job did not run, because a job that it needed failed. |
| 127  | There is no job with that id. |

The code 124 has the same meaning as the code of the `timeout` command. A
timeout on `qex wait` stops your wait only. It does not stop the job.

Add `--passthrough` to exit with the exit code of the job.

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

## The order of the queue

qex starts the jobs in the order of the queue. The first job that cannot start
is the job at the front. The rule for the jobs behind it depends on **who holds
the capacity**:

| The holder | The jobs behind |
| ---------- | --------------- |
| The jobs of this queue | Two jobs pass, then qex keeps the capacity and starts nothing. |
| Another user | Every job starts. qex keeps no capacity. |
| A program outside qex | Every job starts. qex keeps no capacity. |
| The size of the job, with `oversized = "run-when-idle"` | Two jobs pass, then qex keeps the capacity and the queue becomes empty. |
| The size of the job, with `oversized = "queue"` | Every job starts. That job never runs. |

qex controls the release of the capacity that its own jobs hold. It controls
nothing else. To keep the machine empty for a job that waits for another user
gives that job nothing, and it stops every other job — a queue that never moves
with no cause.

`[queue] max_bypass` gives the number of jobs that may pass. The default is 2.
Set `max_bypass = 0` for a strict order, in which no job passes the job at the
front.

qex counts the jobs that pass in the status field `passed_by`, and
`blocked_since` gives the time when the job reached the front. **The count is
not reset when the holder changes.** A job that another user held for an hour
keeps its count. In the same scheduler cycle in which the holder becomes a job
of this queue, that count is already at the limit, and the job at the front is
unpassable. A wait behind another user thus costs one cycle, and not the life of
the other user's job.

Each job that waits gives a reason of its own in `blocked_reason`. A job behind
a job that qex keeps capacity for reads that fact and the id of the job at the
front.

### Is the queue healthy

```sh
qex info
```

The last line answers the question:

```
queue: running · last start 8s ago · 2 running, 5 queued
queue: waits for another user · no job started for 42m · 1 other user holds 6 cores and 16GB · the job at the front is a1b2c3d4 (train)
queue: held for the job a1b2c3d4 (train) · 2 job(s) started before it · no job started for 12s · 0 running, 3 queued
```

The queue is healthy when a job started recently, **or** when the line names a
cause outside this queue: another user or the machine. The queue is stuck when
no job started and the cause is a job of this queue.

`qex top` gives the same line in its header. `qex info --json` gives the fields
`queue_state`, `last_start_at`, `peer_count`, `peer_cpu`, `peer_mem`,
`head_job`, `head_blocker` and `head_passed_by`. A `null` in one of those fields
means **unknown**, because the coordinator is too old to measure it. It does not
mean zero.

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
max_bypass = 2                # jobs that may start before the job at the front

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
qex help exit-codes  the exit code of each command
qex help config      each configuration field
qex schema job       the JSON Schema of a job file
qex schema status    the JSON Schema of status.json
```
