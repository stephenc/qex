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
    "events",
    "output",
    "exit-codes",
    "pipeline",
    "each-line",
    "pause",
    "abort",
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
        "events" | "event" | "stream" => Some(EVENTS),
        "output" | "json" => Some(OUTPUT),
        "exit-codes" | "exit" | "exitcodes" => Some(EXIT_CODES),
        "pipeline" | "pipelines" => Some(PIPELINE),
        "each-line" | "eachline" | "fan-out" | "fanout" => Some(EACH_LINE),
        "pause" | "resume" => Some(PAUSE),
        "abort" => Some(ABORT),
        _ => None,
    }
}

pub const AGENTS: &str = "\
qex for agents
==============

You cannot see the work that another agent is about to start, and it cannot see
yours. qex holds the work of every agent on this machine in one queue. Declare
what your job claims. qex starts the job when the claim fits the machine, writes
the result to the disk, and gives you one command that waits for it.

Which command
-------------

    I WANT                 START THE JOB            ATTACH TO A JOB
    the output as it       qex run -- CMD           qex status <id> --follow
    arrives                (= qex submit --follow)
    the record when the    qex submit --wait        qex status <id> --wait
    job stops
    the exit code only     qex submit --wait -q     qex status <id> --wait -q
                                                    qex wait <id>... for many

Each one gives the exit code of the job. They differ in what they WRITE: the
output of the job, the record of the job, or nothing.

`--quiet` silences the RECORD and the reason that a job waits, and `--json`
puts the record on stdout as JSON. NEITHER silences a fault of the wait — a job
that does not exist, a wait that reached its limit, a wait that a signal stopped
— because those lines give the id that attaches to the job again, and they go to
stderr where they cannot mix with JSON.

`--follow` writes the log from its FIRST line, so a job that already stopped
gives you the whole log. Use `qex logs <id> --tail` or `qex status <id> --wait`
for such a job.

USE `qex submit --wait` FOR YOUR LONG WORK:

    qex submit --wait --cpu 2 --mem 4GB --id-file train.id -- uv run train.py

- One command. `qex submit` and then a wait is two, and the second is a thing to
  forget: a job that nobody waits for still runs, and nobody reads the result.
- Your harness waits, because the command waits. Run it as a background command
  of your harness: the harness reports the end, so you need no timer and no
  second command.
- A SUBAGENT WAITS IN THE FOREGROUND. A harness often does not wake a subagent
  when a background command ends. Such a harness resumes the subagent only when
  the parent sends a message. A subagent that ends its turn to wait then stays
  stopped until its parent notices the silence. Give every wait a limit below
  the command limit of your harness:
  `qex submit --wait --wait-timeout TIME`, or `qex wait <id> --timeout TIME`.
  Wait again when the command gives 124.
- The output goes to the log file, so a job of two hours does not fill your
  context. The command ends with the RECORD: the state, the exit code, the
  resources, and the last lines of both streams.
- `--id-file` reaches the disk BEFORE the wait, so any interruption still leaves
  you a handle: `qex status $(cat train.id) --wait`.

EVERY SUBMISSION JOINS A WAIT OR GETS ITS OWN. With `--wait` that is automatic.

`qex run` writes the output of the job to your terminal as it arrives, which is
right for a short task that you watch, and wrong for a long one that fills your
context. Ctrl-C and SIGTERM on `qex run` stop the JOB. Every other command in
the table watches the job and never stops it: Ctrl-C there gives 122, and the
job continues.

A COMMAND THAT WRITES THE OUTPUT TO YOUR TERMINAL OWNS THE JOB UNTIL THE JOB
STARTS. That is `qex run` and `qex submit --follow`. When such a command stops,
and its job still waits in the queue, the coordinator cancels that job: the
output had one reader, and that reader went away. This holds for a stop that
the command cannot catch, such as SIGKILL, because the coordinator reads the
connection and not a process number.

A job that ALREADY OPERATES continues. It holds a claim and does work, and its
output waits in the log file for `qex status <id> --follow`.

Every other command leaves the job. `qex submit --wait` and `qex wait` put the
job in the queue, or find it there, to live on its own, so a reader that stops
the wait has not asked qex to throw the work away. Use `qex submit` for work
that must live longer than the command that starts it, and `qex cancel <id>`
to remove a job that waits.

With `--wait` and with `--follow`, the id goes to STDERR, because stdout carries
the result. Read it there, or use `--id-file`.

Exit codes
----------

    0 to 96     THE JOB. The exit code of the job, unchanged.
    97 to 127   QEX. The queue or the wait, never the job.
      97        the job gave a code from 97 to 255. Read the record for it.
      98        A SIGNAL ENDED THE JOB: not TERM, not KILL, and not from qex.
                A fault in the job, or `kill -INT`. The record names it.
      99        THE KERNEL STOPPED THE JOB FOR MEMORY. qex reports this with no
                configuration. Give a larger `--mem` and submit the work again.
      100       the job has not stopped, so there is no result.
      121       qex could not do what you asked. No job ran.
      122       your wait stopped, and the job did not. Attach to it again.
      123       the job gave up in the queue. It reached `--max-queue-time`.
      124       qex stopped waiting, because it reached a time limit.
                Your wait reached `--timeout`, and the job continues. EVERY
                command gives this code when the COORDINATOR did not answer
                inside its limit, `qex list` and `qex top --once` included.
                The page of `qex top` that a person watches keeps drawing and
                gives 0: a display that drew did its work, and `--once` is a
                query that an agent reads.
      125       something stopped the job: a kill, a cancel or a time limit
      126       a job that this job needed did not succeed
      127       there is no job with that id
    128 and up  QEX ITSELF died from a signal. The job is not described, and it
                can still operate. Attach to it again.

THE CODE ANSWERS PASS OR FAIL. THE RECORD ANSWERS WHY. Read `qex status` when
you act on the difference between \"the job failed\" and \"my wait stopped\".
A code from 125 does NOT say that your work failed: another agent on this
machine can stop your job, so read the line on stderr before you run it again.

`qex submit`, `qex pipeline` and `qex rerun` use the same band: a refusal is
121, and never 1. `qex list`, `qex logs` and the other commands never speak for
a job, so they use 0, 1, 2 and 127 in the usual way. Run `qex help exit-codes`
for the band.

If qex cannot start at all
--------------------------

Some harnesses run each command in a SANDBOX, and a sandbox can refuse the Unix
socket that qex needs. qex then says so, and it names the directory.

You cannot correct that yourself: the permissions belong to the person who
starts you. Give them this page, and stop:

    https://github.com/stephenc/qex/blob/main/docs/sandbox.md

Do not write a monitor script
-----------------------------

Every monitor waits for a PROXY: a pattern in the process list, a line in a log,
a file that appears. A proxy can become permanently false, and nothing tells the
monitor. It then waits for ever.

    while pgrep -f solve.py; do sleep 60; done
        The command line of this shell holds those letters, so the pattern
        matches the monitor itself. The count never reaches zero.

    until grep -q DONE run.log; do sleep 60; done
        Correct, until somebody stops the task that writes that line.

    while true; do [ $(ps -Ao args | grep -c solver) -eq 0 ] && break
                   sleep 300; done
        A COUNT, and not a test of one process. On a machine that two agents
        share, the work of the other agent holds it above zero for ever.

The last two hold no pattern fault. They are careful commands, and the fault is
the proxy: evidence of the work stops when the work stops, in a way that the
monitor cannot see.

qex waits for the PROCESS. It is the parent of your task and it uses `waitpid`
on that exact process, so an answer always arrives. To find the coordinator, use
`qex info`, which asks the coordinator. Never search the process list for it.

    qex watchers    find the monitors of this kind on this machine

