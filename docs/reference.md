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
qex submit --each-line FILE [--max-jobs N] -- COMMAND... {}
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

## One job for each line of a file

`--each-line` reads a file and submits one job for each line. Put `{}` in the
command. Each job gets the text of one line in the place of `{}`.

```sh
GROUP=$(qex submit --each-line inputs.txt -- ./process {})
qex list --group $GROUP
```

The jobs share one group id. The group id goes to stdout, and the name and the
id of each job go to stderr, so `GROUP=$(qex submit --each-line ...)` operates
in the same way as `qex pipeline`.

The name `-` reads the lines from standard input:

```sh
ls *.parquet | qex submit --each-line - -- ./convert {}
```

### A line is data, and never a command

qex starts no shell. Each line becomes exactly **one** argument, whatever it
holds: a space, a quotation mark, a semicolon, a dollar sign or a newline. A
file of names that came from a directory listing, a database or another program
is therefore safe.

```
a b"; rm -rf ~; echo $HOME
```

That line gives one argument with those characters in it. Nothing reads them.

To use a shell feature, name the shell, and give the line as an argument. Do
not put `{}` inside the text of the script:

```sh
qex submit --each-line names.txt -- bash -c 'echo "$1" | tr a-z A-Z' _ {}
```

### A line that starts with a dash

A line becomes an argument, so a line such as `-v` or `--out=/etc/passwd`
becomes an **option** of your program. qex cannot know which arguments your
program reads as options, so it does not change the line.

Put `--` in the command before `{}`. Almost every program then reads the line
as data and not as an option:

```sh
qex submit --each-line names.txt -- ./process -- {}
```

This is the same rule as `xargs`. Use it for input that you did not write
yourself.

### Where `{}` goes

`{}` goes in any argument, in the program name, or inside an argument:

```sh
qex submit --each-line urls.txt -- curl -o {}.html https://{}/
```

Every `{}` takes the line. A command with **no** `{}` gives an error and
submits nothing, because each job would then be the same command and the lines
would have no effect. Write `{{}}` for a literal `{}`. Nothing else in the
command changes.

### Which lines give a job

| The line | The result |
| -------- | ---------- |
| ordinary text | one job |
| an empty line | no job |
| a line that starts with `#` | no job, it is a comment |
| space at the start or the end | qex removes it |
| a CRLF ending | the same job as an LF ending |
| no final newline | the last line still gives a job |

qex writes to stderr how many lines it passed over. A line that you expected to
run never goes away in silence.

A file that is not UTF-8 gives an error with the line number, and qex submits
nothing. The command of a job is text, so qex cannot run such a line.

### All or nothing

qex tests the command, reads the whole input and makes every job specification
before it submits the first job. A fault in the input gives an error and no job
at all, in the same way as `qex pipeline`. A fan-out that stopped in the middle
would leave a part of the work in the queue and a part with no job.

### The limit

`--each-line` submits 1000 jobs at most. Each job holds a directory in the
state of qex, so a file with 100000 lines would fill the disk. The limit asks
no question, because an agent cannot answer one. Raise it when you need to:

```sh
qex submit --each-line big.txt --max-jobs 5000 -- ./process {}
```

### The name of each job

Each job gets a name for `qex list`: the program name, the position in the
file, and as much of the line as fits.

```
process-01-data-a.csv
process-02-data-b.csv
```

The position comes before the line and it has the same width for every job, so
the names sort in the order of the file and two long lines never give one name.
Give `--name` to change the first part and the name of the group.

A name holds the letters, the numbers, `.`, `_` and `-` only. A line can hold a
terminal control sequence, and a name goes to your terminal in `qex list`.

### The other options

`--cpu`, `--mem`, `--timeout`, `--lock`, `--tag`, `--priority`, `--env`,
`--needs`, `--after` and `--retries` apply to every job of the fan-out.

qex calculates the claim one time, from the command of the **first** line, and
gives it to every job. The lines of a fan-out are the same kind of work, so one
claim is correct for them.

### A fan-out learns as one task

qex records what each job used, and gives that measurement to the next job of
the same command. A fan-out does not fit that rule: `./process a.csv` and
`./process b.csv` are two commands, and each one runs one time.

qex therefore measures every job of a fan-out against the **template**
`./process {}`. One fan-out makes one record, and the second run of the same
fan-out gets its claim from the first run. Without this rule a fan-out of 1000
lines would add 1000 records that no later job can use.

The record keeps the largest measurement, so the claim goes to the size of the
largest line. `qex status` says `(from the earlier jobs of this fan-out)` for
such a claim, and not `(from the earlier jobs of this command)`: the command of
one line can have no measurement at all.

`--id-file` writes the group id and the id of each job. A name that ends in
`.json` gives a JSON object.

`qex run` does not accept `--each-line`. It waits for one job and gives the
output and the exit code of that job.

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

## More help inside the tool

Each topic below is also in the binary, so an agent needs no network:

```sh
qex help agents      the one page for an agent
qex help job-file    the fields of a job file
qex help resources   claims, the budget and the several-user accounting
qex help each-line   one job for each line of a file
qex help states      each job state and what causes it
qex help exit-codes  the exit code of each command
qex help config      each configuration field
qex schema job       the JSON Schema of a job file
qex schema status    the JSON Schema of status.json
```
