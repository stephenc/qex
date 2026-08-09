---
title: qex reference
description: Every command, option, claim word and configuration field.
---

*[Home](index.md) · [Agents](agents.md) · [Reference](reference.md) · [Design](design.md) · [Security](security.md)*

# Reference

## Commands

```
qex submit [--cpu N] [--mem SIZE] [--timeout TIME] [--max-queue-time TIME]
           [--needs ID,ID] [--after ID,ID] [--name NAME] [--job FILE] [--json]
           [--dedupe-key KEY] [--dedupe-window TIME] -- COMMAND...
qex submit --each-line FILE [--max-jobs N] -- COMMAND... {}
qex wait   <id>... [--timeout TIME] [--passthrough]
qex list   [--state STATE] [--tag TAG] [--json]
qex status <id> [--json] [--show-env]
qex logs   <id> [--follow] [--tail N] [--stdout|--stderr] [--hook]
qex kill   <id>...          stop a job that operates
qex cancel <id>...          remove a job from the queue
qex clean  [<id>|completed|done|--state STATE|--older-than 7d|--all]
qex events [--json] [--since STREAM:SEQ|start|now] [--count N] [--timeout TIME]
qex info                    the coordinator: its pid, its budget and its load
qex pause  [queue|lock NAME] [--reason TEXT] [--for TIME] [--drain]
qex resume [queue|lock NAME]
qex config show             the values that qex uses now
qex schema job|status|pipeline|event    the JSON Schema of each format
qex completions <shell>     the completions for bash, zsh or fish
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
| 123  | The job never started. It reached its `--max-queue-time`. |
| 124  | Your wait reached its time limit. The job continues. |
| 125  | Something stopped the job: kill, cancel, timeout or out-of-memory. |
| 126  | The job did not run, because a job that it needed failed. |
| 127  | There is no job with that id. |

The code 124 has the same meaning as the code of the `timeout` command. A
timeout on `qex wait` stops your wait only. It does not stop the job.

The code 123 is not 125. A job with the code 125 ran and wrote output. A job with
the code 123 never got the machine, so it wrote nothing.

Add `--passthrough` to exit with the exit code of the job.

### Exit codes of `qex run`

| Code | Meaning |
| ---- | ------- |
| the exit code of the job | The job ran. `qex run -- sh -c 'exit 7'` gives 7. |
| 123  | The job never started. It reached its `--max-queue-time`. |
| 124  | Your wait stopped, and the job continues. See the dedupe key. |
| 125  | Something stopped the job: kill, cancel, Ctrl-C, timeout, out-of-memory. |
| 126  | The job did not run, because a job that it needed failed. |
| 127  | There is no job with that id. |

`qex run` writes the output of the job, so it gives the exit code of the job
when the job RAN.

A job of `qex run` is a job like any other, so `qex kill` and `qex cancel` from
a different command can stop it. Such a job gave no exit code of its own, and
`qex run` then gives 125. `qex run` also writes a line to stderr that names the
cause, and that line says when this command did not stop the job.

The code 1 has two causes. Your work ran and it gave the exit code 1, or qex
could not finish its own work: the coordinator stopped while `qex run` waited,
for example. qex writes the second cause on stderr, and the job can then still
operate.

For each state in which the job gave NO exit code of its own, `qex run` gives
the same code as `qex wait`. For a job that RAN, `qex run` gives the exit code
of the job, and `qex wait` gives 0 or 1 unless you add `--passthrough`.

`qex run` gives 124 in ONE case: a dedupe key gave it the job of a different
caller, and a signal then arrived. This command did not start that job, so
Ctrl-C stops this wait and the job continues. Run `qex status $ID --wait` to
wait again, or `qex kill $ID` to stop the job.

`qex run` gives 124 for no other reason. It waits with no limit of its own, and
a job that reaches the time limit of `--timeout` gives 125, because something
stopped that job.

## A job that never starts

A job waits until the machine has capacity for it. Use `--max-queue-time` to
limit that wait:

```sh
ID=$(qex submit --max-queue-time 30m -- make test)
qex wait $ID              # this gives an answer inside 30 minutes
```

The job does not start after that time. Its state becomes `expired`, `qex wait`
gives the code 123, and the `error` field says what the job waited for and how
long it waited.

The remedy in that field fits the cause, because the three causes take three
different corrections. A job that waited for CAPACITY takes a smaller claim, a
quiet machine or a longer limit. A job that waited for a job in `--needs` takes
a limit that covers the whole pipeline, because a smaller claim changes nothing
for it. A job that waited for a `--lock` takes a limit that covers the longest
job with the same lock, or two different lock names, because qex gives a lock to
one job at a time whatever the machine has free.

A job that needs this job gets its answer at the same moment. `qex wait` on such
a job returns with the code 126 and the state `skipped`: measured at 3.5 seconds
with a limit of 3 seconds.

`--timeout` limits the time that the job **runs**. `--max-queue-time` limits the
time that the job **waits**. A job that reaches the first has output to read; a
job that reaches the second has none.

The clock starts at the submission. A coordinator that stops and starts again
continues the same count, so a restart does not give a queued job a new full
wait.

**qex counts the wait in whole seconds.** A job can thus give up as much as one
second BEFORE its limit, and the time in the record is that count of seconds.
Give a limit of a minute or more, where one second changes nothing. With a limit
of `3s`, 65 measured runs of `qex wait` returned between 2.0 and 3.1 seconds
after the submission, on an idle machine and on a busy one alike.

The wait for a job in `--needs` counts also, because the option answers one
question: does this id give an answer inside this time? A clock that stopped for
a dependency could not answer it. Give a stage of a pipeline a value that covers
the whole pipeline, or give it no value.

There is no value by default, and the config file has none until you write one. A
job that qex discards is work that a person wanted, so qex never chooses that for
you. Write `[defaults] max_queue_time` to get the rule for every job.

A job takes this value at its SUBMISSION. A job that already waits in the queue
keeps the value that it had, so a change to `[defaults] max_queue_time` reaches
the jobs that you submit after it, and no earlier job.
## The event stream

```sh
qex events --json
```

One JSON object on one line for each change of state, at the moment of the
change. Read this stream in place of a loop that asks about each job. An agent
that drives twenty jobs reads one stream, and it learns of each result when the
result happens.

```json
{"event":"job","seq":12,"time":1770000000,"id":"a1b2...","name":"build",
 "state":"failed","previous":"running","change":"state","job":{ ... }}
