//! This module holds the text for the `qex help <topic>` command.
//!
//! An agent reads this text to learn the tool. Each topic is thus short and
//! complete, and it contains commands that the agent can copy.

/// The banner that `qex` writes before the usage text when it has no arguments.
///
/// The banner points to the `agents` topic. An agent then reads one page and
/// does not read each command help.
/// The banner that `qex` writes before the usage text when it has no arguments.
///
/// The banner gives the length of the agents topic. A reader that knows the
/// length reads the page one time, and does not open it again to see if there
/// is more.
pub fn banner() -> String {
    format!(
        "  ==> AGENTS: run `qex help agents` first. It is {} lines, and it is complete.\n\
     \x20     It shows how to start a job, wait for the job, and read the output.\n\
     \x20     Do not write a monitor script. The command `qex wait` does that work.\n",
        AGENTS.lines().count()
    )
}

/// The list of topic names, for the error message and for the `--help` text.
pub const TOPICS: &[&str] = &[
    "agents",
    "job-file",
    "config",
    "resources",
    "states",
    "output",
    "exit-codes",
    "pipeline",
];

/// Gives the text for one topic.
///
/// The name `agent` is an alias of `agents`.
pub fn topic(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "agents" | "agent" => Some(AGENTS),
        "job-file" | "jobfile" | "job" => Some(JOB_FILE),
        "config" | "configuration" => Some(CONFIG),
        "resources" | "resource" | "budget" => Some(RESOURCES),
        "states" | "state" => Some(STATES),
        "output" | "json" => Some(OUTPUT),
        "exit-codes" | "exit" | "exitcodes" => Some(EXIT_CODES),
        "pipeline" | "pipelines" => Some(PIPELINE),
        _ => None,
    }
}

pub const AGENTS: &str = "\
qex for agents
==============

Use qex to run a long task. qex holds the task in a queue, starts it when the
machine has capacity, and records the result. You can then wait for the result
with one command.

Do not write a monitor script
-----------------------------

Every monitor that you write waits for a PROXY: a pattern in the process list, a
line in a log file, a file that appears. A proxy can become permanently false,
and nothing tells the monitor. It then waits for ever.

Four monitors were measured on one machine in one day, and together they slept
for 95 hours. Not one of the conditions could ever become true. Three of them:

    while pgrep -f \"solve.py\"; do sleep 60; done
        The command line of this shell holds the letters `solve.py`, so the
        pattern matches the monitor itself. The task stops, one process stays,
        and the count never reaches zero.

    until grep -q \"DONE\" run.log; do sleep 60; done
        Correct, until somebody stopped the task that writes that line. The
        marker will never arrive now.

    until grep -q \"READY\" ~/other.log; do sleep 60; done
        That file was never made. This monitor slept for 41 hours.

A different user found this one later, on a machine that two agents shared. It
had slept for 63 hours:

    while true; do M=$(ps -Ao args | grep -c solver)
                   K=$(ssh other-host 'ps -Ao args | grep -c solver')
                   [ $M -eq 0 ] && [ $K -eq 0 ] && break; sleep 300; done
        A COUNT, and not a test of one process. This monitor waits until nothing
        matches. On a machine that two agents share, that condition is not
        satisfiable: the work of the other agent holds the count above zero for
        ever. The work of this author finished two days before, and the
        monitor opened about 750 connections to the other machine while it
        waited.

The last three hold NO PATTERN FAULT. They are careful commands. The fault is
the proxy: a log line is evidence of the work, and evidence stops when the work
stops, in a way that the monitor cannot see. The last one is the most dangerous,
because a careful author writes it: on a machine that two agents share, \"wait
until nothing matches\" can never become true.

qex waits for the process, and not for a proxy of the process. qex is the parent
of your task and it uses `waitpid` on that exact process. A process ends or it
does not, and no third condition exists. `qex wait` thus always gives an answer:

    the job succeeded            -> 0
    the job failed               -> 1
    the job never started, and it
    had a `--max-queue-time`     -> 123
    somebody stopped the job     -> 125
    a job before it failed       -> 126

A task that somebody stops gives the code 125 at that moment. A monitor that
watches a log file would still be waiting.

The same fault applies to every search of the process list. `pgrep -f qex` also
matches the shell command that holds those letters. To find the coordinator, use
`qex info`, which gives the process id from the coordinator itself.

    qex watchers

That command finds the monitors of this kind on your machine. It removes its own
process and the processes that started it before it reports anything, so it
never finds itself. A user who looked for this fault with `pgrep -f pgrep` found
the search, and that was the fourth time in one day that the fault appeared.

When to use `qex run`
---------------------

    qex run -- make test

Use `qex run` for work that is SHORT AND HEAVY and that you wait for now: a test
suite, a release build, a data conversion. The job goes in the queue, so it
starts when the machine has room, and the other people and agents on this
machine keep the capacity that they claimed. The output arrives as it happens,
on the same two streams, and the exit code is the exit code of the job, or 125
when something stopped the job. Nothing else in your script changes.

WHAT YOU GIVE UP. `qex run` ties the job to this command, but only for the stops
that it can catch:

    Ctrl-C stops the job, and not this command only.
    A SIGTERM on this command stops the job too.
    A SIGKILL does NOT stop the job, and neither does the hangup of
    a terminal that closes. The job continues, and `qex list` finds it.

A job that operates receives a SIGTERM. A job that still waits in the queue
leaves the queue instead, because a job with no process cannot receive a signal.

That is correct for work that you are waiting for, and it is WRONG for work that
lives longer than your attention. `qex submit` gives the job a life of its own:
it continues when your session stops, and a later session reaches it with the
id.

    short, and you wait for it now    ->  qex run -- ...
    long, or you come back to it      ->  qex submit, then qex status <id> --wait

When something stops the job, `qex run` gives 125 and not 1. Another agent on
this machine can run `qex kill` or `qex cancel` on your job, because a job of
`qex run` is a job like any other. The code 125 says that something stopped the
job before it could finish, and it does not say that your work failed. Do not
start the work again before you read the line on stderr. Run
`qex help exit-codes` for the full table.

A job that a dedupe key gave you is the one exception. This command did not
start that job, so Ctrl-C stops this wait only, and the job continues. `qex run`
then gives 124, which says that YOUR WAIT ended. Read the section on the key
below.

The three commands you need
---------------------------

    ID=$(qex submit --cpu 2 --mem 4GB -- uv run train.py)
    qex wait $ID
    qex logs $ID

If you operate inside a harness
-------------------------------

`qex wait` blocks. Your harness, and not qex, tells you when a background
command ends. Put the two together:

    ID=$(qex submit -- make test)      # gives the id at once
    qex status $ID --wait              # run THIS in the background of your harness

qex watches the process correctly, and your harness reports the end of the
command. You thus need no timer and no second command.

