---
title: qex for agents
description: The one page that an agent needs, and the property that makes qex safe to interrupt.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

# qex for agents

This page is also in the binary. Run `qex help agents` to read it with no
network.

## The three commands

```sh
ID=$(qex submit --cpu 2 --mem 4GB -- uv run train.py)
qex wait $ID
qex logs $ID
```

`qex submit` writes the id to stdout and writes nothing else, so `ID=$(qex
submit ...)` is correct. A warning goes to stderr.

For work that you wait for now, put `qex run` in front of the command instead:

```sh
qex run -- make test
```

The output arrives as it happens, and the exit code is the exit code of the job.

## Your session can stop, and the work continues

**This is the property that makes qex safe for an agent that a person can
stop.**

The job is not a child of your shell, and it is not a child of your agent. qex
starts a supervisor in its own session, and the supervisor starts the job.

| What happens | The job |
| ------------ | ------- |
| Somebody stops your agent | continues |
| Your terminal closes | continues |
| The coordinator stops, or a new version replaces it | continues, and it still writes its result |

Nothing is lost, because the record of the job is on the disk and not in the
memory of a process. Your wait is the only thing that stops.

You can therefore attach the wait again, at any time, from any session:

```sh
qex submit --id-file build.id -- make    # session 1
# a person stops the agent here. `make` continues.
qex status "$(cat build.id)" --wait      # session 2, and the result is there
```

The id is the handle. That command gives the same answer whether the job
operates now, stopped one second ago, or stopped last night.

**Put the id file where it lasts longer than your session.** Your project
directory or your home directory is correct. A scratch directory that your
harness owns is not correct, and neither is `/tmp`. The job continues when the
session stops, but the file goes with the session, and you then have no handle
for a job that still operates. qex gives a warning when the file goes to such a
directory.

If you lose an id, `qex list` shows each job with its directory and its command,
and `qex list --cwd .` shows the jobs of this directory only.

A monitor script cannot do this. A monitor holds the answer in its own memory:
stop the monitor, and the answer is gone. Keep the id in a file, and the answer
waits for you instead.

**Before you start work again in a new session, ask first.** `qex list` shows
what already operates, and `qex status <id>` gives the result of what stopped.
A person who stops you pays nothing, and a person who stops you and then gets
the same build twice pays for it.

## Do not write a monitor script

Every monitor that you write waits for a **proxy**: a pattern in the process
list, a line in a log file, a file that appears. A proxy can become permanently
false, and nothing tells the monitor. It then waits for ever.

Four monitors were measured on one machine in one day, and together they slept
for 95 hours. Not one of the conditions could ever become true. Three of them:

```sh
while pgrep -f "solve.py"; do sleep 60; done      # matches its own command line
until grep -q "DONE" run.log; do sleep 60; done   # the writer was killed
until grep -q "READY" ~/other.log; do sleep 60; done  # that file never existed
```

A different user found another kind later, on a machine that two agents shared.
It had slept for 63 hours:

```sh
while true; do M=$(ps -Ao args | grep -c solver)
               K=$(ssh other-host 'ps -Ao args | grep -c solver')
               [ $M -eq 0 ] && [ $K -eq 0 ] && break; sleep 300; done
```

It waits for a COUNT to reach zero. The work of its author finished two days
before, and the work of the OTHER agent held the count above zero. It also
opened about 750 connections to the other machine while it waited.

The last three hold no pattern fault. They are careful commands. The fault is
the proxy: a log line is evidence of the work, and evidence stops when the work
stops, in a way that the monitor cannot see. On a machine that two agents share,
"wait until nothing matches" can never become true, because the condition
depends on somebody else.

qex waits for the process, and not for a proxy of the process.

```sh
qex watchers
```

That command finds the monitors of this kind on your machine. It removes its own
process and the processes that started it before it reports anything, so it
never finds itself.

## If you operate inside a harness

`qex wait` blocks, and your harness — not qex — reports when a background
command ends. Put the two together:

```sh
ID=$(qex submit -- make test)   # gives the id at once
qex status $ID --wait           # run THIS in the background of your harness
```

Use `qex status --wait` and not `qex wait` here. It blocks in the same way and
gives the same exit code, and its output also holds the state, the exit code and
the last lines of the error output. One command gives everything.

## The exit codes

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

## Two fields that need care

**`pid` is null after the job stops.** While the job operates, `pid` gives the
process. After the job stops, the machine can give that number to a different
process, so qex removes it. The historical value is in `last_pid`, and it is for
a reader only: never send a signal to it. To stop a job, use `qex kill <id>`,
which is correct at each moment.

**A claim is a promise, and not a measurement.** Give `guess` when you do not
know the size, and read `usage` in `qex status --json` after the job to learn
the true size. qex also learns it for you and uses it for the next job of the
same command in the same directory.

## Tell the people who make qex

qex exists because somebody measured their own monitor scripts and gave the
numbers: four of them on one machine, 95 hours between them. Every part of this
page came from a report of that kind.

**Something helped you** —
[leave a comment on the testimonials issue](https://github.com/stephenc/qex/issues/1).
Give the number if you have one: the hours, the jobs, the size of the machine.
Say **which model you are and which harness runs you**, because the faults are
not the same in each harness: one owns a scratch directory and loses an id file,
another reports a background command and needs `qex status --wait` instead of
`qex wait`.

**A recipe that is not in the documentation** —
[the tips and tricks issue](https://github.com/stephenc/qex/issues/2). qex has no
timer of its own, because cron with `qex submit --lock` does it better; that
recipe is there, with the guard that stops the queue from growing at each tick.

**Something did not work** — **look for it first, and add to what is there.**

```sh
gh issue list --repo stephenc/qex --state all --search "the words of your fault"
gh issue comment <number> --repo stephenc/qex      # add your machine and harness
```

A second report of one fault is useful: it says that the fault is common, and it
frequently holds the detail that the first one did not. A second *issue* for one
fault is not: it divides the answer between two pages, and the person who
corrects it must read both and close one. Search the closed issues as well —
your fault may be corrected already, in a version later than yours.

[Open a new issue](https://github.com/stephenc/qex/issues/new/choose) when
nothing matches. A **small** fault is worth one: an option that you looked for
and did not find, a message that made you look in the wrong place, an exit code
that surprised you. Those reports are the ones that make a tool good, and each is
cheaper to correct than the hour it costs the next reader.

Give four things, and no more:

1. The command that you gave, in full.
2. What qex wrote, in full. Use `--json` where the command has it.
3. What you expected instead.
4. `qex version`, and `qex info --no-start` if a coordinator operated.

> **Never put the output of `--show-env` in a public page**, and never anything
> that a token can hide in. `qex submit` copies the environment of your shell to
> `spec.json`, so that output is a secret. See [security](security.md).

If you cannot reach GitHub, give the words to the person that you work with and
let them decide whether to send them.

## Install this page as a skill

Your operator can put this material where you load it without asking:

```sh
mkdir -p ~/.claude/skills/qex
curl -fsSL https://raw.githubusercontent.com/stephenc/qex/main/skills/qex/SKILL.md \
  -o ~/.claude/skills/qex/SKILL.md
```

The page is ordinary Markdown, so it goes into `AGENTS.md`, a rule file or a
prompt with no change. `qex help agents` holds the same material inside the
binary, for a machine with no network.

## Everything else

`qex help <topic>` covers `job-file`, `resources`, `states`, `exit-codes` and
`config`. `qex schema job` and `qex schema status` give the JSON Schema of each
format. See the [reference](reference.md) for the full command list.
