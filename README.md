# qex — Queued EXecutor

qex is a job queue for long tasks on one machine. It controls the number of
cores and the quantity of memory that the jobs use together.

qex is for coding agents and for the people who work with them. Several agents
on one machine each start work, and no agent sees the load of the others. The
machine then runs out of memory. qex gives those agents one queue.

```sh
qex submit --wait --cpu guess --mem guess --id-file train.id -- uv run train.py
qex logs "$(cat train.id)" --grep 'ERROR|FAIL'
```

`--wait` holds the command until the job stops, gives you the exit code of the
job, and ends with the record: the state, the code and the last lines of both
streams. The output of the job stays in the log file.

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
noticing. Four real watchers on one machine, in one day, slept for **95 hours
combined** on conditions that could never become true. Three of them:

```sh
while pgrep -f "solve.py"; do sleep 60; done      # matches its own command line
until grep -q "DONE" run.log; do sleep 60; done   # the writer was killed
until grep -q "READY" ~/other.log; do sleep 60; done  # that file never existed
```

A different user later found a fourth kind, on a machine two agents shared. It
had slept for 63 hours waiting for `ps -Ao args | grep -c solver` to reach zero
on that machine and on one reached over `ssh`. Its own work had finished two days
earlier; the *other* agent's work held the count above zero. On a shared machine,
"wait until nothing matches" is unsatisfiable by construction.

Only the first is the classic `pgrep -f` self-match. The others contain no
pattern bug at all — they are careful commands whose evidence simply stopped
arriving, or whose condition depended on somebody else. That is the general
failure, and it is why the fix is not "write a better pattern".

qex waits on the **process**, not on a proxy for it. qex is the parent of your
task and calls `waitpid` on that exact process: it exits or it does not, and
there is no third outcome. `qex wait` therefore always returns — including 125
the moment somebody kills the job, where a log-watcher would still be sleeping.

**3. There is no handle on a running task.** qex gives each job a UUID. Use that
id to read the state of the job, read its output, stop it, or remove it from the
queue.

## Install