Use `qex status --wait` and not `qex wait` for this. It blocks in the same way
and it gives the same exit code, and its output also holds the state, the exit
code and the last lines of the error output. One command gives everything.

`qex submit` writes the job UUID to stdout and writes nothing else. You can
thus put the UUID in a shell variable.

A shell variable does not last between your commands. Use `--id-file` to keep
the id in a file:

    qex submit --id-file build.id -- make
    qex status \"$(cat build.id)\" --wait

PUT THE ID FILE WHERE IT LASTS LONGER THAN YOUR SESSION. Your project directory
or your home directory is correct. A scratch directory that your harness owns is
NOT correct, and neither is /tmp: the job continues when your session stops, but
the file goes with the session, and you then have no handle for a job that still
operates. qex gives a warning when the file goes to such a directory.

If you lose an id, `qex list` shows each job with its directory and its command,
and `qex list --cwd .` shows the jobs of this directory only.

Your session can stop, and the work continues
---------------------------------------------

THIS IS THE PROPERTY THAT MAKES qex SAFE FOR AN AGENT THAT A PERSON CAN STOP.

The job is not a child of your shell, and it is not a child of your agent. qex
starts a supervisor in its own session, and the supervisor starts the job. Three
things follow, and all three matter:

    Somebody stops your agent           the job continues.
    Your terminal closes                the job continues.
    The coordinator stops or is replaced the job continues, and it still writes
                                        its result.

Each line is true for a job of `qex run` as well, with one exception: Ctrl-C or
a SIGTERM on the waiting `qex run` stops the job. See WHAT YOU GIVE UP above.

Nothing is lost, because the record of the job is on the disk and not in the
memory of a process. Your wait is the only thing that stops.

You can therefore attach the wait again, in a later session, in a new shell,
from a different agent, at any time:

    qex status $ID --wait

The id is the handle. That command gives the same answer whether the job
operates now, stopped one second ago, or stopped last night. A job that stopped
while nobody watched loses nothing at all.

This is what a monitor script cannot do. A monitor holds the answer in its own
memory: stop the monitor, and the answer is gone. Keep the id in a file with
`--id-file`, and the answer waits for you instead.

    qex submit --id-file build.id -- make    # session 1
    # the person stops the agent here. `make` continues.
    qex status \"$(cat build.id)\" --wait      # session 2, and the result is there

A person can thus stop you at any moment with no cost.

Give each submission a key, and a second run starts nothing
-----------------------------------------------------------

YOU LOSE YOUR CONTEXT AND YOU RUN YOUR SCRIPT AGAIN. Without a key, qex starts a
SECOND copy of a four-hour training run beside the first copy. Both copies then
hold the machine, and both write to the same files.

Give the submission a key. The second run of the same script starts nothing:

    ID=$(qex submit --dedupe-key train:$(pwd) -- uv run train.py)
    qex wait $ID

The second run gives THE SAME id and exits with the code 0, so your script does
not change and `ID=$(qex submit ...)` stays correct. qex writes the reason to
stderr:

    qex: this submission started no job. The dedupe key `train_home_me_p`
    gives the job 7f3c8a12-..., and that job is in the state `running`.