Your session can stop, and the work continues
---------------------------------------------

The job is not a child of your shell and not a child of you. Somebody stops your
agent, your terminal closes, or qex replaces the coordinator: the job continues
and it still writes its result. Your wait is the only thing that stops, and you
attach again with the id.

    qex submit --wait --id-file build.id -- make   # a person stops you here
    qex status \"$(cat build.id)\" --wait            # a later session, and the
                                                   # result is there

PUT THE ID FILE WHERE IT LASTS LONGER THAN YOUR SESSION: your project directory
or your home directory. NOT a scratch directory of your harness, and not /tmp.
qex gives a warning when the file goes to such a place. If you lose an id,
`qex list --cwd .` gives the jobs of this directory.

Many jobs at one time
---------------------

Give each job its own `qex submit --wait`. Each notification from your harness
then names its own job, and no job waits with nobody to read it.

To read many results in one place, read the stream:

    qex events --json      # one JSON object for each change of state

Keep the `stream_id` of the first line and the largest `seq` that you read, and
give both to `--since` when you start again. Run `qex help events`.

`qex wait A B C` waits for all of them and gives one line for each.
`qex wait --next A B C` gives control back when the NEXT job stops — ONE TIME.
The jobs that did not stop then have no watcher, so wait again for them. qex
names them when it returns.

Resource claims
---------------

Give `--cpu` and `--mem`. qex compares the claims against the budget, which is
75% of the cores and the memory. `qex info` gives it. A CLAIM IS A PROMISE AND
NOT A LIMIT: qex measures a job WHILE it runs, and nothing stops a job that goes
above its claim. qex limits no job.

    half, guess   one half of the budget. Two such jobs operate together.
    full, max     the full budget. The job operates alone.

DO NOT RUN A SMALL TEST JOB TO MEASURE A TASK. It costs time and it measures
different work. Give `guess` and start the REAL task:

    qex submit --wait --cpu guess --mem guess -- ./task   # run 1
    qex submit --wait -- ./task                           # run 2: the claim is
                                                          # ready

qex records what each job really used, and it uses those numbers as the claim
for the next job of the same command. It keeps the LARGEST measurement and adds
a margin. A job that the kernel stopped for memory teaches qex nothing today, so
give a larger `--mem` yourself. `qex status` says where a claim came from, and
`qex status --json` gives `max_rss` and `cpu_secs`.

A claim that is larger than the whole budget still runs: qex starts the job
alone when nothing else operates, and the field `forced` is true. The job can
then swap or stop for memory, and that result is data.

Give each submission a key
--------------------------

You lose your context and you run your script again. Without a key, qex starts a
SECOND copy of a four-hour run beside the first, and both write the same files.

    qex submit --wait --dedupe-key train:$(pwd) -- uv run train.py

While a job with that key waits or operates, a second submission starts nothing:
qex gives the id of that job and exits with the code 0, so your script does not
change. DO NOT READ `qex list` AND DECIDE FOR YOURSELF: that test is a proxy,
and another agent can submit between your read and your submission. The
coordinator makes the test and the submission one step.

Choose a key that names the work AND the place: `build:$(pwd)`. A key names the
work, and qex does not compare the command, so give each different piece of work
its own key. The key is free when the job stops; `--dedupe-window 1h` keeps the
key of a job that SUCCEEDED for an hour also. A wait that a key gave you is a
wait for the job of another agent, so Ctrl-C stops your wait only and gives 122.

A stage for each step
---------------------

Do not put the steps in one script. One exit code and one mixed log leave you to
find the cause. Give each step its own job:

    BUILD=$(qex submit --name build -- make)
    TEST=$(qex submit --name test --needs $BUILD -- make test)
    qex status $TEST --wait

    --needs <id>,<id>   wait for these jobs, and stop if one does not succeed
    --after <id>,<id>   wait for these jobs, whatever their result

A stage that does not start becomes `skipped`, and its record names the FIRST
job that failed, so you read the cause and not the chain. `qex wait` gives 126
for a skipped job and the code of the job for a job that failed, so your script
separates its own failure from an earlier one.

An ID must exist, and that is the only rule, so a script can submit its last
stage while an earlier stage already failed. A NAME must give a job that waits
or operates, because a name can give the job of yesterday. Use an id in a
script, and a name when you type a command yourself. `qex pipeline ci.toml`
gives the stages of one file their own names; run `qex help pipeline`.

A job that never starts
-----------------------

A job waits until the machine has capacity, and a claim that no budget can meet
waits with no end.

    qex submit --wait --max-queue-time 30m -- make test

The job does not start after that time. Its state becomes `expired`, the code is
123, and `qex status` says what the job waited for. Nothing ran, so there is no
output. The clock starts at the submission and a restart of the coordinator
continues it. There is no limit by default: work that a person wanted is work
that qex does not discard for you.

Other options
-------------

    --name NAME        letters, numbers, `-`, `_` and `.` only. Not a first
                       `-`. 128 characters or fewer.
    --timeout 4h       stop the JOB after this time. `--wait-timeout` stops
                       your wait instead, and the job continues.
    --retries 3        run the job again when it fails. One id, one record,
                       every attempt in the log. The job keeps its claim until
                       it stops. A kill for memory starts no new attempt: give
                       a larger `--mem` and submit again.
    --lock NAME        two jobs with one lock name never operate together. Use
                       it for a build directory, a port or a database.
    --nice N           -20 to 19. A larger number gives way to a person. The
                       default is 10.
    --gpu N            claim N devices from the pool `gpu`. qex says which
                       index the job gets and writes CUDA_VISIBLE_DEVICES.
    --vram SIZE        claim SIZE on EACH GPU. qex does NOT add the memory of
                       the devices together.
    --claim NAME=N     claim N units of the pool NAME. `--lock NAME` is the
                       same as `--claim NAME=1`. Run `qex help resources`.
    --env K=V          add or replace one variable. qex copies your environment
                       and your directory, so use `--env-capture minimal` when
                       your shell holds secrets.
    --job FILE         read the job from a TOML, YAML or JSON file. Run
                       `qex help job-file`.
    --each-line FILE   one job for each line, and one group id. `{}` takes the
                       line, and qex starts no shell, so a line is one argument
                       and never a command. Run `qex help each-line`.

Other commands
--------------

    qex list --json            all the jobs, their states and why they wait
    qex status <id> --json     one job in detail, with the last error lines
    qex logs <id> --grep ERR   the lines that you want from a large log
    qex kill <id>              stop a job that operates
    qex cancel <id>            remove a job from the queue
    qex abort                  stop your jobs and empty your part of the queue
    qex rerun <id>             submit the same job again, with a new id
    qex clean --state done     delete the records of the jobs that stopped
    qex info                   the coordinator and the free capacity
    qex pause / qex resume     stop the queue, or give it back

Every command that reads data accepts `--json`, and each command accepts the
first characters of an id or the name of a job. `qex schema status|job|pipeline`
gives the JSON Schema of each format.

If the coordinator is older than your command
---------------------------------------------

A new build can replace the qex program while a coordinator operates. qex asks
the coordinator what it can do and REFUSES a job that it cannot obey, because a
field that the coordinator does not know is ignored in silence: a lock that
nothing applies looks exactly like a lock that operates. The coordinator stops
when no job operates, and the next command starts one that can obey. The jobs
continue, because a new coordinator reads the same records. `qex version` gives
what each side can do.

Tell the people who make qex
----------------------------

