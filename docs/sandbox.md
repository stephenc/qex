---
title: qex inside a sandbox
description: What qex needs from a sandbox, how to test it, and what to change when an agent reports that qex cannot start.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md) · [Sandbox](sandbox.md)*

# qex inside a sandbox

**This page is for a person.** An agent that meets this fault cannot correct it
by itself: the permissions belong to whoever starts the agent.

Some agent harnesses run every command inside a sandbox. Codex uses
[bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`) on Linux, and
other harnesses use a container, `seccomp` or macOS Seatbelt. A sandbox of that
kind can stop qex completely, and the message that an agent sees is:

```
qex cannot make a Unix socket in /home/you/.local/state/qex/run:
Operation not permitted (os error 1).
```

That is not a fault in qex or in the job. It says that the sandbox refuses the
one thing that qex needs.

## What qex needs

qex keeps **one coordinator process** for each user, and every qex command talks
to it over a **Unix-domain socket**. The queue exists so that commands from
different agents and different sessions see one another; a design with no shared
process cannot do that. Four things must hold:

| | |
| --- | --- |
| A writable state directory | `$XDG_STATE_HOME/qex`, or `~/.local/state/qex`. It holds the records, the logs and the socket. |
| Permission to `bind` and `connect` an `AF_UNIX` socket in it | The coordinator binds; every command connects. A sandbox that refuses either one stops qex. |
| The same directory in every qex command | A sandbox that gives each command its own private `/tmp` or its own state directory gives each command its own empty queue. |
| A process that lives after the command ends | The coordinator and the jobs outlive the command that started them. That is the property that makes a job survive a person who stops the agent. |

## Test it in one command

Run this **inside** the sandbox, as the agent runs:

```sh
qex info
```

A queue that answers prints the coordinator, the budget and the load. If it
reports that it cannot make a Unix socket, the sandbox is the cause. To test
the socket alone, with no qex:

```sh
python3 -c 'import socket, os
d = os.environ.get("XDG_STATE_HOME", os.path.expanduser("~/.local/state")) + "/qex/run"
os.makedirs(d, exist_ok=True)
p = os.path.join(d, "probe")
os.path.exists(p) and os.unlink(p)
s = socket.socket(socket.AF_UNIX); s.bind(p); s.close(); os.unlink(p)
print("a Unix socket is allowed in", d)'
```

Test the directory that **qex** uses, and not `/tmp`: a sandbox frequently gives
`/tmp` its own rules. `qex info --no-start` names the directory when qex can
read it.

## What to change

**Take one of these. The first is the simplest.**

### 1. Let the qex commands run outside the sandbox

Every harness has a way to approve a command or a program. In Codex, approve the
`qex` commands, or add `qex` to the programs that run with elevated permission.
qex is a queue for long work, so it is a reasonable thing to allow: it starts
the same programs that the agent already runs, and it starts fewer of them at
one time.

### 2. Give the sandbox the state directory and the socket

With `bwrap`, the state directory must be a real, shared, writable mount, and it
must be the same one for every command:

```sh
bwrap \
  --bind / / \
  --dev /dev \
  --proc /proc \
  --bind "$HOME/.local/state/qex" "$HOME/.local/state/qex" \
  --setenv XDG_STATE_HOME "$HOME/.local/state" \
  -- your-agent
```

The important part is that the directory is **bound**, and not a fresh `tmpfs`
for each command. A `--tmpfs /tmp` or a private state directory gives each
command its own queue, and the queue is then useless: no command finds the jobs
of any other.

If the sandbox filters system calls, `socket`, `bind`, `connect` and `listen`
for `AF_UNIX` must be allowed. qex needs no network socket, and it opens none.

### 3. Put the queue somewhere the sandbox already allows

qex follows the XDG variables, so you can move the whole queue into a directory
that the sandbox shares:

```sh
export XDG_STATE_HOME=/workspace/.qex-state
export XDG_CONFIG_HOME=/workspace/.qex-config
```

Give the **same** values to every command, and to the agent itself. A directory
inside the workspace is frequently shared already, because the agent writes
files there.

This corrects the state directory only. It does **not** help when the sandbox
refuses the socket itself; the message names that case exactly.

## What does not work

- **A per-command sandbox with no shared directory.** Each command then starts
  its own coordinator, sees no other job, and the queue cannot do the one thing
  it exists for.
- **A sandbox that stops every process at the end of the command.** The
  coordinator and the jobs live longer than the command on purpose. A harness
  that kills them stops the work, and `qex list` then shows a job that no
  process serves.

## If nothing above is possible

Use `qex run` for the work that you can wait for, and accept that the queue is
one command deep, or run the agent with no sandbox on a machine that you own.
qex has no mode that removes the coordinator: the coordinator IS the queue, and
a design with none cannot stop two agents from filling the machine, which is the
reason qex exists.

Tell us what your sandbox does, and what you had to change:
[issue 2](https://github.com/stephenc/qex/issues/2) holds the recipes.
