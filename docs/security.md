---
title: qex security
description: What qex writes to the disk, who can read it, and where the trust boundaries are.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

# Security

## What qex writes, and who can read it

| Path | Mode | What it holds |
| ---- | ---- | ------------- |
| `~/.local/state/qex/jobs/<uuid>/` | 0700 | one job |
| `spec.json` | 0600 | the command, the directory, the claims and THE CAPTURED ENVIRONMENT |
| `status.json` | 0600 | the state, the exit code, the times and the measured use |
| `stdout.log`, `stderr.log` | 0600 | everything that the job wrote |
| `hook.log` | 0600 | everything that the stop hook wrote |
| `~/.config/qex.toml` | your mode | the configuration |
| `/tmp/qex/u<uid>/` | 0755 | the claims of your coordinator, for the other users |

The job directory and the files in it are for you only. No other user of the
machine can read the environment of your job, or its output.

## The environment

`qex submit` copies your environment by default, because a job must operate in
the same way as a command that you type now. **That environment goes to the disk
in `spec.json`.** A shell that holds a token gives that token to the file.

Three answers:

```sh
qex submit --env-capture minimal -- ...   # PATH, HOME, USER, LOGNAME, SHELL, LANG, TZ
qex submit --no-env-capture -- ...        # nothing at all
qex submit --env-capture all -- ...       # everything (the default)
```

`qex status` hides the environment, and `qex status --json` hides it as well.
Add `--show-env` to see it. This is deliberate: an agent that writes the output
of `qex status --json` into a log must not write your token into that log.

**The command line is not hidden.** `qex list` and `qex status` show the program
and its arguments, and the directory. Put a secret in the environment or in a
file, and never in an argument.

## The command when a job stops

`[hooks] on_stop` names a command that qex starts each time a job stops. The
command is in your config file, so it is your command, and qex trusts it in the
same way as it trusts a job that you submit.

**The data of the job is not yours.** A job name comes from the person or the
agent that submitted the job, and the output of a job comes from the program.
qex therefore gives the values of the job to the hook IN THE ENVIRONMENT, and it
builds no command line from them. A job with the name `; rm -rf ~` gives a hook
the variable `QEX_JOB_NAME` with those letters in it, and never a command.

qex starts no shell for the hook, in the same way as for a job. A shell that you
name in `on_stop` reads the variables, and the quotation is then yours to write.

The hook has a time limit and a size limit. Its output goes to `hook.log` in the
job directory with mode 0600, and `qex logs <id> --hook` reads that file. A hook
that fails changes no job.

## The several-user accounting

Each coordinator writes its claims to `/tmp/qex/u<uid>/`, and it reads the
records of the other users before it starts a job.

**This method is cooperative, and it is not a control.** A different user can
write an incorrect value, and qex will believe it. The threat model is a
colleague or an agent that does not know about your work. It is not a person who
wants to damage your work.

qex protects the method against the usual faults of a directory that everybody
can write:

- `/tmp/qex` must be a directory, and it must have the sticky bit, and it must
  belong to root or to you. Without those, the accounting stops and qex operates
  for one user only.
- qex reads the owner of each file, and it discards a file whose owner does not
  agree with the name of its directory.
- qex opens each file without following a symbolic link.
- qex discards a record that is old, or whose process is dead, or that comes
  from an earlier start of the machine.
- A file that qex cannot read gives no error. It is discarded.

qex also tests the free memory of the machine before each start, so it finds a
load that no coordinator reports.

Turn the method off with `[peers] enabled = false`.

## The limits on a job

qex applies no memory limit by default. A claim controls the queue only.

Linux can apply a real limit with cgroup v2. Set `[enforce] mode` to `soft` or
`hard`. The coordinator needs a cgroup that it owns, and a coordinator that
starts from a login shell frequently does not have one.

**qex never reports a limit that it did not apply.** `qex config show` says `NOT
ACTIVE` with the reason, and a job whose limit qex could not apply records that
fact in its own status, where `qex status` shows it. A configuration file that
qex cannot read gives the same treatment: the job continues with the default
values, and the record says that no limit operates.

## The coordinator and the version

A coordinator can operate for hours, and a new build can replace the program in
that time. The CLI then holds options that the coordinator does not know, and a
JSON field that a program does not know is IGNORED, in silence.

qex asks the coordinator what it can do, and it REFUSES a job that needs
something the coordinator cannot obey. A user who writes `--lock target` against
an earlier coordinator gets an error and a remedy, and not a job id for a job
that would run with no lock.

## Reporting a fault

Write to the [security
advisories](https://github.com/stephenc/qex/security/advisories/new) of the
repository, and not to a public issue.