Each part of this page came from a report of a fault or a measurement.

    Something helped you        https://github.com/stephenc/qex/issues/1
    A recipe worth sharing      https://github.com/stephenc/qex/issues/2
    Something did NOT work, including a word that sent you the wrong way:
        LOOK FIRST, AND ADD TO WHAT IS THERE.
            gh issue list --repo stephenc/qex --state all --search \"words\"
            gh issue comment <number> --repo stephenc/qex
        Open a new issue only when nothing matches. A SMALL FAULT IS WORTH AN
        ISSUE: an option that you looked for and did not find, a message that
        sent you the wrong way, an exit code that surprised you.

Give four things: the command in full, what qex wrote in full (use `--json`),
what you expected, and the output of `qex version`. DO NOT PUT THE OUTPUT OF
`--show-env` IN A PUBLIC PAGE: it holds the environment of your shell.
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

    name = \"train-model\"          # letters, numbers, `-`, `_` and `.` only
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
    cpu  = 3
    mem  = \"8GB\"
    gpu  = 2                      # devices from the pool `gpu`
    vram = \"20GB\"                 # on EACH device that this job gets

    [resources.claims]
    net = 1                       # 1 unit of the pool `net`

    [env]
    HF_HOME = \"/data/hf\"

Fields
------

`command` is a list of arguments. It is not a shell command line. qex does not
start a shell, so you need no quotation marks and no escape characters. To use
a shell feature such as a pipe, name the shell:

    command = [\"bash\", \"-lc\", \"a | b > c.txt\"]

`mem` accepts `8GB`, `8G`, `512MB` or a number of bytes. One unit step is 1024.

`gpu` and `vram` claim the pool `gpu`. `vram` is the quantity on EACH device
that the job gets, and qex does NOT add the memory of the devices together.
With no `vram`, the job takes the whole of each device that it gets.

`[resources.claims]` claims the other pools. Give a number, or a table with a
count and a size: `tpu = { count = 2, size = \"8GB\" }`.

DO NOT SET `CUDA_VISIBLE_DEVICES` IN `[env]` FOR A JOB THAT CLAIMS A GPU. qex
gives the devices to the job and writes that variable, so the two values would
disagree. qex refuses such a job and says so.

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

pub const PAUSE: &str = "\
qex pause and qex resume
========================

Use these commands to take the machine, or one resource of it, back for a
moment.

    qex pause queue              start no new job
    qex resume queue             start the queue again

    qex pause lock <name>        take that lock for yourself
    qex resume lock <name>       give the lock back

    qex pause                    say what is paused now

`qex resume` with no word starts the queue again.

Pause the queue
---------------

    qex pause queue --reason \"recording a demo\"

qex then starts NO job. There is no exception: every job in qex has a claim, so
a job that costs nothing does not exist, and `paused` is one fact that you can
act on.

THE JOBS THAT OPERATE NOW CONTINUE. Each one already holds its capacity, and a
stop would lose that work. A job with `--retries` is still that job: the next
attempt starts, and the job keeps its locks, until the last attempt stops. To
wait for a quiet machine:

    qex pause queue --drain      # gives control back when no job operates

To stop a job that operates, use `qex kill <id>`.

Give the pause an end
---------------------

    qex pause queue --for 30m

A pause with no end continues until you run `qex resume`. Every command that
lists jobs says so, because a pause that a person forgets gives an empty queue
in the morning.

A second `qex pause queue` KEEPS the end and the reason of the first one. A
command that replaced them would change a pause of 30 minutes into a pause with
no end. To replace an end, run `qex resume queue` first.

`--for 0` is an error. To end a pause now, run `qex resume queue`.

Pause a lock
------------

A lock names a resource that one job at a time may hold (`qex submit --lock
gpu0`). You frequently need that same resource by hand.

    qex pause lock gpu0

qex gives the lock TO YOU as soon as no job holds it. Every job that needs it
waits, and `qex list` gives the reason:

    b0bb2614  queued  train  ...  waits for the lock `gpu0`, which a person holds

The command never fails when a job holds the lock now. qex records the request,
that job keeps the lock, no other job takes it, and the lock comes to you when
that job stops. The command is thus safe to type at any moment.

What survives
-------------

The pause is a file beside the job records, so it survives a coordinator that
stops. A new coordinator reads it and the queue stays paused.

If qex cannot read that file, it HOLDS the queue and says so. A file that qex
cannot read can hold a pause, and qex does not know. `qex resume queue` writes
a new file and starts the queue again.

The pause covers YOUR queue only. It does not pause another user of the
machine. `qex info` says so.

What a pause does NOT do
------------------------

A pause does not expire a job. `--max-queue-time` measures the time that a job
waits for the QUEUE, and a person who holds the machine is not the queue. The
clock of that limit stops at the pause and runs again at the resume, so a pause
of 30 minutes does not kill every job with a smaller limit. `qex status` gives
that time in `queue_pause_secs`.

This holds when the pause ends by itself while no coordinator operates. The next
coordinator finds it and gives the time back.

A pause of a LOCK does not stop that clock. A job that waits for a lock already
expires in the same way, whatever holds it. Give such a job a
`--max-queue-time` that covers the hold, or no limit at all.

A pause refuses no command. `qex submit` gives you a job id and the exit code 0,
and the job waits with the pause as its reason.

Who may end it
--------------

Anybody who can reach this queue. A pause is not a lock on the queue: the queue
belongs to one user of the machine, and everybody who reaches it already shares
every job in it. Each line below names the pid that asked for the pause, so you
can find the owner before you start the queue again.

Where to read it
----------------

    qex pause                    what is paused, for how long, and who asked
    qex info                     the same line, with the budget and the load
    qex top                      the same line, on the page
    qex list                     the same line, before the jobs
    qex wait <id>                the same line, before the wait begins
    qex status <id>              the reason that one job waits
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
    reserve_mem  = \"2GB\"  # memory to keep free for other programs (see mode)
    max_pressure = 20     # maximum PSI memory pressure (Linux only)

    [enforce]
    mode = \"cooperative\" # cooperative, or single-user

    [peers]
    enabled = true
    dir = \"/tmp/qex\"
    stale_after = \"30s\"

    [queue]
    oversized = \"run-when-idle\"   # run-when-idle, reject or queue
    settle = \"3s\"
    max_bypass = 2        # jobs that may start before the job at the front

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
    vram = \"0\"            # 0 means: a job with no --vram takes the whole device
    # A pool with devices. qex says WHICH one each job gets.
    [[pool]]
    name    = \"gpu\"
    size    = \"vram\"                          # the quantity each device holds
    devices = [\"24GB\", \"24GB\", \"24GB\", \"24GB\"]
    env     = \"CUDA_VISIBLE_DEVICES\"

    # A pool with no devices. The number is sufficient.
    [[pool]]
    name  = \"net\"
    count = 4

    [hooks]
    on_stop = []          # the command that qex runs when a job stops
    on_stop_states = [\"completed\", \"failed\", \"killed\", \"timeout\", \"expired\", \"oom\"]
    timeout = \"30s\"       # the time limit for that command

    [update]
    check   = \"7d\"        # how often qex looks for a newer release, or `never`
    url     = \"https://api.github.com/repos/stephenc/qex/releases/latest\"
    timeout = \"5s\"        # how long qex waits for that service

A newer qex
-----------

qex looks for a newer release of itself, and it says one line when it finds
one. It never installs anything: the person who installed qex chose how, and a
package manager can own that file.

`[update] check = \"never\"` stops the AUTOMATIC check completely. qex then opens
no connection of its own, writes no file for it, and says nothing about a
version, for ever.

`qex version --check` still asks, because a person asked it to. A command that
refused would leave that person with no way to answer the question at all.

THE COORDINATOR ASKS, AND YOUR COMMAND NEVER DOES. A check must not delay a
command and must not fail one, so the network stays out of the path of a
command: the coordinator asks on its own time, in its own thread, and every
command reads the answer from a file. One call also serves every agent on the
machine.

THE FIRST WEEK IS QUIET. A fresh install writes the time and asks nothing,
because a person who installed qex a moment ago holds the newest release
already. The first question comes after the first interval.

qex says the line ONE TIME for each release, on stderr, so
`ID=$(qex submit ...)` stays correct.

    qex version --check          ask now, and say what the answer was
    qex version --check --json   the same answer for a program

That command asks at the moment that you run it, because you asked it to. It
gives 0 when the answer arrived, whatever the answer says, and 1 when qex could
not ask: a newer release is information and not a fault. Read `newer` in the
JSON to act on it.

qex runs `curl`, and then `wget`, and it says so when the machine has neither.
An HTTP client inside qex would bring a TLS stack to a tool that holds nine
dependencies. `url` takes a mirror, and every answer names the service that
gave it.

A DEVELOPMENT BUILD IS NEITHER NEW NOR OLD. A build from a working copy carries
`0.0.0-dev+g98513e2`, which is not a release and takes no place in the order.
`qex version --check` says what it is, and the automatic line never appears for
it.

Quotation marks around a number
-------------------------------

A field that takes a number, a size, a time or a percentage accepts the value
with quotation marks and without them. `cpu = 2` and `cpu = \"2\"` give the same
budget, and `margin = 1.5` and `margin = \"1.5\"` give the same margin. A size
with no unit is bytes, and a time with no unit is seconds.

What qex may assume about the machine
------------------------------------

`[enforce] mode` says who else uses this machine. QEX LIMITS NO JOB IN EITHER
VALUE: a claim decides what STARTS and when, and a job that claims two gigabytes
and uses twenty still fills the machine.

    cooperative   other users, other agents and people share this machine.
                  This is the default.
    single-user   qex decides what runs here.

`single-user` changes three values, and each one follows from that assumption:

    the budget       75% of the machine becomes 90%. The quarter that
                     `cooperative` leaves is room for work that qex does not
                     control, and there is no such work here.
    the peers        qex looks for no other coordinator, and reads the shared
                     directory for nothing.
    the reserve      `[system] reserve_mem` falls from 2GB to 512MB.

A value that you write always wins, in either mode. Run `qex info` to see the
budget that qex uses now, and `qex config show` for the mode.

The section is named `[enforce]` because a way to hold a job to its claim would
attach to `single-user`, and a file that already names the mode would then need
no new key. qex holds a job to nothing today.

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

Name an ABSOLUTE path. The hook starts in the directory of the job, and the
person who submitted the job chose that directory, so `[\"./notify\"]` selects a
program that the submitter can put there.

The value is a program and its arguments. qex starts no shell, in the same way
as for a job. To use a shell feature, name the shell:

    [hooks]
    on_stop = [\"bash\", \"-lc\", \"echo \\\"$QEX_JOB_NAME $QEX_STATE\\\" >> ~/qex.log\"]

The job supplies these variables. A variable with no value is empty text.

    QEX_JOB_ID        the job id
    QEX_JOB_NAME      the job name, in the safe form that `qex list` shows
    QEX_STATE         the final state
    QEX_EXIT_CODE     the exit code of the job, if the job ran to its own end
    QEX_SIGNAL        the signal number, if a signal stopped the job
    QEX_ELAPSED_SECS  the seconds that the job ran
    QEX_CWD           the directory of the job
    QEX_JOB_DIR       the directory of the record, which holds the logs
    QEX_ATTEMPTS      the number of times that qex started the job
    QEX_MAX_RSS       the maximum memory in bytes
    QEX_TAGS          the tags, separated by a space

The values arrive in the environment and never in a command line. qex builds no
text that a shell reads, so a job name such as `x; rm -rf ~` is a name and never
a command, whatever the hook does with it.

QEX_JOB_NAME is the SAFE name: the letters, the numbers and `-_.` only, which is
the one form of a name that qex shows anywhere. A hook puts a name in front of a
person, and a raw name with an ESC byte in it moves the cursor of a terminal and
writes over the text around it. That name goes back into `qex status` as it
stands. A hook that needs the name that the submitter typed reads `status.json`
in QEX_JOB_DIR. QEX_TAGS and QEX_CWD have no such rule, so qex replaces each
control character in them with a space.

QEX_EXIT_CODE is the code of the JOB. It is empty for a job that something
stopped, because such a job gave no code of its own. Read QEX_STATE first: it
holds the same word that `qex status` prints. Run `qex help exit-codes` for the
codes of the commands.

`on_stop_states` selects the jobs that give a message. The default list holds
each state of a job that ran, and `expired`: a job that gave up waiting never
ran, so nothing else says so. `cancelled` and `skipped` are not in it: you
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
process group of its own. The hook writes into a pipe and never into a file, so
at 1MB of output qex stops reading, shuts the pipe and stops the hook. The two
streams of the hook thus stop growing AT 1MB. This bounds those streams only: a
hook that opens a file of its own is a program that you chose to run. That size is fixed, and it is NOT `[logs]
max_bytes`, which limits the output of a job. A hook that
fails does not change the job, and qex writes nothing in the `error` field of
the job for it.

A hook is not a job. It does not take the `[politeness]` values, because those
make work give way to a person and a notification is FOR the person. It does not
receives no variable of `[claims]`, because it makes no claim on the budget.

qex reads the config file at each job that stops. A hook that you delete thus
runs no more, and a hook that you add runs at once. You do not restart the
coordinator.

The output goes to `hook.log` in the directory of the job. Read it with
`qex logs <id> --hook`, which also gives the verdict of qex: a hook that did not
start, a hook that was too slow, or a hook that stopped with an error.
Pools
-----

A pool is a name and a total. A job claims units of a pool with `--claim
NAME=N`, and `--lock NAME` is the same as `--claim NAME=1`.

A pool with `devices` also gives a capacity to each device. qex then says WHICH
device each job gets, writes the index into `QEX_<NAME>_DEVICES`, and writes it
into the variable that `env` names.

Give `count` or `devices`, and not both. Use `devices` when qex must say WHICH
one a job gets. Use `count` when the number is sufficient.

A pool cannot use the name `cpu` or `mem`. Those two are in `[budget]`.

A pool that this file declares is shared with the other users of the machine:
each coordinator publishes the units and the device indices that it gave away.
A name that this file does NOT declare is a lock, and a lock stays inside one
queue.

qex reads no driver. The devices come from this file only, so a machine with no
CUDA and no driver library schedules GPU claims correctly.

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
    GOMAXPROCS                             Go
    OMP_NUM_THREADS                        OpenMP: C, C++ and Fortran
    OPENBLAS_NUM_THREADS, MKL_NUM_THREADS  numpy, pandas and the libraries
    NUMEXPR_NUM_THREADS                      below them
    VECLIB_MAXIMUM_THREADS                 Accelerate, on macOS
    RAYON_NUM_THREADS, CARGO_BUILD_JOBS    Rust
    JULIA_NUM_THREADS                      Julia
    DOTNET_PROCESSOR_COUNT                 .NET
    POLARS_MAX_THREADS                     Polars

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

A kill for memory
-----------------

The kernel stops a process when the machine runs out of memory. qex REPORTS
that, and qex acts on it in no way: the job gets the state `oom` and the exit
code 99, and no new attempt starts.

qex cannot say that your job was the victim. It reads a count of out-of-memory
kills below the cgroup of its own process, and every program of your user raises
that count. A machine that is short of memory is also the machine on which a
person uses `kill -9`, so the two arrive together.

So the claim of your job CAN BE CORRECT. Read the `usage` field of the record
and compare it with the claim. Run the same work again when the memory is free,
and give a larger `--mem` value if the usage was near the claim.

qex applies no memory limit, so a job that claims two gigabytes and uses twenty
still fills the machine. The claim decides what STARTS and when.

ON macOS THERE IS NO SUCH COUNT, so a kill for memory gives the state `killed`
and the code 125. Do not wait for the code 99 on a Mac.

The order of the queue
----------------------

The order is a RESERVATION WITH A BOUNDED BYPASS, and it is not strict order.

`[queue] max_bypass` gives the number of jobs that may start before the job at
the front of the queue. The default is 2. After that number, qex keeps the
capacity for the job at the front and starts nothing else, so a stream of small
jobs cannot hold a large job in the queue for ever.

With `max_bypass = 0`, no job passes a job at the front THAT KEEPS CAPACITY.
The order is then strict for those jobs, and one large job stops the queue while
it waits. The value changes nothing for the classes below that keep no capacity
at any value: qex starts the jobs behind those, because it does not control the
holder and no wait for it would end.

qex keeps capacity only while the jobs of THIS queue hold it, or while a large
job waits for a quiet machine. qex schedules those releases. If another user or
a program outside qex holds the capacity, qex starts the jobs behind the job at
the front and keeps no capacity: an empty machine gives that job nothing,
because qex does not control the holder. The count of the jobs that passed is
NOT reset when the holder changes, so the job becomes unpassable in the same
scheduler cycle in which the other user releases the capacity.

A job that waits for a job that it needs, for a lock, or for a pause never
keeps capacity. None of the three takes capacity, so there is nothing to keep,
and a paused queue starts no job at all.

A bypass does not change the queue. The order stays the order of `--priority`
and then of the submission, and the job at the front starts before every job
behind it as soon as it can start.

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

Pools: GPUs, VRAM and counted locks
-----------------------------------

The cores and the memory are two quantities. Everything else that a machine can
count is a POOL: a name, a total, and, when qex must say WHICH one, a list of
devices.

    --gpu N            claim N devices from the pool `gpu`.
    --vram SIZE        claim SIZE on EACH GPU that this job gets.
    --claim NAME=N     claim N units of the pool NAME.
    --lock NAME        the same as `--claim NAME=1`.

    qex submit --cpu 4 --mem 16GB --gpu 1 --vram 20GB -- uv run train.py
    qex submit --cpu 8 --mem 32GB --gpu 2 -- uv run train.py    # 2 whole devices
    qex submit --claim net=1 -- ./download.sh
    qex run --lock target -- cargo test

Declare a pool in `~/.config/qex.toml`. Run `qex help config` for the form.

qex does not add the VRAM of the devices together
-------------------------------------------------

A job that needs 40GB on one device cannot run on two devices of 24GB. qex
refuses such a job and says that it can never start.

`--vram SIZE` is the quantity on EACH device that the job gets. With no
`--vram`, the job takes the whole of each device that it gets. That is the safe
default: a claim that consumed nothing would let qex put four unlimited jobs on
one card. `[defaults] vram` lets you change it.

qex says which device, and it tells the job
-------------------------------------------

qex gives the devices with the most free capacity first, and the lowest index
for a tie. The job then sees both:

    CUDA_VISIBLE_DEVICES=2,3      # because the pool `gpu` names this variable
    QEX_GPU_DEVICES=2,3           # always, for every indexed pool
    QEX_GPU_VRAM=21474836480      # the quantity on each device, in bytes
    QEX_CLAIM_NET=1               # for a pool with no devices

    qex status <id>               # the line `devices: gpu 2,3`
    qex status <id> --json        # the field `assigned`

The variable is what a framework reads with no change to its code. The record is
what you read AFTERWARDS to explain a failure: the record stays, and the
environment goes with the job.

Do not set `CUDA_VISIBLE_DEVICES` yourself for a job that claims a GPU. qex
refuses that job, because the two values would disagree.

qex does not read a driver
--------------------------

The devices come from the configuration only. A machine with no CUDA and no
driver library thus schedules GPU claims correctly: a count in the file, a claim
on the job, and the same arithmetic that admits a job today.

Two users who give different device counts disagree, in the same way and for the
same reason that they can disagree about `[budget]`. The accounting is
cooperative.

A claim above the pool total is always refused
----------------------------------------------

This behaviour is different from the cores and the memory. A memory job that is
too large can run alone and swap, and that result is data. An empty machine does
not make a fifth GPU, so `qex submit --gpu 8` against a pool of 4 gives an error
at the submission, whatever `[queue] oversized` says.

When does a job start
---------------------

qex starts a job when all these conditions are true:

  1. The claims of the jobs that operate, plus this claim, are in the budget.
  2. The claims of the other users leave sufficient capacity.
  3. Each pool that the job claims has free units, or free devices with
     sufficient capacity.
  4. The free memory stays above `reserve_mem` and the memory pressure is below
     `max_pressure`.

If a job waits, `qex status` gives the reason in the `blocked_reason` field.

Why a job waits, and what holds the queue
-----------------------------------------

`blocked_reason` names the holder of the capacity. There are four holders, and
they do not have the same effect on the jobs behind:

  1. The jobs of this queue. qex knows that they stop, so it keeps the capacity
     for the job at the front after `[queue] max_bypass` jobs passed it.
  2. Another user. qex does not control that user, so the wait has no known end.
     qex starts the jobs behind, and it keeps no capacity.
  3. A program outside qex, or memory pressure. The same rule as 2.
  4. The size of the job. A job that is larger than the budget waits for a quiet
     machine, or the config file keeps it in the queue.

Each job that waits gives a reason of ITS OWN. A job behind a job that qex keeps
capacity for gives that fact and the id of the job at the front.

qex counts the jobs that pass the job at the front in the field `passed_by`, and
`blocked_since` gives the time when that job reached the front. The count is not
reset when the holder changes. A job that another user held for an hour keeps its
count, and it is unpassable in the same cycle in which a job of this queue
becomes the holder.

Is the queue healthy
--------------------

    qex info

The last line answers the question. The queue is healthy when a job started
recently, OR when the line names a cause outside this queue: another user or the
machine. The queue is stuck when no job started and the cause is a job of this
queue. `qex top` gives the same line in its header.

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

The `usage` field gives `max_rss` in bytes and `cpu_secs`. For a job with
retries, `max_rss` is the largest peak of any attempt, and `cpu_secs` belongs to
the latest attempt. A task that always uses much less than its claim wastes
capacity: put an exact claim in a job file, and more jobs then operate together.
A task that the kernel stops for memory needs a larger claim, and you must give
it: the correction that raises a claim by itself does not run in this build.
Issue #88 holds the fault.

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
    oom         the kernel stopped the job, because the job used more
                memory than its claim.
    cancelled   qex removed the job from the queue before it started.
    skipped     a job that this job needed did not succeed, so this job
                did not start. The field `caused_by` names the job that
                failed first.

The states `queued`, `starting` and `running` are not final. Each other state is
final and does not change.

The state `oom` is different from `failed`. It says that the kernel stopped the
job for memory, and the code is 99.

A JOB WITH THIS STATE STARTS NO NEW ATTEMPT. qex reads a count of out-of-memory
kills below the cgroup of its own process, and every program of your user raises
that count, so qex cannot say that your job was the victim and cannot say that
the claim was too small. Read the `usage` field, compare it with the claim, and
submit the work again when the memory is free.

The state `killed` also covers a kill that qex cannot explain. The kernel and
`qex kill` both use the signal KILL, and a machine with no cgroup keeps no count
of the kills for memory. qex then gives `killed`, which starts no new attempt,
and the record says that qex could not tell.

The state `expired` is different from `timeout`. For `timeout`, the work is too
slow, and the log file holds the output of the part that ran. For `expired`, the
machine never gave the job a place, so the log file is empty. Read the `error`
field: it says what the job waited for and how long it waited.

Use `qex list --state running` to select the jobs in one state.
";

pub const EVENTS: &str = "\
qex events: one stream for every job
====================================

    qex events --json

The command writes ONE JSON OBJECT ON ONE LINE for each change, as it happens.
Use it in place of a loop that asks about each job. An agent that drives twenty
jobs reads one stream, and it does not send twenty commands again and again.

    qex events --json | while read -r line; do ... done

The command needs no timer. It writes each line at the moment of the change,
and it uses no CPU time while it waits.

The lines
---------

Each line has the field `event`, which gives its type. Ignore a type that you do
not know: a later version of qex can add one.

    stream   the first line. It gives `stream_id`, which is the name of this
             stream, and the numbers that the coordinator holds. KEEP THE
             NAME. See `--since`.
    job      the record of one job changed. See below.
    gap      you lost events, and this line counts them.
    bye      the coordinator stops now, and it says why.

A `job` line holds:

    seq        the number of this event. KEEP IT. See `--since`.
    time       the time of the change
    id, name   the job
    state      the state now
    previous   the state before, or null for the first line of a job
    change     `state`, or `reason` for a job that waits in the queue
    job        the whole record, the same as `qex status --json`

The field `job` holds everything, so you need no second command to learn the
exit code, the measured use or the cause of a failure.

A job that waits gives a line with `change` = `reason`. The field
`job.blocked_reason` then says what the job waits for: memory, a lock, or a
different job. The reason arrives a moment after the job enters the queue,
because the scheduler writes it.

The stream reports what the coordinator SAW
-------------------------------------------

The supervisor of a job writes the record, and the coordinator reads that record
twice each second. A job that is shorter than that period thus gives `starting`
and then `completed`, WITH NO `running` LINE. The field `previous` of that line
says `starting`, so the sequence that you read is the true sequence.

The stream gives no line for a state that the coordinator did not see. A line
for such a state would be a statement that qex cannot support.

Read the stream again after a stop
----------------------------------

    qex events --json --since <stream_id>:348   # the events after 348
    qex events --json --since start             # everything that it holds
    qex events --json --since now               # the new events only

The default is `start`.

KEEP TWO VALUES: the `stream_id` of the first line, and the largest `seq` that
you read. Give both to `--since` when your program starts again, as
`<stream_id>:<seq>`. You then lose nothing while the same coordinator operates.

THE NUMBERS BELONG TO ONE COORDINATOR. The coordinator stops when no job
operates, and the next command starts a new one. That coordinator starts its
numbers at 1 again, and it makes one event for each record that it reads. Your
number 348 thus names a DIFFERENT event there.

With the stream name, qex compares the two and gives you a `gap` line that says
that the coordinator changed, then continues with the events that the new
coordinator holds. Its job records are the same records.

A NEW COORDINATOR THUS GIVES YOU SOME LINES A SECOND TIME. It makes one event
for each record that it reads, so a job that stopped while you were away arrives
again as `completed`, with a new number. This is the ordinary case: the
coordinator retires when no job operates, which is when your program is away.
ACT ON `id` AND `state`, AND NOT ON THE ARRIVAL OF A LINE. Keep the states that
you acted on, by job id, and do the work of a line one time.

WITH A NUMBER ALONE, qex cannot make that comparison, and you can lose events
with no message. Give the name. `qex events` writes a warning when you give a
number with no name.

What happens when you do not read fast enough
---------------------------------------------

The coordinator keeps the last 512 events. It NEVER waits for a reader, and it
never grows its memory for one. If you do not read the stream fast enough, the
coordinator drops the oldest events and you receive a `gap` line that COUNTS
them. Do the work of an event in a different thread or process, and keep the
reader reading.

qex reports a gap. It does not hide one, because a reader that loses `failed`
and hears nothing waits for a result that will never arrive.

The field `missed` counts the events. It is `null` when qex cannot count them,
which occurs when your number comes from a different stream: the two streams
have no common measure, so a number there would say something that qex cannot
support. The `reason` field says what happened.

The end of the stream
---------------------

The coordinator stops when no job operates and no command arrives for one hour.
A reader does NOT hold it open. Before it stops, it writes a `bye` line, and the
command then exits with the code 0.

If the stream ends with NO `bye` line, something stopped the coordinator. The
command writes a message to stderr and exits with the code 1. The records of the
jobs are on the disk and they are correct; run the command again to read the
stream of the next coordinator.

Options
-------

    --json            one JSON object for each line. Use this option.
    --since VALUE     `start`, `now`, `<stream_id>:<seq>`, or a bare `seq`
    --count N         stop after N events
    --timeout TIME    stop after this time. The exit code is then 124.

An earlier coordinator
----------------------

A coordinator that operates can be older than this command. Such a coordinator
does not know this request, and `qex events` REFUSES to run: it names the
coordinator, and it gives the command that stops it. It never gives you an empty
stream, because an empty stream and a stream with no events look the same.
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
    qex schema event

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

    qex events --json  one line for each change of state, as it happens.
                       Use this command in a program. See `qex help events`.
    qex top            the jobs, the claim of each one, and its true use now
    qex top --once     one page, for a script
    qex top -i 5       a refresh every 5 seconds

The page that a person watches fits the screen. Use the arrow keys or j and k
to move the selection. x stops the selected job. c takes it out of the queue.
i shows the command, the directory, the wait, the run and the note. t shows
the tail of the logs in the lower half of the screen. x stops a job that
operates. c takes a job out of the queue. C deletes the record of a job that
has stopped. q stops the command.

`--once` writes every job that the page names, and it does not wait for a key.
Use it in a script.

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
    qex clean --older-than 7d      each job older than 7 days
    qex clean --all                every job
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

Two rules keep a record, and a record that one rule keeps stays:

  * every job that an unfinished job waits for, through the WHOLE chain. A job
    that waits for one job also depends on the jobs behind that job.
  * every job of a pipeline that has one job which has not stopped. The jobs of
    a pipeline are one piece of work, and a pipeline can divide, so a stage
    that no later stage waits for is still a stage of that pipeline.

Each rule needs a job that has not stopped, so no record stays after that work
stops. A job that never stops, such as a stage that waits for a lock, holds its
records while it waits.

`qex clean` deletes the directory of the job. It does not stop a job that
operates.

qex keeps the id of a deleted job for one day, so `qex status` can tell you that
a job existed and that its work happened. An agent thus does not repeat work
after a deletion. Change that time with `[history] keep` in the config file.

`qex clean --all` deletes the record of EVERY job of this user, including the
jobs of a different agent that shares this machine. Use `qex clean <id>` when
another agent uses qex at the same time.
";

pub const ABORT: &str = "\
qex abort
=========

Use this command to give up a piece of work: a sweep of thousands of jobs, or a
run that must not continue.

    qex abort                    your jobs of this directory
    qex abort --tag phase        the same, with the tag `phase` only
    qex abort --keep-running     cancel the queued jobs; let the jobs that operate finish
    qex abort --cwd              every job of this directory, whatever process submitted it
    qex abort --all              every job of your queue

What it does, in order
----------------------

1. It pauses the queue. No job starts from this moment.
2. It cancels every queued job of the scope, and it deletes their records. A
   job that never ran leaves nothing that a reader needs, and `qex status <id>`
   still says that the job existed and was cancelled. A record that a job
   outside the scope needs stays, in the state `cancelled`.
3. It sends TERM to every job of the scope that operates, and KILL after the
   grace time (`--grace`, default 10s) to each one that continues. This is
   `qex kill`, for each job. Their records stay, so you can read their output.
4. It reports what it did, and the queue STAYS PAUSED. Run `qex resume queue`
   to start new work.

Steps 1 to 3 happen in one request to the coordinator, which does the first
two under one lock. No job can start between the pause and the cancel, and
the cost does not grow with one round trip for each job.

The report counts what the coordinator DID. A job that it could not signal is
listed with the reason, and the exit code is then 1. The `outside` line counts
the jobs that wait or operate outside the scope, so you know what the command
did not touch.

The scope
---------

Several agents run as one user on one machine, and one queue holds the jobs of
all of them. Without an option, the scope is therefore narrow:

    the jobs of THIS DIRECTORY that YOUR PROCESS TREE submitted

`qex submit` records the chain of processes above it, up to the first process
of the machine. `qex abort` reads its own chain. A job is yours when the two
chains share one process that is still the same process, below the point where
the session ends: a terminal multiplexer, a login service, a terminal program,
a service manager, the first process of the machine, or the supervisor of a
qex job. `qex status <id>` shows the chain of a job with that point marked.

What you can predict:

  * Two commands of one agent share the agent process. They are one context,
    whatever shell the agent ran for each command.
  * Two agents in two panes of one multiplexer, or in two windows of one
    terminal program, share nothing below the boundary. They are two contexts.
  * A job that a job submitted has the supervisor of that job as its boundary.
    It is in the context of that job, and in no context of an agent.
  * A job from an earlier qex, or from a helper that left your process tree,
    has no chain that reaches you. It is outside your context.
  * When qex cannot find the end of your session, your context is empty, and
    the default scope is no job. The scope line says so.

`--cwd` drops the process test: every job of this directory, whatever
submitted it. `--all` is every job of your queue. `--tag` narrows each level.
qex stops only the jobs of the user who runs it. A queue belongs to one user,
and no option reaches the jobs of another user.

The pause
---------

The pause is the pause of `qex pause queue`: it covers the whole queue of your
user, so the jobs of the other agents wait as well until somebody runs
`qex resume queue`. Every command that lists jobs says so, and names the pid
that asked. A pause that a person made earlier keeps its end and its reason.
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
`max_queue_time`, `tags`, `priority`, `env_capture`, `nice`, `locks`, `retries` and
`[resources]`. A stage thus claims a GPU in the same way as a job file:
    [[jobs]]
    name = \"train\"
    command = [\"uv\", \"run\", \"train.py\"]
    needs = [\"build\"]

    [jobs.resources]
    cpu  = 4
    mem  = \"16GB\"
    gpu  = 1
    vram = \"20GB\"

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

For one command and many inputs, use `--each-line`. Run `qex help each-line`.

";

pub const EACH_LINE: &str = "\
qex fan-out: one job for each line
==================================

One command, many inputs. `--each-line` reads a file and submits one job for
each line. Put `{}` in the command, and each job gets the text of one line
there.

    qex submit --each-line inputs.txt -- ./process {}

The jobs share one group id. The group id goes to stdout, and the name and id of
each job go to stderr, so this operates:

    GROUP=$(qex submit --each-line inputs.txt -- ./process {})
    qex list --group $GROUP

Read the lines from another program with the name `-`:

    ls *.parquet | qex submit --each-line - -- ./convert {}

Where `{}` goes
---------------

`{}` goes in any argument, in the program name, or inside an argument:

    qex submit --each-line urls.txt -- curl -o {}.html https://{}/

Every `{}` takes the line. A command with NO `{}` gives an error, because each
job would then be the same command and the lines would have no effect.

Write `{{}}` for a literal `{}`. Nothing else in the command changes.

A line is data, and never a command
-----------------------------------

qex starts no shell. Each line becomes exactly ONE argument, whatever it holds:
a space, a quotation mark, a semicolon, a dollar sign or a newline. A file of
names from a directory listing or from a database is therefore safe.

    a b\"; rm -rf ~; echo $HOME

That line gives one argument with those characters in it. No shell reads it.

To use a shell feature, name the shell, and give the line as an argument and
never inside the text of the script:

    qex submit --each-line names.txt -- bash -c 'echo \"$1\" | tr a-z A-Z' _ {}

A line that starts with a dash
------------------------------

A line becomes an argument, so a line such as `-v` or `--out=/etc/passwd`
becomes an OPTION of your program. qex cannot know which arguments your program
reads as options, so it does not change the line.

Put `--` in the command before `{}`. Almost every program then reads the line as
data and not as an option:

    qex submit --each-line names.txt -- ./process -- {}

This is the same rule as `xargs`. Use it for input that you did not write
yourself.

Which lines give a job
----------------------

    a line                 one job
    an empty line          no job
    a line that starts #   no job, it is a comment
    the space at each end  qex removes it
    a CRLF ending          the same job as an LF ending
    no final newline       the last line still gives a job

qex says on stderr how many lines it passed over, so a line that you expected
never goes away in silence.

A file that is not UTF-8 gives an error with the line number, and qex submits
nothing. The command of a job is text, so qex cannot run such a line.

All or nothing, and the one case that is not
-------------------------------------------

qex reads the whole input and tests the command first. Every fault that qex can
find gives an error and NO job at all.

One case remains: qex submits the jobs one at a time, so a coordinator that
stops in the middle leaves the earlier jobs in the queue. qex then writes the
group id and the id of every job that it submitted, and how to stop them. Read
the group id from that message, because stdout holds the group id of a fan-out
that succeeded in full.

The limits
----------

`--each-line` submits 1000 jobs at most. Each job holds a directory, so a file
with 100000 lines would fill the disk. Raise the limit when you need it:

    qex submit --each-line big.txt --max-jobs 5000 -- ./process {}

qex reads 64 MiB at most, from a file and from a pipe, because it holds the
whole input in memory. `--max-jobs` does not raise that limit.

The options that a fan-out refuses
----------------------------------

    --dedupe-key, --dedupe-window   A key holds ONE job. Every job of the
                                    fan-out would carry the same key, so qex
                                    would start the first line only.
    --json                          It writes the id of one job. Use
                                    `--id-file NAME.json` for the group and
                                    every job.
    --job                           The place for the line belongs on the
                                    command line, where a reader sees it.

`qex run` does not accept `--each-line` at all. It waits for ONE job.

The name of each job
--------------------

Each job gets a name for `qex list`: the program name, the position in the
file, and as much of the line as fits.

    process-01-data-a.csv
    process-02-data-b.csv

Give `--name` to change the first part and the name of the group.

A name holds the letters, the numbers, `.`, `_` and `-` only. A line can hold a
terminal control sequence, and a name goes to your terminal in `qex list`.

The other options
-----------------

`--cpu`, `--mem`, `--timeout`, `--max-queue-time`, `--lock`, `--tag`,
`--priority`, `--env`, `--nice`, `--needs`, `--after` and `--retries` apply to
every job of the fan-out.

qex calculates the claim one time, from the command of the FIRST line, and
gives it to every job. The lines of a fan-out are the same kind of work.

`--max-queue-time` is the time that ONE job waits, and not the time of the
group. The jobs that still wait at the end of that time become `expired`.

N jobs make N of everything
---------------------------

`qex events` writes at least 3 lines for each job, and the coordinator holds
the last 512 events only. Start `qex events` BEFORE you submit a large fan-out,
and filter on `.job.group`. A reader that falls behind receives a `gap` line
with the number of events that it lost.

`[hooks] on_stop` runs one time for EACH job that stops, and that includes
`expired` and `skipped`. A fan-out of 1000 lines runs the hook 1000 times. The
hook environment names the job and not the group, so give the fan-out a `--tag`
and read `QEX_TAGS`.

A fan-out learns as one task
----------------------------

qex records what each job used, and gives that measurement to the next job of
the same command. A fan-out does not fit that rule: `./process a.csv` and
`./process b.csv` are two commands, and each one runs one time.

qex therefore measures every job of a fan-out against the TEMPLATE
`./process {}`. One fan-out makes one record, and the second run of the same
fan-out gets its claim from the first run.

`qex status` says `(from the earlier jobs of this fan-out)` for such a claim.

Use `--lock` when the jobs must not operate together:

    qex submit --each-line inputs.txt --lock db -- ./load {}

Keep the ids in a file
----------------------

    qex submit --each-line inputs.txt --id-file ids.env -- ./process {}
    . ids.env                       # gives $group and one name for each job

A name that ends in `.json` gives a JSON object instead, for a parser.
";

pub const EXIT_CODES: &str = "\
qex exit codes
==============

One table. Every command that starts a job or gives you the result of a job
obeys it.

    0 to 96     THE JOB. qex gives the exit code of the job, unchanged.
    97 to 127   QEX. The code describes the queue or the wait, never the job.
      97        the job gave a code from 97 to 255. Read the record for it.
      98        A SIGNAL ENDED THE JOB: not TERM, not KILL, and not from qex.
                A fault in the job, or `kill -INT`. The record names it.
      99        THE KERNEL STOPPED THE JOB FOR MEMORY. qex reports this with no
                configuration. See the note below.
      100       the job has not stopped, so there is no result.
      121       qex could not do what you asked. No job ran.
      122       your wait stopped, and the job did not. Attach to it again.
      123       the job gave up in the queue. It reached `--max-queue-time`.
      124       qex stopped waiting, because it reached a time limit.
                Your wait reached `--timeout`, and the job continues. EVERY
                command gives this code when the COORDINATOR did not answer
                inside its limit, `qex list` and `qex top --once` included.
                The page of `qex top` that a person watches keeps drawing and
                gives 0: a display that drew did its work, and `--once` is a
                query that an agent reads.
      125       something stopped the job: a kill, a cancel or a time limit
      126       the job did not run, because a job that it needed did not succeed
      127       there is no job with that id
    128 and up  QEX ITSELF died from a signal. The job is not described. It can
                still operate, so attach to it again.

The code 98 says that a SIGNAL ended the job. qex tests three things: the job
gave no exit code of its own, no qex command sent the signal, and the signal was
not TERM and not KILL. The record holds the number.

READ THE NUMBER BEFORE YOU ACT, because two causes give this code and they need
opposite answers. A fault INSIDE the job, such as SIGSEGV, SIGABRT or SIGFPE,
says that the work must change. A signal from OUTSIDE, such as `kill -INT` or
the SIGHUP of a terminal, says that somebody stopped the work and that the same
command can run again.

TERM and KILL are the two signals that qex keeps for 125. A kill, a cancel and a
time limit give 125, and an EXTERNAL `kill -9` gives 125 as well: qex cannot
know who sent it.

THE PROCESS THAT TAKES THE SIGNAL IS THE COMMAND THAT QEX STARTED. `qex submit
-- sh -c my_binary` gives the shell to qex, and a shell that survives the crash
of its child exits 139 itself. That is a code from the band, so qex gives 97 and
the record holds 139.

The code 99 says that the kernel stopped the job for memory. qex reads a cgroup
counter before and after the attempt, and a new kill in that counter, with a
SIGKILL that no qex command sent, is the out-of-memory killer.

QEX REPORTS IT, AND QEX ACTS ON IT IN NO WAY. That counter holds every program
of your user below that cgroup, so a kill in a different program raises it as
well, and a machine that is short of memory is also the machine on which a
person uses `kill -9`. The claim of your job can be correct. qex reports 99,
says what you can do, and starts no new attempt.

A machine that keeps no cgroup counter, such as macOS, gives the state `killed`
and the code 125 for the same kill. DO NOT WAIT FOR 99 ON A MAC: qex reports
what it can prove.

THE CODE ANSWERS `PASS OR FAIL`. THE RECORD ANSWERS `WHY`. An agent that acts
on the difference between \"the job failed\" and \"my wait stopped\" reads
`qex status`. An agent that needs pass or fail reads the code.

Every command that starts a job or gives you the result of a job obeys it:
`qex submit`, `qex run`, `qex pipeline`, `qex rerun`, `qex wait`,
`qex status --wait`, `qex status --follow` and `qex status --quiet`.
`qex submit`, `qex pipeline` and `qex rerun` give 0 when they start the work;
they do not wait, so 0 is not the result of the job. A fault of those commands
is 121, and never 1.

Every other command gives 0 for success, 1 for a failure, 2 for a command line
that qex cannot read, and 127 for a job that does not exist. `qex list` never
speaks for a job, so those codes are not ambiguous there.

Why a band, and why a sentinel
------------------------------

A job can exit with any code from 0 to 255. Every code that qex gives itself is
thus a code that a job can give as well, and no single free number escapes that.

The sentinel escapes it. A job that exits 124 of its own accord gives you 97,
and `qex status` holds the 124. A wait that reached its time limit gives you
124. Each code thus has one meaning.

The cost is small and it is real: a job that exits between 97 and 255 loses its
exact code AT THE SHELL, and keeps it in the record. A wrapper that reads `$?`
alone thus sees 97 for each of those jobs, and it reads `qex status --json` to
get the number. Programs that exit in that range are rare, and most of them
speak the same convention as qex: 126 for a program that cannot be executed,
127 for a program that does not exist.

The band starts at 97, and the job keeps 0 to 96. It starts there because the
codes above it hold the two conventions that a shell already uses, 126 and 127,
with room for the codes of qex between them.

Why 128 and above is qex, and not the job
-----------------------------------------

A program that a signal stops conventionally gives `128 + N`, so an
out-of-memory kill gives 137. That form cannot serve here:

    qex wait <id>
    ^C
    echo $?      <- 130 from the shell, and THE JOB IS STILL RUNNING

A dead process writes no exit code. `128 + N` from a qex command can thus only
mean that the qex command itself died, and the job is then not described at all.
qex gives 98 for a job that a signal stopped, and the record names the signal.
A kill, a cancel or a time limit gives 125 in place of 98, and a kill for memory
gives 99: qex knows the cause of those, and each of them takes a different
correction from the reader.

qex catches Ctrl-C and SIGTERM during a wait, so the usual case gives 122 with a
sentence, and not 130 in silence. A SECOND Ctrl-C stops the command immediately,
in the usual way. A SIGKILL cannot be caught, so a wait that the out-of-memory
killer takes still gives 137 — which, by this table, says exactly what happened:
your command died, and your job did not.

`qex run` and `qex submit --wait`
---------------------------------

Both give the exit code of the job when the job RAN. `qex run -- sh -c 'exit 7'`
gives 7.

`qex run` writes the output of the job to your terminal. `qex submit --wait`
writes it to the log file, so the context of an agent stays small; read the part
that you want with `qex logs --grep`.

A job of either command is a job like any other, so `qex kill` and `qex cancel`
from a DIFFERENT command can stop it. That job gave no exit code of its own, and
the command then gives 125 and not 1: 125 says that something stopped your work
before it could finish. Both commands also write a line to stderr that names the
cause, and that line says when this command did not stop the job.

A dedupe key can give `qex run` the job of a different caller. A signal then
stops your wait and not the job, and the code is 122.

`qex wait --next`
----------------

`--next` gives control back ONE time. The jobs that did not stop then have no
watcher, so wait again for them. qex names them on STDERR when it returns, in a
line that a person reads. That line is not a data format: your program gave
those ids, so it holds them already, and `--json` writes the record of the job
that stopped on stdout.
";
