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
on the same two streams, and the exit code is the exit code of the job. Nothing
else in your script changes.

WHAT YOU GIVE UP. `qex run` ties the job to this command:

    Ctrl-C stops the job, and not this command only.
    Somebody stops your session, and the job stops with it.

That is correct for work that you are waiting for, and it is WRONG for work that
lives longer than your attention. `qex submit` gives the job a life of its own:
it continues when your session stops, and a later session reaches it with the
id.

    short, and you wait for it now    ->  qex run -- ...
    long, or you come back to it      ->  qex submit, then qex status <id> --wait

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

A person can thus stop you at any moment with no cost. Do not start the work
again in a new session before you ask: `qex list` shows what already operates,
and `qex status <id>` gives the result of what stopped.

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
    124  your wait timed out; the job still operates
    125  something stopped the job
    126  the job did not run, because a job that it needed failed
    127  there is no job with that id

Add `--timeout` to limit your wait. Example: `qex wait $ID --timeout 30m`.
A timeout stops your wait only. It does not stop the job.

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

    --lock NAME        two jobs with one lock name never operate together.
                       Use it for work that shares something that a claim
                       cannot express: a build directory, a port, a database.
                       `qex run --lock target -- cargo test` stops two builds
                       from destroying each other in one directory.

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
    tags = [\"ml\"]                 # for `qex list --tag ml`
    priority = 0                  # a larger number starts earlier
    needs = [\"build\"]             # stop if these jobs do not succeed
    after = [\"cleanup\"]           # wait for these jobs, whatever the result
    env_capture = \"all\"           # all, minimal or none

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

`env_capture` selects the environment that the job receives:

    all       every variable from your shell (the default)
    minimal   PATH, HOME, USER, LOGNAME, SHELL, LANG, TZ only
    none      no variable from your shell

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

    [submit]
    env_capture = \"all\"           # all, minimal or none
    minimal_env = [\"PATH\", \"HOME\", \"USER\", \"LOGNAME\", \"SHELL\", \"LANG\", \"TZ\"]

    [learn]
    enabled = true        # use the earlier jobs of a command as the claim
    margin = 1.5          # the multiplier for a measurement

    [history]
    keep = \"1d\"           # how long to keep the id of a job after its removal

    [gc]
    keep = \"1d\"           # the age of a record that `qex gc` deletes

    [defaults]
    cpu = 1               # the default is 1 core
    mem = \"2GB\"           # the default is the machine memory / the core count
    timeout = \"0\"         # the default is no limit

Default job size
----------------

A submission without `--cpu` or `--mem` uses the `[defaults]` section. If that
section gives no value, qex uses 1 core and an equal part of the machine
memory. On a machine with 16 cores and 32GB, the default job is 1 core and 2GB.
The default job size thus scales with the machine.

Enforcement
-----------

The default mode is `off`. A claim then controls the queue only, and qex sets
no limit on the job. This behaviour is the same on Linux and on macOS.

The modes `soft` and `hard` need cgroup v2, so they operate on Linux only. In
`soft` mode the kernel slows a job at its claim. In `hard` mode the kernel stops
a job at its claim. If qex cannot set a limit, it writes a warning and continues
in the `off` mode.

A key name with a spelling error gives an error. qex does not ignore it.
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
    oom         the out-of-memory killer stopped the job.
    cancelled   qex removed the job from the queue before it started.
    skipped     a job that this job needed did not succeed, so this job
                did not start. The field `caused_by` names the job that
                failed first.

The states `queued`, `starting` and `running` are not final. Each other state is
final and does not change.

The state `oom` is different from `failed`. For `oom`, correct your memory claim
or use a larger machine.

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
    qex logs <id> --follow --grep ERROR  the matches as they arrive

Every path has a limit. A search reports the number of lines that match, so a
pattern that matches 3000 lines tells you that the pattern is too wide.

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

Each job in the file takes every field of a job file: `cwd`, `env`, `timeout`, `tags`,
`priority`, `env_capture` and `[resources]`.

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

Every job of one submission shares a group id:

    GROUP=$(qex pipeline ci.toml)
    qex list --group $GROUP

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
    124  your wait timed out. The job still operates.
    125  something stopped the job: kill, timeout or out-of-memory
    126  the job did not run, because a job that it needed did not succeed
    127  there is no job with that id

The code 124 has the same meaning as the code of the `timeout` command.

A timeout on `qex wait` stops your wait only. It does not stop the job. Use
`qex kill` to stop the job.

To get the exit code of the job itself, add `--passthrough`:

    qex wait $ID --passthrough

`qex wait` then exits with the exit code of the job. Use this option to send the
result of the job to a script.

Other commands
--------------

    0    the command succeeded
    1    the command failed
    2    the command line is not correct
    127  there is no job with that id
";