```

| Field | Meaning |
| ----- | ------- |
| `event` | `stream`, `job`, `gap` or `bye`. Ignore a value that you do not know. |
| `seq` | The number of this event. Keep the largest number that you read, with the `stream_id` of the first line. |
| `state`, `previous` | The state now, and the state before. `previous` is `null` for the first line of a job, which says that qex accepted it. |
| `change` | `state`, or `reason` for a job that stays in the queue and whose reason to wait changed. |
| `job` | The whole record, the same as `qex status --json`. |

`qex schema event` gives the full schema.

The field `job` holds everything, so a reader needs no second command to learn
the exit code, the measured use or the cause of a failure.

The stream reports what the coordinator **saw**. The supervisor of a job writes
the record, and the coordinator reads it twice each second, so a job that is
shorter than that period gives `starting` and then `completed` with no `running`
line. `previous` gives the true sequence, and qex writes no line for a state that
it did not see.

### Read the stream again after a stop

```sh
qex events --json --since "$STREAM_ID:348"   # the events after the number 348
qex events --json --since start              # everything that it holds
qex events --json --since now                # the new events only
```

The default is `start`. A program that keeps two values — the `stream_id` of the
first line and the largest `seq` — and gives both to `--since` loses nothing when
it stops and starts again. **This is the reason for the numbers**: a stream that
begins at "now" makes a reader that restarts lose the results that arrived while
it was away.

**The numbers belong to one stream.** The coordinator stops when no job operates,
and the next command starts a new one. That coordinator starts its numbers at 1
again, and it makes one event for each record that it reads, so the number 348
names a different event there.

With the stream name, qex compares the two, gives a `gap` line that says the
coordinator changed, and continues with the events that the new coordinator
holds. Its job records are the same records.

With a **number alone**, qex cannot make that comparison: it sees a number that
this stream also issued, and it continues from there. You then lose events with
no message. Give the name. `qex events` writes a warning when you give a number
with no name.

### A reader that is slow

The coordinator keeps the last 512 events. **It never waits for a reader**, and
its memory does not grow for one. A reader that falls behind receives a `gap`
line that COUNTS the events that it lost:

```json
{"event":"gap","time":1770000000,"missed":37,"next_seq":420,"reason":"..."}
```

qex reports a gap and never hides one. A reader that loses the line `failed` and
hears nothing waits for a result that will never arrive.

`missed` is `null` when qex cannot count the events, which happens when the
number comes from a different stream: the two streams have no common measure, so
a number there would say something that qex cannot support. `reason` says what
happened.

### The end of the stream

A reader does **not** hold the coordinator open: a stream that keeps a
coordinator alive for ever is a leak. The coordinator stops when no job operates
and no command arrives for the idle time, and it writes a `bye` line first. The
command then exits with the code 0.

A stream that ends with **no** `bye` line means that something stopped the
coordinator. `qex events` writes a message to stderr and exits with the code 1.
The records of the jobs are on the disk and they are correct.

### An earlier coordinator

A coordinator that operates can be older than your command. Such a coordinator
does not know this request, so `qex events` refuses to run, names the
coordinator and gives the command that stops it. It never gives an empty stream,
because an empty stream and a stream with no events look the same.

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

## A submission that a script can repeat

A script that runs a second time must not start a second copy of the work. Give
the submission a key:

```sh
ID=$(qex submit --dedupe-key build:$(pwd) -- make)
```

While a job with that key waits or operates, a second submission with the same
key **starts no job**. qex writes the id of the first job to stdout and exits
with the code 0, so `ID=$(qex submit ...)` gives a usable id in both cases and
the script needs no test.

The reason goes to stderr:

```
qex: this submission started no job. The dedupe key `build_home_me_p` gives
the job 7f3c8a12-..., and that job is in the state `running`.
```

**The coordinator makes the test and the submission one step.** Two agents that
run the same script in the same moment thus get one job and one id. A test that
you write yourself (`qex list`, then decide, then submit) has a gap between the
read and the submission, and both agents start a job in that gap.

### When a key becomes free

| The job with the key | A new submission with that key |
| -------------------- | ------------------------------ |
| waits in the queue, or operates | gets that job's id, and starts nothing |
| stopped, for any reason | starts a new job |
| succeeded inside the `--dedupe-window` of the **new** submission | gets that job's id, and starts nothing |
| its record is deleted | starts a new job |

A key that held a job for ever would give an agent the id of a job of yesterday,
and that answer would look like a success. A key thus stops a second copy of the
work, and it does nothing else.

**A key names the work. qex does not compare the command.** A second submission
with the same key gives you the first job, although you wrote a different
command. A comparison would make the key mean two things, and it would refuse
the legitimate case of the same work with one option more. Give each different
piece of work its own key. The message on stderr names the command of the job
that you get, so you can see which work the key holds.

`--dedupe-window 1h` extends the rule for a caller that wants a completed result
to count. A job that did **not** succeed never keeps its key, whatever the
window is: the remedy for a failure is another run, and a window that blocked it
would make the option dangerous.

**The window of the submission that asks applies, and not the window of the job
that holds the key.** The window is a question — "how old an answer do I
accept?" — and it is not a property of the earlier job. A submission that gives
no window thus starts a new job, although a different submission gave a window a
moment before:

```sh
A=$(qex submit --dedupe-key w1 -- true); qex wait $A
B=$(qex submit --dedupe-key w1 --dedupe-window 1h -- true)   # B is A
C=$(qex submit --dedupe-key w1 -- true)                      # C is a new job
```

Each caller thus states its own rule, and the coordinator holds no policy of its
own. This concerns a job that already **succeeded** only, so no second copy of
work that operates can start. Give the same window in each command that shares a
key.

The key is in the record of the job, so `qex status` and `qex list --json` show
it, and a coordinator that starts again gives each key back to its job.

qex shows the key in the same safe form that it uses for a job name: it keeps
the letters, the numbers, `-`, `_` and `.`, and it replaces each other character
with `_`. A key is text that another agent chose, and a key that held an ESC
byte would move the cursor of the reader. The key that qex holds is the key that
you gave, so `--dedupe-key` still needs the form that you wrote.

### How a script learns which case it got

```sh
qex submit --json --dedupe-key build:$(pwd) -- make
{
  "id": "7f3c8a12-...",
  "deduplicated": true
}
```

`deduplicated` is `false` when this command started the work. Without `--json`,
`qex submit` writes the id alone, and the message goes to stderr.

`qex rerun <id>` never keeps the key of the first job. That command exists to
run the work again.

### `qex run` with a key

`qex run` accepts `--dedupe-key` and waits for the job that the key gives. It
does not accept `--json`, because its stdout holds the output of the job.

**Ctrl-C stops the job only when this command started the job.** A key can give
the job of a different caller. `qex run` says so when it attaches, and a signal
then stops the wait and nothing else:

```
qex: this command waits for that job. It did not start it, so Ctrl-C stops
this wait only.
qex: to stop the job itself, run `qex kill 7f3c8a12-...`.
```

The exit code of that wait is 124, the code that says "your wait stopped, and
the job continues". Without this rule, Ctrl-C in one agent would stop a
four-hour run that a different agent started, and neither agent would know why.

`qex run` that started the job keeps its earlier behaviour: Ctrl-C stops the
job.

A pipeline stage has no dedupe key. A key on one stage would answer for that
stage alone, and the stages after it would wait for a job of an earlier run.

## Pause the queue, or one lock

A person sometimes needs the machine back: a video call starts, the laptop goes
on battery, or an interactive task needs the cores.

```sh
qex pause queue --reason "recording a demo"   # start no new job
qex pause queue --for 30m                     # end the pause by itself
qex pause queue --drain                       # wait for a quiet machine
qex resume queue                              # start the queue again
```

A paused queue starts **nothing**. The jobs that operate now continue, because
each one already holds its capacity and a stop would lose that work. Use
`qex kill <id>` to stop one.

A lock is the better half:

```sh
qex pause lock gpu0     # the lock goes to you, and every job that needs it waits
qex resume lock gpu0    # the next job takes it
```

`qex pause lock` never fails when a job holds the lock now. qex records the
request, that job keeps the lock, no other job takes it, and the lock comes to
you when that job stops.

```
ID        STATE   NAME   ...  NOTE
b0bb2614  queued  train  ...  waits for the lock `gpu0`, which a person holds
```

The pause is a file beside the job records, so it survives a coordinator that
stops. It covers your queue only; it does not pause another user of the
machine. A job with `--retries` waits too: its supervisor reads the same file.

If qex cannot read that file, it holds the queue and says so. A file that qex
cannot read can hold a pause, and qex does not know. `qex resume queue` writes
a new file.

A second `qex pause queue` keeps the end and the reason of the first one. To
replace an end, run `qex resume queue` first.

`qex pause` with no word says what is paused now. `qex info`, `qex top`,
`qex list` and `qex wait` say it too, and a pause with no end is reported loudly
each time. Each line names the pid that asked for the pause: a queue is shared,
and the second person must be able to find the owner before that person types
`qex resume queue` over the work of somebody else.

Any command on this queue can end any pause on it. A pause is not a lock on the
queue, and qex refuses nobody: the queue belongs to one user of the machine, and
the people and the agents that reach it already share every job in it.

**A pause does not expire a job.** `--max-queue-time` measures the time that a
job waits for the QUEUE. A person who pauses the queue is not the queue, so the
clock of that limit stops at the pause and runs again at the resume. Without
this rule a pause of 30 minutes killed every job with a smaller limit, fired the
stop hook of each one, and gave the person an empty queue on the return — from a
command that exists to protect the machine. `qex status` gives the time in
`queue_pause_secs`. A pause of a LOCK does not stop that clock: a lock is one
name, the person holds it in place of a job, and a job that waits for a lock
already expires in the same way.

`qex submit` into a paused queue still gives a job id and the exit code 0. The
job waits, and the reason of the job says the pause. No command is refused
because the queue is paused. A command IS refused when the coordinator is too
old to pause at all, and it then gives the code 1 with the pid to kill.

`qex submit --each-line` into a paused queue says the pause one time, and not
one time for each job.

A pause of the queue changes the reason of every job that waits, so a reader of
`qex events` sees one `job` line with `change: "reason"` for each of those jobs,
and the same at the resume. The pause itself is not a job, so it has no event of
its own; a reader that must know the pause reads `qex pause --json`.

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

### The group id is a handle

`qex pipeline` writes a group id to stdout, and that id names **every stage** of
the pipeline:

```sh
GROUP=$(qex pipeline ci.toml)

