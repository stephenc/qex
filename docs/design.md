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

## The support floor

qex supports the first published version and above. A coordinator below that
number comes from a build that no release holds, so no promise covers it, and
the CLI refuses it and says how to replace it:

```
the coordinator (pid 4321) is version 0.5.2, and qex supports 0.6.0 and above.
...
    kill 4321
The jobs that operate now continue; a new coordinator reads the same records.
```

The jobs are safe in that operation. A coordinator holds no result: the record
of each job is on the disk, and a new coordinator reads the same records.
