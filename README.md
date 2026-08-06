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

**2. A monitor script counts itself.** An agent that has no way to wait writes a
monitor script. That script uses `pgrep -f train.py`, which also matches the
command line of the script. The script sees two processes, the task stops, one
process stays, and the script waits for ever. The agent then writes a monitor
for the monitor.

qex does not search a command line. It is the parent process of your task, and
it uses `waitpid` on that exact process. `qex wait` blocks until the job stops
and gives you the result in its exit code.

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
qex submit [--cpu N] [--mem SIZE] [--timeout TIME] [--job FILE] -- COMMAND...
qex wait   <id>... [--timeout TIME] [--passthrough]
qex list   [--state STATE] [--tag TAG] [--json]
qex status <id> [--json] [--show-env]
qex logs   <id> [--follow] [--tail N] [--stdout|--stderr]
qex kill   <id>...          stop a job that operates
qex cancel <id>...          remove a job from the queue
qex clean  [--all|--state done|--older-than 7d]
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
cargo test           # 104 unit tests and 30 end-to-end tests
cargo build --release
```

Each end-to-end test makes its own config, state and runtime directory, starts
its own coordinator, and stops it at the end. The tests do not touch the
coordinator of the user.

The documentation, the code comments, the help text and the error messages use
Simplified Technical English (ASD-STE100).

## License

Apache License 2.0. See [LICENSE](LICENSE).