qex wait $GROUP        # wait for every stage
qex status $GROUP      # the state of every stage
qex kill $GROUP        # stop every stage
qex clean $GROUP       # delete every record
qex list --group $GROUP
```

`--needs $GROUP` waits for the whole pipeline in the same way.

The name of the pipeline works in the same way as its id, with one limit: a
pipeline takes its name from its file, so a second run of the same file has the
same name. qex refuses a name that gives two runs and shows the group id of
each. Use the group id in a script.

`qex status --json` gives an array for a pipeline, and one object for one job,
so a script that reads one job does not change. A pipeline of one stage still
gives an array, because the shape comes from what you named.

`qex logs` reads one job. It refuses a pipeline and names the stages, because
qex must not choose a stage for you.

`qex kill $GROUP` stops every stage: it signals the stages that operate, and it
takes the stages that wait out of the queue. A stage that already stopped is not
a fault. `qex kill $ID` for one job that waits still tells you to use `qex
cancel`, because you asked about that one job.

A group needs a coordinator. The records on the disk hold the jobs, and they
hold no group, so `qex wait $GROUP` after the coordinator retires says that
there is no job with that id. Keep the id of a stage as well when a script must
work with no coordinator.

## Job files

```sh
qex submit --job train.toml
```

```toml
name = "train-model"
command = ["uv", "run", "train.py", "--epochs", "50"]
timeout = "4h"          # the limit on the run
max_queue_time = "30m"  # the limit on the wait
tags = ["ml"]

[resources]
cpu = 3          # or "guess", or "full"
mem = "8GB"

[env]
CUDA_VISIBLE_DEVICES = "0"
```

A job file also accepts `dedupe_key` and `dedupe_window`:

```toml
command = ["uv", "run", "train.py"]
dedupe_key = "train:experiment-7"
dedupe_window = "1h"
```

`--dedupe-key` on the command line replaces the value in the file.

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

### All or nothing, and the one case that is not

qex tests the command, reads the whole input and makes every job specification
before it submits the first job. **Every fault that qex can find gives an error
and no job at all**, in the same way as `qex pipeline`: a command with no `{}`,
a file that qex cannot read, a file that is not UTF-8, an input with no line, a
count above the limit, and an option that a fan-out refuses.

One case remains. qex submits the jobs one at a time, so a coordinator that
stops in the middle of a fan-out leaves the earlier jobs in the queue. qex then
writes **the group id and the id of every job that it submitted**, and it says
how to see them and how to stop them:

```
qex: qex lost the coordinator at the job `process-07-g.csv`. 6 jobs of this
     fan-out are in the queue.
qex: the group of those jobs is 4f2c.... They are:
...
qex: Run `qex list --group 4f2c...` to see them, and `qex cancel <id>` or
     `qex kill <id>` to stop them.