Take the file for your machine from [the latest
release](https://github.com/stephenc/qex/releases/latest):

```sh
curl -fsSL "https://github.com/stephenc/qex/releases/latest/download/qex-$(uname -s)-$(uname -m).tar.gz" | tar xz
install -m 755 qex ~/.local/bin/qex
```

Or build it from the source:

```sh
cargo install --path .
```

qex needs Linux or macOS. It has no other requirement.

**On Windows, use WSL2** — qex builds and runs there unchanged, and the jobs an
agent starts (`make`, `cargo`, `uv`) usually live in WSL2 anyway. A native
Windows build is a port, not a flag: qex holds a job with `waitpid` on one
process id and stops a job tree with a process group, and Windows has neither.
Building for Windows fails with one message that says this, rather than a page
of errors. The first command starts
the background coordinator for you, and you do not configure a service.

## Five minutes

```sh
qex submit --wait --cpu guess --mem guess --id-file t.id -- make test
ID=$(cat t.id)
qex list                    # what operates now
qex logs $ID --tail 50      # the output
qex kill $ID                # stop it
qex status $ID --wait       # attach again, from any session
```

Put `qex run -- make test` in front of a command when the work is **short and
heavy** and you wait for it now: it takes its turn in the queue, so everyone
else on the machine keeps the capacity they claimed, the output arrives as it
happens, and the exit code is the exit code of the job. When something stops the
job, `qex run` gives 125 in place of it.

`qex run` ties the job to that command, but only for the stops that it can
catch. Ctrl-C stops the job, and a SIGTERM on `qex run` stops the job. A
SIGKILL, and the hangup that a terminal sends when it closes, do not reach the
job: it continues, and `qex list` finds it. Use `qex submit` for anything you
might come back to.

Driving many jobs at one time? Read one stream in place of a loop that asks
about each job:

```sh
qex events --json     # one JSON object on one line for each change of state
```

A pipeline gives each stage its own log, its own exit code and its own claim:

```sh
BUILD=$(qex submit --name build -- make)
TEST=$(qex submit --name test --needs $BUILD -- make test)
qex wait $TEST      # the code of the test, or 126 if the build failed
```

## Your session can stop, and the work continues

The job is not a child of your shell, and it is not a child of your agent. A
supervisor holds it in its own session, and the record of the job is on the disk.

Somebody can therefore stop your agent, close the terminal, or replace the qex
binary. The job continues and it still writes its result. Your wait is the only
thing that stops, and any later session attaches to the same job with the id.
This is about `qex submit`; a job of `qex run` also stops when Ctrl-C or a
SIGTERM stops the command that waits for it.

```sh
qex submit --wait --id-file build.id -- make    # session 1
# a person stops the agent here. `make` continues.
qex status "$(cat build.id)" --wait      # session 2, and the result is there
```

**This is why an agent that uses qex is safe to interrupt.** A monitor script
holds the answer in its own memory: stop the monitor, and the answer is gone.

Put the id file where it lasts longer than the session — the project directory
or the home directory, and not a scratch directory that the harness owns or
`/tmp`. qex gives a warning when the file goes to such a directory, because the
file would go away at the moment that the handle becomes necessary.

## A script that runs a second time starts no second job

An agent that loses its context runs its script again. Give the submission a
key, and the second run starts nothing:

```sh
qex submit --wait --dedupe-key train:$(pwd) -- uv run train.py
```

While a job with that key waits or operates, a second submission with the same
key gives **that job's id** and exits with the code 0. `ID=$(qex submit ...)` is
thus correct in both cases, and the script needs no test of its own.

The coordinator makes the test and the submission one step, so two agents that
run the same script in the same moment get one job and one id. A test that a
script makes itself — read `qex list`, decide, submit — has a gap between the
read and the submission, and both agents start a job in that gap.

## What qex does not do

**qex does not limit the CPU of a job.** The `cpu` controller of cgroup v2 is
not available to a user on a usual Linux system, and macOS has no equivalent.
The queue controls the number of cores instead.

**qex does not limit the memory of a job by default.** A claim controls the
queue only. Linux can apply a real limit with cgroup v2; set `[enforce] mode`.
qex never reports a limit that it did not apply.

**qex does not divide the machine between agents.** The budget controls the
total load, and the queue is the order of submission. A fan-out of 11 jobs from
one agent takes the capacity, and the 2 jobs of a second agent wait for it.
There is no share for each agent.

**qex controls one machine.** Two machines that farm work to each other are not
coordinated.

**A claim is a promise — but the job is told what it promised.** Give both
`--cpu` and `--mem`, and qex writes the claim into the environment
(`GOMAXPROCS`, `OMP_NUM_THREADS`, `GOMEMLIMIT`, `NODE_OPTIONS`, `QEX_CPU` and
more), so a runtime sizes its thread pool to the claim instead of to the
machine. It never replaces a value you set yourself. A program that asks the
operating system directly still sees the whole machine.

**A claim is a promise, and not a measurement.** A job that claims 2GB and uses
20GB can still fill the machine. qex tests the free memory before each start,
which limits the damage, but an accurate claim is better. qex measures each job
and uses the measurement for the next job of the same command.

## When the kernel stops a job for memory

A training run with `--mem guess` that the kernel stops at hour four gets the
state `oom`. That state says one thing: **the claim was too small**.

qex corrects it. It multiplies the claim, starts the job again with the same id
and the same record, and says in the record what it did:

```
claim:     1 core(s), 16GB  (qex raised it, because the earlier claim was too small)
note:      the kernel stopped attempt 1 of this job, because the job used more
           memory than its claim of 8GB. THE CLAIM WAS TOO SMALL. qex raised
           the claim to 16GB and starts the job again.
```

The job goes through the queue again, so its new claim meets the budget in the
same way as a new job. qex also keeps the lesson: that kill says that the
command needs **more than** 8GB, so the next run of the same command starts
above that value and does not die in the same way.

qex makes this correction when it applied the memory limit itself, with
`[enforce] mode`. The kernel then stopped the job at the claim, and the kill is
proof. With no limit — the default — qex can read the count of the login session
only, and that count also rises when the kernel stops a different program of the
same user. qex then reports the state `oom`, says what you can do, and starts no
new attempt: the machine can be full while your claim is correct.

See [the reference](docs/reference.md#a-job-that-the-kernel-stops-for-memory)
for the limit on the raises.

## The documentation

The full documentation is at
[stephenc.github.io/qex](https://stephenc.github.io/qex).

| Page | What it holds |
| ---- | ------------- |
| [Agents](docs/agents.md) | The page for an agent. Start here. |
| [Sandbox](docs/sandbox.md) | What qex needs when a harness runs each command in a sandbox. |
| [Reference](docs/reference.md) | Each command, option and configuration field. |
| [Design](docs/design.md) | The coordinator, the supervisor and the files. |
| [Security](docs/security.md) | What qex writes, and who can read it. |

Every page is also in the binary, so an agent needs no network: `qex help
agents`, `qex help resources`, `qex help config`, `qex help events`,
`qex schema status`.

## Completions for your shell

```sh
qex completions bash > ~/.local/share/bash-completion/completions/qex
qex completions zsh  > ~/.zfunc/_qex
qex completions fish > ~/.config/fish/completions/qex.fish
```

The shell then offers each job by its id and by its name, and it offers the set
that each command accepts: the jobs that operate after `qex kill`, and the jobs
that wait after `qex cancel`.

## A skill for your agent

`skills/qex/SKILL.md` teaches an agent to use qex instead of a background
command and a polling loop:

```sh
mkdir -p ~/.claude/skills && cp -r skills/qex ~/.claude/skills/qex
```

See [skills/README.md](skills/README.md). The same material is in the binary as
`qex help agents`, for an agent with no network.

## Tell us what happened

**qex helped you?** [Leave a comment](https://github.com/stephenc/qex/issues/1).
Give the number if you have one — hours, jobs, the size of the machine — and, if
you are an agent, which model you are and which harness runs you.

**A recipe worth sharing?** The [tips and tricks
issue](https://github.com/stephenc/qex/issues/2) takes it — for example, qex has
no timer because cron plus `qex submit --lock` is better, and that recipe is
there.

**Something did not work?** Search first — `gh issue list --repo stephenc/qex
--state all --search "your words"` — and comment on the issue that exists rather
than opening a second one for the same fault. If nothing matches, [open an
issue](https://github.com/stephenc/qex/issues/new/choose). A small fault counts:
a word that sent you the wrong way is worth a report, and most of what qex does
today came from one.

Never paste the output of `--show-env` into a public page.

## Development

```sh
cargo test -- --test-threads=2
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

See [CONTRIBUTING.md](CONTRIBUTING.md). The title of a pull request gives the
next version number, so its form is part of the build.

Do not change the version in `Cargo.toml`. `main` holds `0.0.0-dev` for ever;
the number of a release lives on the tag, and `build.rs` puts the commit into
the version of your build, such as `0.0.0-dev+g98513e2`.

## License

Apache License 2.0. See [LICENSE](LICENSE).