DO NOT READ `qex list` AND DECIDE FOR YOURSELF. That test is a PROXY: you read
the list, you decide, and you submit, and a different agent can submit between
your read and your submission. The coordinator makes the test and the submission
ONE step. Two commands in the same moment thus give one job and one id.

    --dedupe-key KEY     start no second job while a job with this key waits or
                         operates. The key is free when that job stops.

    --dedupe-window 1h   keep the key of a job that SUCCEEDED for this time
                         also. A job that did not succeed never keeps its key,
                         because the remedy for a failure is another run.

    --json               write {\"id\": \"...\", \"deduplicated\": true} in place of
                         the id alone. Use it when your script must know if IT
                         started the work.

Choose a key that names the work AND the place: `build:$(pwd)`. A key such as
`build` alone stops the build of every other project on the machine.

THE WINDOW OF THE COMMAND THAT ASKS APPLIES, and not the window of the job that
holds the key. The window is a question: how old an answer do you accept? A
command that gives no window thus starts a new job, although a different command
gave a window a moment before. Give the same window in each command that shares
a key. This concerns a job that already SUCCEEDED only, so no second copy of
work that operates can start.

`qex run --dedupe-key` waits for the job that the key gives. CTRL-C THEN STOPS
YOUR WAIT ONLY, because a different agent can be the owner of that job. qex says
so when it attaches, and the wait gives the code 124: your wait stopped, and the
job continues. Use `qex kill <id>` to stop the job itself.

`qex status <id>` shows the key of a job. You can thus see which key gave you an
id, and `qex status <id>` gives the result of a job that stopped.

One command gives the result and the cause
------------------------------------------

`qex status` of a job that did not succeed also writes the last lines of its
standard error. You thus need no second command for the usual question.

    qex status $ID                  the state, the exit code and the last lines
                                    of BOTH streams
    qex status $ID --wait           the same, but wait for the job first
    qex status $ID --tail 50        more lines
    qex status $ID --stderr         one stream only
    qex status $ID --grep ERROR     the lines that match
    qex status $ID --no-logs        the state only

qex gives both streams, because a program frequently writes its result to the
standard output and its failure summary to the standard error. The error alone
reads as a complete failure.

`qex wait` stops until the job stops. Its exit code tells you the result:

    0    the job succeeded
    1    the job failed
    123  the job never started; it reached its `--max-queue-time`
    124  your wait timed out; the job still operates
    125  something stopped the job
    126  the job did not run, because a job that it needed failed
    127  there is no job with that id

Add `--timeout` to limit your wait. Example: `qex wait $ID --timeout 30m`.
A timeout stops your wait only. It does not stop the job.

Give up on a job that never starts
----------------------------------

A job waits until the machine has capacity for it. On a busy machine that wait
can be long, and a job with a claim that no budget can meet waits with no end.

    ID=$(qex submit --max-queue-time 30m -- make test)
    qex wait $ID                 # this gives an answer inside 30 minutes

The job does not start after that time. Its state becomes `expired`, `qex wait`
gives the code 123, and `qex status` says what the job waited for. Nothing ran,
so there is no output to read.

The clock starts at the submission. A coordinator that stops and starts again
continues the same count, so a restart does not give the job a new full wait.

qex counts the wait in whole seconds. A job can thus give up as much as one
second BEFORE its limit, and the time in the record is that count of seconds.
Give a limit of a minute or more, where one second changes nothing.

The wait for a job in `--needs` counts also. Give a value that covers the whole
pipeline, or give no value on a stage that waits for an earlier stage.

There is no value for this option by default, and there is none in the config
file until you write one. A job that qex discards is work that a person wanted,
so qex never chooses that for you.

A job takes this value at its SUBMISSION. A job that already waits in the queue
keeps the value that it had, so a change to `[defaults] max_queue_time` reaches
the jobs that you submit after it, and no earlier job.

What qex captures
-----------------

`qex submit` copies your environment and your current directory. Your job thus
operates in the same way as a command that you type now. Use `--env K=V` to add
or replace one variable. Use `--env-capture minimal` if your shell holds
secrets.

Resource claims
---------------

Give `--cpu` and `--mem`. qex uses these claims to decide how many jobs operate
together. Claims stop two agents from starting too much work at the same time.

If you do not know the size of the task, use a word in place of a number:

    qex submit --cpu guess --mem guess -- ./unknown-task

    half, guess   one half of the budget. Two such jobs operate together.
    full, max     the full budget. The job operates alone.

Use `guess` to start an unknown task safely. The words also operate in a job
file:

    [resources]
    cpu = \"guess\"
    mem = \"half\"

Do not measure a task before you run it
---------------------------------------

Do not run a small test job to find the size of a task. That method costs you
time and gives a poor measurement, because a small job does different work.

Give `--cpu guess --mem guess` and start the REAL task. That run gives you a
true measurement, and it does the work at the same time.

qex then uses that measurement for you. The next job of the same command gets a
claim from the earlier runs, so you give no claim at all:

    qex submit --cpu guess --mem guess -- ./task    # run 1
    qex submit -- ./task                            # run 2: the claim is ready

`qex status` says where a claim came from.

Read the numbers yourself when you want an exact claim:

    qex status $ID --json      # the usage field gives max_rss and cpu_secs

The first run with `guess` is thus not wasted effort: it produces both the
result and the measurement that makes every later run cheap. What is wasted is
a separate test job that produces no result.

If your claim is larger than the full budget, qex starts the job alone when no
other job operates. The job can then swap or stop with an out-of-memory error.
The status field `forced` is `true` for such a job. That result is data: your
claim or the machine is too small.

qex learns the size of a task
-----------------------------

qex records what each job really used, and it uses those numbers as the claim
for the next job of the same command. You thus give no claim at all after the
first run:

    qex submit -- cargo test        # run 1: the default claim
    qex submit -- cargo test        # run 2: the claim comes from run 1

`qex status` says where a claim came from. The record is for the command, and
not for the name, because `cargo build` and `cargo test` need different sizes.

qex uses the LARGEST measurement that it holds, and it adds a margin. A claim
that is too small stops the job, and a claim that is a little too large costs
some capacity only.

qex records a job that completed only. A job that the out-of-memory killer
stopped shows the memory that it reached, and not the memory that it needs.

In short: give `guess`, start the task, and read the result. Add an exact claim
later, and only if you repeat the task. After the first run of a command, qex
gives the claim for you.

A pipeline of stages
--------------------

Do not put the stages of a pipeline in one script. If stage 3 of that script
fails, you get one exit code and one log file with the output of every stage
mixed together, and you must find the cause.

Give each stage its own job, and name the jobs that must succeed first:

    BUILD=$(qex submit --name build -- make)
    TEST=$(qex submit --name test  --needs $BUILD -- make test)
    SHIP=$(qex submit --name ship  --needs $TEST  -- ./deploy.sh)
    qex wait $SHIP

Keep the id of each stage and give the id to the next stage. An id names one
job for ever, so the script stays correct when you run it again.

Each stage has its own log file, its own exit code and its own claim. If `build`
fails, `test` and `ship` do not start. Their state becomes `skipped`, and their
record names the job that failed:

    qex list
    ID        STATE     NAME   ...  NOTE
    a1b2c3d4  failed    build  ...  the job stopped with the exit code 2
    b2c3d4e5  skipped   test   ...  the job a1b2c3d4 (build) is failed, ...
    c3d4e5f6  skipped   ship   ...  the job a1b2c3d4 (build) is failed, ...

There is one failure only, and it is the cause. Run `qex logs a1b2c3d4` to read
the output of that stage, and no other output.

Each skipped job names the first job that failed, and not the job before it. A
read of the last stage thus gives you the cause immediately.

    --needs <id>,<id>   wait for these jobs, and stop if one does not succeed
    --after <id>,<id>   wait for these jobs, whatever their result

Use `--after` to control the order only. A cleanup job that must run after a
build, and must run also when the build fails, uses `--after`.

`qex wait` gives the code 126 for a skipped job, and the code 1 for a job that
failed. Your script can thus separate a failure of your stage from a failure of
an earlier stage.

A job can name the jobs that you started before it. A job cannot name a job that
does not exist, so a circle of dependencies is not possible.

An id and a name have different rules
-------------------------------------

An ID must exist. That is the only rule. qex accepts an id whatever the state
of that job, so a script can submit its last stage even when the first stage
already failed. The last stage then becomes `skipped` with the correct cause.

A NAME must give a job that is in the queue or operates. A name can give a job
of an earlier run: you write `--needs test`, you forgot to start a new test job,
and the name gives the test job of yesterday. That job already succeeded, so
your stage would start immediately and wait for nothing. qex refuses a name in
that case and tells you what happened.

Use an id in a script. Use a name when you type a command yourself.

Other useful options
--------------------

    --retries 3        run the job again when it fails, up to 3 times.
                       The job keeps one id and one record, and the log holds
                       every attempt. Use it for a fault outside the task,
                       such as a network that is not ready.

    --nice N           how much the job gives way to the work of a person.
                       -20 to 19, and a larger number gives way. The default
                       comes from `[politeness] nice`, and it is 10. Use
                       `--nice 0` to ask that this job does not give way.
                       qex can only make a job give way MORE than the
                       coordinator does: a coordinator that a user started
                       under `nice 5` keeps its jobs at 5 or above, because
                       a lower number needs privilege.

    --lock NAME        two jobs with one lock name never operate together.
                       Use it for work that shares something that a claim
                       cannot express: a build directory, a port, a database.
                       `qex run --lock target -- cargo test` stops two builds
                       from destroying each other in one directory.

    --dedupe-key KEY   start no second job while a job with this key waits or
                       operates. qex gives the id of that job and exits with
                       the code 0. Use it in a script that can run a second
                       time: `--dedupe-key build:$(pwd)`.

    --id-file FILE     write the job id to a file as well as to stdout.

    qex wait A B --any   give control back when the FIRST job stops.
    qex rerun <id>       submit the same job again, with a new id.

If the coordinator is older than your command
---------------------------------------------

A coordinator operates for hours, and a new build can replace the qex program.
The coordinator then holds earlier code.

qex asks the coordinator what it can do, and it REFUSES a job that the
coordinator cannot obey:

    qex: the coordinator (pid 3507877) is version 0.3.0, and it cannot
    obey --lock.

    qex refuses this job. The coordinator would ignore that option in
    silence, give you a job id, and run the job without the rule that you
    asked for.

A refusal is safer than a job that starts. A job specification travels as JSON,
and a field that the coordinator does not know is ignored with no message. A
lock that nothing applies looks exactly like a lock that operates, until two
jobs destroy each other.

The coordinator stops when no job operates, and the next command starts one that
can obey. `kill <pid>` changes it at once; the jobs that operate continue,
because a new coordinator reads the same records.

`qex version` gives what your command can do and what the coordinator can do.

Other commands
--------------

    qex list --json            all the jobs and their states
    qex status <id> --json     one job in detail, with the last error lines
    qex logs <id> --follow     the output while the job operates
    qex kill <id>              stop a job that operates
    qex cancel <id>            remove a job from the queue
    qex clean --state done     delete the records of the jobs that stopped
    qex info                   the coordinator and the free capacity

Every command that reads data accepts `--json`. Use `qex schema status`,
`qex schema job` and `qex schema pipeline` to get the JSON Schema of each
format.

Short forms of a job id
-----------------------

Each command accepts the first characters of an id, and it also accepts the
name of a job:

    qex submit --name build -- make
    qex status build

Give a name with `--name` when you start a job. You can then use that name in
place of the id, and you do not keep the UUID.

A name that gives more than one job gives an error with the list of the jobs.

Start with a job file for a complex task
----------------------------------------

    qex submit --job train.toml

Run `qex help job-file` for the fields.

For several stages in one file, use a pipeline:

    qex pipeline ci.toml

Run `qex help pipeline`. A pipeline gives each stage a name that belongs to
that one submission, so two runs of one file never share a name.

Tell the people who make qex
----------------------------

qex exists because somebody measured their own monitor scripts and gave the
numbers. Each of the parts above came from a report of that kind.

    Something helped you:
        https://github.com/stephenc/qex/issues/1
        Leave a comment. Give the number if you have one: the hours, the jobs,
        the size of the machine. Say which model you are and which harness runs
        you, because the faults are not the same in each harness.

    A recipe that is not in the documentation:
        https://github.com/stephenc/qex/issues/2
        qex has no timer, because cron and `qex submit --lock` do it better.
        That recipe is there, with the guard that stops the queue from growing.

    Something did NOT work, or a word sent you the wrong way:
        LOOK FOR IT FIRST, AND ADD TO WHAT IS THERE.
            gh issue list --repo stephenc/qex --state all --search \"your words\"
        A second report of one fault says that the fault is common, and that is
        useful. A second ISSUE for one fault divides the answer between two
        pages, and the person who corrects it must read both and close one.
        Add your machine, your model and your harness to the issue that exists:
            gh issue comment <number> --repo stephenc/qex
        Open a new one only when nothing matches:
            https://github.com/stephenc/qex/issues/new/choose

        A SMALL FAULT IS WORTH AN ISSUE. An option that you looked for and did
        not find, a message that made you look in the wrong place, an exit code
        that surprised you: those reports are the ones that make a tool good.

    Give four things, and no more: the command in full, what qex wrote in full
    (use --json), what you expected, and the output of `qex version`.

DO NOT PUT THE OUTPUT OF `--show-env` IN A PUBLIC PAGE, and do not put anything
that a token can hide in. `qex submit` copies the environment of your shell, so
that output is a secret. This is the same rule as `qex help output`.

If you cannot reach GitHub, give the words to the person that you work with and
let them decide.
";

pub const JOB_FILE: &str = "\
qex job files
=============

A job file describes one job. Use a job file for a long command, for many
environment variables, or to keep the job in your repository.

    qex submit --job train.toml

qex reads TOML, YAML and JSON. The file extension selects the format. TOML is
the format in this documentation.

One job file holds ONE job. For several stages in one file, use a pipeline file
and the command `qex pipeline`. Run `qex help pipeline`.

A minimal file
--------------

    command = [\"uv\", \"run\", \"train.py\"]

A full file
-----------

    name = \"train-model\"          # the name in `qex list`
    cwd  = \"/home/me/project\"     # the default is your current directory
    command = [\"uv\", \"run\", \"train.py\", \"--epochs\", \"50\"]
    timeout = \"4h\"                # the default is no limit
    max_queue_time = \"30m\"        # give up if the job waits this long
    tags = [\"ml\"]                 # for `qex list --tag ml`
    priority = 0                  # a larger number starts earlier
    needs = [\"build\"]             # stop if these jobs do not succeed
    after = [\"cleanup\"]           # wait for these jobs, whatever the result
    env_capture = \"all\"           # all, minimal or none
    nice = 10                     # -20 to 19; a larger number gives way
    no_limit_env_hints = false    # true: do not tell the job its claim size
    dedupe_key = \"train:p1\"       # start no second job while this one operates
    dedupe_window = \"0\"           # keep the key after a job that succeeded

    [resources]
    cpu = 3
    mem = \"8GB\"

    [env]
    CUDA_VISIBLE_DEVICES = \"0\"

Fields
------

`command` is a list of arguments. It is not a shell command line. qex does not
start a shell, so you need no quotation marks and no escape characters. To use
a shell feature such as a pipe, name the shell:

    command = [\"bash\", \"-lc\", \"a | b > c.txt\"]

`mem` accepts `8GB`, `8G`, `512MB` or a number of bytes. One unit step is 1024.

`timeout` accepts `30s`, `5m`, `4h`, `2d`, or `0` for no limit.

`max_queue_time` accepts the same values. It limits the time that the job WAITS,
and `timeout` limits the time that the job RUNS. A job that reaches this limit
does not start, and its state becomes `expired`. The time counts from the
submission, and the wait for a job in `needs` counts also.

`env_capture` selects the environment that the job receives:

    all       every variable from your shell (the default)
    minimal   PATH, HOME, USER, LOGNAME, SHELL, LANG, TZ only
    none      no variable from your shell

`dedupe_key` makes the submission idempotent. While a job with that key waits or
operates, a second submission with the same key starts NO job: qex gives the id
of the first job and exits with the code 0. Use it for a job file that a script
submits each time it runs. `--dedupe-key` on the command line replaces the value
in the file.

`dedupe_window` accepts a time such as `1h`. The key of a job that SUCCEEDED
stays for that time. A job that did not succeed never keeps its key, because the
remedy for a failure is another run. The default is `0`.

A pipeline stage has NO dedupe key. A key on one stage would answer for that
stage alone, and the stages after it would wait for a job of an earlier run.

The sequence of the sources
---------------------------

A later source replaces an earlier source:

    environment from the shell  ->  job file [env]  ->  --env K=V
    directory from the shell    ->  job file cwd    ->  --cwd D
    config file defaults        ->  job file        ->  command line options

Secrets
-------

qex writes your captured environment to `spec.json` with mode 0600. If your
shell holds secrets, use `--env-capture minimal`. The command `qex status` hides
the environment. Add `--show-env` to see it.

A field name with a spelling error gives an error. qex does not ignore it.
";

pub const CONFIG: &str = "\
qex configuration
=================

The config file is `~/.config/qex.toml`. The file is optional. Run
`qex config path` to see its location and `qex config show` to see the values
that qex uses now.

    [budget]
    cpu = \"75%\"          # cores that qex can use; an integer or a percentage
    mem = \"75%\"          # memory that qex can use; a size or a percentage

    [system]
    reserve_mem  = \"2GB\"  # memory to keep free for other programs
    max_pressure = 20     # maximum PSI memory pressure (Linux only)

    [enforce]
    mode = \"off\"          # off, soft or hard
    mem_overcommit = 1.5  # soft mode: memory.max = claim * this value
    use_systemd = true    # permit a temporary systemd unit for the cgroup

    [peers]
    enabled = true
    dir = \"/tmp/qex\"
    stale_after = \"30s\"

    [queue]
    oversized = \"run-when-idle\"   # run-when-idle, reject or queue
    settle = \"3s\"

    [politeness]
    nice = 10             # -20 to 19; a larger number gives way
    io = \"none\"           # none, best-effort or idle (Linux)
    oom_score_adj = 0     # a larger number offers the job to the OOM killer
                          # first (Linux)

    [submit]
    env_capture = \"all\"           # all, minimal or none
    minimal_env = [\"PATH\", \"HOME\", \"USER\", \"LOGNAME\", \"SHELL\", \"LANG\", \"TZ\"]

    [claims]
    export_env = true     # tell the job how large its claim is
    also = []             # \"java\", \"make\", or both

    [learn]
    enabled = true        # use the earlier jobs of a command as the claim
    margin = 1.5          # the multiplier for a measurement

    [logs]
    max_bytes = \"32MB\"    # the output that qex keeps for each stream of a job

    [history]
    keep = \"1d\"           # how long to keep the id of a job after its removal

    [gc]
    keep = \"1d\"           # the age of a record that `qex gc` deletes

    [defaults]
    cpu = 1               # the default is 1 core
    mem = \"2GB\"           # the default is the machine memory / the core count
    timeout = \"0\"         # the default is no limit
    max_queue_time = \"0\"  # the default is no limit on the wait

    [hooks]
    on_stop = []          # the command that qex runs when a job stops
    on_stop_states = [\"completed\", \"failed\", \"killed\", \"timeout\", \"oom\"]
    timeout = \"30s\"       # the time limit for that command

Quotation marks around a number
-------------------------------

A field that takes a number, a size, a time or a percentage accepts the value
with quotation marks and without them. `cpu = 2` and `cpu = \"2\"` give the same
budget, and `margin = 1.5` and `margin = \"1.5\"` give the same margin. A size
with no unit is bytes, and a time with no unit is seconds.

The quotation marks do not change WHICH values a field takes. `[budget] cpu`
takes a percentage, because it gives a part of the machine to all the jobs
together. `[defaults] cpu` gives the cores for ONE job, so it takes a whole
number only, and a percentage there gives an error.

A command when a job stops
--------------------------

`[hooks] on_stop` names a command that qex runs each time a job reaches its
final state. Use it for a notification: a message on the screen, a line in a
file, or a message to a chat. A person who left the machine thus learns that
the job of four hours stopped.

    [hooks]
    on_stop = [\"notify-send\", \"a qex job stopped\"]

The hook is in the config file only. A job file has no hook field. The hook
belongs to the machine and to the person at it, and not to the work: the same
pipeline runs on a laptop with a screen and on a build machine with none.

The value is a program and its arguments. qex starts no shell, in the same way
as for a job. To use a shell feature, name the shell:

    [hooks]
    on_stop = [\"bash\", \"-lc\", \"echo \\\"$QEX_JOB_NAME $QEX_STATE\\\" >> ~/qex.log\"]

The job supplies these variables. A variable with no value is empty text.

    QEX_JOB_ID        the job id
    QEX_JOB_NAME      the job name
    QEX_STATE         the final state
    QEX_EXIT_CODE     the exit code, if the job stopped without a signal
    QEX_SIGNAL        the signal number, if a signal stopped the job
    QEX_ELAPSED_SECS  the seconds that the job ran
    QEX_CWD           the directory of the job
    QEX_JOB_DIR       the directory of the record, which holds the logs
    QEX_ATTEMPTS      the number of times that qex started the job
    QEX_MAX_RSS       the maximum memory in bytes
    QEX_TAGS          the tags, separated by a space

The values arrive in the environment and never in a command line. A job name
with a shell character is thus a name, and never a command.

`on_stop_states` selects the jobs that give a message. The default list holds
each state of a job that ran. `cancelled` and `skipped` are not in it: you
cancelled the job yourself, and one failure in a pipeline of twenty stages
would give twenty messages. Add those names to get them. For a message on a
failure only:

    [hooks]
    on_stop_states = [\"failed\", \"timeout\", \"oom\"]

The hook cannot damage the queue. qex runs it after the final state is on the
disk, so the job has its result, the budget is free, and the next job starts
before the hook does anything.

qex never runs the hook two times for one job. It runs the hook one time for
each job that stops, EXCEPT when the machine or the process stops in the moment
between the record of the run and the run itself: qex then loses that message,
and it does not try again. A message that arrives two times is worse than a
message that is lost.

A hook that uses more than `[hooks] timeout` receives TERM and then KILL, in a
process group of its own. A hook that writes more than 1MB stops in the same
way, and qex cuts the file to that size. A hook that fails does not change the
job.

The output goes to `hook.log` in the directory of the job. Read it with
`qex logs <id> --hook`, which also gives the verdict of qex: a hook that did not
start, a hook that was too slow, or a hook that stopped with an error.

Default job size
----------------

A submission without `--cpu` or `--mem` uses the `[defaults]` section. If that
section gives no value, qex uses 1 core and an equal part of the machine
memory. On a machine with 16 cores and 32GB, the default job is 1 core and 2GB.
The default job size thus scales with the machine.

The limit on the output of a job
--------------------------------

`[logs] max_bytes` is the space that one stream of one job can use. The default
is 32MB for `stdout.log` and 32MB for `stderr.log`.

qex applies this limit WHILE THE JOB WRITES. A job that writes 400MB thus never
puts 400MB on the disk. The same disk holds the record of each job, and qex is
made to be started and left, so a job with no limit can fill that disk while
nobody looks.

qex keeps the first part of the output and the last part. The first part holds
the start-up and the configuration. The last part holds the failure. Between
the two, qex writes a line that says how many bytes and how many lines went:

    [qex] ---- 361MB and 4201177 line(s) of the output are not in this file ----

`qex status` and `qex logs` also give that count, so a reader always knows that
the file is not the whole output. `qex status --json` gives it in the field
`logs_dropped`.

Use `max_bytes = \"0\"` for no limit. The words \"none\", \"never\" and \"unlimited\"
do the same, and they are the words that `[defaults] timeout` takes. Then a job
can fill the disk.

The supervisor of a job reads this field one time, when the job starts. A change
to the file thus does nothing to a job that already writes, and it controls the
next job to start. The supervisor reads the file itself, so the new value does
not wait for the coordinator to read the file again. To give a new limit to a
job that operates, stop it and use `qex rerun`.

The claim in the job
--------------------

A claim controls the queue. It does not control the job: a job that asks the
machine how many cores it has receives the number of the MACHINE. qex therefore
writes the claim into the environment of the job, and most runtimes read those
variables in place of the machine.

    QEX_CPU, QEX_MEM, QEX_MEM_MB           your own script: make -j\"$QEX_CPU\"
    GOMAXPROCS, GOMEMLIMIT                 Go
    OMP_NUM_THREADS                        OpenMP: C, C++ and Fortran
    OPENBLAS_NUM_THREADS, MKL_NUM_THREADS  numpy, pandas and the libraries
    NUMEXPR_NUM_THREADS                      below them
    VECLIB_MAXIMUM_THREADS                 Accelerate, on macOS
    RAYON_NUM_THREADS, CARGO_BUILD_JOBS    Rust
    JULIA_NUM_THREADS                      Julia
    DOTNET_PROCESSOR_COUNT                 .NET
    POLARS_MAX_THREADS                     Polars
    NODE_OPTIONS                           node, at 3/4 of the claim

qex writes these ONLY when you gave both `--cpu` and `--mem`. A default claim
and a learned claim are not a decision that you made, and a job that heard
`one core` would run single-threaded. qex never replaces a value that is
already there.

`[claims] also` adds two more. Each of the two has a cost, so neither is a
default:

    java   JAVA_TOOL_OPTIONS=-XX:ActiveProcessorCount=N -XmxMm
           Every JVM then writes `Picked up JAVA_TOOL_OPTIONS: ...` to its
           standard error, and that line goes into the log of the job.
    make   MAKEFLAGS=-jN
           A Makefile that gives its own `-j` wins, so this changes a Makefile
           that gives none. It thus makes a build parallel that its author
           never ran in parallel, and a Makefile with an incomplete dependency
           graph then fails.

Turn it all off with `export_env = false`, or for one job with
`qex submit --no-limit-env-hints`.

Enforcement
-----------

The default mode is `off`. A claim then controls the queue only, and qex sets
no limit on the job. This behaviour is the same on Linux and on macOS.

The modes `soft` and `hard` need cgroup v2, so they operate on Linux only. In
`soft` mode the kernel slows a job at its claim. In `hard` mode the kernel stops
a job at its claim. If qex cannot set a limit, it writes a warning and continues
in the `off` mode.

A key name with a spelling error gives an error. qex does not ignore it.

When the coordinator reads this file
------------------------------------

The coordinator reads this file at its start, and it reads it again when the
content of the file changes. qex looks at the file about ten times in half a
second, and it takes the content when every look gave the same content, so the
new values arrive in about half a second to one second. They apply to the jobs
that START after the change. A job that operates keeps the claim that it made.

Those looks are not a delay for its own sake. A program that writes this file
one line at a time leaves a file that stops in the middle, and a file that stops
in the middle is still valid TOML. Every key that the writer did not reach yet
takes its DEFAULT value, and a stop in the middle of a line gives a wrong value
that is not a default value: a file that is becoming `cpu = 16` reads as
`cpu = 1`. qex says nothing in either case, because it CAN read such a file.

A file that changes back and forth in step with those looks can still be taken.
qex LOOKS at the file, and it gets no message when the file changes, so a writer
that puts two whole files at the path in turn at the period of the looks gives
every look the same content. No number of looks removes that. Write this file in
one step to be safe: write a temporary file, then rename it over this one.

A file that qex cannot read does not become the default values. The coordinator
keeps the values that it had, and `qex info` says so. That covers a file with a
fault, an empty file, a file that is gone, and a path that is not a regular
file. Correct the file, and the coordinator reads it again with no other step.

The path must be a regular file, or a link to one. The open of a FIFO waits for
a writer, and a read of a device gives bytes with no end, so every command that
reads this file refuses a path of another kind and says so at once.

A NEW option is different. The coordinator holds the code that started it, so a
coordinator of an earlier build does not know a name that a later build added.
It refuses the file and keeps the values that it had. Install the new qex FIRST,
then run `qex info` for the pid and `kill <pid>` to replace the coordinator, and
put the new option in the file last.
";

pub const RESOURCES: &str = "\
qex resources and the budget
============================

Claims
------

Each job has a claim: a number of cores and a quantity of memory. Give the claim
with `--cpu` and `--mem`, or in the `[resources]` section of a job file.

A claim is an estimate of the peak use. qex uses the claims to decide how many
jobs operate together. Two agents on one machine thus do not start too much work
at the same time.

Words in place of a number
--------------------------

    half, guess   one half of the budget
    full, max     the full budget

qex calculates these words against the budget at the time of the submission, so
the record of the job holds an exact value.

Use `guess` for a task of an unknown size. Two jobs with the claim `guess`
operate together, and a third job waits. Use `full` for a task that must have
the machine to itself; every other job then waits for it.

If you give no claim, qex uses the `[defaults]` section of the config file. If
that section gives no value, a job gets 1 core and the machine memory divided by
the number of cores.

By default a claim sets no limit on the job. See `qex help config` to make qex
apply the claim as a limit.

When does a job start
---------------------

qex starts a job when all these conditions are true:

  1. The claims of the jobs that operate, plus this claim, are in the budget.
  2. The claims of the other users leave sufficient capacity.
  3. The free memory stays above `reserve_mem` and the memory pressure is below
     `max_pressure`.

If a job waits, `qex status` gives the reason in the `blocked_reason` field.

A job that is larger than the budget
------------------------------------

A claim can be larger than the full budget. Such a job can never meet condition
1, so qex starts it alone when no other job operates.

The job can then cause swap operations, use all the cores, or stop with an
out-of-memory error. Each of these results is data for you. A job that waits for
ever gives no data.

The status field `forced` is `true` for such a job, and `forced_reason` gives
the text. `qex submit` also writes a warning to stderr immediately. The UUID
stays alone on stdout.

To change this behaviour, set `[queue] oversized` to `reject` or to `queue`.

When to look at the measured use
--------------------------------

qex measures each job and writes the values in the status. You do not need a
test job, and you do not need to read the values after each job.

Give `guess` and start the real task. Look at the measured use only when both of
these conditions are true:

  1. You run the same kind of task many times.
  2. The jobs wait in the queue, or a job stopped with an out-of-memory error.

    qex status <id> --json

The `usage` field gives `max_rss` in bytes and `cpu_secs`. A task that always
uses much less than its claim wastes capacity: put an exact claim in a job file,
and more jobs then operate together. A task that stops with an out-of-memory
error needs a larger claim.

For one task, this step is not necessary.

Other users
-----------

Each qex coordinator writes its current claims to `/tmp/qex`. A coordinator
reads the files of the other users before it starts a job. This method needs no
administrator rights.

This method is cooperative. A different user can write an incorrect value. qex
also tests the free memory of the machine, so it finds a load that no
coordinator reports.
";

pub const STATES: &str = "\
qex job states
==============

    queued      qex accepted the job. It waits for capacity.
    starting    qex started the supervisor. The job process starts.
    running     the job operates.
    completed   the job stopped with the exit code 0.
    failed      the job stopped with an exit code that is not 0.
    killed      the command `qex kill` stopped the job.
    timeout     the job used more time than its `--timeout` value.
    expired     the job waited more time than its `--max-queue-time` value,
                so it never started. There is no output and no exit code.
    oom         the out-of-memory killer stopped the job.
    cancelled   qex removed the job from the queue before it started.
    skipped     a job that this job needed did not succeed, so this job
                did not start. The field `caused_by` names the job that
                failed first.

The states `queued`, `starting` and `running` are not final. Each other state is
final and does not change.

The state `oom` is different from `failed`. For `oom`, correct your memory claim
or use a larger machine.

The state `expired` is different from `timeout`. For `timeout`, the work is too
slow, and the log file holds the output of the part that ran. For `expired`, the
machine never gave the job a place, so the log file is empty. Read the `error`
field: it says what the job waited for and how long it waited.

Use `qex list --state running` to select the jobs in one state.
";

pub const OUTPUT: &str = "\
qex output and files
====================

JSON
----

Each command that reads data accepts `--json`. The output is one JSON document.

    qex list --json
    qex status <id> --json
    qex wait <id> --json

For the schema of these documents:

    qex schema status
    qex schema job

Job files on the disk
---------------------

qex writes one directory for each job:

    ~/.local/state/qex/jobs/<uuid>/
        spec.json     the command, the environment and the claims (mode 0600)
        status.json   the state, the exit code, the times and the true use
        stdout.log    the standard output of the job
        stderr.log    the standard error of the job

A job that writes more than `[logs] max_bytes` also has `stdout.log.tail` or
`stderr.log.tail` while it operates. That file holds the last part of the
output, and qex writes it into the log file and deletes it when the job stops.

The directory has mode 0700 because `spec.json` can contain secrets.

`status.json` is the primary record. The supervisor of the job writes it in one
operation, so a reader sees the old contents or the new contents. `qex wait`
reads this file directly if the coordinator does not operate.

Logs
----

`qex logs` and `qex status` accept the same options to select lines.

    qex logs <id>                 both streams, the last 500 lines
    qex logs <id> --all           every line
    qex logs <id> --stdout        one stream
    qex logs <id> --tail 100      the last 100 lines
    qex logs <id> --head 20       the first 20 lines; a fault at the start
    qex logs <id> --lines 400:430 the lines from 400 to 430
    qex logs <id> --number        write the line number before each line
    qex logs <id> --grep ERROR    the lines that match
    qex logs <id> --grep E -C 3   with 3 lines before and after each match
    qex logs <id> --grep x --fixed  read the value as plain text
    qex logs <id> --max-matches 20  show 20 matches, and count the others
    qex logs <id> --follow        the output while the job operates
    qex logs <id> --follow --tail 50   the last 50 lines, then the new lines
    qex logs <id> --hook          the output of the stop hook, and the verdict
                                  of qex on it
    qex logs <id> --follow --grep ERROR  the matches as they arrive

Every path has a limit. A search reports the number of lines that match, so a
pattern that matches 3000 lines tells you that the pattern is too wide.

The output of a job also has a limit
------------------------------------

A job can write more than `[logs] max_bytes` (the default is 32MB for each
stream). qex then keeps the first part of the output and the last part, and it
writes one line between them:

    [qex] ---- 361MB and 4201177 line(s) of the output are not in this file ----

Those lines are NOT on the disk. `--all` does not give them back, because
nothing holds them. `qex status` and `qex logs` say how much went, and
`qex status --json` gives the numbers in the field `logs_dropped`.

qex removes nothing until the output passes the limit. A job that writes less
than the limit, less the room that qex keeps for the notes (2KB), keeps every
byte in one piece, and a second attempt of a job that failed keeps the output of
the first attempt. Above that point, a job that passes the limit by one byte
gets the same file as a job that passes it by a gigabyte: qex writes the file
while the job runs, and at that moment nobody knows how much output follows.

The log file becomes shorter at the moment that the output passes the limit.
`qex logs --follow` says so, and it continues at the new end of the file.

Make `[logs] max_bytes` larger for a job that must keep everything, or write
the output of the job to a file of your own.

The output of a job is a pipe
-----------------------------

The supervisor reads the output through a pipe and writes the file itself. The
standard output and the standard error of a job are thus a pipe, and not a
regular file. Almost every program sees no difference. Three things change:

    lseek gives ESPIPE, and stat gives a FIFO in place of a regular file. A
        program that asks for its position in its own output meets an error.
    Two children of one job that write more than 4096 bytes in one operation
        can mix in the middle of a line. A regular file kept each write
        together.
    isatty gives false, as it did before.

If a program needs a regular file, give it one:

    qex submit -- sh -c 'my-program > out.txt'

A pipe closes when the last process that holds it stops, so a job that leaves a
process behind (`setsid`, `nohup ... &`, a daemon that a test starts) keeps its
output open after the job ends. qex waits 30 seconds for the output to close and
then writes the result: a record that arrives is worth more than a wait with no
end. The record then says `incomplete` in the field `logs_dropped`, and `error`
says that a log file can be missing its last part. The wait does not fail the
job. To get the result at once, give that process an output of its own:

    qex submit -- sh -c 'setsid my-daemon > daemon.log 2>&1 &'

Use `--follow --grep` in place of a pipe to `grep`. A pipe holds the lines in a
buffer and shows nothing until the buffer fills, because `grep` needs the option
`--line-buffered`. qex writes each line as it reads it.

Watch the queue
---------------

    qex top            the jobs, the claim of each one, and its true use now
    qex top --once     one page, for a script
    qex top -i 5       a refresh every 5 seconds

The CPU column gives the cores in use. Compare it with the CPU CLAIM column to
find a claim that is much larger than the need.

This command never starts a coordinator, and it gives the jobs when no
coordinator operates.

Delete the records
------------------

    qex clean <id>                 one job
    qex clean completed            each job that succeeded
    qex clean done                 each job that stopped
    qex clean --state failed       each job in one state
    qex clean --cwd                the jobs of this directory
    qex clean --under              the jobs of this directory and below
    qex clean --under /path        the jobs of that directory and below
    qex clean --auto               a short form of `--state done
                                   --older-than 1h`, on this directory and
                                   below. A job of the last hour stays,
                                   because it is frequently the job that you
                                   read now.
    qex gc                         every record of every directory that
                                   stopped more than one day ago. It also
                                   deletes a job directory that holds no
                                   record. Use `--dry-run` first, and
                                   `[gc] keep` to change the time.

    qex du                         how much disk space qex holds, and the
                                   job records that hold the most

`qex list` takes `--cwd` and `--under` as well, so you can see what a deletion
would remove.

A job that a job in the queue still needs is NOT finished for a deletion,
whatever its own state says. The job in the queue reads that record to decide
whether to run, and to explain why it did not. `qex clean` and `qex gc` keep
such a record and say so, and it goes when the other job stops.
    qex clean --older-than 7d      each job older than 7 days
    qex clean --all                every job

`qex clean` deletes the directory of the job. It does not stop a job that
operates.

qex keeps the id of a deleted job for one day, so `qex status` can tell you that
a job existed and that its work happened. An agent thus does not repeat work
after a deletion. Change that time with `[history] keep` in the config file.

`qex clean --all` deletes the record of EVERY job of this user, including the
jobs of a different agent that shares this machine. Use `qex clean <id>` when
another agent uses qex at the same time.
";

pub const PIPELINE: &str = "\
qex pipelines
=============

A pipeline file describes several jobs, and one command submits them all. The
key in the file is `[[jobs]]`, and each entry becomes a qex job with its own id,
its own record and its own log file. This text says `job` for that reason.

    qex pipeline ci.toml

The command writes the group id to stdout, and the id of each stage to stderr,
so `GROUP=$(qex pipeline ci.toml)` operates.

    name = \"ci\"

    [[jobs]]
    name = \"build\"
    command = [\"make\"]

    [[jobs]]
    name = \"unit\"
    command = [\"make\", \"test\"]
    needs = [\"build\"]

    [[jobs]]
    name = \"lint\"
    command = [\"make\", \"lint\"]
    needs = [\"build\"]

    [[jobs]]
    name = \"ship\"
    command = [\"./deploy.sh\"]
    needs = [\"unit\", \"lint\"]

    [[jobs]]
    name = \"cleanup\"
    command = [\"./clean.sh\"]
    after = [\"ship\"]

Each job in the file takes every field of a job file: `cwd`, `env`, `timeout`,
`max_queue_time`, `tags`, `priority`, `env_capture`, `nice` and `[resources]`.

A stage that waits for an earlier stage also uses its `max_queue_time`, because
that clock counts every wait. Give a value that covers the whole pipeline, or
give no value on such a stage.

Why a pipeline file, and not several submissions
------------------------------------------------

A name is easy to write, and a name is not unique in time. If you run the same
four jobs twice with `qex submit --needs build`, that name gives two jobs.

The names in a pipeline file belong to that file and to that one submission.
qex changes each one into the id that it made a moment before, and no name
leaves the file. A second run of the same file makes new jobs with new ids, and
the two runs never meet.

One command for the whole pipeline
----------------------------------

Every job of one submission shares a group id, and that id names every stage:

    GROUP=$(qex pipeline ci.toml)

    qex wait $GROUP                 # wait for every stage
    qex status $GROUP               # the state of every stage
    qex kill $GROUP                 # stop every stage
    qex clean $GROUP                # delete every record
    qex list --group $GROUP

The name of the pipeline works in the same way as its id, with one limit: a
pipeline takes its name from its file, so a second run of that file has the same
name. qex refuses a name that gives two runs, and it shows the group id of each.
Use the group id in a script.

`qex status --json` gives an array for a pipeline and one object for one job,
so a script that reads one job does not change. A pipeline of one stage still
gives an array, because the shape comes from what you named.

`qex logs` reads one job, so it refuses a pipeline and names the stages.

Use `--id-file` to keep every id in a file:

    qex pipeline ci.toml --id-file ids.env
    . ids.env                       # gives $group, $build, $unit, ...
    qex status \"$ship\" --wait

A name that ends in `.json` gives a JSON object instead, for a parser.

qex reads the whole file before it submits anything. A circle of jobs, a name
that no job has, and a job with no command each give an error, and no job
starts.

";

pub const EXIT_CODES: &str = "\
qex exit codes
==============

`qex wait`
----------

    0    the job succeeded (exit code 0)
    1    the job failed (a different exit code, or a signal)
    123  the job never started. It waited more time than its
         `--max-queue-time` value, and its state is `expired`.
    124  your wait timed out. The job still operates.
    125  something stopped the job: kill, cancel, timeout or out-of-memory
    126  the job did not run, because a job that it needed did not succeed
    127  there is no job with that id

The code 124 has the same meaning as the code of the `timeout` command.

The code 123 is not 125. A job with the code 125 ran and wrote output. A job with
the code 123 never got the machine, so it has no output. Read the `error` field
of `qex status` for the wait that stopped it.

A timeout on `qex wait` stops your wait only. It does not stop the job. Use
`qex kill` to stop the job.

To get the exit code of the job itself, add `--passthrough`:

    qex wait $ID --passthrough

`qex wait` then exits with the exit code of the job. Use this option to send the
result of the job to a script.

`qex run`
---------

    the exit code of the job    the job ran (0, 7, 1, whatever it gave)
    123  the job never started; it reached its `--max-queue-time`
    124  your wait stopped, and the job continues. See the dedupe key below.
    125  something stopped the job: kill, cancel, Ctrl-C, timeout, out-of-memory
    126  the job did not run, because a job that it needed did not succeed
    127  there is no job with that id

`qex run` writes the output of the job, so it gives the exit code of the job
when the job RAN. `qex run -- sh -c 'exit 7'` gives 7.

A job of `qex run` is a job like any other, so `qex kill` and `qex cancel` from
a DIFFERENT command can stop it. That job gave no exit code of its own, and
`qex run` then gives 125 and not 1. The two are thus separate: 125 says that
something stopped your work before it could finish. `qex run` also writes a line
to stderr that names the cause, and that line says when this command did not
stop the job.

The code 1 has two causes. Your work ran and it gave the exit code 1, or qex
could not finish its own work: the coordinator stopped while `qex run` waited,
for example. qex writes the second cause on stderr, and the job can then still
operate.

For each state in which the job gave NO exit code of its own, `qex run` gives
the same code as `qex wait`. Two commands must not answer one question two ways.
For a job that RAN, `qex run` gives the exit code of the job, and `qex wait`
gives 0 or 1 unless you add `--passthrough`.

`qex run` gives 124 in ONE case: a dedupe key gave it the job of a different
caller, and a signal then arrived. This command did not start that job, so
Ctrl-C stops this wait and the job continues. The code 124 says the same thing
there as on `qex wait`: YOUR WAIT ended, and the work did not. Run
`qex status $ID --wait` to wait again, or `qex kill $ID` to stop the job.

`qex run` gives 124 for no other reason. It waits with no limit of its own, and
a job that reaches the time limit of `--timeout` gives 125, because something
stopped that job.

Other commands
--------------

    0    the command succeeded
    1    the command failed
    2    the command line is not correct
    127  there is no job with that id
";