```

The exit code is not 0 in that case. Read the group id from the message, and
not from stdout: stdout holds the group id of a fan-out that succeeded in full.

### The limits

`--each-line` submits **1000 jobs** at most. Each job holds a directory in the
state of qex, so a file with 100000 lines would fill the disk. The limit asks
no question, because an agent cannot answer one. Raise it when you need to:

```sh
qex submit --each-line big.txt --max-jobs 5000 -- ./process {}
```

A fan-out submits the jobs **one at a time**, so the command takes time in
proportion to the number of lines: 300 jobs took 4.9 s on this machine, and it
wrote 301 lines to stderr. A `--max-jobs 100000` fan-out therefore holds your
terminal for about half an hour. Submit a large fan-out in the background, or
divide it.

qex also reads **64 MiB** at most, from a file and from a pipe. qex holds the
whole input in memory, because it makes every job before it submits the first
one, so an input larger than this gives an error and no job. `--max-jobs` does
not raise this limit: qex must read the bytes before it can count the lines.

A directory in the place of the input file gives the message of a file that qex
cannot read, and no job.

### The options that a fan-out refuses

| The option | Why |
| ---------- | --- |
| `--dedupe-key`, `--dedupe-window` | A key holds ONE job. Every job of the fan-out would carry the same key, so qex would start the first line and give you the id of that job for every other line. Put the key on a job that starts the fan-out. |
| `--json` | That option writes the id of one job. A fan-out makes a group and one job for each line. Use `--id-file NAME.json`, which holds both. |
| `--job` | The place for the line belongs on the command line, where the reader sees it. |
| `--each-line` on `qex run` | `qex run` waits for one job and gives the output and the exit code of that job. |

### The name of each job

Each job gets a name for `qex list`: the program name, the position in the
file, and as much of the line as fits.

```
process-01-data-a.csv
process-02-data-b.csv
```

The position comes before the line and it has the same width for every job, so
the names sort in the order of the file and two long lines never give one name.
Give `--name` to change the first part and the name of the group. Without
`--name`, the name of the group is the name of the input file with no
extension, so two fan-outs of `inputs.txt` in two directories share one group
NAME. Their group **ids** stay different, so use the id in a script.

A name holds the letters, the numbers, `.`, `_` and `-` only. A line can hold a
terminal control sequence, and a name goes to your terminal in `qex list`.

### The other options

`--cpu`, `--mem`, `--timeout`, `--max-queue-time`, `--lock`, `--tag`,
`--priority`, `--env`, `--nice`, `--needs`, `--after` and `--retries` apply to
every job of the fan-out.

`--max-queue-time` is the time that **one job** waits, and not the time of the
group. A fan-out of 1000 jobs behind a small budget therefore runs the jobs
that it can, and the jobs that still wait at the end of that time become
`expired`. Read the group with `qex list --group $GROUP` to see which lines
ran.

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

### A fan-out of N jobs makes N of everything

A fan-out is N ordinary jobs, so every rule of a job applies N times. Two of
those are loud:

* **The event stream.** `qex events` writes one line for each change of a
  record, so a fan-out of N jobs writes **at least 3N lines** (`queued`,
  `running`, and the state that stops the job). Each `job` event carries the
  whole record, so filter on `.job.group`:

  ```sh
  qex events | jq --arg g "$GROUP" 'select(.event=="job" and .job.group==$g)'
  ```

  The coordinator holds the last 512 events only. A fan-out of more than about
  170 jobs therefore makes more events than that ring holds, so **start `qex
  events` before you submit the fan-out**. A reader that starts after it, or
  that falls behind, receives a `gap` line with the number of events that it
  lost. qex never hides that loss.
* **The stop hook.** `[hooks] on_stop` runs one time for **each** job that
  reaches a state that stops it, and that includes `expired` and `skipped`. A
  fan-out of 1000 lines therefore runs the hook 1000 times, and a hook that
  sends a message sends 1000 messages. The hook environment names the job and
  not the group, so give the fan-out a tag and read `QEX_TAGS`:

  ```sh
  qex submit --each-line inputs.txt --tag nightly -- ./process {}
  ```

  A hook that must report the fan-out one time only belongs on one job that
  waits for the group, and not on every job of it.

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

[logs]
max_bytes = "32MB"    # the output that qex keeps for each stream of each job

[defaults]
cpu = 1               # the default is 1 core
mem = "2GB"           # the default is the machine memory / the core count
timeout = "0"         # the default is no limit
max_queue_time = "0"  # the default is no limit on the wait
```

With no `[defaults]` section, a job gets 1 core and an equal part of the machine
memory. The default job size thus scales with the machine.

A field that takes a number, a size, a time or a percentage accepts the value
with quotation marks and without them. `cpu = 2` and `cpu = "2"` give the same
budget, and `margin = 1.5` and `margin = "1.5"` give the same margin. A size
with no unit is bytes, and a time with no unit is seconds.

The quotation marks do not change **which** values a field takes. `[budget] cpu`
takes a percentage, because it gives a part of the machine to all the jobs
together. `[defaults] cpu` gives the cores for one job, so it takes a whole
number only, and a percentage there gives an error.

### A job gives way to a person

The queue controls **how many** cores a job uses. It does not control **how
rudely** it uses them: a build inside its budget still makes an editor stutter
and a call break up, because the job and the person ask the scheduler for the
same cores and the scheduler treats them alike.

qex knows what that scheduler does not — this work sat in a queue, so nobody is
waiting for the next second of it. Every job therefore starts at `nice 10`:

```toml
[politeness]
nice = 10             # -20 to 19; a larger number gives way
io = "none"           # none, best-effort or idle (Linux only)
oom_score_adj = 0     # a larger number offers the job to the OOM killer first
                      # (Linux only)
```

`nice` operates on Linux and on macOS. `io` and `oom_score_adj` are Linux only:
macOS has no equivalent of either, so qex reads the two values there and does
nothing with them.

`qex submit --nice 0` asks that one job does not give way, and `nice = 0` in the
configuration returns to the earlier behaviour for every job. A job file and a
pipeline stage take `nice` as well, and `qex config show` names the three values
that qex uses now.

The value comes from three places, and the last one wins:

```text
[politeness] nice   ->   job file or pipeline stage `nice`   ->   --nice N
```

`0` is a value and not an absence: `--nice 0` asks for 0 against a configuration
that says 10.

**qex can only make a job give way MORE than the coordinator does.** A lower
number needs privilege, and qex does not ask for privilege. A coordinator that
you start under `nice 5` therefore keeps every job at 5 or above, and
`--nice 0` gives a job at nice 5 and says nothing. Start the coordinator at the
priority that you want as the floor.

qex refuses a `nice` outside -20 to 19, an `io` that is not one of the three
names, and an `oom_score_adj` outside -1000 to 1000. It applies each of these
between the fork and the exec of the job. The only fault that this code can
report there is one that STOPS THE JOB, and a job that gives way at the wrong
priority is better than no job, so each of these steps gives up in silence
instead. A value with a fault would thus give every job something that nobody
asked for and say nothing. Measured on Linux, from nice 0, with no privilege and with the tests
removed: `nice = 100` gave a job at nice 19, because `setpriority` takes 19 for
any number above the range and reports success; `nice = -21` gave EACCES and the
job kept the priority that it had; `io = "iddle"` read as `io = "none"`; and the
kernel refused a write of `oom_score_adj = 90000`, so the job kept the score
that it had.

