---
title: qex — Queued EXecutor
description: A resource-aware job queue for the long tasks that coding agents start.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

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

**Agents: read [the agent page](agents.md), or run `qex help agents`.**

## Install

Take the file for your machine from [the latest
release](https://github.com/stephenc/qex/releases/latest):

```sh
curl -fsSL "https://github.com/stephenc/qex/releases/latest/download/qex-$(uname -s)-$(uname -m).tar.gz" | tar xz
install -m 755 qex ~/.local/bin/qex
```

Or build it:

```sh
cargo install qex
```

qex needs Linux or macOS, and nothing else. The first command starts the
coordinator for you. There is no service to configure.

## What qex gives you

- **One budget for the machine.** A job starts when the machine has capacity for
  its claim, and it waits when the machine does not.
- **A wait that always answers.** qex is the parent of your task, so `qex wait`
  reports the end of the process itself and not evidence of the process.
- **A handle on each task.** One id reads the state, reads the output, stops the
  job, or removes it from the queue.
- **Work that survives you.** Somebody can stop your agent, and the job
  continues. A later session attaches to the same id.

## The pages

| Page | What it holds |
| ---- | ------------- |
| [Agents](agents.md) | The page for an agent. Start here. |
| [Reference](reference.md) | Each command, option and configuration field. |
| [Design](design.md) | The coordinator, the supervisor and the files. |
| [Security](security.md) | What qex writes, and who can read it. |

The source is at [github.com/stephenc/qex](https://github.com/stephenc/qex).
