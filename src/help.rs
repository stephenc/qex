//! This module holds the text for the `qex help <topic>` command.
//!
//! An agent reads this text to learn the tool. Each topic is thus short and
//! complete, and it contains commands that the agent can copy.

/// The banner that `qex` writes before the usage text when it has no arguments.
///
/// The banner points to the `agents` topic. An agent then reads one page and
/// does not read each command help.
pub const BANNER: &str = "\
  ==> AGENTS: run `qex help agents` first. It is one page.
      It shows how to start a job, wait for the job, and read the output.
      Do not write a monitor script. The command `qex wait` does that work.
";

/// The list of topic names, for the error message and for the `--help` text.
pub const TOPICS: &[&str] = &[
    "agents",
    "job-file",
    "config",
    "resources",
    "states",
    "output",
    "exit-codes",
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

A monitor script that uses `pgrep -f` or `ps` finds its own command line. It
counts itself as the task. The task stops, one process stays, and the script
waits for ever. This is a frequent fault.

qex does not have this fault. qex is the parent process of your task. It uses
`waitpid` on the process. It never searches a command line.

Use `qex wait`. It blocks until the job stops. Do not poll.

The same fault applies to every search of the process list. A command such as
`pgrep -f qex` also matches the shell command that contains those letters. To
find the coordinator, use `qex info`. It gives the process id from the
coordinator itself.

The three commands you need
---------------------------

    ID=$(qex submit --cpu 2 --mem 4GB -- uv run train.py)
    qex wait $ID
    qex logs $ID

`qex submit` writes the job UUID to stdout and writes nothing else. You can
thus put the UUID in a shell variable.

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
time and gives a poor measurement. Give `--cpu guess --mem guess` and start the
real task.

Do not read the use of every job. For one task, or for a task that you run one
time, `guess` is sufficient and you need no other step.

Read the use only when you run the same kind of task many times, and the queue
is slow. qex measures each job, so the numbers are already there:

    qex status $ID --json

The `usage` field gives `max_rss` and `cpu_secs`. If a task always uses much
less than `guess` gives it, put an exact claim in a job file. More jobs then
operate together.

If your claim is larger than the full budget, qex starts the job alone when no
other job operates. The job can then swap or stop with an out-of-memory error.
The status field `forced` is `true` for such a job. That result is data: your
claim or the machine is too small.

In short: give `guess`, start the task, and read the result. Add an exact claim
later, and only if you repeat the task.

A pipeline of stages
--------------------

Do not put the stages of a pipeline in one script. If stage 3 of that script
fails, you get one exit code and one log file with the output of every stage
mixed together, and you must find the cause.

Give each stage its own job, and name the jobs that must succeed first:

    BUILD=$(qex submit --name build -- make)
    TEST=$(qex submit --name test  --needs build  -- make test)
    SHIP=$(qex submit --name ship  --needs test   -- ./deploy.sh)
    qex wait $SHIP

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

Other commands
--------------

    qex list --json            all the jobs and their states
    qex status <id> --json     one job in detail
    qex logs <id> --follow     the output while the job operates
    qex kill <id>              stop a job that operates
    qex cancel <id>            remove a job from the queue
    qex clean --state done     delete the records of the jobs that stopped
    qex info                   the coordinator and the free capacity

Every command that reads data accepts `--json`. Use `qex schema status` and
`qex schema job` to get the JSON Schema for these two formats.

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
";

pub const JOB_FILE: &str = "\
qex job files
=============

A job file describes one job. Use a job file for a long command, for many
environment variables, or to keep the job in your repository.

    qex submit --job train.toml

qex reads TOML, YAML and JSON. The file extension selects the format. TOML is
the format in this documentation.

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

    qex logs <id>              both streams
    qex logs <id> --stdout     one stream
    qex logs <id> --follow     the output while the job operates
    qex logs <id> --tail 100   the last 100 lines

Delete the records
------------------

    qex clean <id>                 one job
    qex clean completed            each job that succeeded
    qex clean done                 each job that stopped
    qex clean --state failed       each job in one state
    qex clean --older-than 7d      each job older than 7 days
    qex clean --all                every job

`qex clean` deletes the directory of the job. It does not stop a job that
operates.
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