The supervisor of a job tests these three values again when it starts the job,
because the file can change after the submission. A file with a fault at that
moment gives a job with the DEFAULT politeness values, and `qex status` names
the fault in the `error` field of the job. A job that meets more than one fault
before it starts gets all of them in that field.

A change to `[politeness]` reaches the jobs that START after it, and it does not
touch a job that operates. qex sets these values once, between the fork and the
exec, and it never sets them again. Measured: a job at `nice 10` that operated
stayed at 10 when the file changed to `nice 0`, and a job submitted immediately
after a change to `nice 17` ran at 17.

The supervisor of a job reads the file for itself, as it does for `[enforce]`,
so a new `[politeness]` reaches the NEXT job and does not wait for the
coordinator to read the file again.

`io = "idle"` gives the disk to everything else first, which matters when a
build reads a whole source tree while somebody saves a file. **Use `idle`, and
not `best-effort`, to make a job give way for the disk.** The man page of
`ionice` gives the level of a process that asked for no class as
`(cpu_nice + 20) / 5`, so a job at the default `nice = 10` already behaves as
best-effort level 6. `io = "best-effort"` asks for level 4, which is MORE of the
disk than the job would take with `io = "none"`.

`oom_score_adj` decides who the kernel stops when the machine runs out of
memory. A background build should lose that competition before an editor that
holds an hour of work. A larger number needs no privilege. A number that LOWERS
the score does: measured with `oom_score_adj = -500`, the kernel refused the
write, the job ran with the score 0, and qex said nothing, because the write
happens between the fork and the exec where qex cannot report a fault.

**Not one of these can stop a job.** A machine that refuses the change runs the
job at the priority that it had, which is what qex did before. Measured:
`--nice -5` and `oom_score_adj = -500` were both refused by the kernel, and both
jobs completed.

### The coordinator reads this file again when it changes

A coordinator operates for hours. It reads the configuration file again when the
content of the file changes, so `qex config show` and `qex info` no longer
disagree about the budget of qex. Measured: the new values arrive in 0.52s to
0.56s on a busy coordinator, and in 0.68s to 1.01s on one with nothing to do.

The new values apply to the jobs that START after the change. A job that
operates keeps the claim that it made, and the coordinator keeps that claim
against the budget until the job stops.

**qex compares the CONTENT of the file, and not its time.** Linux takes the time
of a file from a coarse clock with the granularity of one tick, which is 4
milliseconds on a usual machine. Two writes inside one tick give a file the same
time, so a test of the time misses the second write, and it misses it for ever.

**qex looks at the file about ten times in half a second, and it takes the
content when every look gave the same content.** A shell `>` and a redirect, and
every program that writes one line at a time, leave a file that stops in the
middle for a moment, and a file that stops in the middle is still valid TOML. It
parses, and it is wrong in two ways:

- Every key that the writer did not reach yet takes its DEFAULT value. That is
  the one road by which a budget of 2 cores CAN become the default budget of 12.
- A stop in the MIDDLE OF A LINE gives a wrong value that is not a default
  value. A file that was becoming `cpu = 16` reads as `cpu = 1`, and `qex config
  show` then reports `budget: 1 cores`.

qex says nothing in either case, because it CAN read such a file. This is why
qex looks more than one time, and why the new values take about one second and
not half a second.

**The wait is a TIME, and not a count of turns of the scheduler.** The scheduler
waits 500 milliseconds for a change, but every request wakes it, so a
coordinator with work in the queue turns much faster. Measured with a mark on
each turn: the median gap was 500.7ms with nothing to do, and 17.0ms with a loop
of `qex submit` running. While the file settles, qex looks at it every 50
milliseconds.

**A file that changes back and forth in step with those looks can still be
taken.** qex LOOKS at the file; it does not get a message when the file changes.
A writer that puts two different whole files at the path in turn, at a period
near the period of the looks, gives every look the same content while the file
was never that content for longer than one period. Measured: a writer that
changed the file every 25 milliseconds made the coordinator take a half-written
file in 3 trials of 5.

No number of looks removes that. A sampler always has a frequency that walks
past it; more looks only move which frequency. A writer with that regularity is
not a shell `>` and not an editor, because those write the file one time: a
shell loop with its usual jitter could not do it in 5 trials of 5, and only a
writer with an exact period could. Write the file in one step — write a
temporary file and rename it over this one — and none of this applies.

**The path must be a regular file, or a link to one.** qex opens this path on
every turn of the scheduler. A FIFO stops that open until somebody writes to
the FIFO, and the coordinator would then answer nothing at all. Every command
that reads this file applies the same rule, so `qex config show` and `qex
submit` give an error at once in place of a wait with no end.

**A file that qex cannot read does not become the default values.** The
coordinator keeps the values that it had and says so. The same holds for a file
that is empty, for a file that is gone, and for a path that is not a regular
file:

```
qex: WARNING: the configuration file changed, and qex cannot read it:
qex:   config [budget] cpu: invalid core count `two`; expected an integer or a percentage
qex:   The coordinator keeps the values that it had, and they are the values below.
qex:   Correct the file. The coordinator reads it again by itself. Run `qex config show` for the full message.
```

`qex info` gives that warning, and `qex info --json` gives the same text in the
field `config_error`. Correct the file, and the coordinator reads it again with
no other step: the warning then goes away.

The reload tests the values in the same way as the start of a coordinator, so a
value that stops qex from starting cannot arrive by an edit.

### Update the coordinator before you use a new option

qex refuses a field that it does not know. That rule finds a name with a
spelling fault, and a name with a spelling fault must not be ignored in silence.

It has a second cause. A new option belongs in the config file only **after the
coordinator is the new build**:

```sh
qex info                # the version and the pid of the coordinator
# install the new qex
kill <pid>              # the jobs that operate continue
qex info                # the new version now
# NOW put the new option in ~/.config/qex.toml
```

**The program on the disk is not sufficient.** A coordinator operates for hours,
and it holds the code that started it. It reads the config file again when the
file changes, but it reads that file with the code that it holds, and that code
does not know the new option. The coordinator therefore refuses the file, keeps
the values that it had, and `qex info` reports the fault. The new option has no
effect until a NEW coordinator reads it. The coordinator stops by itself when no
job operates, and `kill <pid>` changes it at once.

**Install the new qex before you kill the coordinator.** While the old qex is
the program on the disk, no coordinator can start from a file that holds the new
option, and the commands in the next paragraph that need a coordinator go with
it.

In the other order, `qex submit`, `qex run`, `qex pipeline`, `qex gc`, `qex du`
and `qex config show` stop, and qex cannot start a coordinator — so a queue whose
coordinator retires stays where it is. The jobs that operate continue. These are
the commands you keep, and the second group is the larger one:

