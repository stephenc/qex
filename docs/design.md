---
title: How qex operates
description: The coordinator, the supervisor, the files on the disk, and the limits.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

# Design

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
    hook.ran       the record that the stop hook of this job ran (mode 0600)
    hook.log       the output of that hook (mode 0600)
```

`hook.ran` gives the stop hook one run for each job. Several processes can make
a job terminal: the supervisor writes the usual result, and the coordinator
writes `cancelled`, `skipped` and the failure of a job whose supervisor stopped.
Each of them makes this file with `create_new` before it starts the hook, and
that operation succeeds for one process only. A person thus receives one
message, also when the coordinator stops and starts again while the job runs.

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

## The property that an agent needs most

The job is not a child of your shell, and it is not a child of your agent. The
supervisor holds the job in its own session, and the record of the job is on the
disk.

Somebody can therefore stop your agent at any moment. The job continues, it
writes its result, and a later session attaches to the same job with the id:

```sh
qex status "$(cat build.id)" --wait
```

A monitor script cannot do this, because a monitor holds the answer in its own
memory. Stop the monitor, and the answer is gone.

## The promise about the format on the socket

The CLI and the coordinator speak JSON on a Unix socket. A coordinator can
operate for hours, and a new build can replace the program in that time, so a
mixture of versions is normal and qex must say what it promises.

**The format is additive only.**

- A new version can add a field to a request or to a response.
- A new version can add a request name.
- A new version does not change the meaning of a field that exists, and it does
  not change the meaning of a request name that exists.
- A change that cannot obey those rules takes a new name, and a capability gates
  it.

Each side thus ignores what it does not know, and that is correct: a field that
a program does not know is a field that did not exist when somebody made that
program, so no earlier behaviour depends on it.

**A newer coordinator is therefore safe, and an older coordinator is not.** An
early CLI sends the fields that it knows, and a new coordinator understands each
of them. In the opposite direction, a new CLI can send an option that an old
coordinator ignores in silence — and a job that runs without the lock that you
asked for is worse than a job that does not run. qex thus asks the coordinator
what it can do, and it refuses the job with the option and the remedy.

## The capability floor

A coordinator says what it can do from the first published version and above.
One below that number gives no answer, so the CLI cannot learn which options it
obeys, and it must not let a user believe a rule holds when it may not. It
refuses such a coordinator and says how to replace it:

```
the coordinator (pid 4321) is version 0.5.2, and a coordinator says what it can
do from 0.6.0 and above.
...
    kill 4321
The jobs that operate now continue; a new coordinator reads the same records.
```

The jobs are safe in that operation. A coordinator holds no result: the record
of each job is on the disk, and a new coordinator reads the same records.

### A development build gets a warning, and not a refusal

A build that a person makes reports a version such as `0.0.0-dev+g98513e2`,
which names the commit that it holds. qex names such a version a development
build, and a coordinator from one gets a warning only.

The floor is a backstop for a coordinator so early that it came before the
capability handshake: such a build cannot say what it cannot do, so the only
safe answer is to refuse it. A development build is not that. It answers the
`Capabilities` request like every other build, so qex still refuses each option
that it cannot obey, and it names the option. The floor is the coarse gate, and
the capability test is the exact one.

A refusal would make every build that a person makes unusable by its own CLI,
which is a worse fault than the fault that the floor guards against.

## Three questions that a user asked, with the answers

### Does the memory test understand swap and reclaimable cache?

**Reclaimable cache: yes. Swap: deliberately not.**

On Linux the number is `MemAvailable`, which the kernel calculates. It counts
the page cache that the kernel can reclaim, so a machine with 12GB of cache does
not look full. On macOS the number is the free pages, the inactive pages, the
purgeable pages and the speculative pages, which is the nearest equivalent.

Memory that the kernel wrote to swap is **not** in that number, and this is the
case to know about:

```
MemAvailable 1.6GB, 10GB in swap, no page operations, the machine is healthy
```

A job that claims 6.5GB waits there, although the machine can supply the memory
by taking it back from swap. qex now says so in the reason: when the pressure is
low, the reason states that the machine is not short of memory, that the number
does not count the memory in swap, and that a smaller claim or a lower
`reserve_mem` corrects it.

The pressure, and not the free memory, is the measurement that separates a
machine that is full from a machine that parked memory nobody wants. qex reads
it (`/proc/pressure/memory`, Linux only) as a second test with `max_pressure`.
**qex does not yet use the pressure to RELEASE a job that the free-memory test
holds.** That is the correct change for this case, and it needs a machine in
that state to test it, so it waits.

### Does the learned claim record the peak memory?

**Yes. The peak, and the largest peak of the samples.**

The measurement is `max_rss` from `getrusage(RUSAGE_CHILDREN)`, which is the
highest memory that the job reached, and not its memory at the end and not an
average. qex keeps several samples and uses the **largest** of them with a
margin.

This is deliberate, and the reason is the job that climbs for hours: a claim
that is too small stops the job, and a claim that is a little large costs
capacity only.

qex records a job that COMPLETED only. A job that somebody stopped, or that the
out-of-memory killer stopped, shows the memory that it reached and not the
memory that it needs, and that number would teach qex the wrong size.

### Is there fairness between agents?

**No. It is capacity, and then the order of submission.**

Each user has one coordinator, and each coordinator holds its own queue in the
order of submission, with the priority first. Between users, the coordinators
publish their claims to `/tmp/qex` and each one subtracts the claims of the
others from its budget.

That controls the total load, and it does NOT divide the load. A fan-out of 11
jobs from one agent takes the capacity, and the 2 jobs of the other agent wait
until it ends. Nothing gives a share to each agent, and nothing reserves a slot
for the agent that arrives second.

Use `--priority` inside one queue. Between agents there is no answer in qex
today, and a user who needs one must divide the budget by hand, with `[budget]`
in the configuration of each user.

## The boundary: one machine

qex controls one machine. Two machines that farm work to each other are not
coordinated, and the accounting on each machine sees its own jobs and the jobs
of the other users OF THAT MACHINE only.
