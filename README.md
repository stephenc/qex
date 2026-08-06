# qex — Queued EXecutor

qex is a job queue for long tasks on one machine. It controls the number of
cores and the quantity of memory that the jobs use together.

qex is for coding agents and for the people who work with them. Several agents
on one machine each start work, and no agent sees the load of the others. The
machine then runs out of memory. qex gives those agents one queue.

```sh
ID=$(qex submit --cpu guess --mem guess -- uv run train.py)
qex wait $ID
qex logs $ID
```

**Agents: run `qex help agents` first.** It is one page and it covers everything.

## The three problems that qex solves

**1. No agent sees the load of the others.** Each agent finds free memory,
starts a large task, and the out-of-memory killer selects a victim. qex holds a
budget for the machine. A job starts when the machine has capacity for its
claim, and it waits when the machine does not.

**2. Every hand-rolled watcher waits on a proxy, and a proxy can go permanently
false.** An agent with no way to wait writes a monitor. That monitor watches a
*proxy* for the work — a pattern in the process list, a line in a log, a file
that should appear — and a proxy can stop being reachable without anything
noticing. Three real watchers on one machine, in one day, slept for **54 hours
combined** on conditions that could never become true:

```sh
while pgrep -f "solve.py"; do sleep 60; done      # matches its own command line
until grep -q "DONE" run.log; do sleep 60; done   # the writer was killed
until grep -q "READY" ~/other.log; do sleep 60; done  # that file never existed
```

Only the first is the classic `pgrep -f` self-match. The other two contain no
pattern bug at all — they are careful commands whose evidence simply stopped
arriving. That is the general failure, and it is why the fix is not "write a
better pattern".

qex waits on the **process**, not on a proxy for it. qex is the parent of your
task and calls `waitpid` on that exact process: it exits or it does not, and
there is no third outcome. `qex wait` therefore always returns — including 125
the moment somebody kills the job, where a log-watcher would still be sleeping.

**3. There is no handle on a running task.** qex gives each job a UUID. Use that
id to read the state of the job, read its output, stop it, or remove it from the
queue.

## Install

```sh
cargo build --release
install -m 755 target/release/qex ~/.local/bin/qex
```

qex needs Linux or macOS. It has no other requirement. The first command starts
the background coordinator for you, and you do not configure a service.

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

## If you drive qex from an agent harness

`qex wait` blocks, and your harness — not qex — reports when a background
command ends. Compose the two:

```sh
ID=$(qex submit -- make test)   # returns at once
qex status $ID --wait           # run THIS in the background of your harness
```

qex watches the process correctly, the harness reports the end. No timer, no
polling, no second command: the output of `qex status --wait` holds the state,
the exit code and the last lines of the error output.

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

## How qex operates

qex is one program with three roles.

**The CLI** reads your command. It calculates the full job specification at this
moment, because the coordinator can start many hours before from a different
shell. If no coordinator operates, the CLI starts one. A lock file lets one CLI
process only start the coordinator, so twenty commands at the same time give one
coordinator.

**The coordinator** holds the queue and starts each job when the machine has
capacity. It stops one hour after the last job and the last command, so it uses
no memory between your tasks.

**A supervisor** controls one job. It starts the job in a new session and a new
process group, writes the two output files, and records the result.

The supervisor is a separate process for one reason: the coordinator can stop,
fail or restart, and the job must continue and must still record its result. A
`kill -9` on the coordinator loses no job and no exit code.

### Files

```
~/.config/qex.toml                  the config file
~/.local/state/qex/jobs/<uuid>/
    spec.json      the command, the environment and the claims (mode 0600)
    status.json    the state, the exit code, the times and the true use
    stdout.log
    stderr.log
```

`status.json` is the primary record. It records the command, the directory, the
state, the exit code, the times and the measured use. The supervisor writes it
in one operation,
so a reader sees the old contents or the new contents, and never a part of them.
`qex wait` reads this file directly when no coordinator operates.

### Several users

Each coordinator writes its claims to `/tmp/qex`, and it reads the records of
the other users before it starts a job. This method needs no administrator
rights.

The method is cooperative, and a different user can write an incorrect value.
qex also tests the free memory of the machine, so it finds a load that no
coordinator reports.

## Limits

**qex does not limit the CPU of a job.** The `cpu` controller of cgroup v2 is
not available to a user on a usual Linux system, and macOS has no equivalent.
The queue controls the number of cores instead: qex does not start more work
than the budget permits.

**qex does not limit the memory of a job by default.** A claim controls the
queue only. This behaviour is the same on Linux and on macOS.

Linux can apply a memory limit with cgroup v2. Set `[enforce] mode` to `soft` or
`hard` to use it. The coordinator needs a cgroup that it owns, and a coordinator
that starts from a login shell does not have one. `qex config show` reports
`NOT ACTIVE` with the reason when this occurs. qex never reports a limit that it
did not apply.

**A claim is a promise, and not a measurement.** A job that claims 2GB and uses
20GB can still fill the machine. qex tests the free memory of the machine before
each start, which limits the damage, but an accurate claim is better.

## Development

```sh
cargo test -- --test-threads=2      # 105 unit tests and 47 end-to-end tests
cargo build --release
```

Each end-to-end test makes its own config and state directory, starts its own
coordinator, and stops it at the end. The tests do not touch the coordinator of
the user, and they turn the peer accounting off.

Use two test threads. Each end-to-end test starts real processes and waits for
them. With more threads, the machine becomes busy, a job starts late, and a test
reports a failure that the program does not have.

The documentation, the code comments, the help text and the error messages use
Simplified Technical English (ASD-STE100).

## License

Apache License 2.0. See [LICENSE](LICENSE).