| Continue in every state | Continue while a coordinator operates |
| --- | --- |
| `qex wait`, `qex top`, `qex logs`, `qex version` | `qex info`, `qex list`, `qex status`, `qex kill`, `qex cancel`, `qex clean`, `qex rerun` |

`qex rerun` is in the second group, so you can still start work even though `qex
submit` stops: it asks the coordinator for a job that the records already hold,
and it needs no config file. With no coordinator, each command in the second
group waits 10 seconds and then reports that the coordinator did not start. That
message names no cause, which is the second reason to install the new qex first.

A job that starts in this state uses the default values, and `qex status` says
so. Remove the section from the file to go back.

Two people or two agents that share a machine each run their own coordinator, so
each must make this change for itself.

Run `qex help config` for every field.

## Completions for your shell

```sh
qex completions bash | sudo tee /etc/bash_completion.d/qex   # bash, for everybody
qex completions bash > ~/.local/share/bash-completion/completions/qex
qex completions zsh  > ~/.zfunc/_qex        # with ~/.zfunc in your fpath
qex completions fish > ~/.config/fish/completions/qex.fish
```

The commands and the options come from the command line definition itself, so
they cannot disagree with the commands that qex has.

**bash, zsh and fish also offer the jobs.** A job id is a uuid, and nobody types
a uuid. After `qex status`, `qex wait`, `qex logs`, `qex rerun`, `qex clean`,
`qex kill` and `qex cancel`, the shell offers each job by its id AND by its
name. `qex kill` offers the jobs that operate, and `qex cancel` offers the jobs
that wait: a candidate that the command would refuse teaches the wrong command.

The shell asks qex at the moment of the TAB, with a hidden command,
`qex __complete`. **That command never starts a coordinator.** It reads the
records on the disk. A press of TAB is not a request to start a process, and a
user who pressed TAB in a directory with no work must not leave a coordinator
behind.

`elvish` and `powershell` are also accepted, and they get the commands and the
options only. qex does not test them, and a completion that nobody tested
teaches a value that may not exist.

**A job name is text that another agent chose, so the shell must not run it.**
Each shell puts the name on the line as ONE word, and a name such as
`build; rm -rf ~` is thus an argument of qex and never a command.

**qex SHOWS a safe form of each name.** `qex list`, `qex status`, `qex top`, the
sentence that says why a job waits, the completions, and the JSON of each of
them hold a name that uses these characters and no other. The same rule applies
to a group name and to a dedupe key, because each of them is text that another
agent chose:

- the letters `A` to `Z` and `a` to `z`
- the numbers `0` to `9`
- the characters `-`, `_` and `.`

Every other character becomes `_`, and a run of them becomes ONE `_`. A first
character of `-` becomes `_`, because a word that starts with `-` has the form
of an option. The result stops at 128 characters.

```
deploy prod$(id)   ->  deploy_prod_id_
-version           ->  _version
```

**The record on the disk keeps the name that you gave.** qex changes no record.

**A safe name goes back into a command as it stands.** Take it from `qex list
--json` or from `qex status`, which give the whole name. The NAME column of the
table stops at 16 characters, as it did before this rule, so a long name in that
column is not the whole name. The name that you gave still finds the job as
well:

```sh
qex status deploy_prod_id_     # the name that qex shows
qex status 'deploy prod$(id)'  # the name that you gave
qex status -- -version         # a name that starts with `-`: put it after `--`
```

Two names that give one safe form make that word name more than one job. qex
then gives the error that it already gives for such a word: it lists the jobs
and it asks for an id.

**Why.** A name is text that another agent chose. A name that holds an ESC byte,
written to a terminal by `qex list`, moves the cursor and writes over the text
around it; no shell and no TAB are needed for that. A name that holds a space or
a `;` teaches a word that you cannot paste back.

This rule covers the NAME of a job and the name of a group. A lock name, a tag,
a value of `--show-env` and the name of the program still reach the terminal as
they are. See issue #49.

The rule holds for a record that qex wrote at any time, because the safe form
comes from the name in the record. Nothing waits for `qex gc`.

The rule does not replace the quoting: each shell still puts a word on the line
as ONE word, because the answer of `qex __complete` is text that came off a disk
and it is not a guarantee. The two work together.

No candidate starts with `~` or with `$`, because the safe form replaces both
with `_`: a job named `~/tilde` is offered as `_tilde`. A press of TAB is thus
correct in bash, zsh and fish.

**Take care when you TYPE such a name yourself.** Every shell reads `~/x` as a
home directory and `$x` as a variable, so give the name inside a single quote:
`qex status '~/tilde'`.

This command is for a person. An agent writes the full command and needs no
completion.
## The limit on the output of a job

`[logs] max_bytes` is the space that one stream of one job can use. The default
is 32MB for `stdout.log` and 32MB for `stderr.log`.

qex applies the limit **while the job writes**. The supervisor reads the output
through a pipe, so a job that writes 400MB never puts 400MB on the disk. That
disk also holds the record of each job, and qex is made to be started and left,
so a job with no limit can fill it while nobody looks.

qex keeps **both ends** of the output:

```
line 1                        <- the head: the start-up and the configuration
...
[qex] ---- 361MB and 4201177 line(s) of the output are not in this file ----
[qex] The limit is `[logs] max_bytes` = 32MB. qex kept the first 8MB and the
      last 24MB. To keep more, make max_bytes larger.
...
Error: the build failed         <- the tail: the failure
```

The head holds the reason that the job started. The tail holds the reason that
it stopped. A reader needs both.

qex removes nothing until the output passes the limit. A job that writes less
than `max_bytes`, less the room that qex keeps for the notes (2KB), thus keeps
every byte in one piece and gets no note. A second attempt of a job that failed
keeps the output of the first attempt in the same way.

Above that point, qex keeps the first quarter of the limit and the last part.
A job that passes the point by one byte therefore gets the same file as a job
that passes it by a gigabyte. The reason is that qex writes the file while the
job runs: at that moment, nobody knows how much output comes after it.

A job never fails because of this limit. Reaching the limit is normal.

`qex status` and `qex logs` say how much went, so a reader never takes a part of
the output for the whole output:

```sh
qex status $ID --json    # the field logs_dropped gives the bytes and the lines
```

Those lines are not on the disk, so `qex logs --all` does not give them back.

While the job operates, qex holds the last part of the output in a file beside
the log file (`stdout.log.tail`). It writes that part into the log file and
deletes it when the job stops. **The log file thus becomes shorter at the moment
that the output passes the limit.** `qex logs --follow` watches for that, says
that qex removed the middle, and continues at the new end of the file. It shows
the head, then the line that says that the limit is reached, and then the last
part when the job stops.

The last part starts in the middle of a line, because qex removed the bytes
before it. qex removes that fragment when it is a small part of the last part,
and keeps it in each other case with a line that says that the text starts in
the middle of a line. One JSON document, one base64 block and a progress display
that uses `\r` all give output with no line end, or with one line end far into
the file.

Use `max_bytes = "0"` for no limit. The words `"none"`, `"never"` and
`"unlimited"` do the same, and they are the words that `[defaults] timeout`
takes. A job can then fill the disk.

### When a change to the limit takes effect

The supervisor of a job reads `[logs] max_bytes` one time, when the job starts.
A change to the file therefore:

* does **nothing** to a job that already writes. That job keeps the limit that
  it started with, to the end of its last attempt.
* controls the **next job to start**, immediately. The supervisor is a new
  process and it reads the file itself, so this change does not wait for the
  coordinator to read the file again.

Measured: with `max_bytes = "64KB"`, a job that writes 4000 lines kept a file of
63848 bytes although the file changed to `"1MB"` while the job wrote. The next
job kept 1046928 bytes.

To give a new limit to a job that already operates, stop the job and start it
again with `qex rerun $ID`.

### What the job sees

The standard output and the standard error of a job are a **pipe**, and not a
regular file. The supervisor reads that pipe and writes the file. Almost every
program sees no difference, but three things change:

* `lseek` on the output gives `ESPIPE`, and `stat` gives a FIFO in place of a
  regular file. A program that asks for its position in its own output, or that
  reads back what it wrote, meets an error.
* Two children of one job that write more than 4096 bytes in one operation can
  now mix in the middle of a line. With a regular file, each write stayed
  together.
* `isatty` gives false, as it did before. A program that tests for a terminal
  behaves in the same way.

If a program needs a regular file, give it one:

```sh
qex submit -- sh -c 'my-program > out.txt'
```

### A job that leaves a process with the output open

A pipe closes when the last process that holds it stops. A job that starts a
process which outlives it therefore keeps the pipe open after the job itself
ends. `setsid`, `nohup ... &` and a daemon that a test starts all make that
shape.

**qex waits 30 seconds for the output to close, and then it writes the result.**
A record that arrives is worth more than a wait with no end. The record of such
a job says:

* `state` is the state that the job earned. The wait does not fail the job.
* `error` says that the output did not close, and that a log file can be
  missing its last part.
* `logs_dropped.incomplete` is `true`. The counts beside it are the counts that
  arrived, and not the full quantity, so a program must not read them as
  complete.

Measured: a job that runs `setsid sh -c "sleep 120" &` and then writes one line
finished 30 seconds after it started, with `state` `completed`,
`incomplete: true`, and that `error`.

To get the result at once, stop the process that holds the output, or give that
process an output of its own:

```sh
qex submit -- sh -c 'setsid my-daemon > daemon.log 2>&1 &'
```

## The claim reaches the job

A claim controls the queue. It does not control the job: a job that asks the
machine how many cores it has receives the number of the **machine**, so a job
with a claim of 2 cores on a machine of 16 starts 16 threads and takes the
capacity that qex gave to the other jobs.

qex therefore writes the claim into the environment of the job, and most
runtimes read those variables in place of the machine:

```console
$ qex submit --cpu 2 --mem 2GB -- go run main.go
$ qex logs <id> --stdout
go: NumCPU=16 GOMAXPROCS=2
```

| Variable | For |
| --- | --- |
| `QEX_CPU`, `QEX_MEM`, `QEX_MEM_MB` | your own script: `make -j"$QEX_CPU"` |
| `GOMAXPROCS`, `GOMEMLIMIT` | Go |
| `OMP_NUM_THREADS` | OpenMP: C, C++, Fortran |
| `OPENBLAS_NUM_THREADS`, `MKL_NUM_THREADS`, `NUMEXPR_NUM_THREADS`, `VECLIB_MAXIMUM_THREADS` | numpy, pandas and the libraries below them |
| `RAYON_NUM_THREADS`, `CARGO_BUILD_JOBS` | Rust |
| `JULIA_NUM_THREADS`, `DOTNET_PROCESSOR_COUNT`, `POLARS_MAX_THREADS` | Julia, .NET, Polars |
| `NODE_OPTIONS=--max-old-space-size` | node, at three quarters of the claim |

**Give both `--cpu` and `--mem`.** qex writes these variables only when the
whole claim came from you: a job that gives one of the two takes the other from
the default or from what qex learned, and qex then writes nothing rather than
guess which half you meant.

**qex writes these only when you chose the claim.** `--cpu` and `--mem` are a
decision; the default claim of one core is not, and a job that heard it would
run single-threaded on a machine of sixteen cores. A learned claim is not a
decision either, and it would make that fault permanent: qex would measure the
job it had capped at one core, learn one core, and cap it again.

**qex never replaces a value that is already there.** A value from your shell,
from the job file or from `--env` is a decision that somebody made, and qex
fills the values that nobody chose. With `--env-capture minimal` the shell's
value does not survive the capture, so qex writes its own — the rule is about
the environment that the job receives, and not about the shell you typed in.

This is the nearest thing to a limit that operates on macOS as well as on Linux,
and it needs no cgroup and no privilege. It stays a promise: a program that asks
the operating system directly still sees the whole machine.

Two variables need a request, because each has a cost:

```toml
[claims]
also = ["java", "make"]
```

`java` writes `JAVA_TOOL_OPTIONS`, and every JVM then writes `Picked up
JAVA_TOOL_OPTIONS: ...` to its standard error, which lands in the log of the
job.

`make` writes `MAKEFLAGS=-jN`. **A Makefile that gives its own `-j` wins**, so
this changes a Makefile that gives none — and that is the cost. It makes a
build parallel that its author never ran in parallel, and a Makefile with an
incomplete dependency graph then fails.

A name in `also` that is not `java` or `make` gives an error. qex does not
ignore it.

Turn it all off with `[claims] export_env = false`.

`qex submit --no-limit-env-hints` turns it off for one job, for a job that must
see the machine as it is. A job file and a pipeline stage say the same thing
with a field:

```toml
no_limit_env_hints = true
```

**These three sources are not the usual order.** Most values take the command
line, then the file, then the configuration, and a later source replaces an
earlier one. Here each source can only turn the claim OFF: there is no
`--limit-env-hints`, so a job file that says `true` stands, and a machine whose
configuration says `export_env = false` stays off for every job.

`--env-capture none` also turns it off for that job. That mode says the job
starts with an empty environment and receives `[env]` and `--env` only, and
`none` means none.

## A command when a job stops

`[hooks] on_stop` names a command that qex runs when a job reaches its final
state. Use it for a notification, so that a person who left the machine learns
that a long job stopped.

```toml
[hooks]
on_stop = ["notify-send", "a qex job stopped"]
on_stop_states = ["completed", "failed", "killed", "timeout", "expired", "oom"]
timeout = "30s"
```

The hook is in the config file, and a job file has no hook field. The hook
belongs to the machine and to the person at it, and not to the work.

The value is a program and its arguments. qex starts no shell, in the same way
as for a job. To use a shell feature, name the shell:

```toml
[hooks]
on_stop = ["bash", "-lc", "echo \"$QEX_JOB_NAME $QEX_STATE\" >> ~/qex.log"]
```

The job supplies these variables. A variable with no value is empty text.

| Variable | Value |
| -------- | ----- |
| `QEX_JOB_ID` | the job id |
| `QEX_JOB_NAME` | the job name, in the safe form that `qex list` shows |
| `QEX_STATE` | the final state |
| `QEX_EXIT_CODE` | the exit code of the job, if the job ran to its own end |
| `QEX_SIGNAL` | the signal number, if a signal stopped the job |
| `QEX_ELAPSED_SECS` | the seconds that the job ran |
| `QEX_CWD` | the directory of the job |
| `QEX_JOB_DIR` | the directory of the record, which holds the logs |
| `QEX_ATTEMPTS` | the number of times that qex started the job |
| `QEX_MAX_RSS` | the maximum memory in bytes |
| `QEX_TAGS` | the tags, separated by a space |

**Name an absolute path in `on_stop`.** The hook starts in the DIRECTORY OF THE
JOB, and the person who submitted the job chose that directory. A relative name
such as `["./notify"]` therefore selects a program that the submitter can put
there. Job data cannot become a command line, but a relative name lets it select
WHICH program runs.

**The values arrive in the environment and never in a command line.** qex builds
no text that a shell reads: it starts the program of `on_stop` directly, with the
arguments that you wrote and no others. A job name such as `x; rm -rf ~` is thus
a name, and never a command, whatever the hook does with it.

**`QEX_JOB_NAME` is the SAFE name.** It holds the letters, the numbers and
`-_.` only, which is the one form of a name that qex shows anywhere, and which
`qex list` and `qex status` already print. A hook exists to put a name in front
of a person, and a raw name with an ESC byte in it moves the cursor of a
terminal and writes over the text around it. The safe name goes back into `qex
status` and `qex logs` as it stands. A hook that needs the name that the
submitter typed reads `status.json` in `QEX_JOB_DIR`.

`QEX_TAGS` and `QEX_CWD` have no such rule, so qex replaces each control
character in them with a space. A NUL byte is the reason: the system takes no
variable that holds one, and the hook of that job would not start at all.

`QEX_EXIT_CODE` is the code of the JOB, and it is the number that `qex run`
gives for a job that ran to its own end. It is EMPTY for a job that something
stopped, because such a job produced no code of its own; `QEX_STATE` says what
happened, with the same word that `qex status` prints. Read `QEX_STATE` first
and `QEX_EXIT_CODE` after it. See [Exit codes of `qex run`](#exit-codes-of-qex-run).

`on_stop_states` selects the jobs that give a message. The default list holds
each state of a job that ran, and `expired` — a job that gave up waiting is the
case that a person most wants to hear about, because nothing ran and nothing
else says so. `cancelled` and `skipped` are not in it: you cancelled the job
yourself, and one failure in a pipeline of twenty stages would give twenty
messages. Add those names to get them.

qex gives these guarantees:

- **qex never runs the hook two times for one job.** The job directory holds the
  file `hook.ran`, and the process that makes that file is the process that runs
  the hook. This holds also when the coordinator stops and starts again while
  the job runs. qex runs the hook one time for each job that stops, EXCEPT when
  the machine or the process stops between the two steps: qex makes `hook.ran`
  first, so a failure in that moment loses the message and qex does not try
  again. A message that arrives two times is worse than a message that is lost.
- **A hook cannot hold the queue.** qex starts it after the final state is on
  the disk, so the job has its result, the budget is free and the next job
  starts before the hook does anything.
- **A hook has a time limit and a size limit, and the size limit FAILS
  CLOSED.** A hook that uses more than `[hooks] timeout` receives TERM and then
  KILL, in a process group of its own. The hook writes into a pipe and never
  into a file, so at 1MB qex stops reading and shuts the pipe: the hook then
  meets EPIPE, qex stops it, and **the standard output and the standard error
  of the hook stop growing at 1MB**. This bounds those two streams and nothing
  else: a hook that opens a file of its own and writes to it is a program that
  you chose to run, and qex limits it as little as it limits a job. The earlier
  form cut the file afterwards, which bounded what qex KEPT and not what
  reached the disk — a peak of 25.3MB for a cap of 1MB, and no bound at all if
  qex stopped before it could cut. `qex logs <id> --hook` says when this
  happened and how much qex kept.
- **`setpgid` gives the hook a process group of its own.** That is deliberate:
  it lets qex stop a hook together with the children that it started. It also
  means a hook OUTLIVES a qex that dies, so the stop above is the only thing
  that bounds a hook that runs away. The cap holds even then, because the pipe
  dies with qex and the hook can no longer write anywhere.
- **A change to `[hooks]` acts on the next job that stops.** qex reads the
  config file at each job, and not one time at the start. A hook that you delete
  runs no more, and a hook that you add runs at once. You do not restart the
  coordinator.
- **A hook that fails does not change the job.** The result of a job is the
  result of the job, and a notification that did not arrive does not change it.
  qex writes nothing into the `error` field of the job for a hook.

**A hook is not a job, and it receives nothing that belongs to one.** It does
not take the `[politeness]` values, because those make WORK give way to a
person and a notification is FOR the person: the hook runs at the priority of
the qex process that starts it. It does not join the cgroup of the job, so
`[enforce]` never applies the memory limit of the job to it. It does not receive
`QEX_CPU`, `GOMAXPROCS` or any other variable of [`[claims]`](#the-claim-reaches-the-job),
because it makes no claim on the budget. Its output limit is the fixed 1MB
above, and NOT [`[logs] max_bytes`](#the-limit-on-the-output-of-a-job), which is
the limit for the output of a job.

The output of the hook goes to `hook.log` in the directory of the job. Read it
with `qex logs <id> --hook`, which also gives the verdict of qex on the hook: a
hook that did not start, that was too slow, or that stopped with an error.

A job that failed and ran again gives one message, with the final result.
`QEX_ATTEMPTS` gives the number of attempts.

## More help inside the tool

Each topic below is also in the binary, so an agent needs no network:

```sh
qex help agents      the one page for an agent
qex help job-file    the fields of a job file
qex help resources   claims, the budget and the several-user accounting
qex help each-line   one job for each line of a file
qex help states      each job state and what causes it
qex help events      the event stream, its numbers and its gaps
qex help exit-codes  the exit code of each command
qex help config      each configuration field
qex help pause       stop the queue, or take a lock for yourself
qex schema job       the JSON Schema of a job file
qex schema status    the JSON Schema of status.json
qex schema event     the JSON Schema of one line of `qex events`
```
