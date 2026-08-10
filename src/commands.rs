//! This module holds the commands that talk to the coordinator.

use crate::cli::{self, StateFilter};
use crate::client::Client;
use crate::config::{Config, EnvCapture};
use crate::job::{safe_name, JobState, JobStatus};
use crate::paths;
use crate::proto::{ErrorKind, Request, Response};
use crate::spec::{JobSpec, SubmitOptions};
use crate::units::{count_of, format_duration, format_size, parse_duration};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

/// The lowest exit code that qex keeps for itself.
///
/// A job can exit with any code from 0 to 255, so every code that qex gives
/// itself is a code that a job can give as well. A reserved BAND with a
/// sentinel removes that collision, and one free number cannot:
///
/// - a code from 0 to 96 comes from the job, and nothing else gives one;
/// - a code from 97 to 255 comes from qex, and it describes the queue, the
///   wait, or qex itself;
/// - the sentinel 97 says that the job gave a code in the reserved band. The
///   record then holds the true code of the job.
///
/// See `EXIT_CODES` in `help.rs` for the table that a reader sees.
pub const EXIT_BAND_FLOOR: i32 = 97;
/// The exit code when the job gave a code that qex keeps for itself.
///
/// Read `qex status` for the code of the job. Without this sentinel, a job that
/// exits 124 of its own accord looks the same as a wait that reached its time
/// limit.
pub const EXIT_JOB_RESERVED: i32 = 97;
/// The exit code when a signal stopped the job, and qex did not send it.
///
/// The usual form is `128 + N`, and it collides with the band: a job that the
/// out-of-memory killer stops gives 137, and a WAIT that the out-of-memory
/// killer stops gives 137 also. The two need different actions, so qex gives
/// the job its own code and puts the name of the signal in the record.
pub const EXIT_JOB_SIGNAL: i32 = 98;
/// The exit code when the kernel stopped the job because the machine ran out of
/// memory.
///
/// This code is not 125. A script that reads 125 knows that something stopped
/// the job, and every one of the four causes takes a different action. This one
/// takes MORE MEMORY: the same job, with a larger `--mem`, on the same machine.
/// A time limit takes more time, and a cancel takes no new run at all.
pub const EXIT_OOM: i32 = 99;
/// The exit code when the job has not stopped, so there is no result.
///
/// `qex status --quiet` with no wait gives it. This code is not 122: 122 says
/// that a WAIT stopped, and this reader never waited. The two need different
/// words, because a code with two meanings is the fault that the band removes.
pub const EXIT_NO_RESULT: i32 = 100;
/// The exit code when qex could not do what the command line asked.
///
/// A command that gives the exit code of a job must not report a fault of its
/// own with a code from the job band. The code 1 is the most common code of a
/// program that failed, so `qex run` that could not start would otherwise look
/// exactly like a job that failed.
pub const EXIT_QEX_FAILED: i32 = 121;
/// The exit code when the wait stopped and the job did not.
///
/// The job continues, and the caller must attach to it again. This code is not
/// 124: 124 says that the READER set a time limit and the limit passed, and
/// this code says that the reader lost the wait with no decision of its own.
pub const EXIT_WAIT_BROKEN: i32 = 122;
/// The exit code of `qex wait` when the job never started.
///
/// The job waited more time than its `--max-queue-time` value. This code is not
/// 125: a job that ran too long wrote output and used the machine, and this job
/// did neither. A script must be able to separate "the work is too slow" from
/// "the machine had no capacity", because the two need different corrections.
pub const EXIT_EXPIRED: i32 = 123;
/// The exit code of `qex wait` when the wait reached its time limit.
/// The command `timeout` uses the same code.
pub const EXIT_TIMEOUT: i32 = 124;
/// The exit code of `qex wait` when something stopped the job.
pub const EXIT_KILLED: i32 = 125;
/// The exit code of `qex wait` when the job did not run.
///
/// A job that this job needed did not succeed. The fault is in that job, and
/// not in this one, so this code is separate.
pub const EXIT_SKIPPED: i32 = 126;
/// The exit code when there is no job with the given id.
pub const EXIT_NO_SUCH_JOB: i32 = 127;

/// The age of a record that `qex clean --auto` deletes.
///
/// A job that stopped in the last hour is frequently the job that a user reads
/// now. A job that stopped before that is history.
const AUTO_CLEAN_AGE: u64 = 3600;

pub fn submit(args: cli::SubmitArgs) -> Result<i32> {
    // `--quiet` describes the end of a wait, so it needs a wait. Say so here,
    // and not through a required-argument error of the command line reader:
    // `qex run -q` gave "the argument --wait was not provided" for a command
    // that refuses `--wait`.
    if args.quiet && !args.wait && !args.follow {
        bail!(
            "`--quiet` needs `--wait`. It says that qex writes no record when the job \
             stops.\n\n\
             \x20   qex submit --wait --quiet -- COMMAND"
        );
    }

    // `--follow` is `qex run` in a longer form, so it IS `qex run`. One
    // behaviour has one implementation.
    if args.follow {
        return run(cli::RunArgs { submit: args });
    }

    if args.each_line.is_some() {
        // `--wait` gives the exit code of ONE job, and `--each-line` makes
        // many. A command cannot give one code for many jobs, so refuse the
        // pair and give the two commands that do it.
        if args.wait {
            bail!(
                "`qex submit --each-line` does not accept `--wait`, because it makes many jobs \
                 and one exit code cannot describe them all.\n\n\
                 Submit them, then wait for the group:\n\
                 \x20   GROUP=$(qex submit --each-line inputs.txt -- ./process {{}})\n\
                 \x20   qex wait $GROUP"
            );
        }
        return submit_each_line(args);
    }

    let cfg = Config::load()?;
    cfg.validate()?;

    let env_capture = if args.no_env_capture {
        Some(EnvCapture::None)
    } else {
        args.env_capture
    };

    let opts = SubmitOptions {
        name: args.name,
        cwd: args.cwd,
        cpu: args.cpu,
        mem: args.mem,
        timeout: args.timeout,
        max_queue_time: args.max_queue_time,
        tags: args.tags,
        priority: args.priority,
        env: args.env,
        env_capture,
        command: args.command,
        job_file: args.job_file,
        needs: args.needs,
        after: args.after,
        locks: args.locks,
        retries: args.retries,
        nice: args.nice,
        no_limit_env_hints: args.no_limit_env_hints,
        dedupe_key: args.dedupe_key,
        dedupe_window: args.dedupe_window,
        // An ordinary job measures against its own command.
        learn_key: None,
        gpu: args.gpu,
        vram: args.vram,
        claims: args.claims,
    };

    let (mut spec, deps) = JobSpec::resolve_with_deps(&opts, &cfg)?;

    let mut client = Client::connect()?;
    warn_if_version_differs(&mut client);

    // Change each dependency name into an id.
    //
    // A dependency must exist now. This rule makes a circle of dependencies
    // impossible: a job can name the jobs before it only. It also gives an
    // error at the submission, and not later, when the job would wait with no
    // end for a job that does not exist.
    spec.needs = resolve_dependencies(&mut client, &deps.needs, "--needs")?;
    spec.after = resolve_dependencies(&mut client, &deps.after, "--after")?;
    require_capabilities(&mut client, &spec)?;

    match client.call(&Request::Submit {
        spec: Box::new(spec),
    })? {
        Response::Submitted {
            id,
            warning,
            deduplicated,
        } => {
            // The warning goes to stderr. The id stays alone on stdout, so the
            // command `ID=$(qex submit ...)` continues to operate.
            //
            // A submission that a dedupe key answered writes its message here
            // also. The exit code stays 0 and the id stays alone on stdout,
            // because a script that captures the id must operate in the same
            // way in both cases. The difference belongs on the other stream.
            if let Some(text) = warning {
                eprintln!("qex: {text}");
            }
            // With `--wait`, stdout carries the RESULT of the job, so the id
            // goes to stderr. Two answers on one stream would give
            // `ID=$(qex submit --wait ...)` the whole record. Use `--id-file`
            // to keep the id, which also survives an interruption.
            //
            // The file reaches the disk BEFORE the wait, and it is closed, so
            // it is a handle after a crash and not after a tidy stop only.
            if args.wait {
                say_the_id_and_write_the_file(id, args.id_file.as_ref());
                return wait_after_submit(
                    &id.to_string(),
                    args.wait_timeout.as_deref(),
                    args.json,
                    args.quiet,
                );
            }
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "id": id.to_string(),
                        // False says: this command started the work.
                        "deduplicated": deduplicated,
                    }))?
                );
            } else {
                println!("{id}");
            }
            // The id is on stdout already, so a file that fails takes nothing
            // from the caller. Say the fault, and give the code of the fault:
            // the caller asked for that file, and a later command reads it.
            if let Some(path) = &args.id_file {
                write_id_file(path, &format!("{id}\n"))?;
            }
            Ok(0)
        }
        other => report_for_a_job(other),
    }
}

/// Says the id, and then writes the id file.
///
/// THE ID GOES FIRST, AND A FILE THAT FAILS DOES NOT TAKE IT.
///
/// The job exists at this moment: the coordinator answered, and it waits or it
/// operates. A write that fails cannot undo that, so a command that stops here
/// leaves a job that runs with nobody who knows its id — which is the fault
/// that `--id-file` exists to remove. A full disk and a read-only directory are
/// exactly the conditions in which a caller needs the handle most.
///
/// The failure is still loud, because the caller asked for that file and a
/// later command will read it.
fn say_the_id_and_write_the_file(id: uuid::Uuid, path: Option<&std::path::PathBuf>) {
    eprintln!("qex: job {id}");
    if let Some(path) = path {
        if let Err(e) = write_id_file(path, &format!("{id}\n")) {
            eprintln!(
                "qex: the id file did not reach the disk: {e:#}\n\
                 qex: the job {id} still operates. Keep the id from the line above, or read \
                 `qex list --cwd .`"
            );
        }
    }
}

/// Writes the record of one job, in the form of `qex status`.
///
/// Every command that waits for ONE job ends with this, so a caller needs no
/// second command to learn the state, the exit code and the last lines of the
/// error output. One renderer also means that the four commands cannot show
/// one job four ways.
fn report_the_record(id: &str, json: bool) {
    let args = cli::StatusArgs {
        id: id.to_string(),
        json,
        show_env: false,
        no_logs: false,
        wait: false,
        follow: false,
        quiet: false,
        timeout: None,
        select: crate::logsel::LogSelect::default(),
    };
    // The exit code comes from the wait, and not from this report. A record
    // that qex cannot read does not change the result of the job, so the
    // message goes to stderr and the caller keeps its code.
    if let Err(e) = status(args) {
        eprintln!("qex: qex could not read the record of the job {id}: {e:#}");
    }
}

/// Waits for the job that `qex submit --wait` started.
///
/// # Why this option exists
///
/// A submission and a wait in two commands make the wait a thing to remember,
/// and an agent that forgets it never learns that the job stopped: the job
/// succeeds and nobody reads the result (issue #47).
///
/// `qex run` also waits, and it writes the output of the job to the terminal,
/// so a long job fills the context of an agent. `qex submit --wait` gives both
/// properties: the harness of the agent waits, and the output stays in the log
/// file.
///
/// One command also closes a race. Between a submission and a wait a short job
/// can stop, and a `qex clean` in that window deletes the record.
fn wait_after_submit(raw_id: &str, timeout: Option<&str>, json: bool, quiet: bool) -> Result<i32> {
    let deadline = match timeout {
        Some(t) => parse_duration(t)
            .map_err(|e| anyhow::anyhow!("--wait-timeout: {e}"))?
            .map(|d| Instant::now() + d),
        None => None,
    };

    catch_signals_while_waiting();
    // `--quiet` silences the REPORT and the narration of the queue. It does not
    // silence a fault of the wait: those lines name the fault and give the id
    // that attaches to the job again, and a caller that loses them holds
    // nothing. See `qex help exit-codes`.
    let mut reporter = ReasonReporter::new(json || quiet);
    let ids = [raw_id.to_string()];

    match wait_one(raw_id, deadline, &mut reporter)? {
        WaitOutcome::Finished(status) => {
            let code = exit_code_for(&status);
            if !quiet {
                report_the_record(raw_id, json);
            }
            Ok(code)
        }
        // A FAULT of the wait always reaches stderr. stdout keeps its JSON,
        // and `--quiet` keeps the record away; neither hides the line that
        // carries the id.
        WaitOutcome::TimedOut => {
            eprintln!("qex: the wait reached its time limit. The job continues.");
            eprintln!("qex: attach to it again:  qex status {raw_id} --wait");
            Ok(EXIT_TIMEOUT)
        }
        WaitOutcome::NoSuchJob => {
            eprintln!("qex: there is no job with the id {raw_id}");
            Ok(EXIT_NO_SUCH_JOB)
        }
        WaitOutcome::Interrupted => Ok(report_broken_wait(&ids, false)),
    }
}

/// Submits one job for each line of a file, and gives the jobs one group id.
///
/// # Why qex reads the whole input before it submits anything
///
/// `qex pipeline` does the same, and for the same reason. A fan-out that stops
/// in the middle leaves a part of the work in the queue and a part of the work
/// with no job. The user then holds a group that is not the file, and the only
/// correction is to find and cancel each job by hand. qex therefore reads the
/// input, tests the command, tests the count and makes every specification
/// first.
///
/// A fault can still arrive after the first submission. The coordinator can
/// refuse a job, and the connection to the coordinator can stop. A fan-out
/// holds up to 1000 jobs, so that window is much wider than the window of a
/// pipeline. EVERY message for such a fault names the group and the command
/// that finds the jobs: a user who never learns the group id cannot reach the
/// jobs that qex already made.
///
/// # Why one claim covers every job
///
/// qex resolves the claim one time, from the command of the FIRST line, and
/// gives it to every job. The lines of a fan-out are the same kind of work, so
/// one claim is correct for them. A claim for each line separately would come
/// from a different measurement for each line, and the order of the queue would
/// then change with the history and not with the file.
///
/// The measurements come from the TEMPLATE, and not from the command of the
/// line. `JobSpec::learn_key` holds the reason. The claim of the first line is
/// thus the claim of the whole fan-out of the last run.
///
/// `qex status` says `fan-out` for such a claim, and not `learned`. The word
/// `learned` means "from the earlier jobs of this command", and the command of
/// one line can have no measurement at all.
fn submit_each_line(args: cli::SubmitArgs) -> Result<i32> {
    let cfg = Config::load()?;
    cfg.validate()?;

    let path = args
        .each_line
        .clone()
        .expect("the caller tested this option");

    // A key holds ONE job. Every job of a fan-out would carry the same key, so
    // the coordinator would answer the second line and every line after it with
    // the id of the first job, and start nothing. A fan-out of 1000 lines would
    // then give 1 job and no error at all, which is the quiet wrong answer that
    // this project exists to prevent.
    if args.dedupe_key.is_some() || args.dedupe_window.is_some() {
        bail!(
            "`qex submit --each-line` does not accept `--dedupe-key` or `--dedupe-window`.\n\n\
             A key holds one job. Every job of a fan-out would carry the same key, so qex \
             would start the first line only and give you the id of that job for every \
             other line.\n\n\
             To run a fan-out one time only, put the key on a job that guards it:\n\n\
             \x20   qex submit --dedupe-key nightly -- ./fan-out.sh"
        );
    }

    // `qex submit` writes one id to stdout, and `--json` writes that id as an
    // object. A fan-out writes a group id and N job ids, which is a different
    // shape, and `--id-file NAME.json` already gives it.
    if args.json {
        bail!(
            "`qex submit --each-line` does not accept `--json`.\n\n\
             That option writes the id of ONE job, and a fan-out makes a group and one job \
             for each line.\n\n\
             Use an id file with the name `.json`. It holds the group and every job:\n\n\
             \x20   qex submit --each-line inputs.txt --id-file jobs.json -- ./process {{}}"
        );
    }

    // Test the command before qex reads the file. A command with no `{}` is a
    // fault of the command line, and the user must see that fault first.
    let template = args.command.clone();
    crate::fanout::check_command(&template)?;

    let input = crate::fanout::read(&path)?;
    let max = args.max_jobs.unwrap_or(crate::fanout::DEFAULT_MAX_JOBS);
    crate::fanout::check_count(input.lines.len(), max)?;
    let count = input.lines.len();

    // Say what qex passed over. A line that a user expected to run, and that
    // qex passed over in silence, is a quiet wrong answer.
    if input.skipped() > 0 {
        let mut passed: Vec<String> = Vec::new();
        if input.blank > 0 {
            passed.push(count_of(input.blank, "empty line"));
        }
        if input.comments > 0 {
            passed.push(count_of(input.comments, "comment line"));
        }
        eprintln!(
            "qex: this input gives {}. qex passed over {}.",
            count_of(count, "job"),
            passed.join(" and ")
        );
    }

    let env_capture = if args.no_env_capture {
        Some(EnvCapture::None)
    } else {
        args.env_capture
    };

    let opts = SubmitOptions {
        name: args.name.clone(),
        cwd: args.cwd,
        cpu: args.cpu,
        mem: args.mem,
        timeout: args.timeout,
        // Every job of the fan-out gets the same limit. The limit is on the
        // time that ONE job waits, and not on the time of the group, so a
        // fan-out of 1000 jobs behind a small budget expires the jobs that
        // still wait at the end of it.
        max_queue_time: args.max_queue_time,
        tags: args.tags,
        priority: args.priority,
        env: args.env,
        env_capture,
        // The command of the first line. It gives the directory, the
        // environment and the name of the program, and the claim comes from the
        // template below.
        command: crate::fanout::substitute(&template, &input.lines[0]),
        job_file: None,
        needs: args.needs,
        after: args.after,
        locks: args.locks,
        retries: args.retries,
        // Every job of the fan-out gets the same claim on the pools. A pool is
        // a claim in the same way as the cores and the memory are.
        gpu: args.gpu,
        vram: args.vram,
        claims: args.claims,
        nice: args.nice,
        no_limit_env_hints: args.no_limit_env_hints,
        // A fan-out refuses these options above, so no job of it holds a key.
        dedupe_key: None,
        dedupe_window: None,
        // Every job of this fan-out measures against the template, so the whole
        // fan-out makes one record and the next run reads it.
        learn_key: Some(template.clone()),
    };

    let (first, deps) = JobSpec::resolve_with_deps(&opts, &cfg)?;

    let group = uuid::Uuid::new_v4();
    let group_name = args.name.clone().unwrap_or_else(|| {
        if path == std::path::Path::new("-") {
            "stdin".to_string()
        } else {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("each-line")
                .to_string()
        }
    });
    // `first.name` is the `--name` value, or the program name of the command.
    let base = first.name.clone();

    let mut client = Client::connect()?;
    warn_if_version_differs(&mut client);

    // Every job of the fan-out waits for the same jobs, so qex changes each
    // name into an id one time.
    let needs = resolve_dependencies(&mut client, &deps.needs, "--needs")?;
    let after = resolve_dependencies(&mut client, &deps.after, "--after")?;

    let mut specs: Vec<JobSpec> = Vec::with_capacity(count);
    for (position, line) in input.lines.iter().enumerate() {
        let mut spec = first.clone();
        spec.id = uuid::Uuid::new_v4();
        spec.command = crate::fanout::substitute(&template, line);
        spec.name = crate::fanout::job_name(&base, position + 1, count, line);
        spec.needs = needs.clone();
        spec.after = after.clone();
        spec.group = Some(group);
        spec.group_name = Some(group_name.clone());
        specs.push(spec);
    }

    // Test the coordinator one time, before the first job.
    require_capabilities(&mut client, &specs[0])?;

    let mut submitted: Vec<(String, uuid::Uuid)> = Vec::with_capacity(count);
    // Every job of the fan-out has the same claim, so the coordinator gives the
    // same warning for each one. A fan-out of 1000 jobs would write that
    // warning 1000 times and hide every other line. Say it one time, and count
    // the rest.
    //
    // The comparison uses the FIRST line of the warning. The other lines hold
    // the id of the job, which is different for each job and is not the reason
    // for the warning.
    let mut warned: Option<String> = None;
    let mut warned_again = 0usize;
    for spec in specs {
        let name = spec.name.clone();
        // Take the answer in two steps. A transport fault gives an `Err` here,
        // and the `?` of the first form would carry that fault out of this
        // function with the group id still in this frame only. The jobs that
        // qex already submitted are on the disk, and a user who never saw the
        // group id has no way to reach them.
        let answer = match client.call(&Request::Submit {
            spec: Box::new(spec),
        }) {
            Ok(answer) => answer,
            Err(e) => {
                partial_fan_out(
                    &format!("qex lost the coordinator at the job `{name}`"),
                    group,
                    &submitted,
                );
                return Err(e);
            }
        };

        match answer {
            // `deduplicated` is always false here. A fan-out refuses
            // `--dedupe-key`, so no job of it holds a key and the coordinator
            // has nothing to match a new job against.
            Response::Submitted {
                id,
                warning,
                deduplicated: _,
            } => {
                if let Some(text) = warning {
                    let head = text.lines().next().unwrap_or_default().to_string();
                    if warned.as_deref() == Some(head.as_str()) {
                        warned_again += 1;
                    } else {
                        eprintln!("qex: {name}: {text}");
                        warned = Some(head);
                    }
                }
                submitted.push((name, id));
            }
            other => {
                partial_fan_out(
                    &format!("the coordinator refused the job `{name}`"),
                    group,
                    &submitted,
                );
                return report_for_a_job(other);
            }
        }
    }

    if warned_again > 0 {
        eprintln!(
            "qex: the same warning applies to {} of this fan-out. \
             Run `qex list --group {group}` to see them all.",
            count_of(warned_again, "more job")
        );
    }

    if let Some(path) = &args.id_file {
        let text = pipeline_id_file(path, group, &group_name, &submitted)?;
        write_id_file(path, &text)?;
    }

    // The detail goes to stderr and the group id goes to stdout alone, so
    // `GROUP=$(qex submit --each-line ...)` operates.
    // The SAFE forms: these lines go to a terminal. `job_name` already cleans
    // the name of a job, and the group name comes from `--name` or from the
    // path of the input file, which a user can write with any byte at all.
    // See `job::safe_name`. The id file above keeps the name as it is, because
    // a machine reads that name as a KEY.
    for (name, id) in &submitted {
        eprintln!("{}: {id}", crate::job::safe_name(name));
    }
    eprintln!(
        "qex: {} in the group `{}`",
        count_of(count, "job"),
        crate::job::safe_name(&group_name)
    );
    println!("{group}");
    Ok(0)
}

/// Reports a fan-out that stopped in the middle.
///
/// This function exists because of the ONE thing that a user must not lose: the
/// group id. The jobs that qex already submitted are on the disk and they
/// operate, and the group id is the only short handle to all of them. A message
/// that gives the fault and no group id leaves the user with jobs that they
/// cannot find.
///
/// The ids come as well. `qex list --group` needs a coordinator, and this
/// function frequently reports that qex just lost the coordinator.
fn partial_fan_out(what: &str, group: uuid::Uuid, submitted: &[(String, uuid::Uuid)]) {
    eprintln!(
        "qex: {what}. {} of this fan-out {} in the queue.",
        count_of(submitted.len(), "job"),
        if submitted.len() == 1 { "is" } else { "are" }
    );
    if submitted.is_empty() {
        return;
    }
    eprintln!("qex: the group of those jobs is {group}. They are:");
    for (name, id) in submitted {
        eprintln!("{}: {id}", crate::job::safe_name(name));
    }
    eprintln!(
        "qex: Run `qex list --group {group}` to see them, and `qex cancel <id>` or \
         `qex kill <id>` to stop them."
    );
}

pub fn list(args: cli::ListArgs) -> Result<i32> {
    let filter = match args.state.as_deref() {
        Some(s) => Some(StateFilter::parse(s).map_err(|e| anyhow::anyhow!("--state: {e}"))?),
        None => None,
    };

    let mut client = Client::connect()?;
    // Report the answer that arrived. A second request would hide the first
    // fault and could give a different answer.
    let response = client.call(&Request::List)?;
    let Response::Jobs { mut jobs } = response else {
        return report(response);
    };

    if let Some(f) = &filter {
        jobs.retain(|j| f.matches(j.state));
    }
    if let Some(tag) = &args.tag {
        jobs.retain(|j| j.tags.iter().any(|t| t == tag));
    }
    let list_cwd = match &args.cwd {
        Some(p) => Some(resolve_directory(p, "--cwd")?),
        None => None,
    };
    let list_under = match &args.under {
        Some(p) => Some(resolve_directory(p, "--under")?),
        None => None,
    };
    if list_cwd.is_some() || list_under.is_some() {
        jobs.retain(|j| matches_directory(&j.cwd, list_cwd.as_deref(), list_under.as_deref()));
    }
    if let Some(group) = &args.group {
        // A group takes its id or its name. A name is easier to type, and the
        // names of a pipeline belong to one submission.
        jobs.retain(|j| names_group(j, group));

        // Say when the word gives more than one run.
        //
        // A pipeline takes its name from its file, so a second run of that
        // file has the same name. This command reads and deletes nothing, so
        // it shows every run; but a reader who does not know that the table
        // holds two runs reads one pipeline that ran each stage two times.
        //
        // The commands that stop or delete work REFUSE such a word. This one
        // is where a user looks after that refusal, so it must give the group
        // id of each run.
        let mut runs: Vec<uuid::Uuid> = jobs.iter().filter_map(|j| j.group).collect();
        runs.sort();
        runs.dedup();
        if runs.len() > 1 && !args.json {
            eprintln!(
                "qex: `{group}` names {} pipelines, and this table holds all of them. \
                 A pipeline takes its name from its file, so a second run of that file \
                 has the same name. Use a group id for one run: {}",
                runs.len(),
                runs.iter()
                    .map(|g| g.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    // Show the jobs in the order of submission. A pipeline then reads from the
    // first stage to the last stage.
    jobs.sort_by_key(|j| (j.submitted_at, j.sequence));

    // From here the records are for a READER. See `for_display`.
    let jobs = all_for_display(jobs);

    // Say the pause before the jobs, and say it every time.
    //
    // A queue that does nothing and does not say why is the fault that this
    // tool exists to remove. The text goes to stderr, so `qex list --json`
    // still writes JSON alone on stdout.
    warn_if_paused(&mut client);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(0);
    }

    if jobs.is_empty() {
        println!("no jobs");
        return Ok(0);
    }

    println!(
        "{:<8}  {:<10}  {:<16}  {:>5}  {:>8}  {:>8}  NOTE",
        "ID", "STATE", "NAME", "CPU", "MEM", "TIME"
    );
    for j in &jobs {
        let elapsed = j
            .elapsed()
            .map(format_duration)
            .unwrap_or_else(|| "-".to_string());
        let mut note = String::new();
        if j.forced {
            note.push_str("FORCED ");
        }
        if let Some(r) = &j.blocked_reason {
            note.push_str(r);
        } else if j.state.is_terminal() {
            note.push_str(&describe_result(j));
        }

        println!(
            "{:<8}  {:<10}  {:<16.16}  {:>5}  {:>8}  {:>8}  {}",
            short_id(&j.id),
            j.state.as_str(),
            j.name,
            j.cpu,
            format_size(j.mem),
            elapsed,
            note
        );
    }
    Ok(0)
}

pub fn status(args: cli::StatusArgs) -> Result<i32> {
    let mut client = Client::connect()?;
    // Use one exit code for every "no such job" result. Without this test, a
    // name that is not a UUID gives the code 1 and a UUID gives the code 127,
    // for the same fault. A script cannot use two codes for one condition.
    // A pipeline gives every one of its stages, in the order of submission.
    let found = match resolve_targets(&mut client, &args.id) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("qex: {e}");
            return Ok(EXIT_NO_SUCH_JOB);
        }
    };
    let is_pipeline = found.group.is_some();
    let ids = found.ids;

    if args.timeout.is_some() && !args.wait && !args.follow {
        bail!(
            "`--timeout` limits a WAIT, and this command does not wait. Add `--wait` or \
             `--follow`.\n\n\
             \x20   qex status <id> --wait --timeout 30m\n\n\
             Use `qex submit --timeout` to limit the JOB."
        );
    }

    // A limit for the wait, whatever form the wait takes.
    let deadline = match &args.timeout {
        Some(t) => parse_duration(t)
            .map_err(|e| anyhow::anyhow!("--timeout: {e}"))?
            .map(|d| Instant::now() + d),
        None => None,
    };

    // `--follow` is the way back to a job of `qex run`, from any session.
    //
    // It writes the output of the job and no text of its own on stdout, and it
    // gives the exit code of the job. This command did not start the job, so a
    // signal stops this wait and never the job.
    if args.follow {
        // This command writes the output of the job on stdout. A JSON object
        // there would mix with that output, and neither part could be read.
        // `qex run` refuses `--json` for the same reason.
        if args.json {
            bail!(
                "`qex status --follow` does not accept --json, because it writes the output \
                 of the job to stdout.\n\n\
                 Use `qex status <id> --wait --json` for the record, or `qex logs <id> --json` \
                 for the output."
            );
        }
        if ids.len() != 1 {
            bail!(
                "`--follow` takes one job, and `{}` gives {}. Name the stage that you want.",
                args.id,
                ids.len()
            );
        }
        let id = ids[0];
        catch_run_signals();
        let dir = paths::job_dir(&id)?;
        return stream_until_done(&mut client, id, &dir, false, deadline);
    }

    // Wait for the job first, if the user asked for that.
    //
    // An agent runs this command in the background of its harness. The harness
    // then reports the end of the command, and this output holds the state, the
    // exit code and the cause of a failure. One command gives everything.
    let mut wait_code = 0;
    if args.wait {
        // A signal must not look like a result of the job. See
        // `catch_signals_while_waiting`.
        catch_signals_while_waiting();

        // The deadline is one moment, and not a limit for each job, so a
        // pipeline of ten stages does not get ten times the time of the user.
        let mut waited = 0usize;
        for id in &ids {
            // ONE REPORTER FOR EACH JOB. A reporter stops when its job starts,
            // so a reporter that many jobs share goes silent for all of them
            // at the first job that is not queued. In a pipeline the LAST
            // stage is the one that waits, and it is the one that must speak.
            let mut reporter = ReasonReporter::new(args.json || args.quiet);
            match wait_one(&id.to_string(), deadline, &mut reporter)? {
                WaitOutcome::Finished(s) => {
                    waited += 1;
                    // Report the FIRST fault. A later stage that qex skipped
                    // would otherwise hide the stage that failed.
                    let code = exit_code_for(&s);
                    if code != 0 && wait_code == 0 {
                        wait_code = code;
                    }
                }
                // A FAULT of the wait always reaches stderr. See
                // `wait_after_submit`.
                WaitOutcome::TimedOut => {
                    eprintln!("qex: the wait for {id} reached its time limit. The job continues.");
                    wait_code = EXIT_TIMEOUT;
                    break;
                }
                WaitOutcome::NoSuchJob => {
                    eprintln!("qex: there is no job with the id {id}");
                    return Ok(EXIT_NO_SUCH_JOB);
                }
                // Give the code of a wait that stopped, and NOT `128 + N`. The
                // job continues, so the caller must attach to it again.
                WaitOutcome::Interrupted => {
                    let names: Vec<String> =
                        ids.iter().skip(waited).map(|i| i.to_string()).collect();
                    return Ok(report_broken_wait(&names, false));
                }
            }
        }
    }

    // `--quiet` gives the exit code and nothing else. A script that tests the
    // result needs no text, and an agent that reads the text pays for it.
    if args.quiet {
        if args.wait {
            return Ok(wait_code);
        }
        // With no wait, the code comes from the records as they stand now.
        //
        // Read EVERY job. A pipeline handle gives every stage, and a code that
        // describes the first stage alone says that a build succeeded when a
        // later stage failed.
        let mut worst = 0;
        for id in &ids {
            let status = match client.call(&Request::Status { id: *id })? {
                Response::Status { status } => status,
                other => return report_for_a_job(other),
            };
            let code = if status.state.is_terminal() {
                exit_code_for(&status)
            } else {
                // The job did not stop, so it has no result. This reader set no
                // wait, so the code is not 122: nothing stopped a wait here.
                EXIT_NO_RESULT
            };
            if code != 0 && worst == 0 {
                worst = code;
            }
        }
        return Ok(worst);
    }

    let mut values: Vec<serde_json::Value> = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        let id = *id;
        let status = match client.call(&Request::Status { id })? {
            Response::Status { status } => status,
            other => return report_for_a_job(other),
        };
        // From here the record is for a READER. See `for_display`.
        let status = Box::new(for_display(*status));

        // Read the output of the job in the same call.
        //
        // A reader of a job that failed always wants the last lines of its
        // standard error. Without this, every failure costs two commands and
        // two answers, and the reader is frequently an agent with a limited
        // context.
        let excerpt = job_excerpt(&status, &args)?;

        if args.json {
            let mut value = serde_json::to_value(&*status)?;
            if args.show_env {
                // The environment can hold secrets, so qex adds it only when
                // the user asks for it.
                if let Ok(spec) = crate::job::read_spec(&paths::job_dir(&id)?) {
                    value["env"] = serde_json::to_value(&spec.env)?;
                }
            }
            if !excerpt.is_empty() {
                // One field for each stream. A reader that wants the result of
                // a test program needs the standard output, and a reader that
                // wants the cause needs the standard error.
                let mut logs = serde_json::Map::new();
                for (name, selected) in &excerpt {
                    let mut one = serde_json::Map::new();
                    one.insert(
                        "text".into(),
                        serde_json::Value::from(selected.text.clone()),
                    );
                    if let Some(found) = selected.matches {
                        one.insert("matches".into(), serde_json::Value::from(found));
                    }
                    if selected.truncated {
                        one.insert(
                            "hidden_lines".into(),
                            serde_json::Value::from(selected.hidden),
                        );
                    }
                    // The lines that the LIMIT removed are different from the
                    // lines that this selection did not show. The file itself
                    // does not hold them, and no option gives them.
                    if let Some((bytes, lines)) =
                        status.logs_dropped.and_then(|d| d.of(name.as_str()))
                    {
                        one.insert("dropped_bytes".into(), serde_json::Value::from(bytes));
                        one.insert("dropped_lines".into(), serde_json::Value::from(lines));
                    }
                    logs.insert(name.clone(), serde_json::Value::Object(one));
                }
                value["logs"] = serde_json::Value::Object(logs);
            }
            values.push(value);
        } else {
            if n > 0 {
                println!();
            }
            print_status(&status, args.show_env)?;
            for (name, selected) in &excerpt {
                println!();
                println!("--- {name} ---");
                if let Some(notice) = dropped_notice(&status, name) {
                    println!("{notice}");
                }
                if let Some(notice) = selected.notice() {
                    println!("{notice}");
                }
                print!("{}", selected.text);
            }
        }
    }

    if args.json {
        // One job gives an object, as before. A pipeline gives an array. A
        // script that reads one job must not become a script that reads an
        // array because qex learned about pipelines.
        //
        // The shape comes from what the user NAMED, and not from the number of
        // jobs. A pipeline of one stage must still give an array, or a script
        // that reads `.[0]` of a group breaks on the day a pipeline has one
        // stage, or on the day `qex clean` removes all the stages but one.
        if is_pipeline {
            println!("{}", serde_json::to_string_pretty(&values)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&values[0])?);
        }
    }

    Ok(wait_code)
}

/// Chooses the parts of the output of a job to show with its status.
///
/// With no option, a job that did not succeed gives the last lines of BOTH
/// streams. A job that succeeded gives nothing, because the reader did not ask.
///
/// Both streams matter. A test program writes its failure summary to the
/// standard error and its result to the standard output. The standard error
/// alone then reads as a complete failure, and the reader needs a second
/// command to learn what really happened.
///
/// With `--stdout` or `--stderr`, the reader chose one stream, so this function
/// gives that stream only.
fn job_excerpt(
    status: &JobStatus,
    args: &cli::StatusArgs,
) -> Result<Vec<(String, crate::logsel::Selected)>> {
    if args.no_logs {
        return Ok(Vec::new());
    }

    let one_stream = args.select.stdout || args.select.stderr;
    let explicit = args.select.is_explicit() || one_stream;
    let failed = status.state.is_terminal() && status.state != JobState::Completed;
    if !explicit && !failed {
        return Ok(Vec::new());
    }

    let dir = paths::job_dir(&status.id)?;
    let wanted: Vec<(&str, &str)> = if args.select.stdout {
        vec![("stdout", "stdout.log")]
    } else if args.select.stderr {
        vec![("stderr", "stderr.log")]
    } else {
        // The standard error comes first, because it usually holds the cause.
        vec![("stderr", "stderr.log"), ("stdout", "stdout.log")]
    };

    // Read each stream with a generous limit first, to learn which streams hold
    // anything. The limit for the output then depends on that count.
    let mut found = Vec::new();
    for (name, file) in &wanted {
        let text = read_log(&dir, file);
        if !text.trim().is_empty() {
            found.push((*name, *file));
        }
    }

    let limit = if explicit {
        crate::logsel::DEFAULT_LINES
    } else if found.len() > 1 {
        // Two streams. Give fewer lines of each, so the total stays small for
        // a reader with a limited context.
        crate::logsel::STATUS_LINES / 2
    } else {
        crate::logsel::STATUS_LINES
    };

    let mut out = Vec::new();
    for (name, file) in found {
        let selected = select_log(&dir, file, &args.select, limit)?;
        if !selected.text.trim().is_empty() {
            out.push((name.to_string(), selected));
        }
    }
    Ok(out)
}

/// Gives one line that says what the limit removed from one stream.
///
/// This text is not the same as the notice of a selection. A selection hides
/// lines that the file still holds, and an option gives them back. These lines
/// are not on the disk, and no option gives them back, so the two must never
/// look the same to a reader.
fn dropped_notice(status: &JobStatus, stream: &str) -> Option<String> {
    let dropped = status.logs_dropped?;
    let limit = if dropped.limit > 0 {
        format!(
            " The limit is `[logs] max_bytes` = {}.",
            format_size(dropped.limit)
        )
    } else {
        String::new()
    };

    let mut text = match dropped.of(stream) {
        Some((bytes, lines)) => format!(
            "... qex removed {} and {lines} line(s) from the middle of this stream.{limit} \
             The first part and the last part are here. To keep more, make max_bytes larger \
             in the configuration file.",
            format_size(bytes)
        ),
        // A stream with no count can still be incomplete, so the test of the
        // count comes second.
        None if dropped.incomplete => String::new(),
        None => return None,
    };

    if dropped.incomplete {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(
            "... The output of this job did not close, so this file can be missing more \
             than the count above. A process of the job kept the output open after the job \
             stopped.",
        );
    }
    Some(text)
}

/// Reads one log file, and accepts a byte that is not UTF-8.
fn read_log(dir: &std::path::Path, file: &str) -> String {
    match std::fs::read(dir.join(file)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

fn print_status(s: &JobStatus, show_env: bool) -> Result<()> {
    println!("id:        {}", s.id);
    println!("name:      {}", s.name);
    println!("state:     {}", s.state);
    // A pid that a reader can act on, and a pid that a reader must not act on,
    // get different words. After the job stops, the machine can give that
    // number to another process, so the line says `was`.
    if let Some(pid) = s.pid {
        println!("pid:       {pid}");
    } else if let Some(pid) = s.last_pid {
        println!("pid:       {pid} (was; the job stopped, and this pid is history)");
    }
    if let Some(code) = s.exit_code {
        println!("exit code: {code}");
    }
    if let Some(sig) = s.signal {
        println!("signal:    {sig}");
    }
    println!(
        "claim:     {} core(s), {}{}",
        s.cpu,
        format_size(s.mem),
        match s.claim_source.as_str() {
            // Say where the claim came from. A reader then knows that qex
            // calculated it from the earlier jobs, and that no agent chose it.
            "learned" => "  (from the earlier jobs of this command)",
            // A job of a fan-out learns against its template, so its claim can
            // come from a different line of an earlier run. Do not say `this
            // command`: the command of this job can have no measurement at all.
            "fan-out" => "  (from the earlier jobs of this fan-out)",
            "default" => "  (the default; give --cpu and --mem to change it)",
            // A claim that qex raised must say so. Without this text, a reader
            // sees a number that no agent gave and no measurement produced.
            "raised" => "  (qex raised it, because the earlier claim was too small)",
            _ => "",
        }
    );

    if s.usage.max_rss > 0 || s.usage.cpu_secs > 0.0 {
        println!(
            "used:      {} of memory, {:.1}s of CPU time",
            format_size(s.usage.max_rss),
            s.usage.cpu_secs
        );
        // Show the difference. An agent reads this line and corrects its next
        // claim without a calculation.
        if s.mem > 0 && s.usage.max_rss > 0 {
            let pct = (s.usage.max_rss as f64 / s.mem as f64) * 100.0;
            println!("           the job used {pct:.0}% of its memory claim");
        }
    }

    if let Some(d) = s.elapsed() {
        println!("time:      {}", format_duration(d));
    }
    if s.forced {
        println!("forced:    yes");
        if let Some(r) = &s.forced_reason {
            println!("           {r}");
        }
    }
    if let Some(r) = &s.blocked_reason {
        println!("waits for: {r}");
    }
    if let Some(e) = &s.error {
        // A job that succeeded can also hold a text here. qex writes the story
        // of an out-of-memory kill in this field, and that story stays in the
        // record after a later attempt succeeded. The word `error` would then
        // contradict the state, so this line uses the word `note`.
        if s.state == JobState::Completed {
            println!("note:      {e}");
        } else {
            println!("error:     {e}");
        }
    }
    // A reader must never take a part of the output for the whole output. This
    // line is in the status itself, because a reader who gives `--no-logs` or
    // `--tail 20` never sees the line that the file holds.
    if let Some(d) = s.logs_dropped {
        let mut parts = Vec::new();
        if d.stdout_bytes > 0 {
            parts.push(format!(
                "{} ({} line(s)) from stdout",
                format_size(d.stdout_bytes),
                d.stdout_lines
            ));
        }
        if d.stderr_bytes > 0 {
            parts.push(format!(
                "{} ({} line(s)) from stderr",
                format_size(d.stderr_bytes),
                d.stderr_lines
            ));
        }
        if !parts.is_empty() {
            println!("output:    qex removed {}", parts.join(", and "));
            println!(
                "           The job wrote more than `[logs] max_bytes`{}. qex kept the \
                 first part and the last part of each file.",
                if d.limit > 0 {
                    format!(" ({})", format_size(d.limit))
                } else {
                    String::new()
                }
            );
        }
        if d.incomplete {
            println!("output:    a log file of this job is not complete.");
            println!(
                "           The output did not close after the job stopped, so qex could \
                 not count what went. Start the job again if you need the full output."
            );
        }
    }
    if s.attempts > 1 || s.retries_left > 0 {
        println!(
            "attempts:  {}{}",
            s.attempts,
            if s.retries_left > 0 {
                format!(" ({} retry left)", s.retries_left)
            } else {
                String::new()
            }
        );
    }
    if !s.locks.is_empty() {
        println!("locks:     {}", s.locks.join(", "));
    }
    // Show the key. A caller that received this id from a second submission can
    // then see which key gave it, and it does not read the job file again.
    if let Some(key) = &s.dedupe_key {
        println!("dedupe:    {key}");
    }
    if !s.claims.is_empty() {
        println!(
            "claims:    {}",
            s.claims
                .iter()
                .map(|(name, c)| match c.size {
                    Some(size) => format!("{name} {} ({} each)", c.count, format_size(size)),
                    None => format!("{name} {}", c.count),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // Show the devices that the job received. The environment of a job goes
    // away with the job. This record stays, and it is what an agent reads
    // AFTERWARDS to explain a failure.
    for (name, given) in &s.assigned {
        if given.devices.is_empty() {
            continue;
        }
        let each = match given.size {
            Some(size) => format!(" ({} each)", format_size(size)),
            None => " (the whole of each device)".to_string(),
        };
        println!(
            "devices:   {name} {}{each}",
            given
                .devices
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    if !s.needs.is_empty() {
        println!(
            "needs:     {}",
            s.needs
                .iter()
                .map(|d| d.to_string()[..8].to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(root) = &s.caused_by {
        println!("caused by: {}", &root.to_string()[..8]);
    }
    if !s.tags.is_empty() {
        println!("tags:      {}", s.tags.join(", "));
    }

    if show_env {
        let spec = crate::job::read_spec(&paths::job_dir(&s.id)?)?;
        println!("environment:");
        for (k, v) in &spec.env {
            println!("  {k}={v}");
        }
    }
    Ok(())
}

/// Waits for one job or many jobs.
///
/// This command replaces a monitor script. It does not poll. The coordinator
/// does not answer until the job stops.
pub fn wait(args: cli::WaitArgs) -> Result<i32> {
    let deadline = match &args.timeout {
        Some(t) => parse_duration(t)
            .map_err(|e| anyhow::anyhow!("--timeout: {e}"))?
            .map(|d| Instant::now() + d),
        None => None,
    };

    // A pipeline gives every one of its stages. `qex wait $GROUP` thus waits
    // for the whole pipeline, and the group id that `qex pipeline` writes is a
    // handle that works.
    //
    // A value that the resolver refuses gives its own message here, with the
    // code for "no such job". An earlier version kept the value and let the
    // wait fail later, and the user then read "there is no job with the id x"
    // for a value that named a job AND a pipeline.
    let ids = match expand_ids(&args.ids) {
        Ok(ids) => ids,
        Err(e) => {
            // A fault, and not a report. `--quiet` keeps it: the reader has no
            // result and no id, so silence would leave it with a number only.
            eprintln!("qex: {e}");
            return Ok(EXIT_NO_SUCH_JOB);
        }
    };

    // Say the pause BEFORE the wait begins.
    //
    // Issue #28: `qex wait` gives no word where `qex run` explains itself. A
    // pause makes that silence unbounded — a wait with no `--timeout`, behind a
    // pause with no end, never returns and never says why. One line on stderr
    // is the whole remedy, and stdout keeps the result alone.
    //
    // `--json` keeps its silence, in the same way as the answers below: a
    // reader that asked for JSON reads the exit code.
    if !args.json && !args.quiet {
        if let Ok(mut client) = Client::connect() {
            warn_if_paused(&mut client);
        }
    }

    // A signal must not look like a result of the job. See
    // `catch_signals_while_waiting`.
    catch_signals_while_waiting();

    // With `--next`, give control back when the NEXT job stops. An agent that
    // started several jobs can then read a result as soon as it arrives, in
    // place of the order of submission.
    if args.next {
        return wait_for_the_next(&args, &ids, deadline);
    }

    let mut results: Vec<JobStatus> = Vec::new();
    let mut worst = 0i32;

    for raw_id in &ids {
        // One reporter for each job. See `status`.
        let mut reporter = ReasonReporter::new(args.json || args.quiet);
        let status = match wait_one(raw_id, deadline, &mut reporter)? {
            WaitOutcome::Finished(s) => s,
            WaitOutcome::TimedOut => {
                eprintln!("qex: the wait for {raw_id} reached its time limit. The job continues.");
                return Ok(EXIT_TIMEOUT);
            }
            WaitOutcome::NoSuchJob => {
                eprintln!(
                    "qex: there is no job with the id {raw_id}. A `qex clean` deletes the \
                     record of a job that stopped, so a job that succeeded a moment ago can \
                     give this answer."
                );
                return Ok(EXIT_NO_SUCH_JOB);
            }
            // A signal stopped this wait. Name every job that this command
            // still watched, so the user attaches to all of them again.
            WaitOutcome::Interrupted => {
                let rest: Vec<String> = ids
                    .iter()
                    .skip(results.len())
                    .map(|i| i.to_string())
                    .collect();
                return Ok(report_broken_wait(&rest, false));
            }
        };

        let code = exit_code_for(&status);
        if code != 0 && worst == 0 {
            worst = code;
        }
        // From here the records are for a READER. See `for_display`.
        results.push(for_display(*status));
    }

    if args.quiet {
        // The exit code and nothing else.
    } else if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        // ONE LINE for each job. This command takes many jobs, so it gives a
        // short report of each. Use `qex status $ID --wait` for the record of
        // one job: the state, the exit code, the cause and the last lines of
        // the error output.
        for s in &results {
            // Use punctuation. Without it, the line reads as one sentence:
            // "a1b2c3d4 completed the job succeeded".
            println!("{}: {} — {}", short_id(&s.id), s.state, describe_result(s));
        }
    }

    Ok(worst)
}

/// Waits until the first job of a set stops.
///
/// This function tests each job in turn with a short limit, so no job holds the
/// wait while a different job is ready. It is the one place where qex polls,
/// and it polls its own records, which always answer.
///
/// This wait gives control back ONE time. The jobs that did not stop then have
/// no watcher, so the function names them and gives the command that watches
/// them again (issue #47).
fn wait_for_the_next(
    args: &cli::WaitArgs,
    ids: &[String],
    deadline: Option<Instant>,
) -> Result<i32> {
    let mut delay = Duration::from_millis(50);
    // One reporter for each job, in the same way as every other wait. This
    // loop reads the record of each job already, so the reason costs nothing.
    let mut reporters: Vec<ReasonReporter> = ids
        .iter()
        .map(|_| ReasonReporter::new(args.json || args.quiet))
        .collect();

    loop {
        if wait_was_interrupted() {
            return Ok(report_broken_wait(ids, false));
        }
        for (n, raw) in ids.iter().enumerate() {
            // Ask the coordinator, and read the record when the coordinator
            // does not answer. A coordinator that stops must not make this
            // command say that a job which operates does not exist.
            let from_coordinator = match Client::connect_existing() {
                Some(mut client) => match resolve_id_for_wait(&mut client, raw) {
                    Ok(Some(id)) => match client.call(&Request::Status { id }) {
                        Ok(Response::Status { status }) => Some(Some(*status)),
                        // The coordinator answered, and it holds no such job.
                        Ok(_) => Some(None),
                        Err(_) => None,
                    },
                    Ok(None) => Some(None),
                    Err(_) => None,
                },
                None => None,
            };

            let status = match from_coordinator {
                Some(answer) => answer,
                None => read_status_on_disk(raw)?,
            };

            let Some(status) = status else {
                eprintln!("qex: there is no job with the id {raw}");
                return Ok(EXIT_NO_SUCH_JOB);
            };

            reporters[n].note(&status);

            if status.state.is_terminal() {
                // From here the record is for a READER. See `for_display`.
                let status = for_display(status);
                if args.quiet {
                    // The exit code and nothing else.
                } else if args.json {
                    println!("{}", serde_json::to_string_pretty(&vec![&status])?);
                } else {
                    println!(
                        "{}: {} — {}",
                        short_id(&status.id),
                        status.state,
                        describe_result(&status)
                    );
                }
                warn_about_the_jobs_that_stay(args, ids, raw);
                return Ok(exit_code_for(&status));
            }
        }

        if let Some(d) = deadline {
            if Instant::now() >= d {
                eprintln!("qex: no job stopped before the time limit. They continue.");
                return Ok(EXIT_TIMEOUT);
            }
        }

        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

/// Names the jobs that `--next` leaves with no watcher.
///
/// `--next` gives control back one time. Every other job continues, and no
/// command watches it. An agent that reads one result and stops thus loses the
/// result of every other job. The remedy is one more wait, so give it here.
fn warn_about_the_jobs_that_stay(args: &cli::WaitArgs, ids: &[String], done: &str) {
    if args.json || args.quiet {
        return;
    }
    let rest: Vec<&str> = ids
        .iter()
        .map(|s| s.as_str())
        .filter(|id| *id != done)
        .collect();
    if rest.is_empty() {
        return;
    }
    let names = rest.join(" ");
    eprintln!(
        "qex: {} job(s) did not stop, and no command watches them now. Wait again:",
        rest.len()
    );
    eprintln!("qex:   qex wait --next {names}");
}

enum WaitOutcome {
    // `JobStatus` is large, and the other two answers hold nothing. A box keeps
    // this type small, because each value of it would otherwise take the space
    // of the largest one.
    Finished(Box<JobStatus>),
    TimedOut,
    NoSuchJob,
    /// A signal stopped the wait. The job continues.
    Interrupted,
}

/// True when a signal stopped the wait of this command.
static WAIT_INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_wait_signal(_signal: libc::c_int) {
    // A signal handler may use an atomic store and very little else.
    //
    // The disposition goes back to the system through SA_RESETHAND, and not
    // through a call here. The system makes that change at the moment of the
    // delivery, so a SECOND signal that arrives immediately after the first one
    // always meets the default behaviour: it stops this command. A handler that
    // calls `signal()` itself leaves a window in which the second signal meets
    // the handler again, and a trap that a user cannot escape is worse than no
    // trap.
    WAIT_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Catches SIGINT and SIGTERM while this command waits for a job.
///
/// Without this, Ctrl-C during a wait gives `128 + 2`. That form says "the JOB
/// died from a signal", and the job did not: the wait died and the job
/// continues. A dead process writes no exit code, so only a process that stays
/// alive can say which of the two happened.
///
/// This function is for a command that WATCHES a job and does not own it.
/// `qex run` owns its job, so it uses its own handler and it stops the job.
fn catch_signals_while_waiting() {
    // `sigaction` and not `signal`: `signal` gives different rules on
    // different systems, and this code needs two of them exactly.
    // SA_RESETHAND gives the disposition back to the system at the delivery, so
    // the second signal stops the command. The absence of SA_RESTART lets a
    // system call end with EINTR, which is what wakes the wait.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_wait_signal as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESETHAND;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }
}

fn wait_was_interrupted() -> bool {
    WAIT_INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Writes the message of a wait that a signal stopped, and gives its code.
///
/// The message is worth as much as the code. An agent or a person that stops a
/// wait must learn that the job survived, and must get the command that
/// attaches to it again.
fn report_broken_wait(ids: &[String], quiet: bool) -> i32 {
    if !quiet {
        eprintln!("qex: your wait stopped. The job continues.");
        // ONE job gives the command that also gives the record, because that is
        // what the reader wanted. MANY jobs give `qex wait`, which is the
        // command that takes many.
        if let [one] = ids {
            eprintln!("qex: attach to it again:  qex status {one} --wait");
        } else {
            eprintln!("qex: attach to them again:  qex wait {}", ids.join(" "));
        }
    }
    EXIT_WAIT_BROKEN
}

/// How long the wait sleeps between two tests of the signal flag.
const WAIT_SLICE: Duration = Duration::from_millis(250);

/// How often the wait asks why the job does not start.
const REASON_INTERVAL: Duration = Duration::from_secs(2);

/// How long the wait looks for a coordinator that replaced the earlier one.
///
/// The wait watches the RECORD of the job at the same time, so a job that stops
/// in this time gives its result at once. After this limit the wait reads the
/// record alone, which always answers, so the limit changes the speed of an
/// answer and never the answer.
const RECONNECT_LIMIT: Duration = Duration::from_secs(10);

/// Says why a job does not start, and says it again when the cause changes.
///
/// A queue that does nothing and does not say why is the fault that this tool
/// exists to remove, and that rule holds for every command that waits
/// (issue #28).
struct ReasonReporter {
    last: Option<String>,
    quiet: bool,
    /// True when the job started. The queue has nothing more to say.
    done: bool,
    next_test: Instant,
}

impl ReasonReporter {
    fn new(quiet: bool) -> Self {
        Self {
            last: None,
            quiet,
            done: false,
            next_test: Instant::now(),
        }
    }

    /// Asks for the state of the job, and reports a cause that changed.
    ///
    /// This is the ONE place where a wait asks a question of its own, and it
    /// stops at the moment that the job starts. A job that waits four hours in
    /// the queue and then runs for four more must not cost 14400 questions.
    fn poll(&mut self, raw_id: &str) {
        if self.quiet || self.done || Instant::now() < self.next_test {
            return;
        }
        self.next_test = Instant::now() + REASON_INTERVAL;

        // Use a SECOND connection. The first one holds the wait request, and
        // the coordinator answers that request when the job stops.
        let status = match Client::connect_existing() {
            Some(mut client) => {
                client.set_read_timeout(Some(REASON_INTERVAL)).ok();
                match resolve_id_for_wait(&mut client, raw_id) {
                    Ok(Some(id)) => match client.call(&Request::Status { id }) {
                        Ok(Response::Status { status }) => Some(*status),
                        _ => None,
                    },
                    _ => None,
                }
            }
            None => read_status_on_disk(raw_id).unwrap_or(None),
        };
        if let Some(status) = status {
            self.note(&status);
        }
    }

    /// Reports the cause that the record holds.
    fn note(&mut self, status: &JobStatus) {
        // A job that started has no queue reason, and it takes no new one.
        // Ask no more.
        if status.state != JobState::Queued {
            self.done = true;
            return;
        }
        if self.quiet {
            return;
        }
        let Some(reason) = &status.blocked_reason else {
            return;
        };
        if self.last.as_deref() == Some(reason.as_str()) {
            return;
        }
        eprintln!("qex: {}", crate::job::printable(reason));
        self.last = Some(reason.clone());
    }
}

/// Reads the record of a job from the disk.
///
/// The supervisor writes that record, and it continues after the coordinator
/// stops. The record is thus the answer that always exists.
fn read_status_on_disk(raw_id: &str) -> Result<Option<JobStatus>> {
    let Some(id) = find_id_on_disk(raw_id)? else {
        return Ok(None);
    };
    let dir = paths::job_dir(&id)?;
    Ok(crate::job::read_status(&dir).ok())
}

/// Waits for one job.
///
/// If a coordinator operates, this function sends one request and sleeps. If no
/// coordinator operates, it reads the status file of the job. The second path
/// is necessary because the supervisor continues after the coordinator stops.
///
/// A coordinator that STOPS in the middle of the wait does not end the wait.
/// qex replaces a coordinator on every update of the program, so that event is
/// the normal upgrade path and not an exotic case. The job continues, and its
/// result must reach the caller (issue #45).
fn wait_one(
    raw_id: &str,
    deadline: Option<Instant>,
    reporter: &mut ReasonReporter,
) -> Result<WaitOutcome> {
    loop {
        match wait_through_coordinator(raw_id, deadline, reporter)? {
            Some(outcome) => return Ok(outcome),
            // The coordinator stopped. Its answer never arrives, so find the
            // answer somewhere else.
            None => {
                // The job can have stopped already. The record holds the
                // result, and the supervisor wrote it.
                if let Some(status) = read_status_on_disk(raw_id)? {
                    if status.state.is_terminal() {
                        return Ok(WaitOutcome::Finished(Box::new(status)));
                    }
                }

                // A job that has no record and no coordinator does not exist.
                //
                // Say so NOW. The coordinator makes the directory of a job at
                // the submission, so a job with none never reached a queue, and
                // a search for a new coordinator would cost the reader ten
                // seconds before the same answer. The test is for an ID only: a
                // NAME lives in the coordinator, and the disk cannot resolve
                // one.
                if raw_id.parse::<uuid::Uuid>().is_ok() && find_id_on_disk(raw_id)?.is_none() {
                    return Ok(WaitOutcome::NoSuchJob);
                }

                if !reporter.quiet {
                    eprintln!(
                        "qex: the coordinator stopped. The job continues, and this wait \
                         continues with it."
                    );
                }
                match reconnect(raw_id, deadline) {
                    // A new coordinator answers. Ask it the same question.
                    //
                    // Sleep first. A coordinator that accepts a connection and
                    // then fails every request would otherwise make this loop
                    // spin, and the message above would repeat with it.
                    Reconnect::Ready => {
                        std::thread::sleep(WAIT_SLICE);
                        continue;
                    }
                    Reconnect::Finished(status) => return Ok(WaitOutcome::Finished(status)),
                    Reconnect::TimedOut => return Ok(WaitOutcome::TimedOut),
                    Reconnect::Interrupted => return Ok(WaitOutcome::Interrupted),
                    // No coordinator came back. The record of the job is the
                    // truth, so read it. Nothing about the JOB changed.
                    Reconnect::None => return wait_on_file(raw_id, deadline, reporter),
                }
            }
        }
    }
}

/// The result of a search for a coordinator that replaced the earlier one.
enum Reconnect {
    Ready,
    None,
    TimedOut,
    Interrupted,
    /// The job stopped while qex looked for a coordinator.
    Finished(Box<JobStatus>),
}

/// Looks for a coordinator that replaced the one that stopped.
///
/// It watches the RECORD at the same time. A job frequently stops while no
/// coordinator operates, and the record then holds the answer: a wait that
/// looked for a coordinator first would give that answer ten seconds late.
fn reconnect(raw_id: &str, deadline: Option<Instant>) -> Reconnect {
    let limit = Instant::now() + RECONNECT_LIMIT;
    loop {
        if wait_was_interrupted() {
            return Reconnect::Interrupted;
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Reconnect::TimedOut;
            }
        }
        if let Ok(Some(status)) = read_status_on_disk(raw_id) {
            if status.state.is_terminal() {
                return Reconnect::Finished(Box::new(status));
            }
        }
        if Client::connect_existing().is_some() {
            return Reconnect::Ready;
        }
        if Instant::now() >= limit {
            return Reconnect::None;
        }
        std::thread::sleep(WAIT_SLICE);
    }
}

/// Finds the id of a job for a wait.
///
/// `Ok(None)` says that the coordinator ANSWERED, and that it holds no such
/// job. An `Err` says that the coordinator did not answer, and the caller then
/// tries the record on the disk or a new coordinator. The two must stay apart:
/// "there is no job with that id" about a job that operates is the worst answer
/// that a wait can give.
fn resolve_id_for_wait(client: &mut Client, raw_id: &str) -> Result<Option<uuid::Uuid>> {
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };
    match resolve_targets_in(&jobs, raw_id) {
        Ok(found) if found.group.is_none() && found.ids.len() == 1 => Ok(Some(found.ids[0])),
        // A name that gives many jobs is not a fault of the transport. This
        // command takes one job, so it says so through the caller.
        _ => Ok(None),
    }
}

/// Waits for one job through the coordinator.
///
/// Gives `None` when the coordinator stopped. The caller then finds the answer
/// in the record of the job, or through a new coordinator.
fn wait_through_coordinator(
    raw_id: &str,
    deadline: Option<Instant>,
    reporter: &mut ReasonReporter,
) -> Result<Option<WaitOutcome>> {
    let Some(mut client) = Client::connect_existing() else {
        return Ok(None);
    };
    // Give every read a limit.
    //
    // `wait_readable` holds the deadline of the user, and the reads around it
    // do not. A coordinator that accepts the connection and then says nothing
    // would hold `qex wait --timeout 30m` for ever, and the first Ctrl-C would
    // not reach the test above.
    client
        .set_read_timeout(Some(WAIT_SLICE.max(REASON_INTERVAL)))
        .ok();

    // Separate "the coordinator says that there is no such job" from "the
    // coordinator did not answer".
    //
    // `resolve_id` asks the coordinator for the job list, so a coordinator that
    // stops in this moment gives an error here. To read that error as
    // "no such job" tells the caller that its RUNNING job does not exist, and
    // the caller then stops watching work that continues.
    let id = match resolve_id_for_wait(&mut client, raw_id) {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(Some(WaitOutcome::NoSuchJob)),
        // The coordinator did not answer. Find the answer without it.
        Err(_) => return Ok(None),
    };

    // The coordinator answers when the job stops, so this request waits. A
    // coordinator that stops before it reads the request gives an error here,
    // and the caller then finds the answer without it.
    if client.send(&Request::Wait { id }).is_err() {
        return Ok(None);
    }

    // Look at the socket in short steps.
    //
    // The steps cost one system call each, and they let this command do three
    // things that a blocking read cannot: obey a signal from the user, say why
    // the job waits, and see a coordinator that stopped.
    loop {
        if wait_was_interrupted() {
            return Ok(Some(WaitOutcome::Interrupted));
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Ok(Some(WaitOutcome::TimedOut));
            }
        }
        match client.wait_readable(WAIT_SLICE) {
            Ok(true) => break,
            Ok(false) => {
                reporter.poll(raw_id);
                continue;
            }
            // The socket itself failed. The coordinator is gone.
            Err(_) => return Ok(None),
        }
    }

    match client.recv() {
        Ok(Response::Status { status }) => Ok(Some(WaitOutcome::Finished(status))),
        Ok(Response::Error {
            kind: ErrorKind::NoSuchJob,
            ..
        }) => Ok(Some(WaitOutcome::NoSuchJob)),
        // One message for one fault. `report_for_a_job` writes the message of
        // the coordinator, and a second line here would say the same thing
        // twice in different words.
        Ok(other) => bail!("the coordinator gave an answer that qex did not expect: {other:?}"),
        // The coordinator closed the connection, or the read failed. Both mean
        // that the coordinator stopped: the deadline above ends a wait that
        // reached its time limit, so a fault here is not a time limit.
        Err(_) => Ok(None),
    }
}

/// Writes a warning when the coordinator holds a different version.
///
/// A coordinator can operate for hours, and a new build replaces the program.
/// The coordinator then holds the earlier code. That difference caused a fault
/// that named no cause: every job failed with "No such file or directory".
///
/// qex now starts the program that is on the disk, so a job runs. The two
/// versions can still behave differently, so the user must know.
fn warn_if_version_differs(client: &mut Client) {
    let mine = crate::version::VERSION;
    if let Ok(Response::Info {
        version,
        pid,
        program_replaced,
        ..
    }) = client.call(&Request::Info)
    {
        // Each message below covers one fact, and one command writes one
        // message at the most. Two messages about one fact teach a reader to
        // read neither.
        match crate::capabilities::check_floor(&version, pid) {
            // A coordinator below the floor gives an ERROR, with the same
            // remedy, when the command needs the coordinator. Say nothing here.
            crate::capabilities::Floor::Below(_) => {}

            // A development build is not refused, so this warning is the only
            // place that the user hears about it.
            //
            // It stays quiet while the coordinator reports the SAME version as
            // this command: the two are then one build, the user made it, and
            // there is nothing that they do not know already. A build of qex
            // that runs the tests of qex would otherwise write this line for
            // every command.
            crate::capabilities::Floor::Development(message) if version != mine => {
                eprintln!("qex: {message}");
            }

            _ if version != mine => {
                eprintln!(
                    "qex: the coordinator (pid {pid}) is version {version}, and this command is \
                     version {mine}. The coordinator stops when no job operates, and the next \
                     command starts one with this version. Stop it now with `kill {pid}` if you \
                     need this version immediately."
                );
            }

            _ if program_replaced => {
                eprintln!(
                    "qex: something replaced the qex program after the coordinator (pid {pid}) \
                     started. The coordinator stops when no job operates."
                );
            }

            _ => {}
        }
    }
}

/// Changes each dependency name into a job id.
///
/// Each name must give a job that exists now. A name that gives no job is an
/// error at the submission.
///
/// A name that gives a pipeline gives every stage of it, so `--needs $GROUP`
/// waits for the whole pipeline.
fn resolve_dependencies(
    client: &mut Client,
    names: &[String],
    option: &str,
) -> Result<Vec<uuid::Uuid>> {
    let mut ids = Vec::new();
    for name in names {
        let found = resolve_targets(client, name).map_err(|e| {
            anyhow::anyhow!(
                "{option}: {e}\n\n\
                 A job can wait for the jobs that you started before it. Start the \
                 first job, keep its id, then give that id here."
            )
        })?;
        if found.group.is_some() {
            // A pipeline is one unit of work, so the test for an earlier run
            // applies to the pipeline and not to each stage.
            //
            // The stages of a pipeline stop in order, so a pipeline that
            // operates almost always holds stages that already stopped. A test
            // of each stage would refuse `--needs $PIPELINE` for the ordinary
            // case, and the documentation says that it waits for the whole
            // pipeline.
            pipeline_dependency(client, name, &found.ids, option, &mut ids)?;
        } else {
            for id in found.ids {
                resolve_one_dependency(client, name, id, option, &mut ids)?;
            }
        }
    }
    Ok(ids)
}

/// Tests a dependency that names a whole pipeline, and adds every stage.
///
/// A pipeline that a NAME gives must still hold work. A name gives the newest
/// pipeline of that file, and a pipeline whose every stage stopped is a run of
/// an earlier day: the new job would then wait for nothing, and it would report
/// success although the order was wrong.
fn pipeline_dependency(
    client: &mut Client,
    name: &str,
    stages: &[uuid::Uuid],
    option: &str,
    ids: &mut Vec<uuid::Uuid>,
) -> Result<()> {
    let by_name = name.parse::<uuid::Uuid>().is_err();
    if by_name {
        let mut all_stopped = true;
        for id in stages {
            if let Response::Status { status } = client.call(&Request::Status { id: *id })? {
                if !status.state.is_terminal() {
                    all_stopped = false;
                    break;
                }
            }
        }
        if all_stopped {
            bail!(
                "{option}: the name `{name}` gives a pipeline of {} stage(s), and every \
                 stage already stopped.\n\n\
                 A name can give a pipeline of an earlier run. Did you forget to start a \
                 new `{name}` pipeline?\n\n\
                 Use the group id that `qex pipeline` wrote for this run:\n\
                 \x20   GROUP=$(qex pipeline your-file)\n\
                 \x20   qex submit {option} $GROUP -- ...\n\n\
                 A group id always names one run, so qex accepts it whatever its state.",
                stages.len()
            );
        }
    }

    for id in stages {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    Ok(())
}

/// Tests one dependency, and adds it to the list.
fn resolve_one_dependency(
    client: &mut Client,
    name: &str,
    id: uuid::Uuid,
    option: &str,
    ids: &mut Vec<uuid::Uuid>,
) -> Result<()> {
    // A dependency given by NAME must still be in the queue or operate.
    //
    // A name is the value that can be wrong in silence. An agent runs a script
    // a second time and writes `--needs test`, but it forgot to start a new
    // test job. The name gives the test job of the FIRST run, which already
    // stopped. The new stage then waits for nothing, and the pipeline reports
    // success although the order was wrong.
    //
    // An id does not have that risk. An id names one job for ever, and the
    // agent read it from the `qex submit` of this run. An id thus needs the
    // existence test only, which the resolver already made.
    //
    // This difference also keeps a pipeline script correct. A script that keeps
    // each id can submit its last stage even when the first stage already
    // failed, and that stage then becomes `skipped` with the correct cause.
    let by_name = name.parse::<uuid::Uuid>().is_err();
    if by_name {
        if let Response::Status { status } = client.call(&Request::Status { id })? {
            if status.state.is_terminal() {
                bail!(
                    "{option}: the name `{name}` gives the job {}, which already stopped. \
                     Its state is `{}`.\n\n\
                     A name can give a job of an earlier run. Did you forget to start a \
                     new `{name}` job?\n\n\
                     Use the id that `qex submit` wrote for this run:\n\
                     \x20   ID=$(qex submit --name {name} -- ...)\n\
                     \x20   qex submit {option} $ID -- ...\n\n\
                     An id always names one job, so qex accepts an id whatever its state.",
                    &id.to_string()[..8],
                    status.state
                );
            }
        }
    }

    if !ids.contains(&id) {
        ids.push(id);
    }
    Ok(())
}

/// Tests if a socket fault is the time limit of the read.
///
/// The system gives `WouldBlock` or `TimedOut` for a read that reaches its
/// limit. Every other fault has a different cause, such as a coordinator that
/// stopped.
fn is_read_timeout(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            );
        }
    }
    false
}

/// Waits by reading the status file of the job.
///
/// The supervisor writes that file in one operation, so a reader sees the old
/// contents or the new contents, and never a part of them.
fn wait_on_file(
    raw_id: &str,
    deadline: Option<Instant>,
    reporter: &mut ReasonReporter,
) -> Result<WaitOutcome> {
    let Some(id) = find_id_on_disk(raw_id)? else {
        return Ok(WaitOutcome::NoSuchJob);
    };
    let dir = paths::job_dir(&id)?;
    let mut delay = Duration::from_millis(20);

    loop {
        if let Ok(status) = crate::job::read_status(&dir) {
            if status.state.is_terminal() {
                return Ok(WaitOutcome::Finished(Box::new(status)));
            }
            reporter.note(&status);
        }

        if wait_was_interrupted() {
            return Ok(WaitOutcome::Interrupted);
        }

        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Ok(WaitOutcome::TimedOut);
            }
        }

        // The delay grows to `WAIT_SLICE`. A short job thus gives a fast
        // answer, a long job does not use CPU time, and a signal from the user
        // reaches the test above in a quarter of a second.
        std::thread::sleep(delay);
        delay = (delay * 2).min(WAIT_SLICE);
    }
}

pub fn logs(args: cli::LogsArgs) -> Result<i32> {
    // Use the same code as `status` and `wait` for the same fault. A script
    // must not need two codes for one condition.
    let id = match Client::connect_existing() {
        Some(mut c) => match resolve_id(&mut c, &args.id) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("qex: {e}");
                return Ok(EXIT_NO_SUCH_JOB);
            }
        },
        None => match find_id_on_disk(&args.id)? {
            Some(id) => id,
            None => {
                eprintln!("qex: there is no job with the id {}", args.id);
                return Ok(EXIT_NO_SUCH_JOB);
            }
        },
    };

    let dir = paths::job_dir(&id)?;

    if args.follow {
        // A stream has no total, so the options that count a total have no
        // meaning here.
        if let Err(e) = args.select.check_with_follow() {
            bail!("{e}");
        }
        return follow(&dir, &args.select);
    }

    // The log of the stop hook is not a stream of the job, so it comes alone.
    // A reader who asks why a notification did not arrive must not receive the
    // output of the job with that answer.
    let streams = if args.hook {
        // Say that there is no file. An empty answer would look like a hook
        // that ran and wrote nothing, and the reader would test the wrong
        // thing.
        //
        // The message goes to stderr, and `--json` continues to the document
        // below. A reader that asks for JSON must always receive JSON: an empty
        // answer gives that reader an error from its parser, and not an answer.
        if !dir.join("hook.log").exists() {
            eprintln!(
                "qex: qex ran no stop hook for this job, so there is no log. \
                 Read `qex config show` to see the hook and the states that it runs on."
            );
            if !args.json {
                return Ok(0);
            }
        }
        vec![("hook", "hook.log")]
    } else {
        chosen_streams(&args.select)
    };
    // The record says what the LIMIT removed. That is not in the file, and no
    // option gives it back, so each path below says it.
    let status = crate::job::read_status(&dir).ok();

    if args.json {
        let mut out = serde_json::Map::new();
        out.insert("id".into(), serde_json::Value::String(id.to_string()));
        for (name, file) in &streams {
            let selected = select_log(&dir, file, &args.select, crate::logsel::DEFAULT_LINES)?;
            out.insert((*name).into(), serde_json::Value::String(selected.text));
            if let Some(found) = selected.matches {
                out.insert(format!("{name}_matches"), serde_json::Value::from(found));
            }
            if selected.truncated {
                out.insert(
                    format!("{name}_hidden_lines"),
                    serde_json::Value::from(selected.hidden),
                );
            }
            if let Some((bytes, lines)) = status
                .as_ref()
                .and_then(|s| s.logs_dropped)
                .and_then(|d| d.of(name))
            {
                out.insert(
                    format!("{name}_dropped_bytes"),
                    serde_json::Value::from(bytes),
                );
                out.insert(
                    format!("{name}_dropped_lines"),
                    serde_json::Value::from(lines),
                );
            }
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    for (name, file) in &streams {
        let selected = select_log(&dir, file, &args.select, crate::logsel::DEFAULT_LINES)?;
        if selected.text.is_empty() && selected.matches.unwrap_or(1) > 0 {
            continue;
        }
        if streams.len() > 1 {
            println!("==> {name} <==");
        }
        // The notice goes to stderr, so stdout holds the log lines only.
        // A command such as `qex logs $ID > file` must give a clean file, and a
        // reader that parses the output must not meet a sentence in it.
        //
        // `logs_dropped` counts the output of the JOB, so it says nothing about
        // the log of the hook. Without this test, a job whose output did not
        // close made `qex logs --hook` print "the output of this job did not
        // close" above the hook log, which sends the reader to the wrong file.
        // The hook has its own limit, and its own verdict inside `hook.log`.
        if !args.hook {
            if let Some(notice) = status.as_ref().and_then(|s| dropped_notice(s, name)) {
                eprintln!("{notice}");
            }
        }
        if let Some(notice) = selected.notice() {
            eprintln!("{notice}");
        }
        print!("{}", selected.text);
    }
    Ok(0)
}

/// Gives the streams that the options select.
fn chosen_streams(select: &crate::logsel::LogSelect) -> Vec<(&'static str, &'static str)> {
    if select.stdout {
        vec![("stdout", "stdout.log")]
    } else if select.stderr {
        vec![("stderr", "stderr.log")]
    } else {
        vec![("stdout", "stdout.log"), ("stderr", "stderr.log")]
    }
}

/// Reads one log file and selects the part to show.
///
/// The read is lossy on purpose. A job writes any byte, and one byte that is
/// not UTF-8 must not hide the whole file.
fn select_log(
    dir: &std::path::Path,
    file: &str,
    select: &crate::logsel::LogSelect,
    default_limit: usize,
) -> Result<crate::logsel::Selected> {
    let text = match std::fs::read(dir.join(file)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    };
    select
        .apply(&text, default_limit)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Writes the output of a job while the job operates.
///
/// The command first writes the last lines that the job already wrote, then it
/// writes each new line. `--tail N` sets the number of first lines, in the same
/// way as `tail -f -n N`. Without that option the command writes a few lines,
/// because a job can already have written a very large file.
///
/// With `--grep`, this function writes the lines that match as they arrive.
/// That combination is the reason for the option: a pipe to `grep` holds the
/// lines in a buffer and shows nothing until the buffer fills, because `grep`
/// needs `--line-buffered`. This code writes each line as it reads it.
fn follow(dir: &std::path::Path, select: &crate::logsel::LogSelect) -> Result<i32> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let streams = chosen_streams(select);
    // Say what the limit removed before the first line. A reader who follows a
    // job that already reached the limit must not take the new lines for the
    // whole output. While the job operates, the supervisor writes the same
    // information into the file itself, and this command gives it as it
    // arrives.
    let mut said: Vec<String> = Vec::new();
    if let Ok(status) = crate::job::read_status(dir) {
        for (name, _) in &streams {
            if let Some(notice) = dropped_notice(&status, name) {
                eprintln!("{notice}");
                said.push((*name).to_string());
            }
        }
    }
    let lead = select.tail.unwrap_or(crate::logsel::FOLLOW_LEAD_LINES);
    let stdout = std::io::stdout();
    let mut handles = Vec::new();

    for (name, file) in &streams {
        let path = dir.join(file);
        // The supervisor can make this file after the command starts.
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .create(true)
            // Keep what the file holds. This command reads the output of the
            // job, and it must never remove it.
            .truncate(false)
            .write(true)
            .open(&path)?;

        // Read what the job already wrote, and show the last lines of it.
        let mut existing = Vec::new();
        f.read_to_end(&mut existing).ok();
        let text = String::from_utf8_lossy(&existing).into_owned();

        if lead > 0 {
            let keep: Vec<&str> = text
                .lines()
                .filter(|line| keep_line(select, line))
                .collect();
            let from = keep.len().saturating_sub(lead);
            for line in &keep[from..] {
                let mut out = stdout.lock();
                if streams.len() > 1 {
                    write!(out, "[{name}] ")?;
                }
                writeln!(out, "{line}")?;
            }
            stdout.lock().flush()?;
        }

        // Continue after the text that this code already read. A partial last
        // line stays in the buffer, so a filter never tests half of a line.
        let partial = match text.rfind('\n') {
            Some(end) => text[end + 1..].to_string(),
            None => text,
        };
        let end = f.seek(SeekFrom::End(0))?;
        handles.push(Followed {
            name: name.to_string(),
            file: f,
            partial,
            high_water: end,
            said: said.iter().any(|s| s == name),
        });
    }

    loop {
        let mut moved = false;
        for h in handles.iter_mut() {
            // THE LOG FILE OF A JOB CAN BECOME SHORTER. When the output passes
            // `[logs] max_bytes`, the supervisor keeps the head and removes the
            // middle. The position of this command is then after the end of the
            // file, and every later line goes to nobody: the reader sees the
            // output stop, with no word, and this command exits with the code 0.
            //
            // qex therefore watches the length. A file that became shorter gets
            // a notice and a new position at its end.
            let len = h.file.metadata().map(|m| m.len()).unwrap_or(h.high_water);
            if len < h.high_water {
                if !h.said {
                    eprintln!(
                        "... qex reached the limit `[logs] max_bytes` and removed the middle \
                         of this file. This command continues at the new end of the file. \
                         Read the file again when the job stops."
                    );
                    h.said = true;
                }
                h.file.seek(SeekFrom::Start(len))?;
                h.partial.clear();
                // The new length is the new reference. Without this line, the
                // test above would find the file short at each turn, and the
                // last part of the output would never reach the reader.
                h.high_water = len;
            }

            let mut buf = Vec::new();
            if h.file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                moved = true;
                h.partial.push_str(&String::from_utf8_lossy(&buf));

                while let Some(end) = h.partial.find('\n') {
                    let line: String = h.partial.drain(..=end).collect();
                    let line = line.trim_end_matches('\n');
                    if keep_line(select, line) {
                        let mut out = stdout.lock();
                        if streams.len() > 1 {
                            write!(out, "[{}] ", h.name)?;
                        }
                        writeln!(out, "{line}")?;
                        out.flush()?;
                    }
                }
            }
            h.high_water = h
                .file
                .stream_position()
                .unwrap_or(h.high_water)
                .max(h.high_water);
        }

        match crate::job::read_status(dir) {
            Ok(status) => {
                if status.state.is_terminal() && !moved {
                    // The last word about the output.
                    //
                    // A job that reaches the limit and stops between two reads
                    // of this loop leaves no shorter file for the test above to
                    // find. The record holds the truth, and the supervisor
                    // writes it when the job stops, so this command reads it
                    // here. A reader must never take a part of the output for
                    // the whole output.
                    for h in handles.iter_mut() {
                        if h.said {
                            continue;
                        }
                        if let Some(notice) = dropped_notice(&status, &h.name) {
                            eprintln!("{notice}");
                            h.said = true;
                        }
                    }
                    return Ok(0);
                }
            }
            Err(_) => {
                // The record is gone, so `qex clean` deleted the job.
                if !moved {
                    return Ok(0);
                }
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One stream that `qex logs --follow` reads.
struct Followed {
    name: String,
    file: std::fs::File,
    /// The text after the last line end. A filter must never test half a line.
    partial: String,
    /// The largest length that this command saw.
    ///
    /// The log file of a job becomes shorter when the output passes the limit,
    /// so a position alone does not show that qex removed something.
    high_water: u64,
    /// True after this command said that the output is not complete.
    said: bool,
}

/// Tests one line against the filter of a stream.
///
/// Without `--grep`, every line passes.
fn keep_line(select: &crate::logsel::LogSelect, line: &str) -> bool {
    if select.grep.is_none() {
        return true;
    }
    // An incorrect pattern gives an error before this point, so a fault here
    // can only be unexpected. Show the line in that case; a lost line is worse
    // than an extra line.
    select
        .apply(line, 1)
        .map(|s| s.matches_shown > 0)
        .unwrap_or(true)
}

pub fn kill(args: cli::KillArgs) -> Result<i32> {
    let signal = crate::lifecycle::parse_signal(&args.signal)
        .map_err(|e| anyhow::anyhow!("--signal: {e}"))?;
    let grace = parse_duration(&args.grace)
        .map_err(|e| anyhow::anyhow!("--grace: {e}"))?
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut client = Client::connect()?;
    let mut worst = 0;
    for raw in &args.ids {
        let (found, states) = match resolve_with_states(&mut client, raw) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("qex: {e}");
                worst = EXIT_NO_SUCH_JOB;
                continue;
            }
        };
        let whole_pipeline = found.group.is_some();
        for id in found.ids {
            // A stage that already stopped needs nothing.
            //
            // The stages of a pipeline stop in order, so at the moment a user
            // stops a pipeline the early stages have usually finished. An
            // early version gave the code 1 for that ordinary case, and the
            // documentation says that the command stops every stage.
            if whole_pipeline && stopped(&states, id) {
                println!("{id} already stopped");
                continue;
            }

            // `qex kill $GROUP` says "stop every stage", so a stage that waits
            // in the queue leaves the queue.
            //
            // A user who names ONE job that waits gets the fault and the
            // instruction to use `qex cancel`, because that user asked about
            // that one job. A user who named the pipeline asked for the whole
            // of it to stop, and a stage that qex left in the queue would
            // START after the command said that it stopped everything.
            let waiting = matches!(states.get(&id), Some(JobState::Queued));
            let answer = if whole_pipeline && waiting {
                match client.call(&Request::Cancel { id })? {
                    Response::Ok => {
                        println!("{id} left the queue");
                        continue;
                    }
                    other => other,
                }
            } else {
                match client.call(&Request::Kill {
                    id,
                    signal,
                    grace_secs: grace,
                })? {
                    Response::Ok => {
                        println!("{id} received the signal");
                        continue;
                    }
                    other => other,
                }
            };

            // Every other answer is a fault, and it keeps its code. A refusal
            // that this command reported as a success would tell a script that
            // the work stopped while the work continued.
            let code = report(answer)?;
            if code != 0 && worst == 0 {
                worst = code;
            }
        }
    }
    Ok(worst)
}

/// Tells whether this job already stopped.
///
/// A job that the list does not hold counts as stopped. `qex clean` can remove
/// a record between the list and the command, and a record that went away is
/// not work that continues.
fn stopped(states: &std::collections::HashMap<uuid::Uuid, JobState>, id: uuid::Uuid) -> bool {
    states.get(&id).map(|s| s.is_terminal()).unwrap_or(true)
}

/// Reads the text of the user, and keeps the state of each job.
///
/// A command that stops work must know the state before it acts. Without it,
/// `qex kill $GROUP` cannot tell a stage that already stopped from a stage that
/// waits in the queue, and those two need opposite answers.
fn resolve_with_states(
    client: &mut Client,
    raw: &str,
) -> Result<(Targets, std::collections::HashMap<uuid::Uuid, JobState>)> {
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };
    let found = resolve_targets_in(&jobs, raw)?;
    let states = jobs.iter().map(|j| (j.id, j.state)).collect();
    Ok((found, states))
}

pub fn cancel(args: cli::CancelArgs) -> Result<i32> {
    let mut client = Client::connect()?;
    let mut worst = 0;
    for raw in &args.ids {
        let (found, states) = match resolve_with_states(&mut client, raw) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("qex: {e}");
                worst = EXIT_NO_SUCH_JOB;
                continue;
            }
        };
        let whole_pipeline = found.group.is_some();
        for id in found.ids {
            // A stage that already stopped needs nothing. Every other refusal
            // keeps its code: a stage that OPERATES cannot leave the queue,
            // and a command that reported that as a success would tell a
            // script that the work stopped while the work continues.
            if whole_pipeline && stopped(&states, id) {
                println!("{id} already stopped");
                continue;
            }
            match client.call(&Request::Cancel { id })? {
                Response::Ok => println!("{id} left the queue"),
                other => {
                    let code = report(other)?;
                    if code != 0 && worst == 0 {
                        worst = code;
                    }
                }
            }
        }
    }
    Ok(worst)
}

pub fn clean(args: cli::CleanArgs) -> Result<i32> {
    if args.ids.is_empty()
        && !args.all
        && !args.auto
        && args.state.is_none()
        && args.older_than.is_none()
        && args.cwd.is_none()
        && args.under.is_none()
    {
        bail!(
            "name the jobs to delete.\n\n\
             Examples:\n\
             \x20   qex clean <id>\n\
             \x20   qex clean completed        # or: qex clean --state completed\n\
             \x20   qex clean done             # every job that stopped\n\
             \x20   qex clean --auto           # everything safe, here and below\n\
             \x20   qex clean --cwd            # the jobs of this directory\n\
             \x20   qex clean --under          # the jobs of this directory and below\n\
             \x20   qex clean --older-than 7d\n\
             \x20   qex clean --all"
        );
    }

    let clean_cwd = match &args.cwd {
        Some(p) => Some(resolve_directory(p, "--cwd")?),
        None => None,
    };
    let mut clean_under = match &args.under {
        Some(p) => Some(resolve_directory(p, "--under")?),
        None => None,
    };
    // `--auto` works on this directory and below, unless the user names one.
    // A command that deletes must never reach the work of a different project.
    if args.auto && clean_cwd.is_none() && clean_under.is_none() {
        clean_under = Some(resolve_directory(std::path::Path::new("."), "--auto")?);
    }
    let by_directory = clean_cwd.is_some() || clean_under.is_some();

    // `--auto` is a short form. It gives the two options below, and it works on
    // this directory and below.
    //
    // The age is the safety. A job of the last hour can still be the job that a
    // user reads now, and a job of yesterday is history.
    let older_than = if args.auto {
        Some(AUTO_CLEAN_AGE)
    } else {
        match &args.older_than {
            Some(t) => parse_duration(t)
                .map_err(|e| anyhow::anyhow!("--older-than: {e}"))?
                .map(|d| d.as_secs()),
            None => None,
        }
    };
    let filter = if args.auto {
        Some(StateFilter::Done)
    } else {
        match args.state.as_deref() {
            Some(s) => Some(StateFilter::parse(s).map_err(|e| anyhow::anyhow!("--state: {e}"))?),
            None => None,
        }
    };

    let mut client = Client::connect()?;
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };

    let now = crate::sys::now_secs();
    // A job that a job in the queue still needs is not finished for the purpose
    // of a deletion, whatever its own state says.
    let held = needed_by_unfinished(&jobs);
    let mut held_back = 0usize;
    let mut targets: Vec<uuid::Uuid> = Vec::new();
    let mut word_filters: Vec<StateFilter> = Vec::new();

    for raw in &args.ids {
        // Accept a state name in place of a job id, so `qex clean completed`
        // operates in the same way as `qex clean --state completed`.
        //
        // A job or a pipeline can have the name of a state. Test them first,
        // and give an error when the word gives both. A command that deletes
        // must never choose one of two readings.
        let names_work = jobs.iter().any(|j| names_job(j, raw));
        let names_pipeline = jobs.iter().any(|j| names_group(j, raw));
        match (names_work || names_pipeline, StateFilter::parse(raw)) {
            (true, Ok(_)) => {
                let what = if names_work { "a job" } else { "a pipeline" };
                eprintln!(
                    "qex: `{raw}` is the name of {what} and the name of a state. Give the \
                     id of the {} that you want, or use `--state {raw}` for the state.",
                    if names_work { "job" } else { "pipeline" }
                );
                return Ok(EXIT_NO_SUCH_JOB);
            }
            (false, Ok(f)) => word_filters.push(f),
            _ => match resolve_targets_in(&jobs, raw) {
                Ok(found) => targets.extend(found.ids),
                Err(e) => {
                    // Use the same code as every other command for the same
                    // fault. `qex clean` gave 1 where `qex kill` gave 127.
                    eprintln!("qex: {e}");
                    return Ok(EXIT_NO_SUCH_JOB);
                }
            },
        }
    }

    for j in &jobs {
        if !j.state.is_terminal() {
            continue;
        }
        if held.contains(&j.id) {
            // A job that a job in the queue needs. It is not finished yet for
            // this purpose.
            held_back += 1;
            continue;
        }

        // A directory is a filter and not a selector. A job outside the
        // directory is never deleted, whatever the other options say.
        if by_directory && !matches_directory(&j.cwd, clean_cwd.as_deref(), clean_under.as_deref())
        {
            continue;
        }

        let by_state = filter.as_ref().map(|f| f.matches(j.state)).unwrap_or(false)
            || word_filters.iter().any(|f| f.matches(j.state));
        let by_age = older_than
            .map(|limit| now.saturating_sub(j.finished_at.unwrap_or(j.submitted_at)) >= limit)
            .unwrap_or(false);

        // `--auto` needs BOTH conditions. A job that stopped a minute ago is
        // frequently the job that the user reads now.
        if args.auto {
            if by_state && by_age {
                targets.push(j.id);
            }
            continue;
        }

        // A directory with no other option means every job that stopped in
        // that directory. `qex clean --under` is then the whole answer for a
        // user who wants the records of one project.
        let by_directory_alone = by_directory
            && !args.auto
            && filter.is_none()
            && word_filters.is_empty()
            && older_than.is_none();

        if args.all || by_state || by_age || by_directory_alone {
            targets.push(j.id);
        }
    }

    targets.sort();
    targets.dedup();

    let mut deleted = 0usize;
    let mut worst = 0;
    for id in targets {
        match client.call(&Request::Clean { id })? {
            Response::Ok => deleted += 1,
            other => worst = report(other)?,
        }
    }

    println!("qex deleted the records of {deleted} job(s)");
    if held_back > 0 {
        println!(
            "{held_back} record(s) stayed, because a job that has not stopped still needs \
             them. They go when that job stops."
        );
    }

    Ok(worst)
}

/// Gives the exit code for a job result.
///
/// EVERY command that waits for a job uses this one function, so two commands
/// cannot answer one question two ways.
///
/// The rule is the band. A job that RAN to its own end gives its own code, and
/// qex maps a code that the band keeps onto the sentinel. Every other state
/// belongs to the queue or to the wait, so it takes a code from the band.
fn exit_code_for(status: &JobStatus) -> i32 {
    // A job that ran to its own end gives its own exit code. The caller of
    // `qex run` and of `qex submit --wait` expects the code of the command
    // that it replaced.
    if matches!(status.state, JobState::Completed | JobState::Failed) {
        // A signal stopped the job, so the job gave no code of its own. The
        // record names the signal.
        if status.exit_code.is_none() && status.signal.is_some() {
            return EXIT_JOB_SIGNAL;
        }
        let Some(code) = status.exit_code.or(match status.state {
            JobState::Completed => Some(0),
            // The job stopped, and qex holds neither an exit code nor a signal
            // for it. qex does not know, and 1 would say that the JOB knew and
            // gave 1.
            _ => None,
        }) else {
            return EXIT_QEX_FAILED;
        };
        // The job gave a code that qex keeps for itself. Give the sentinel, so
        // the caller reads the record and does not read the code of the job as
        // an answer about the queue or about the wait.
        if code >= EXIT_BAND_FLOOR {
            return EXIT_JOB_RESERVED;
        }
        return code;
    }

    match status.state {
        JobState::Completed => 0,
        // A job that `qex cancel` removed from the queue never ran and wrote
        // nothing, so it has the code of a job that something stopped. The
        // caller must not read it as a fault in the work: the work did not
        // start. The code is the same for `qex run` and for `qex wait`,
        // because one state gives one answer.
        // The kernel stopped this job for memory. The remedy is a larger claim,
        // and it is not the remedy of the other three, so the code is its own.
        JobState::Oom => EXIT_OOM,
        JobState::Killed | JobState::Timeout | JobState::Cancelled => EXIT_KILLED,
        // A job that never started has its own code. A script can then separate
        // "my job ran too long" from "my job never got the machine".
        JobState::Expired => EXIT_EXPIRED,
        // A job that did not run has its own code. A script can then separate
        // "my job failed" from "a job before mine failed", and it does not read
        // the JSON output.
        JobState::Skipped => EXIT_SKIPPED,
        // A state that is not the end of a job, or a job that ended with
        // neither an exit code nor a signal. qex has no result to give, and 1
        // would say that the job ran and gave 1. No code in the table fits
        // exactly: 121 says "qex could not do what you asked", and the part
        // that qex could not do is the RESULT. The record holds the state, and
        // the rule of the band holds here by construction, and not by the
        // states that happen to reach this function.
        _ => EXIT_QEX_FAILED,
    }
}

/// Writes a short text for a job result.
fn describe_result(s: &JobStatus) -> String {
    match s.state {
        JobState::Completed => "the job succeeded".to_string(),
        JobState::Failed => match (s.exit_code, s.signal) {
            (Some(c), _) => format!("the job stopped with the exit code {c}"),
            (None, Some(sig)) => format!("the signal {sig} stopped the job"),
            _ => "the job failed".to_string(),
        },
        JobState::Killed => "a command stopped the job".to_string(),
        JobState::Timeout => "the job reached its time limit".to_string(),
        // Give the queue reason here. The reader learns what the job waited for
        // with no other command, and there is no log file to read.
        JobState::Expired => s
            .error
            .clone()
            .unwrap_or_else(|| "the job waited more time than its queue limit".to_string()),
        // Say that the CLAIM was too small, and not that the machine was full.
        //
        // The words "the machine ran out of memory" sent the reader to the
        // machine, and the fault was in the claim. qex holds the full story in
        // the error field, with the claim that it tried, so give that text when
        // qex wrote it.
        JobState::Oom => s.error.clone().unwrap_or_else(|| {
            format!(
                "the kernel stopped the job for memory. The claim of {} was too small, and the \
                 job reached {}. Give a larger `--mem` value.",
                format_size(s.mem),
                format_size(s.usage.max_rss)
            )
        }),
        JobState::Cancelled => "the job left the queue".to_string(),
        // Give the cause here. A reader of the last job of a pipeline then
        // learns which job failed, with no other command.
        JobState::Skipped => s
            .error
            .clone()
            .unwrap_or_else(|| "a job that this job needed did not succeed".to_string()),
        other => format!("the job is {other}"),
    }
}

/// Writes an error from the coordinator and gives an exit code.
/// Writes an error of the coordinator for a command that gives the exit code of
/// a JOB.
///
/// The code 1 says "the job ran and it failed". A coordinator that refuses a
/// request is a fault of qex, and no job gave a code for it, so it takes the
/// code of the band. `qex kill` and the other commands never speak for a job,
/// so they keep the conventional 1.
fn report_for_a_job(response: Response) -> Result<i32> {
    let code = report(response)?;
    Ok(if code == 1 { EXIT_QEX_FAILED } else { code })
}

fn report(response: Response) -> Result<i32> {
    match response {
        Response::Error { message, kind } => {
            eprintln!("qex: {message}");
            Ok(match kind {
                ErrorKind::NoSuchJob => EXIT_NO_SUCH_JOB,
                _ => 1,
            })
        }
        Response::Ok => Ok(0),
        other => bail!("the coordinator gave an answer that qex did not expect: {other:?}"),
    }
}

/// Gives the first 8 characters of an id, for the table output.
fn short_id(id: &uuid::Uuid) -> String {
    id.to_string()[..8].to_string()
}

/// Gives the form of a record that qex SHOWS.
///
/// This is the one boundary between a record and a reader. Everything that a
/// command prints — the table, the sentences and the JSON — comes from the
/// value that this gives, so a name reaches a reader in its safe form and it
/// reaches that reader once. See `job::safe_name`.
///
/// The record on the disk is not touched. `resolve_id` reads the stored name,
/// so a user who knows the name that they gave still finds the job.
fn for_display(mut s: JobStatus) -> JobStatus {
    s.name = safe_name(&s.name);
    s.group_name = s.group_name.as_deref().map(safe_name);
    // A dedupe key is text that another agent chose, and `qex status` puts it
    // in front of a reader. It is a LABEL and not a handle: no command finds a
    // job by its key, so the safe form loses nothing. Without this line, a key
    // that holds an ESC byte moves the cursor of the reader.
    s.dedupe_key = s.dedupe_key.as_deref().map(safe_name);
    // A lock name is a word from the command line, and `qex status` prints the
    // list of locks on its own line. `qex pause lock <name>` puts that same
    // word in front of a person as well. Measured with a lock named
    // `esc<ESC>[2Jlock`: `qex status` wrote the ESC byte to the terminal. A
    // lock name is a NAME, so it takes `safe_name`, in the same way as the
    // name of the job.
    s.locks = s.locks.iter().map(|l| safe_name(l)).collect();
    // The two SENTENCES take the rule for a sentence, and not `safe_name`.
    //
    // qex wrote these two, but it wrote them AROUND text that the caller chose.
    // `blocked_reason` names the lock that a job waits for, and a lock name is a
    // word from the command line; `error` on an expired job folds the last
    // `blocked_reason` into itself, so that word reaches the reader again on a
    // terminal line. Measured with a lock named `lk<ESC>[2Jbad`: the ESC byte
    // reached the terminal through both fields.
    //
    // `safe_name` would destroy a sentence — it keeps the letters, the numbers
    // and `-_.` only. `job::printable` takes the bytes that move a cursor and
    // nothing else.
    s.blocked_reason = s.blocked_reason.as_deref().map(crate::job::printable);
    s.error = s.error.as_deref().map(crate::job::printable);
    s
}

/// The same, for a list of records.
fn all_for_display(jobs: Vec<JobStatus>) -> Vec<JobStatus> {
    jobs.into_iter().map(for_display).collect()
}

/// Tells whether the text names this job.
///
/// The user can write the full id, or the start of the id, or the name. A short
/// id is easier to copy from the output of `qex list`.
///
/// The name has two accepted forms: the name that the user gave, AND the safe
/// name that qex shows. `qex list` and `qex status --json` give the safe form,
/// so a script that reads a name from qex and gives it back must find the job.
/// See `job::safe_name`.
fn names_job(job: &JobStatus, raw: &str) -> bool {
    job.id.to_string().starts_with(raw) || job.name == raw || safe_name(&job.name) == raw
}

/// Tells whether the text names the group of this job.
///
/// A group takes its id, the start of its id, or its name, in the same way as a
/// job. `qex list --group` already accepts these three forms.
fn names_group(job: &JobStatus, raw: &str) -> bool {
    job.group
        .map(|g| g.to_string().starts_with(raw))
        .unwrap_or(false)
        // The name that the user gave, AND the name that qex shows. `qex list
        // --json` gives the safe form, so a script that reads that value and
        // gives it back here must find the jobs. See `job::safe_name`.
        || job.group_name.as_deref() == Some(raw)
        || job.group_name.as_deref().map(safe_name).as_deref() == Some(raw)
}

/// Puts the jobs in the order of submission.
///
/// A pipeline then reads from the first stage to the last stage. Two stages can
/// start in the same second, so the sequence separates them.
fn in_submission_order(mut jobs: Vec<&JobStatus>) -> Vec<uuid::Uuid> {
    jobs.sort_by_key(|j| (j.submitted_at, j.sequence));
    jobs.iter().map(|j| j.id).collect()
}

/// What the text of the user named.
///
/// The caller needs more than the ids. A command that stops a job treats a stage
/// that already stopped as a fault when the user named that one job, and as
/// normal when the user named the whole pipeline. `qex status --json` also
/// chooses its shape from this, and not from the number of jobs: a pipeline of
/// one stage must still give an array.
#[derive(Debug)]
struct Targets {
    ids: Vec<uuid::Uuid>,
    /// The pipeline, when the text named one.
    group: Option<uuid::Uuid>,
}

impl Targets {
    fn one(id: uuid::Uuid) -> Self {
        Self {
            ids: vec![id],
            group: None,
        }
    }
}

/// Makes the result for a text that named a pipeline.
///
/// The jobs must belong to ONE pipeline. A pipeline takes its name from its
/// file, so a second run of the same file carries the same name, and `qex
/// pipeline ci.toml` twice gives two pipelines that the word `ci` both names.
/// Without this test `qex kill ci` stopped the work of two runs, and the user
/// named one. A short group id has the same fault, because two ids can start
/// with the same characters.
fn group_targets(by_group: &[&JobStatus], raw: &str) -> Result<Targets> {
    let mut groups: Vec<uuid::Uuid> = by_group.iter().filter_map(|j| j.group).collect();
    groups.sort();
    groups.dedup();

    if groups.len() > 1 {
        let lines: Vec<String> = groups
            .iter()
            .map(|g| {
                let count = by_group.iter().filter(|j| j.group == Some(*g)).count();
                format!("  {g}  {count} stage(s)")
            })
            .collect();
        bail!(
            "`{raw}` names {} pipelines. A pipeline takes its name from its file, so a \
             second run of that file has the same name. Give the group id of the run \
             that you want:\n{}",
            groups.len(),
            lines.join("\n")
        );
    }

    Ok(Targets {
        ids: in_submission_order(by_group.to_vec()),
        group: groups.first().copied(),
    })
}

/// Reads one or more job ids from the text that the user wrote.
///
/// A value that names a job gives that job. A value that names a pipeline gives
/// EVERY job of that pipeline, in the order of submission.
///
/// `qex pipeline` writes the group id to stdout, so that value is the handle
/// that a user keeps. Before this function, every command except `qex list
/// --group` refused it and gave "there is no job with the id ...", and the user
/// had to find the last stage by hand.
fn resolve_targets_in(jobs: &[JobStatus], raw: &str) -> Result<Targets> {
    if raw.is_empty() {
        bail!("give the id or the name of a job or a pipeline.");
    }

    let by_job: Vec<&JobStatus> = jobs.iter().filter(|j| names_job(j, raw)).collect();
    let by_group: Vec<&JobStatus> = jobs.iter().filter(|j| names_group(j, raw)).collect();

    // Test each name in the same way, including a full id.
    //
    // An earlier version gave back each value with the form of a UUID without
    // a test. A `--needs` value with one incorrect character was then accepted,
    // the dependency did not exist, and the job started immediately with no
    // warning. `qex logs` with such a value also wrote nothing and gave the
    // code 0, so a reader could not separate "this job wrote nothing" from
    // "this job does not exist".
    if let Ok(id) = raw.parse::<uuid::Uuid>() {
        if jobs.iter().any(|j| j.id == id) {
            return Ok(Targets::one(id));
        }
        if !by_group.is_empty() {
            return group_targets(&by_group, raw);
        }
        // Say whether qex ever saw this id. An agent must be able to tell "the
        // record was deleted, and the work happened" from "this job never
        // existed, so submit it".
        bail!("{}", crate::history::describe_missing(id));
    }

    // The two sets can hold the same one job, because a short id can be the
    // start of the id of the job AND of the id of its group. That is not an
    // ambiguity: both readings give the same job.
    let same = !by_job.is_empty()
        && by_job.len() == by_group.len()
        && by_job.iter().all(|j| by_group.iter().any(|g| g.id == j.id));

    if !by_job.is_empty() && !by_group.is_empty() && !same {
        let mut groups: Vec<uuid::Uuid> = by_group.iter().filter_map(|j| j.group).collect();
        groups.sort();
        groups.dedup();
        bail!(
            "`{raw}` is the name of a job and the name of a pipeline. Give the \
             full id of the one that you want.\n  job:      {}\n  pipeline: {}",
            by_job
                .iter()
                .map(|j| j.id.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            groups
                .iter()
                .map(|g| g.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    if !by_group.is_empty() && by_job.is_empty() {
        return group_targets(&by_group, raw);
    }

    match by_job.len() {
        1 => Ok(Targets::one(by_job[0].id)),
        0 => bail!("there is no job or pipeline with the id or the name `{raw}`"),
        n => bail!(
            "`{raw}` names {n} jobs. Give the id of the job that you want, or delete \
             the old jobs with `qex clean done` and start again.\n{}",
            by_job
                .iter()
                .map(|j| format!("  {} {}", j.id, j.display_name()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// Expands each value that the user wrote into the jobs that it names.
///
/// A value that names a pipeline gives every stage of it. The result holds each
/// job once, in the order that the user gave, because a user who names a
/// pipeline AND one of its stages wants that job waited for one time.
///
/// When no coordinator operates there is no job list, so every value goes
/// through unchanged and the caller reads the state directory. That directory
/// holds the jobs of a coordinator that retired, and it holds no group, so a
/// pipeline needs a coordinator.
///
/// An error from the resolver comes back to the caller. An earlier version kept
/// the value instead, and `qex wait` then reported "there is no job with the id
/// x" for a value that named a job AND a pipeline. The user read that the value
/// named nothing, and it named two things.
fn expand_ids(raws: &[String]) -> Result<Vec<String>> {
    let jobs = Client::connect_existing().and_then(|mut c| match c.call(&Request::List) {
        Ok(Response::Jobs { jobs }) => Some(jobs),
        _ => None,
    });
    let Some(jobs) = jobs else {
        return Ok(raws.to_vec());
    };

    let mut out: Vec<String> = Vec::new();
    for raw in raws {
        for id in resolve_targets_in(&jobs, raw)?.ids {
            let text = id.to_string();
            if !out.contains(&text) {
                out.push(text);
            }
        }
    }
    Ok(out)
}

/// Asks the coordinator for the jobs, and reads the text of the user.
fn resolve_targets(client: &mut Client, raw: &str) -> Result<Targets> {
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };
    resolve_targets_in(&jobs, raw)
}

/// Reads the text of the user, for a command that operates on ONE job.
///
/// `qex logs` reads one job. A pipeline gives an error that names the stages,
/// because qex must not choose a stage for the reader.
fn resolve_id(client: &mut Client, raw: &str) -> Result<uuid::Uuid> {
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };
    let found = resolve_targets_in(&jobs, raw)?;
    if found.group.is_none() && found.ids.len() == 1 {
        return Ok(found.ids[0]);
    }
    bail!(
        "`{raw}` is a pipeline of {} job(s), and this command takes one job. Name \
         the stage that you want:\n{}",
        found.ids.len(),
        found
            .ids
            .iter()
            .filter_map(|id| jobs.iter().find(|j| j.id == *id))
            // qex SHOWS the safe name only. See `job::safe_name`.
            .map(|j| format!("  {} {}", j.id, j.display_name()))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Finds a job id in the state directory, without a coordinator.
///
/// A job id must name a directory that exists. Without that test, a command
/// with a correct but unknown id waits for a status file that never arrives.
fn find_id_on_disk(raw: &str) -> Result<Option<uuid::Uuid>> {
    if let Ok(id) = raw.parse::<uuid::Uuid>() {
        return Ok(paths::job_dir(&id)?.is_dir().then_some(id));
    }

    let dir = paths::jobs_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };

    let mut found = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(raw) {
            if found.is_some() {
                bail!("`{raw}` names more than one job. Write more characters of the id.");
            }
            found = name.parse::<uuid::Uuid>().ok();
        }
    }
    Ok(found)
}

/// Writes the event stream.
///
/// # Why this command exists
///
/// An agent that drives twenty jobs asked about each job in a loop. It learned
/// of a result late, it used the machine to ask, and it wrote the timer that
/// qex exists to remove. This command gives one stream instead: the reader
/// connects one time, and it receives one line for each change.
pub fn events(args: cli::EventsArgs) -> Result<i32> {
    let since = parse_since(&args.since)?;
    let deadline = match &args.timeout {
        Some(t) => parse_duration(t)
            .map_err(|e| anyhow::anyhow!("--timeout: {e}"))?
            .map(|d| Instant::now() + d),
        None => None,
    };

    let mut client = Client::connect()?;
    warn_if_version_differs(&mut client);

    // Refuse a coordinator that cannot obey, BEFORE the request.
    //
    // Such a coordinator answers the request with an error, so a wait with no
    // end is not possible. This test gives the words that name the remedy, in
    // place of the words of a parser.
    {
        let (have, version, pid) = coordinator_capabilities(&mut client);
        // A development build passes here and takes a warning instead, which
        // `warn_if_version_differs` already wrote. `check_command` below still
        // refuses a coordinator that does not know this request, by name.
        if let crate::capabilities::Floor::Below(message) =
            crate::capabilities::check_floor(&version, pid)
        {
            return Err(anyhow::anyhow!("{message}"));
        }
        crate::capabilities::check_command(
            &have,
            &version,
            pid,
            "events",
            "qex events",
            "That coordinator does not know this request, so the command would give you \
             nothing and no reason.",
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // Warn about a number with no stream name.
    //
    // The numbers of each coordinator start at 1, and a new coordinator makes
    // an event for each record that it reads. A number alone thus points at a
    // place in a stream that means something different, and the coordinator
    // cannot see that. It gives no gap line, and the reader loses events with
    // no message. The form with the name removes that condition, so name it
    // here and give the remedy.
    if let crate::events::Cursor::After { stream: None, .. } = since {
        eprintln!(
            "qex: you gave a number with no stream name. qex cannot then see that the \
             coordinator restarted, and you can lose events with no message.\n\
             Keep the `stream_id` of the first line of the stream, and use \
             `--since <stream_id>:<seq>`."
        );
    }

    client.send(&Request::Events { since })?;

    let mut written = 0u64;
    let out = std::io::stdout();
    loop {
        // Give the socket the time that is left. A stream with no time limit
        // blocks here, and it uses no CPU time.
        if let Some(end) = deadline {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(EXIT_TIMEOUT);
            }
            client.set_read_timeout(Some(left))?;
        }

        let response = match client.recv_opt() {
            Ok(Some(r)) => r,
            Ok(None) => {
                // The stream ended and gave no goodbye. Something stopped the
                // coordinator: a signal, or a failure of the machine. Say so. A
                // reader that takes this for an orderly end waits for a change
                // that will never arrive.
                eprintln!(
                    "qex: the coordinator closed the event stream and gave no reason. \
                     Something stopped it.\n\
                     The records of the jobs are on the disk, and they are correct. \
                     Run `qex events` again to read the stream of the next coordinator."
                );
                return Ok(1);
            }
            Err(e) if is_read_timeout(&e) => return Ok(EXIT_TIMEOUT),
            Err(e) => return Err(e),
        };

        let Response::Event { event } = response else {
            return report(response);
        };

        use crate::events::Event;
        let counts = matches!(*event, Event::Job { .. } | Event::Gap { .. });
        let event = event_for_display(*event);

        {
            // Write the line and send it now. A line that stays in a buffer is
            // a line that did not arrive, and the reader then waits for a
            // change that already happened.
            use std::io::Write as _;
            let mut handle = out.lock();
            if args.json {
                writeln!(handle, "{}", serde_json::to_string(&event)?)?;
            } else {
                writeln!(handle, "{}", event_text(&event))?;
            }
            handle.flush()?;
        }

        if let Event::Bye { .. } = event {
            return Ok(0);
        }
        if counts {
            written += 1;
            if Some(written) == args.count {
                return Ok(0);
            }
        }
    }
}

/// Reads the value of `--since`.
///
/// The form `<stream>:<seq>` carries the name of the stream that gave the
/// number. The coordinator then compares the two, and it reports a gap when the
/// stream is not the same one. A number alone cannot give that comparison,
/// because the numbers of each coordinator start at 1.
fn parse_since(text: &str) -> Result<crate::events::Cursor> {
    use crate::events::Cursor;
    let value = text.trim();
    match value.to_ascii_lowercase().as_str() {
        "start" | "all" => return Ok(Cursor::Start),
        "now" | "live" => return Ok(Cursor::Now),
        _ => {}
    }

    let (stream, number) = match value.rsplit_once(':') {
        Some((name, number)) => {
            let id = name.trim().parse::<uuid::Uuid>().map_err(|_| {
                anyhow::anyhow!(
                    "--since: `{name}` is not a stream name.\n\
                     qex cannot then compare your number with this stream, and you would \
                     lose events with no message.\n\
                     Give the `stream_id` of the first line of the stream, as \
                     `--since <stream_id>:<seq>`."
                )
            })?;
            (Some(id), number)
        }
        None => (None, value),
    };

    match number.trim().parse::<u64>() {
        Ok(seq) => Ok(Cursor::After { seq, stream }),
        Err(_) => bail!(
            "--since: qex cannot read `{text}`.\n\
             The stream would then start at a place that you did not choose, and you \
             would lose events or read events a second time.\n\
             Use `start`, `now`, the `seq` number of the last event that you read, or \
             `<stream_id>:<seq>`, which qex compares with this stream."
        ),
    }
}

/// Gives one line of text for one event, for a person to read.
/// Gives the form of one event that qex SHOWS.
///
/// The stream carries the name that the user gave, and a name can hold an ESC
/// byte. `qex events` writes that name to a terminal, so the name must reach
/// the reader in its safe form, exactly as `qex status` and `qex list` do. See
/// `for_display`, which is the same boundary for a record, and `job::safe_name`
/// for the rule.
///
/// The JSON form takes the same treatment. `serde_json` already escapes a
/// control byte, so the JSON is safe to parse either way; the reason to do it
/// here is that one line of the stream and one line of `qex status --json` must
/// hold the SAME name. A reader that compares the two would otherwise see two
/// names for one job.
///
/// The coordinator keeps the name that the user gave, so `qex wait <name>` and
/// `qex kill <name>` still find the job.
fn event_for_display(event: crate::events::Event) -> crate::events::Event {
    use crate::events::Event;
    match event {
        Event::Job {
            seq,
            time,
            id,
            name,
            state,
            previous,
            change,
            job,
        } => Event::Job {
            seq,
            time,
            id,
            name: safe_name(&name),
            state,
            previous,
            change,
            job: Box::new(for_display(*job)),
        },
        other => other,
    }
}

fn event_text(event: &crate::events::Event) -> String {
    use crate::events::{Change, Event};
    match event {
        Event::Stream {
            time,
            version,
            pid,
            stream_id,
            first_seq,
            last_seq,
            ..
        } => format!(
            "{}  the stream {stream_id} comes from the coordinator pid {pid}, version \
             {version}; it holds the events {first_seq} to {last_seq}",
            crate::sys::clock_text(*time)
        ),
        Event::Job {
            seq,
            time,
            id,
            name,
            state,
            change,
            job,
            ..
        } => {
            let note = match change {
                Change::Reason => job.blocked_reason.clone().unwrap_or_default(),
                Change::State => match (job.exit_code, &job.error) {
                    (_, Some(text)) => text.clone(),
                    (Some(code), _) if code != 0 => format!("the exit code is {code}"),
                    _ => String::new(),
                },
            };
            format!(
                "{}  {seq:>6}  {}  {:<10}  {name}{}",
                crate::sys::clock_text(*time),
                &id.to_string()[..8],
                state.as_str(),
                if note.is_empty() {
                    String::new()
                } else {
                    format!("  {note}")
                }
            )
        }
        Event::Gap {
            time,
            missed,
            reason,
            ..
        } => format!(
            "{}  GAP: {}. {reason}",
            crate::sys::clock_text(*time),
            match missed {
                Some(n) => format!("qex lost {n} event(s)"),
                None => "qex cannot count the events that you lost".to_string(),
            }
        ),
        Event::Bye { time, reason } => {
            format!(
                "{}  the stream ends. {reason}",
                crate::sys::clock_text(*time)
            )
        }
    }
}

/// Gives the one line that says if the queue is healthy.
///
/// A reader answers one question with this line: does the queue move, and if it
/// does not, what holds it? Without the line, a reader must open the reason of
/// each job and calculate the answer.
///
/// The queue is healthy when a job started recently, OR when the line names a
/// cause outside this queue: another user or the machine. The queue is stuck
/// when no job started and the cause is a job of this queue.
///
/// `qex info` and `qex top` both use this function. Two texts for one fact
/// would say two different things after the first change to one of them.
pub fn queue_line(info: &Response) -> String {
    let Response::Info {
        jobs_running,
        jobs_queued,
        queue_state,
        health,
        ..
    } = info
    else {
        return String::new();
    };

    // `queue_state` and `health` are the marks of a coordinator that reports
    // the health. An older coordinator sends neither, and `unknown` is the true
    // answer. A defaulted value would say "the queue is running", which is a
    // statement that qex did not measure.
    let (Some(state), Some(health)) = (queue_state, health) else {
        return "queue: unknown · this coordinator is too old to report the health of the queue"
            .to_string();
    };
    let last_start_at = &health.last_start_at;
    let head_job = &health.head_job;
    let head_passed_by = &health.head_passed_by;

    // A PAUSE HAS ITS OWN LINE, and this function gives none.
    //
    // `qex info` and `qex top` each write the pause with `pause::queue_line`,
    // which names the person, the reason and the end of the pause. A second
    // line here would say the same fact with fewer of those values, and the
    // reader would then have to decide which of the two to trust.
    if state == "paused" || state == "paused-by-fault" {
        return String::new();
    }

    let now = crate::sys::now_secs();
    let age = last_start_at.map(|t| format_duration(Duration::from_secs(now.saturating_sub(t))));
    let started = match (&age, state.as_str()) {
        (Some(a), "running") => format!("last start {a} ago"),
        (Some(a), _) => format!("no job started for {a}"),
        (None, _) => "no job started yet".to_string(),
    };
    let front = match head_job {
        Some(j) => format!(" · the job at the front is {j}"),
        None => String::new(),
    };
    let counts = format!("{jobs_running} running, {jobs_queued} queued");

    match state.as_str() {
        "running" => format!("queue: running · {started} · {counts}"),
        "held" => format!(
            "queue: held for the job {} · {} job(s) started before it · {started} · {counts}",
            head_job.clone().unwrap_or_else(|| "unknown".into()),
            head_passed_by
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".into()),
        ),
        "waits-for-peer" => format!(
            "queue: waits for another user · {started} · {} {} cores and {}{front}",
            if health.peer_count == 1 {
                "1 other user holds".to_string()
            } else {
                format!("{} other users hold", health.peer_count)
            },
            health.peer_cpu,
            format_size(health.peer_mem),
        ),
        "waits-for-machine" => format!(
            "queue: waits for the machine · {started}{front} · the memory belongs to a program \
             outside this queue"
        ),
        "waits-for-capacity" => {
            format!("queue: waits for the capacity of this queue · {started} · {counts}{front}")
        }
        "waits-for-idle" => format!(
            "queue: waits for a quiet machine · {started}{front} · that job is larger than the \
             budget"
        ),
        "parked" => format!(
            "queue: the job at the front is larger than the budget and the config keeps it in the \
             queue · {started}{front} · qex starts the jobs behind it"
        ),
        other => format!("queue: {other} · {started} · {counts}{front}"),
    }
}

/// Writes the state of the coordinator.
///
/// The process id here comes from the coordinator itself. Use this command to
/// find the coordinator. Do not search the process list with `pgrep -f qex`,
/// because that pattern also matches the command that contains it.
pub fn info(args: cli::InfoArgs) -> Result<i32> {
    let mut client = if args.no_start {
        match Client::connect_existing() {
            Some(c) => c,
            None => {
                if args.json {
                    println!("{}", serde_json::json!({ "running": false }));
                } else {
                    println!("no coordinator operates");
                }
                return Ok(1);
            }
        }
    } else {
        Client::connect()?
    };
    let response = client.call(&Request::Info)?;
    let line = queue_line(&response);
    match response {
        Response::Info {
            pid,
            version,
            started_at,
            program_replaced,
            jobs_running,
            jobs_queued,
            cpu_budget,
            mem_budget,
            config_error,
            cpu_claimed,
            mem_claimed,
            queue_state,
            paused_at,
            paused_by_pid,
            paused_reason,
            paused_until,
            paused_locks,
            health,
            pools,
        } => {
            let now = crate::sys::now_secs();
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "pid": pid,
                        "queue_state": queue_state,
                        "paused_at": paused_at,
                        "paused_by_pid": paused_by_pid,
                        "paused_reason": paused_reason,
                        "paused_until": paused_until,
                        "paused_locks": paused_locks,
                        "version": version,
                        "started_at": started_at,
                        "program_replaced": program_replaced,
                        "cli_version": crate::version::VERSION,
                        "jobs_running": jobs_running,
                        "jobs_queued": jobs_queued,
                        "cpu_budget": cpu_budget,
                        "mem_budget": mem_budget,
                        "cpu_claimed": cpu_claimed,
                        "mem_claimed": mem_claimed,
                        "config_error": config_error,
                        // Each value below is null when the coordinator is too
                        // old to measure it. A null says "unknown". It does not
                        // say "zero", and a reader must not read it as zero.
                        "queue_line": line,
                        "last_start_at": health.as_ref().and_then(|h| h.last_start_at),
                        "peer_count": health.as_ref().map(|h| h.peer_count),
                        "peer_cpu": health.as_ref().map(|h| h.peer_cpu),
                        "peer_mem": health.as_ref().map(|h| h.peer_mem),
                        "head_job": health.as_ref().and_then(|h| h.head_job.clone()),
                        "head_blocker": health.as_ref().and_then(|h| h.head_blocker.clone()),
                        "head_passed_by": health.as_ref().and_then(|h| h.head_passed_by),
                        // `null` says that this coordinator cannot answer. It
                        // does not say that the machine has no pool.
                        "pools": pools,
                    }))?
                );
                return Ok(0);
            }
            // Say this first. Every number below comes from the values that
            // the coordinator holds, and those are no longer the values in the
            // file.
            if let Some(fault) = &config_error {
                // Put the `qex:` prefix on EVERY line. A TOML parse error is
                // three lines and a caret, and a prefix on the first line only
                // makes the other lines look like output of the command.
                let fault: String = fault
                    .lines()
                    .map(|l| format!("qex:   {l}\n"))
                    .collect::<Vec<_>>()
                    .concat();
                eprint!(
                    "qex: WARNING: the configuration file changed, and qex cannot read it:\n\
                     {fault}\
                     qex:   The coordinator keeps the values that it had, and they are the \
                     values below.\n\
                     qex:   Correct the file. The coordinator reads it again by itself. Run \
                     `qex config show` for the full message.\n"
                );
            }
            println!("coordinator pid: {pid}");
            println!(
                "version:         {version} (this command: {})",
                crate::version::VERSION
            );
            if program_replaced {
                // Say this clearly. A user that replaces the program during
                // development would otherwise meet a message with no cause.
                println!(
                    "program:        REPLACED. This coordinator holds the code of an \
                     earlier build. It stops when no job operates, and the next command \
                     starts a coordinator with the new program."
                );
            }
            println!("jobs running:    {jobs_running}");
            println!("jobs queued:     {jobs_queued}");
            println!("cores:           {cpu_claimed} of {cpu_budget} in use",);
            println!(
                "memory:          {} of {} in use",
                format_size(mem_claimed),
                format_size(mem_budget)
            );

            // Say what the other users hold. The measurement is the one that
            // the scheduler used, so this answer and the reason of a job that
            // waits name the same numbers.
            match &health {
                Some(h) if h.peer_cpu > 0 || h.peer_mem > 0 => println!(
                    "other users:     {} coordinator(s) with {} cores and {}",
                    h.peer_count,
                    h.peer_cpu,
                    format_size(h.peer_mem)
                ),
                Some(_) => println!("other users:     none"),
                None => println!("other users:     unknown"),
            }

            // Say what the queue does. A queue that does nothing and does not
            // say why is the fault that this tool exists to remove.
            //
            // An earlier coordinator gives no value here. Write `unknown`, and
            // do not write `running`: a guess in this place is a lie, and this
            // is the one place where the honest answer matters most.
            let state_line = match (queue_state.as_deref(), paused_at) {
                (Some(state @ ("paused" | "paused-by-fault")), Some(at)) => {
                    let record = crate::pause::PauseRecord {
                        paused_at: at,
                        // The pid of the PAUSER, and never the pid of the
                        // coordinator. `0` says "this coordinator does not
                        // report it", and `pause::who` prints that as words.
                        by_pid: paused_by_pid.unwrap_or(0),
                        reason: paused_reason,
                        until: paused_until,
                        fault: state == "paused-by-fault",
                    };
                    format!(
                        "{} · other users are not paused",
                        crate::pause::queue_line(&record, now)
                    )
                }
                // Not a pause. `queue_line` gives the full sentence: what holds
                // the queue, and when a job last started. It writes its own
                // `queue: ` prefix, so this arm removes it.
                (Some(state), _) => line
                    .strip_prefix("queue: ")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| state.to_string()),
                (None, _) => "unknown; this coordinator does not report it".to_string(),
            };
            println!("queue:           {state_line}");
            for lock in paused_locks.unwrap_or_default() {
                println!(
                    "                 {}",
                    crate::pause::lock_line(&lock.name, &lock.record, lock.held_by.as_deref(), now)
                );
            }
            match pools {
                // Say `unknown`, and never `none`. An earlier coordinator
                // cannot answer this question, and "this machine has no pool"
                // is a different statement.
                None => println!(
                    "pools:           unknown; this coordinator is version {version} and it \
                     does not report the pools"
                ),
                Some(list) if list.is_empty() => {}
                Some(list) => {
                    for p in list {
                        let peers = if p.peer_used > 0 {
                            format!(", {} held by other users", p.peer_used)
                        } else {
                            String::new()
                        };
                        println!(
                            "pool {:<11} {} of {} in use{peers}",
                            format!("{}:", p.name),
                            p.used,
                            p.total
                        );
                        for d in &p.devices {
                            let who = if d.peer {
                                "held by another user".to_string()
                            } else {
                                format!(
                                    "{} of {} in use",
                                    format_size(d.used),
                                    format_size(d.capacity)
                                )
                            };
                            println!("  {} {}: {who}", p.name, d.index);
                        }
                    }
                }
            }
            Ok(0)
        }
        other => report(other),
    }
}

/// Writes a loud line when a person paused the queue or a lock.
///
/// Every command that lists jobs calls this function. A pause with no end is
/// the pause that a person forgets, and an empty queue in the morning is the
/// result, so qex says it every time and not one time.
fn warn_if_paused(client: &mut Client) {
    let Ok(Response::PauseState { queue, locks }) = client.call(&Request::PauseState) else {
        // An earlier coordinator cannot answer this request, and it cannot
        // pause either, so there is nothing to report.
        return;
    };
    let now = crate::sys::now_secs();
    if let Some(record) = &queue {
        eprintln!(
            "qex: THE QUEUE IS PAUSED, so qex starts no job. {}",
            crate::pause::queue_line(record, now)
        );
    }
    for lock in &locks {
        eprintln!(
            "qex: {}",
            crate::pause::lock_line(&lock.name, &lock.record, lock.held_by.as_deref(), now)
        );
    }
}

/// Changes `--for 30m` into the moment when the pause ends.
///
/// `--for 0` is an error. `parse_duration` gives "no limit" for zero, which is
/// correct for `--timeout` and is the opposite of what this option asks for: a
/// person who writes `--for 0` wants a short pause, and would get a pause with
/// no end.
fn end_of_pause(duration: Option<&str>) -> Result<Option<u64>> {
    let Some(text) = duration else {
        return Ok(None);
    };
    match crate::units::parse_duration(text).map_err(|e| anyhow::anyhow!("--for: {e}"))? {
        Some(d) => Ok(Some(crate::sys::now_secs() + d.as_secs())),
        None => bail!(
            "--for: give a time that is longer than zero, such as `30m`.\n\
             To end a pause now, run `qex resume queue`."
        ),
    }
}

/// Refuses a command that the coordinator cannot obey.
///
/// A development build passes the floor test and takes a warning instead, which
/// `warn_if_version_differs` already wrote. `check_command` below still refuses
/// a coordinator that does not know this request, by name.
fn require_command(client: &mut Client, name: &str, command: &str, danger: &str) -> Result<()> {
    let (have, version, pid) = coordinator_capabilities(client);
    if let crate::capabilities::Floor::Below(message) =
        crate::capabilities::check_floor(&version, pid)
    {
        return Err(anyhow::anyhow!("{message}"));
    }
    crate::capabilities::check_command(&have, &version, pid, name, command, danger)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Why a refused `qex pause` matters.
const PAUSE_DANGER: &str = "The coordinator would start the jobs of the queue, and you would \
     believe that the machine is quiet.";

/// Why a refused `qex resume` matters.
///
/// The danger of a refused resume is the opposite of the danger of a refused
/// pause. A message that gave the pause words here would state a reason that is
/// not true, and a reader who acts on it acts on a fault that does not exist.
const RESUME_DANGER: &str = "That coordinator does not read the pause record, so it already \
     starts the jobs of the queue. This command would change nothing.";

/// Writes what is paused now, as text or as JSON.
fn print_pause_state(
    queue: Option<&crate::pause::PauseRecord>,
    locks: &[crate::proto::LockPause],
    json: bool,
) -> Result<i32> {
    let now = crate::sys::now_secs();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "paused": queue.is_some(),
                "queue": queue,
                "locks": locks,
            }))?
        );
        return Ok(0);
    }

    match queue {
        Some(record) => println!(
            "queue: {} · other users are not paused",
            crate::pause::queue_line(record, now)
        ),
        None => println!("queue: running"),
    }
    for lock in locks {
        println!(
            "{}",
            crate::pause::lock_line(&lock.name, &lock.record, lock.held_by.as_deref(), now)
        );
    }
    if queue.is_none() && locks.is_empty() {
        println!("nothing is paused");
    }
    Ok(0)
}

/// Stops qex from starting work, or takes a lock for the person.
pub fn pause(args: cli::PauseArgs) -> Result<i32> {
    use crate::proto::PauseTarget;

    let Some(target) = args.target else {
        return pause_report(args.json);
    };

    match target {
        cli::PauseTarget::Queue {
            reason,
            duration,
            drain,
            json,
        } => {
            let until = end_of_pause(duration.as_deref())?;
            let mut client = Client::connect()?;
            require_command(&mut client, "pause", "qex pause", PAUSE_DANGER)?;

            let response = client.call(&Request::Pause {
                target: PauseTarget::Queue,
                reason,
                until,
                by_pid: std::process::id() as i32,
            })?;
            let Response::PauseState { queue, locks } = response else {
                return report(response);
            };

            let code = print_pause_state(queue.as_ref(), &locks, json || args.json)?;
            if drain {
                return drain_queue(&mut client);
            }
            // Say what a pause does NOT do. A person who asked for a quiet
            // machine must not believe that the jobs which operate stopped.
            if !(json || args.json) {
                let running = jobs_running(&mut client).unwrap_or(0);
                if running > 0 {
                    println!(
                        "{running} job(s) still operate. They continue, because each one already \
                         holds its capacity. Use `qex pause queue --drain` to wait for them, or \
                         `qex kill <id>` to stop one."
                    );
                }
            }
            Ok(code)
        }

        cli::PauseTarget::Lock {
            name,
            reason,
            duration,
            json,
        } => {
            let until = end_of_pause(duration.as_deref())?;
            let mut client = Client::connect()?;
            require_command(&mut client, "pause", "qex pause", PAUSE_DANGER)?;

            let response = client.call(&Request::Pause {
                target: PauseTarget::Lock { name: name.clone() },
                reason,
                until,
                by_pid: std::process::id() as i32,
            })?;
            let Response::PauseState { queue, locks } = response else {
                return report(response);
            };

            if json || args.json {
                return print_pause_state(queue.as_ref(), &locks, true);
            }

            // A lock name is text that the person typed, and these lines go
            // to a terminal. Show the safe form of the name. See
            // `job::safe_name`.
            let shown = crate::job::safe_name(&name);
            match locks.iter().find(|l| l.name == name) {
                Some(lock) => match &lock.held_by {
                    Some(job) => println!(
                        "the job {job} holds the lock `{shown}` now. qex gives it to you when \
                         that job stops, and no other job takes it. Use `qex list` to watch \
                         that job."
                    ),
                    None => println!(
                        "the lock `{shown}` is yours. Every job that needs it waits until you \
                         run `qex resume lock {shown}`."
                    ),
                },
                None => println!("the lock `{shown}` is yours"),
            }
            Ok(0)
        }
    }
}

/// Starts the queue again, or gives a lock back.
pub fn resume(args: cli::ResumeArgs) -> Result<i32> {
    use crate::proto::PauseTarget;

    // `qex resume` with no word starts the queue. That is the usual need, and
    // it is the command that every pause message names.
    let (target, json) = match args.target {
        None => (PauseTarget::Queue, args.json),
        Some(cli::ResumeTarget::Queue { json }) => (PauseTarget::Queue, json || args.json),
        Some(cli::ResumeTarget::Lock { name, json }) => {
            (PauseTarget::Lock { name }, json || args.json)
        }
    };
    let words = match &target {
        PauseTarget::Queue => "the queue operates again".to_string(),
        PauseTarget::Lock { name } => format!(
            "the lock `{}` is free. The next job that needs it takes it.",
            crate::job::safe_name(name)
        ),
    };

    let mut client = Client::connect()?;
    require_command(&mut client, "pause", "qex resume", RESUME_DANGER)?;

    let response = client.call(&Request::Resume { target })?;
    let Response::PauseState { queue, locks } = response else {
        return report(response);
    };

    if json {
        return print_pause_state(queue.as_ref(), &locks, true);
    }
    println!("{words}");
    Ok(0)
}

/// Writes what is paused now, with no change.
///
/// This command does not start a coordinator. A command that asks a question
/// must not make the thing that it asks about. When no coordinator operates,
/// the file on the disk holds the answer.
fn pause_report(json: bool) -> Result<i32> {
    if let Some(mut client) = Client::connect_existing() {
        let response = client.call(&Request::PauseState)?;
        if let Response::PauseState { queue, locks } = response {
            return print_pause_state(queue.as_ref(), &locks, json);
        }
        // An earlier coordinator cannot answer. It also cannot pause, so the
        // file is the whole truth.
    }

    // End a pause that reached the time of `--for`.
    //
    // The scheduler does this step, and no scheduler operates now. Without it
    // this command reports a pause that the next command ends at once — a
    // report that lies in the dangerous direction, because the reader believes
    // that the machine stays quiet.
    let mut paused = crate::pause::Paused::read();
    paused.expire(crate::sys::now_secs());
    let jobs = crate::job::read_all_from_disk();
    let locks: Vec<crate::proto::LockPause> = paused
        .locks
        .iter()
        .map(|(name, record)| crate::proto::LockPause {
            name: name.clone(),
            record: record.clone(),
            held_by: jobs
                .iter()
                .find(|j| j.state.is_active() && j.locks.iter().any(|l| l == name))
                .map(|j| format!("{} ({})", short_id(&j.id), j.name)),
        })
        .collect();
    print_pause_state(paused.queue.as_ref(), &locks, json)
}

/// Gives the number of jobs that operate now.
fn jobs_running(client: &mut Client) -> Option<usize> {
    match client.call(&Request::Info) {
        Ok(Response::Info { jobs_running, .. }) => Some(jobs_running),
        _ => None,
    }
}

/// Waits until no job of this queue operates.
///
/// A person who takes the machine back asks two things: start nothing new, and
/// tell me when it is quiet. This is the second one.
fn drain_queue(client: &mut Client) -> Result<i32> {
    loop {
        let Some(running) = jobs_running(client) else {
            bail!("the coordinator did not give the number of jobs that operate");
        };
        if running == 0 {
            println!("the machine is quiet: no job of this queue operates");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Submits every stage of a pipeline file with one command.
///
/// The names in the file belong to that file and to this submission. qex reads
/// each one and changes it into the id that it made a moment before, so no name
/// leaves the file. A second run of the same file makes new jobs with new ids,
/// and the two runs never meet. That is the fault that a dependency by name has
/// on the command line.
pub fn pipeline(args: cli::PipelineArgs) -> Result<i32> {
    let cfg = Config::load()?;
    cfg.validate()?;

    let file = crate::pipeline::PipelineFile::load(&args.file)?;
    let order = file.order()?;

    let group = uuid::Uuid::new_v4();
    let group_name = args
        .name
        .clone()
        .or_else(|| file.name.clone())
        .unwrap_or_else(|| {
            args.file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("pipeline")
                .to_string()
        });

    let mut client = Client::connect()?;
    warn_if_version_differs(&mut client);

    // Test the coordinator once, before the first job. A pipeline that stops
    // in the middle leaves jobs with no end.
    {
        let mut probe = crate::pipeline::stage_spec(&file.jobs[0], &cfg, group, &group_name)?;
        probe.group = Some(group);
        // A pipeline always needs the groups and the dependencies.
        probe.needs.push(uuid::Uuid::new_v4());
        require_capabilities(&mut client, &probe)?;
    }

    // The id of each stage that qex already submitted, by its name in the file.
    let mut ids: std::collections::BTreeMap<String, uuid::Uuid> = Default::default();
    let mut submitted: Vec<(String, uuid::Uuid)> = Vec::new();

    for position in order {
        let stage = &file.jobs[position];
        let mut spec = crate::pipeline::stage_spec(stage, &cfg, group, &group_name)?;

        // Change each name of this file into the id of the stage that qex just
        // made. The order puts every stage after the stages that it waits for,
        // so each id is ready.
        for name in &stage.needs {
            match ids.get(name) {
                Some(id) => spec.needs.push(*id),
                None => bail!(
                    "the stage `{}` waits for `{name}`, which qex did not submit",
                    stage.name
                ),
            }
        }
        for name in &stage.after {
            match ids.get(name) {
                Some(id) => spec.after.push(*id),
                None => bail!(
                    "the stage `{}` waits for `{name}`, which qex did not submit",
                    stage.name
                ),
            }
        }

        let id = spec.id;
        match client.call(&Request::Submit {
            spec: Box::new(spec),
        })? {
            Response::Submitted {
                id: given, warning, ..
            } => {
                if let Some(text) = warning {
                    eprintln!("qex: {}: {text}", stage.name);
                }
                ids.insert(stage.name.clone(), given);
                submitted.push((stage.name.clone(), given));
            }
            other => {
                eprintln!(
                    "qex: the stage `{}` was refused. The stages before it are in the queue; \
                     use `qex cancel --group {group}` to remove them.",
                    stage.name
                );
                let _ = id;
                return report(other);
            }
        }
    }

    if let Some(path) = &args.id_file {
        let text = pipeline_id_file(path, group, &group_name, &submitted)?;
        write_id_file(path, &text)?;
    }

    if args.json {
        let jobs: Vec<serde_json::Value> = submitted
            .iter()
            .map(|(name, id)| serde_json::json!({ "name": name, "id": id.to_string() }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "group": group.to_string(),
                // The SAFE form, so that ONE field name carries ONE value.
                // `qex list --json` gives this same value, and `qex list
                // --group` takes it. See `job::safe_name`.
                "group_name": crate::job::safe_name(&group_name),
                "jobs": jobs,
            }))?
        );
    } else {
        // The group id goes on stdout alone, so `GROUP=$(qex pipeline f.toml)`
        // operates in the same way as `ID=$(qex submit ...)`.
        for (name, id) in &submitted {
            // The SAFE name: this line goes to a terminal. The JSON above and
            // the id file keep the name of the stage as the file gives it,
            // because a machine reads that name as a KEY.
            eprintln!("{}: {id}", crate::job::safe_name(name));
        }
        println!("{group}");
    }
    Ok(0)
}

/// Writes the version of this command, and of the coordinator when one operates.
///
/// A user reads `qex version` and `qex --version`. The two must agree, and the
/// long form also gives the version of the coordinator, because a coordinator
/// that holds an earlier build behaves differently.
pub fn version(args: cli::VersionArgs) -> Result<i32> {
    let mine = crate::version::VERSION;

    // Do not start a coordinator. A question about a version must not change
    // the machine.
    let coordinator = Client::connect_existing().and_then(|mut c| match c.call(&Request::Info) {
        Ok(Response::Info {
            version,
            pid,
            program_replaced,
            ..
        }) => Some((version, pid, program_replaced)),
        _ => None,
    });

    if args.json {
        let value = match &coordinator {
            Some((version, pid, replaced)) => serde_json::json!({
                "version": mine,
                "coordinator": {
                    "running": true,
                    "version": version,
                    "pid": pid,
                    "program_replaced": replaced,
                    "matches": version == mine,
                }
            }),
            None => serde_json::json!({
                "version": mine,
                "coordinator": { "running": false }
            }),
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(0);
    }

    println!("qex {mine}");
    println!("can do:      {}", crate::capabilities::ALL.join(", "));
    match coordinator {
        None => println!("coordinator: none operates"),
        Some((version, pid, replaced)) => {
            // Say what the coordinator can do, and name anything that this
            // build can do and the coordinator cannot.
            if let Some(mut c) = Client::connect_existing() {
                let (have, _, _) = coordinator_capabilities(&mut c);
                let missing: Vec<&&str> = crate::capabilities::ALL
                    .iter()
                    .filter(|name| !have.iter().any(|h| h == *name))
                    .collect();
                if !missing.is_empty() {
                    println!(
                        "cannot do:   {} (this coordinator is older)",
                        missing.iter().map(|s| **s).collect::<Vec<_>>().join(", ")
                    );
                }
            }
            if version == mine {
                println!("coordinator: {version} (pid {pid})");
            } else {
                println!("coordinator: {version} (pid {pid})");
                println!(
                    "WARNING: the coordinator holds a different version. It stops when no \
                     job operates, and the next command starts one with this version. \
                     Stop it now with `kill {pid}` if you need this version immediately."
                );
            }
            if replaced {
                println!("the qex program changed after this coordinator started");
            }
        }
    }
    Ok(0)
}

/// Writes the candidates that a shell offers after TAB.
///
/// # Three rules
///
/// 1. THIS COMMAND NEVER STARTS A COORDINATOR. It reads the records on the
///    disk. A press of TAB must not start a process, and a user who presses TAB
///    in a directory with no work must not leave a coordinator behind.
/// 2. It never fails. A completion that writes an error puts that error in the
///    line that the user is typing. An empty answer is the correct answer when
///    something is wrong.
/// 3. It gives the SAFE form of each name. See `job::safe_name`.
pub fn complete(args: cli::CompleteArgs) -> Result<i32> {
    let jobs = crate::job::read_all_from_disk();

    let wanted: Vec<&crate::job::JobStatus> = match args.what.as_str() {
        // `qex kill` takes a job that operates, and nothing else.
        "active" => jobs.iter().filter(|j| j.state.is_active()).collect(),
        // `qex cancel` takes a job that waits in the queue.
        "queued" => jobs
            .iter()
            .filter(|j| j.state == crate::job::JobState::Queued)
            .collect(),
        _ => jobs.iter().collect(),
    };

    // The newest first. A user completes the work of this hour far more often
    // than the work of last week, and a shell shows the first candidates.
    let mut out: Vec<&crate::job::JobStatus> = wanted;
    out.reverse();

    // The id AND the name, for each set. A person types a name far more often
    // than a uuid, and qex accepts either in the same place.
    //
    // A name repeats over time, because two runs of one command can share it.
    // qex answers that with an error that lists the jobs, so an ambiguous name
    // costs a second command and never the wrong job.
    let mut seen = std::collections::BTreeSet::new();
    for job in out {
        println!("{}", job.id);
        // The SAFE FORM of the name, and never the name itself. See
        // `safe_name`.
        let name = safe_name(&job.name);
        if !name.is_empty() && seen.insert(name.clone()) {
            println!("{name}");
        }
    }
    Ok(0)
}

/// Writes the id of a job to a file.
///
/// A shell variable does not last between the commands of an agent, and an
/// agent that loses an id must search for it. A file holds the id.
fn write_id_file(path: &std::path::Path, text: &str) -> Result<()> {
    warn_if_temporary(path);
    // A job id gives no access to anything, so this file uses the usual mode.
    crate::job::write_atomic(path, text.as_bytes(), 0o644)
        .with_context(|| format!("writing the id file {}", path.display()))
}

/// Gives a warning for an id file in a directory that does not last.
///
/// # The trap
///
/// The id is the handle to the job. The job continues when the session of the
/// agent stops, WHICH IS THE PROPERTY THAT MAKES THE ID FILE VALUABLE. An agent
/// that writes the id into its scratch directory therefore loses the handle at
/// the exact moment that it needs the handle: the harness deletes that
/// directory with the session, and the job continues with no name.
///
/// The file operates correctly, and the fault appears in a later session only.
/// A warning at this moment is thus the one opportunity to prevent it.
fn warn_if_temporary(path: &std::path::Path) {
    let full = std::fs::canonicalize(path.parent().unwrap_or(std::path::Path::new(".")))
        .unwrap_or_else(|_| path.to_path_buf());
    let text = full.to_string_lossy().to_string();

    // The directories that a machine or a harness empties. `TMPDIR` covers
    // macOS, where the value is a directory of the user under `/var/folders`.
    //
    // Each name goes through `canonicalize` as well, because the path of the id
    // file went through it. macOS makes this necessary: `TMPDIR` there is
    // `/var/folders/...`, and `/var` is a link to `/private/var`, so the two
    // paths never agree as text.
    let mut roots: Vec<String> = Vec::new();
    let mut add = |value: &str| {
        let path = std::path::Path::new(value);
        if let Ok(real) = std::fs::canonicalize(path) {
            roots.push(real.to_string_lossy().trim_end_matches('/').to_string());
        }
        roots.push(value.trim_end_matches('/').to_string());
    };
    add("/tmp");
    add("/var/tmp");
    for name in ["TMPDIR", "CLAUDE_JOB_DIR", "XDG_RUNTIME_DIR"] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                add(&value);
            }
        }
    }

    let inside = roots
        .iter()
        .any(|r| text == *r || text.starts_with(&format!("{r}/")));
    // A directory with this name belongs to a harness, whatever its position.
    let scratch = full
        .components()
        .any(|c| matches!(c.as_os_str().to_str(), Some("scratchpad") | Some("scratch")));

    if !(inside || scratch) {
        return;
    }

    eprintln!(
        "qex: WARNING: the id file {} is in a directory that does not last.\n\
         qex:   The job continues when your session stops, but this file goes with the\n\
         qex:   session, and you then have no handle for a job that still operates.\n\
         qex:   Put the id file in your project or in your home directory instead.\n\
         qex:   `qex list` finds a job again when the id is lost.",
        path.display()
    );
}

/// Makes the contents of the id file of a pipeline.
///
/// A name that ends in `.json` gives a JSON object, because an agent reads JSON
/// with a parser. Every other name gives `name=id` lines, which a shell reads
/// with `.` or `source`.
fn pipeline_id_file(
    path: &std::path::Path,
    group: uuid::Uuid,
    group_name: &str,
    jobs: &[(String, uuid::Uuid)],
) -> Result<String> {
    let json = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        .unwrap_or(false);

    if json {
        let stages: serde_json::Map<String, serde_json::Value> = jobs
            .iter()
            .map(|(name, id)| (name.clone(), serde_json::Value::from(id.to_string())))
            .collect();
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "group": group.to_string(),
            // The SAFE form. See the note in `pipeline`.
            "group_name": crate::job::safe_name(group_name),
            "jobs": stages,
        }))?);
    }

    let mut text = format!("group={group}\n");
    for (name, id) in jobs {
        // A shell reads a name with a dash as a command, so change those
        // characters. The original name stays in the JSON form.
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        text.push_str(&format!("{safe}={id}\n"));
    }
    Ok(text)
}

/// True when the user pressed Ctrl-C during `qex run`.
static RUN_INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_interrupt(_signal: libc::c_int) {
    // A signal handler may use an atomic store and very little else.
    RUN_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Catches SIGINT and SIGTERM while a command streams the output of a job.
///
/// `qex run` stops its own job with them. `qex status --follow` did not start
/// the job, so it stops its wait only, and `stream_until_done` knows the
/// difference.
fn catch_run_signals() {
    // `qex run` keeps its handler after the first signal, because a second
    // Ctrl-C must stop the job again when the first stop did not succeed. It
    // thus takes no SA_RESETHAND. `sigaction` gives the same rules on every
    // system that qex supports.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_interrupt as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }
}

/// Runs a command through the queue and waits for it here.
///
/// This command exists for one reason: an agent uses the tools that it already
/// knows. `qex run` goes before an existing command, and the output and the
/// exit code are the same as before. The command takes a place in the queue, so
/// it waits when the machine is busy, and it holds a claim while it operates.
///
/// The exit code is the exit code of the job ONLY when the job ran. A job that
/// something stopped gave no exit code of its own, and this command then gives
/// the code of the state, which is the code of `qex wait`. See `exit_code_for`.
///
/// A job of `qex run` is a job like any other. It has a record, a log file and
/// an id.
///
/// This command stops the job when it receives SIGINT (Ctrl-C) or SIGTERM,
/// because a user expects Ctrl-C to stop the work. It stops the job with a
/// `Kill` request to the coordinator, or with a `Cancel` request when the job
/// still waits in the queue, and NOT with a signal: the supervisor gives the
/// job its own process group, inside the session of the supervisor, so a signal
/// to the process group of this command never reaches the job.
///
/// A hangup, such as a terminal that closes, and a SIGKILL therefore do not
/// stop the job. It continues, and `qex list` finds it. Use `qex submit` for
/// work that must live longer than the command that starts it.
pub fn run(args: cli::RunArgs) -> Result<i32> {
    // Name the command that the READER typed. `qex submit --follow` is this
    // same command in a longer form, and a message about `qex run` names a
    // command that such a reader never used.
    let name = if args.submit.follow {
        "qex submit --follow"
    } else {
        "qex run"
    };

    // This command takes the options of `qex submit`, and `--each-line` is the
    // one option that it cannot obey: it waits for ONE job and gives the output
    // and the exit code of that job. To accept the option here and use one line
    // only would give the user a result that is not the file.
    if args.submit.each_line.is_some() {
        bail!(
            "`{name}` does not accept `--each-line`, because it waits for one job and \
             gives the output and the exit code of that job.\n\n\
             Use `qex submit --each-line`, then wait for the group:\n\
             \x20   GROUP=$(qex submit --each-line inputs.txt -- ./process {{}})"
        );
    }

    let cfg = Config::load()?;
    cfg.validate()?;

    // `qex run` waits always, so `--wait` says nothing that this command does
    // not do. Refuse it, and do not accept it in silence: a user that gives it
    // expects a difference.
    if args.submit.quiet {
        bail!(
            "`{name}` does not accept `--quiet`, because it writes the output of the job to \
             your terminal.\n\n\
             Use `qex submit --wait --quiet` for the exit code with no output."
        );
    }

    if args.submit.wait {
        bail!(
            "`{name}` does not accept `--wait`, because it always waits for the job.\n\n\
             `qex run` writes the output of the job to your terminal. Use \
             `qex submit --wait` to keep the output in the log file."
        );
    }

    // `qex run` gives the output of the job on stdout. A JSON object there
    // would mix with that output, and neither part could be read.
    if args.submit.json {
        bail!(
            "`{name}` does not accept --json.\n\n\
             This command writes the output of the job to stdout, so a JSON object there \
             would mix with that output.\n\n\
             Use two commands:\n\
             \x20   ID=$(qex submit --json ... | jq -r .id)\n\
             \x20   qex wait $ID"
        );
    }

    let env_capture = if args.submit.no_env_capture {
        Some(EnvCapture::None)
    } else {
        args.submit.env_capture
    };

    let opts = SubmitOptions {
        name: args.submit.name,
        cwd: args.submit.cwd,
        cpu: args.submit.cpu,
        mem: args.submit.mem,
        timeout: args.submit.timeout,
        max_queue_time: args.submit.max_queue_time,
        tags: args.submit.tags,
        priority: args.submit.priority,
        env: args.submit.env,
        env_capture,
        command: args.submit.command,
        job_file: args.submit.job_file,
        needs: args.submit.needs,
        after: args.submit.after,
        locks: args.submit.locks,
        retries: args.submit.retries,
        nice: args.submit.nice,
        no_limit_env_hints: args.submit.no_limit_env_hints,
        dedupe_key: args.submit.dedupe_key,
        dedupe_window: args.submit.dedupe_window,
        // An ordinary job measures against its own command.
        learn_key: None,
        gpu: args.submit.gpu,
        vram: args.submit.vram,
        claims: args.submit.claims,
    };

    let (mut spec, deps) = JobSpec::resolve_with_deps(&opts, &cfg)?;
    let mut client = Client::connect()?;
    warn_if_version_differs(&mut client);
    spec.needs = resolve_dependencies(&mut client, &deps.needs, "--needs")?;
    spec.after = resolve_dependencies(&mut client, &deps.after, "--after")?;
    require_capabilities(&mut client, &spec)?;

    let (id, deduplicated) = match client.call(&Request::Submit {
        spec: Box::new(spec),
    })? {
        Response::Submitted {
            id,
            warning,
            deduplicated,
        } => {
            if let Some(text) = warning {
                eprintln!("qex: {text}");
            }
            (id, deduplicated)
        }
        other => return report_for_a_job(other),
    };

    // ALWAYS SAY THE ID. This command writes the output of the job on stdout,
    // so the id has no other place to go, and a caller that loses this command
    // has no handle for a job that continues. One line on stderr costs nothing
    // and it cannot mix with the output of the job.
    say_the_id_and_write_the_file(id, args.submit.id_file.as_ref());

    // Catch Ctrl-C, and stop the job with it.
    //
    // Without this, Ctrl-C would stop this command and leave the job in the
    // queue. A user expects Ctrl-C to stop the work, because `qex run` looks
    // like the command that it replaces.
    catch_run_signals();

    // Say now what Ctrl-C will do, because a dedupe key changed it.
    //
    // This command started no job, so it is not the owner of this job, and a
    // different agent can be the owner. The user must know that before the
    // moment of the signal, and not after it.
    //
    // This message comes AFTER the handler exists. The message is thus proof
    // that the rule is active, and a signal that arrives immediately after it
    // meets the rule and not the default behaviour of the system.
    if deduplicated {
        eprintln!(
            "qex: this command waits for that job. It did not start it, so Ctrl-C stops \
             this wait only.\n\
             qex: to stop the job itself, run `qex kill {id}`."
        );
    }

    let dir = paths::job_dir(&id)?;
    // `qex run` sets no limit on its own wait. `--timeout` limits the JOB.
    stream_until_done(&mut client, id, &dir, !deduplicated, None)
}

/// Stops the job of this `qex run`, after Ctrl-C or after a SIGTERM.
///
/// Gives `true` when qex stopped the job. `qex run` then knows that IT stopped
/// the job, and it does not tell the user that a different command did.
///
/// A job that still waits in the queue has no process, so the coordinator
/// refuses to kill it. This function cancels such a job, because Ctrl-C must
/// stop the WORK: a job that stays in the queue starts later, and no command
/// then reads its output.
fn stop_own_job(client: &mut Client, id: uuid::Uuid) -> bool {
    let answer = client.call(&Request::Kill {
        id,
        signal: libc::SIGTERM,
        grace_secs: 10,
    });

    let answer = match answer {
        // The coordinator refuses a kill for four reasons, and only ONE of them
        // wants a cancel: the job waits in the queue, so it has no process.
        // The other three are short moments in the life of a job — the job
        // stopped, the job starts now, or its process left between the two
        // calls. Read the state, and cancel the queued job only.
        //
        // Without this test, a Ctrl-C at the end of a job that succeeded gave
        // the user the message of a cancel that the coordinator refused, which
        // named a job that had already stopped.
        Ok(Response::Error {
            kind: ErrorKind::WrongState,
            ..
        }) => match client.call(&Request::Status { id }) {
            Ok(Response::Status { status }) if status.state == JobState::Queued => {
                client.call(&Request::Cancel { id })
            }
            // Say nothing here. The next turn of the loop reads the state of
            // the job and reports it, and one true sentence is enough.
            _ => return false,
        },
        other => other,
    };

    match answer {
        Err(e) => {
            eprintln!(
                "qex: the coordinator did not answer, so the job {id} received no stop: {e}. The \
                 job can still operate. Use `qex kill {id}`, or `qex cancel {id}` for a job that \
                 waits, when the coordinator answers again."
            );
            false
        }
        Ok(Response::Error { .. }) => false,
        Ok(_) => true,
    }
}

/// Writes the output of a job as it arrives, until the job stops.
///
/// `owns_job` says if THIS command started the job. Ctrl-C stops the job only
/// when that is true. A dedupe key gives the job of a different caller, and a
/// signal to this command must never stop the work of somebody else.
fn stream_until_done(
    client: &mut Client,
    id: uuid::Uuid,
    dir: &std::path::Path,
    owns_job: bool,
    deadline: Option<Instant>,
) -> Result<i32> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut handles: Vec<(bool, Option<std::fs::File>)> = vec![(false, None), (true, None)];
    let mut announced_wait = false;
    let mut announced_loss = false;
    // Keep a record of who stopped the job. A user who pressed Ctrl-C knows
    // already why the job stopped, but a user whose job ANOTHER command stopped
    // knows nothing, and that user needs the sentence most.
    let mut stopped_here = false;

    loop {
        if RUN_INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
            RUN_INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);

            if !owns_job {
                // A dedupe key gave this job to this command. A different agent
                // started the work, and it can be a run of four hours. A signal
                // to this command stops this wait, and nothing else.
                eprintln!(
                    "\nqex: this wait stops. The job {id} continues, because this command \
                     did not start it.\n\
                     qex: to stop the job, run `qex kill {id}`. \
                     To read the output again, run `qex status {id} --follow`."
                );
                // The code says that the WAIT stopped, and not that a limit of
                // the reader passed. 124 says the reader set a limit and the
                // limit came; this reader set none and lost the wait.
                return Ok(EXIT_WAIT_BROKEN);
            }

            eprintln!("\nqex: stopping the job {id}");
            // Set the flag from the ANSWER, and not from the attempt. A stop
            // that failed leaves the job for a different command to stop, and
            // this command must then not tell the user that it stopped the job.
            stopped_here = stop_own_job(client, id) || stopped_here;
        }

        // Open each log file when the supervisor makes it.
        for (is_err, handle) in handles.iter_mut() {
            if handle.is_none() {
                let name = if *is_err { "stderr.log" } else { "stdout.log" };
                if let Ok(mut f) = std::fs::File::open(dir.join(name)) {
                    f.seek(SeekFrom::Start(0)).ok();
                    *handle = Some(f);
                }
            }
        }

        let mut moved = false;
        for (is_err, handle) in handles.iter_mut() {
            if let Some(file) = handle {
                let mut buf = Vec::new();
                if file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                    moved = true;
                    // Send each stream to the same stream of this command, so
                    // a pipe and a redirection behave as they did before.
                    if *is_err {
                        std::io::stderr().write_all(&buf).ok();
                        std::io::stderr().flush().ok();
                    } else {
                        std::io::stdout().write_all(&buf).ok();
                        std::io::stdout().flush().ok();
                    }
                }
            }
        }

        // A coordinator that stops must not end this command.
        //
        // qex replaces the coordinator at every update of the program, so this
        // is the normal upgrade path. The job continues, because its supervisor
        // continues, and the supervisor writes the record. Read that record, or
        // attach to the coordinator that replaced this one.
        let status = match client.call(&Request::Status { id }) {
            Ok(Response::Status { status }) => status,
            Ok(other) => return report_for_a_job(other),
            Err(_) => status_without_the_coordinator(client, id, dir, &mut announced_loss)?,
        };

        // Say why nothing happens yet. A user of `qex run` sees no output while
        // the job waits, and silence with no reason is the fault that qex
        // removes everywhere else.
        if !announced_wait && status.state == JobState::Queued {
            if let Some(reason) = &status.blocked_reason {
                eprintln!("qex: {reason}");
                announced_wait = true;
            }
        }

        if status.state.is_terminal() && !moved {
            let code = exit_code_for(&status);
            report_run_stop(&status, stopped_here);
            return Ok(code);
        }

        // The limit of the reader stops the WAIT, and never the job.
        //
        // A job that ALREADY stopped never gives this code. The test above
        // leaves a terminal job here when it still moved output in this step,
        // so a limit that comes in that same step would report 124 for a job
        // that has a result.
        if let Some(d) = deadline.filter(|_| !status.state.is_terminal()) {
            if Instant::now() >= d {
                eprintln!(
                    "qex: your wait reached its time limit. The job {id} continues.\n\
                     qex: attach to it again:  qex status {id} --follow"
                );
                return Ok(EXIT_TIMEOUT);
            }
        }

        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Gives the state of the job when the coordinator stopped.
///
/// The record on the disk is the truth. The supervisor writes it, and the
/// supervisor continues after the coordinator stops (issue #45).
fn status_without_the_coordinator(
    client: &mut Client,
    id: uuid::Uuid,
    dir: &std::path::Path,
    announced: &mut bool,
) -> Result<Box<JobStatus>> {
    if !*announced {
        eprintln!(
            "qex: the coordinator stopped. The job {id} continues, and this command continues \
             with it."
        );
        *announced = true;
    }

    // A new coordinator can listen already. Take it, so the following requests
    // go the fast way again.
    if let Some(fresh) = Client::connect_existing() {
        *client = fresh;
        if let Ok(Response::Status { status }) = client.call(&Request::Status { id }) {
            return Ok(status);
        }
    }

    crate::job::read_status(dir).map(Box::new).with_context(|| {
        format!(
            "the coordinator stopped, and qex could not read the record of the job {id}. The \
             job can still operate. Use `qex status {id}` when the coordinator answers again"
        )
    })
}

/// Writes why `qex run` stopped, when the job gave no exit code of its own.
///
/// The exit code speaks to a script. A person, and an agent that reads the
/// output, needs the sentence as well. The important case is the job that a
/// DIFFERENT command stopped: the caller must not read that as a fault in its
/// own work, so the text says that this command did not stop the job.
///
/// `stopped_here` is true when this command stopped the job itself, after
/// Ctrl-C or after a SIGTERM: it sent the kill, or it cancelled a job that
/// still waited in the queue. That user knows the cause already, so the text
/// is short.
fn report_run_stop(status: &JobStatus, stopped_here: bool) {
    let id = status.id;
    let text = match status.state {
        // The job ran and gave its own exit code, and that code is now the exit
        // code of this command. A signal in the job gives no code, so name it.
        JobState::Completed => return,
        JobState::Failed => match (status.exit_code, status.signal) {
            (Some(_), _) => return,
            (None, Some(sig)) => format!(
                "the signal {sig} stopped the job {id}, so the job gave no exit code of its \
                 own. Use `qex status {id}` to read the record."
            ),
            _ => return,
        },
        JobState::Killed if stopped_here => {
            format!("this command stopped the job {id}")
        }
        // Say what the RECORD holds, and no more.
        //
        // The record holds the signal that stopped the job. It does not hold
        // the sender, so this text must not name one: a job that sends itself
        // a SIGTERM reaches this same state, and a sentence that blames a
        // different command would then be wrong.
        JobState::Killed => match status.signal {
            Some(sig) => format!(
                "the signal {sig} stopped the job {id}, and this command did not send it. A \
                 different command, or the job itself, sent it. Use `qex status {id}` to read \
                 the record."
            ),
            None => format!(
                "something stopped the job {id}, and this command did not stop it. Use \
                 `qex status {id}` to read the record."
            ),
        },
        JobState::Cancelled if stopped_here => {
            format!("this command removed the job {id} from the queue, and the job did not run")
        }
        JobState::Cancelled => format!(
            "a different command removed the job {id} from the queue. The job did not run and \
             it wrote no output. Start the work again when you still need it."
        ),
        JobState::Timeout => format!(
            "the job {id} reached its time limit, and qex stopped it. Give a longer `--timeout` \
             when the work needs more time."
        ),
        // Say what the RECORD says. `describe_result` holds the full story of
        // a kill for memory — the claim that qex tried, and whether it raised
        // it — and two commands must not explain one state two ways. The words
        // "the machine ran out of memory" also sent readers to the machine
        // when the fault was in the claim.
        JobState::Oom => format!(
            "the job {id} stopped: {}. Give a larger `--mem`, or make the work smaller.",
            describe_result(status)
        ),
        JobState::Skipped => describe_result(status),
        other => format!("the job {id} is {other}"),
    };
    eprintln!("qex: {text}");
}

/// Submits the same job again, with the same command, environment and claim.
///
/// A job that failed for a reason outside itself needs no new command line. The
/// new job has a new id, and the record of the first job stays, so a reader can
/// compare the two.
pub fn rerun(args: cli::RerunArgs) -> Result<i32> {
    let mut client = Client::connect()?;
    warn_if_version_differs(&mut client);

    let id = match resolve_id(&mut client, &args.id) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("qex: {e}");
            return Ok(EXIT_NO_SUCH_JOB);
        }
    };

    // Read the specification of the first job. It holds the environment and the
    // directory of the shell that submitted it, so the new job runs in the same
    // way as the first.
    let dir = paths::job_dir(&id)?;
    let mut spec = crate::job::read_spec(&dir)
        .with_context(|| format!("reading the specification of the job {id}"))?;

    // Use the claim IN FORCE, and not the claim of the submission.
    //
    // The record holds the claim that the job had at the end. That value is the
    // value of the specification, except after a kill for memory: qex then
    // raised the claim, and the job succeeded at the larger value. A rerun from
    // the specification would repeat the claim that the kernel already stopped,
    // and the correction that cost a whole run would go away.
    if let Ok(status) = crate::job::read_status(&dir) {
        if status.mem > spec.mem {
            spec.mem = status.mem;
            spec.cpu = status.cpu.max(spec.cpu);
            spec.claim_source = status.claim_source.clone();
        }
    }

    // A new job needs a new id, and it must not keep the dependencies of the
    // first job: those jobs have stopped, and a dependency on a job that
    // succeeded is not correct.
    spec.id = uuid::Uuid::new_v4();
    spec.submitted_at = crate::sys::now_secs();
    spec.needs.clear();
    spec.after.clear();
    spec.group = None;
    spec.group_name = None;

    // A rerun must not keep the dedupe key of the first job.
    //
    // `qex rerun` is the command that says "run this work again". With the key,
    // qex would give the id of the first job and start nothing, and the command
    // would do the one thing that it exists to prevent: nothing.
    spec.dedupe_key = None;
    spec.dedupe_window = 0;

    match client.call(&Request::Submit {
        spec: Box::new(spec),
    })? {
        Response::Submitted {
            id: new_id,
            warning,
            ..
        } => {
            if let Some(text) = warning {
                eprintln!("qex: {text}");
            }
            eprintln!(
                "qex: the job {} runs again as {new_id}",
                &id.to_string()[..8]
            );
            if let Some(path) = &args.id_file {
                write_id_file(path, &format!("{new_id}\n"))?;
            }
            println!("{new_id}");
            Ok(0)
        }
        other => report(other),
    }
}

/// Asks the coordinator what it can do.
///
/// The `Capabilities` request did not exist in the first versions, and an
/// earlier coordinator gives an error for a request that it cannot read. This
/// function thus reads the version first, because every version answers `Info`,
/// and it uses a table for an earlier coordinator.
fn coordinator_capabilities(client: &mut Client) -> (Vec<String>, String, i32) {
    let (version, pid) = match client.call(&Request::Info) {
        Ok(Response::Info { version, pid, .. }) => (version, pid),
        _ => return (Vec::new(), String::from("unknown"), 0),
    };

    // Every version that qex supports answers this request. A coordinator that
    // does not answer it is below the capability floor, and it gets an empty list;
    // the floor test then refuses it with the correct words.
    match client.call(&Request::Capabilities) {
        Ok(Response::Capabilities { names }) => (names, version, pid),
        _ => (Vec::new(), version, pid),
    }
}

/// Refuses a job that the coordinator cannot obey.
///
/// A field that the coordinator does not know travels in the JSON and is
/// ignored in silence. A user would then receive a job id for a job that runs
/// without the rule that the user asked for.
fn require_capabilities(client: &mut Client, spec: &JobSpec) -> Result<()> {
    let (have, version, pid) = coordinator_capabilities(client);

    // The floor first. A coordinator below it comes from a build that no
    // release holds, so no promise covers it, whatever the job asks for.
    //
    // A development build passes here and takes a warning instead, which
    // `warn_if_version_differs` writes. The test below still refuses each
    // option that such a coordinator cannot obey, and it names the option.
    if let crate::capabilities::Floor::Below(message) =
        crate::capabilities::check_floor(&version, pid)
    {
        return Err(anyhow::anyhow!("{message}"));
    }

    if crate::capabilities::required_by(spec).is_empty() {
        return Ok(());
    }
    crate::capabilities::check(&have, &version, pid, spec).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Tests one job against a directory.
///
/// `exact` gives the jobs of one directory. `under` gives the jobs of that
/// directory and of every directory below it, so a user at the top of a project
/// reaches every job of that project.
fn matches_directory(
    job_cwd: &str,
    exact: Option<&std::path::Path>,
    under: Option<&std::path::Path>,
) -> bool {
    if let Some(dir) = exact {
        if std::path::Path::new(job_cwd) != dir {
            return false;
        }
    }
    if let Some(dir) = under {
        let path = std::path::Path::new(job_cwd);
        // `starts_with` compares whole parts, so `/a/b` does not match `/a/bc`.
        if !path.starts_with(dir) {
            return false;
        }
    }
    true
}

/// Reads a directory from the command line, and makes it absolute.
///
/// A job records the directory that the CLI resolved at the submission, so this
/// value must be resolved in the same way. Without that step, `.` and the full
/// path would not match each other.
fn resolve_directory(path: &std::path::Path, option: &str) -> Result<std::path::PathBuf> {
    path.canonicalize()
        .with_context(|| format!("{option}: the directory {} does not exist", path.display()))
}

/// Collects the old records of every directory.
///
/// `qex clean --auto` works on one directory tree and on one hour, for a user
/// who finished a piece of work. This command works on every directory and on a
/// longer time, for a machine that has run for days.
///
/// It also deletes a job directory that holds no record. A coordinator that
/// stopped between the creation of the directory and the first write of the
/// record leaves one, and nothing else removes it.
pub fn gc(args: cli::GcArgs) -> Result<i32> {
    let cfg = Config::load()?;
    cfg.validate()?;

    let keep = match &args.older_than {
        Some(t) => parse_duration(t)
            .map_err(|e| anyhow::anyhow!("--older-than: {e}"))?
            .unwrap_or(Duration::from_secs(0))
            .as_secs(),
        None => cfg.gc_keep()?.as_secs(),
    };

    let now = crate::sys::now_secs();
    let mut client = Client::connect()?;
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };

    let held = needed_by_unfinished(&jobs);
    let mut held_back = 0usize;
    let mut targets: Vec<(uuid::Uuid, String, u64)> = Vec::new();
    for j in &jobs {
        if !j.state.is_terminal() {
            continue;
        }
        if held.contains(&j.id) {
            // A job that a job in the queue needs is not finished for this
            // purpose. Its record answers a question that the other job has
            // not yet asked.
            held_back += 1;
            continue;
        }
        let stopped = j.finished_at.unwrap_or(j.submitted_at);
        let age = now.saturating_sub(stopped);
        if age >= keep {
            targets.push((
                j.id,
                // For a READER. See `for_display`.
                j.display_name(),
                directory_size(&paths::job_dir(&j.id)?),
            ));
        }
    }

    // A directory with no record belongs to no job. It cannot be deleted by an
    // id, because no id names it in the coordinator.
    let mut orphans: Vec<(std::path::PathBuf, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths::jobs_dir()?) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let known = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<uuid::Uuid>().ok())
                .map(|id| jobs.iter().any(|j| j.id == id))
                .unwrap_or(false);
            if !known && crate::job::read_status(&path).is_err() {
                orphans.push((path.clone(), directory_size(&path)));
            }
        }
    }

    let bytes: u64 = targets.iter().map(|(_, _, b)| b).sum::<u64>()
        + orphans.iter().map(|(_, b)| b).sum::<u64>();

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "older_than_secs": keep,
                "dry_run": args.dry_run,
                "jobs": targets.iter().map(|(id, name, _)| serde_json::json!({
                    "id": id.to_string(), "name": name
                })).collect::<Vec<_>>(),
                "directories_with_no_record": orphans.len(),
                "kept_because_a_job_needs_them": held_back,
                "bytes": bytes,
            }))?
        );
    }

    if args.dry_run {
        if !args.json {
            println!(
                "qex would delete {} record(s) and {} directory(s) with no record, and free {}.",
                targets.len(),
                orphans.len(),
                format_size(bytes)
            );
            println!("Nothing changed. Run the command without `--dry-run` to delete them.");
        }
        return Ok(0);
    }

    let mut deleted = 0usize;
    for (id, _, _) in &targets {
        if let Response::Ok = client.call(&Request::Clean { id: *id })? {
            deleted += 1;
        }
    }
    for (path, _) in &orphans {
        std::fs::remove_dir_all(path).ok();
    }

    if !args.json {
        println!(
            "qex deleted {deleted} record(s) and {} directory(s) with no record, and freed {}.",
            orphans.len(),
            format_size(bytes)
        );
        if held_back > 0 {
            println!(
                "{held_back} record(s) stayed, because a job that has not stopped still \
                 needs them. They go at the next run, after that job stops."
            );
        }
        if deleted < targets.len() {
            println!(
                "{} record(s) that this command chose stayed. The coordinator refused them.",
                targets.len() - deleted
            );
        }
    }
    Ok(0)
}

/// Gives the number of bytes that one directory holds.
fn directory_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_dir() { 0 } else { m.len() })
        .sum()
}

/// Gives the jobs that a job which has not stopped still needs.
///
/// Such a job is not finished for the purpose of a deletion, whatever its own
/// state says. A job in the queue reads the record of the job that it waits
/// for: it needs the state to decide whether to run, and it needs the name and
/// the log to explain why it did not.
///
/// A deletion of that record would take the answer away from a job that has not
/// yet asked the question.
fn needed_by_unfinished(jobs: &[JobStatus]) -> std::collections::BTreeSet<uuid::Uuid> {
    let mut held = std::collections::BTreeSet::new();
    for job in jobs {
        if job.state.is_terminal() {
            continue;
        }
        for id in job.needs.iter().chain(job.after.iter()) {
            held.insert(*id);
        }
    }
    held
}

/// Shows how much disk space qex holds.
///
/// The output of a job has no limit, and a job that writes a large log holds
/// that space until somebody deletes its record. This command says how much,
/// and which jobs hold the most, so a user knows whether `qex gc` is worth the
/// command.
pub fn du(args: cli::DuArgs) -> Result<i32> {
    let cfg = Config::load()?;
    let state = paths::state_dir()?;
    let jobs_dir = paths::jobs_dir()?;

    // Read the records from the disk. This command must answer when no
    // coordinator operates, in the same way as `qex top`.
    let jobs = crate::job::read_all_from_disk();

    let mut per_job: Vec<(uuid::Uuid, String, String, u64, bool)> = Vec::new();
    let mut total_jobs = 0u64;
    let mut orphans = 0u64;
    let mut orphan_count = 0usize;

    if let Ok(entries) = std::fs::read_dir(&jobs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let size = directory_size(&path);
            total_jobs += size;

            match crate::job::read_status(&path) {
                Ok(status) => {
                    let old = crate::sys::now_secs()
                        .saturating_sub(status.finished_at.unwrap_or(status.submitted_at))
                        >= cfg.gc_keep().map(|d| d.as_secs()).unwrap_or(86400);
                    per_job.push((
                        status.id,
                        // For a READER. See `for_display`.
                        status.display_name(),
                        status.state.to_string(),
                        size,
                        status.state.is_terminal() && old,
                    ));
                }
                Err(_) => {
                    orphans += size;
                    orphan_count += 1;
                }
            }
        }
    }

    // The files beside the jobs: the record of the ids, the measurements, the
    // log of the coordinator.
    let other = directory_size(&state) + directory_size(&paths::runtime_dir()?);
    let total = total_jobs + other;
    let reclaimable: u64 = per_job
        .iter()
        .filter(|(_, _, _, _, old)| *old)
        .map(|(_, _, _, size, _)| size)
        .sum::<u64>()
        + orphans;

    per_job.sort_by_key(|(_, _, _, size, _)| std::cmp::Reverse(*size));

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "total_bytes": total,
                "jobs_bytes": total_jobs,
                "other_bytes": other,
                "reclaimable_bytes": reclaimable,
                "job_count": jobs.len(),
                "directories_with_no_record": orphan_count,
                "largest": per_job.iter().take(args.top).map(|(id, name, state, size, _)| {
                    serde_json::json!({
                        "id": id.to_string(), "name": name, "state": state, "bytes": size
                    })
                }).collect::<Vec<_>>(),
            }))?
        );
        return Ok(0);
    }

    println!("qex holds {} in {}", format_size(total), state.display());
    println!(
        "  {} in {} job record(s)",
        format_size(total_jobs),
        per_job.len()
    );
    if orphan_count > 0 {
        println!(
            "  {} in {orphan_count} directory(s) with no record",
            format_size(orphans)
        );
    }
    println!("  {} in the other files", format_size(other));

    if reclaimable > 0 {
        println!();
        println!(
            "{} can go now. Run `qex gc` to free it.",
            format_size(reclaimable)
        );
    }

    if !per_job.is_empty() && args.top > 0 {
        println!();
        println!("The largest job records:");
        for (id, name, state, size, old) in per_job.iter().take(args.top) {
            println!(
                "  {:>9}  {}  {:<10} {:<16.16}{}",
                format_size(*size),
                &id.to_string()[..8],
                state,
                name,
                if *old {
                    "  (qex gc would free this)"
                } else {
                    ""
                }
            );
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Usage;

    fn status_with(state: JobState, code: Option<i32>) -> JobStatus {
        JobStatus {
            id: uuid::Uuid::new_v4(),
            name: "t".into(),
            command: vec!["true".into()],
            cwd: "/".into(),
            state,
            pid: Some(1),
            last_pid: None,
            supervisor_pid: None,
            exit_code: code,
            signal: None,
            submitted_at: 0,
            started_at: Some(0),
            finished_at: Some(1),
            cpu: 1,
            mem: 1 << 30,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            usage: Usage::default(),
            forced: false,
            forced_reason: None,
            sequence: 0,
            queue_pause_secs: 0,
            blocked_reason: None,
            blocked_since: None,
            passed_by: 0,
            error: None,
            needs: vec![],
            after: vec![],
            locks: vec![],
            claims: Default::default(),
            assigned: Default::default(),
            attempts: 1,
            retries_left: 0,
            oom_raises: 0,
            caused_by: None,
            logs_dropped: None,
            tags: vec![],
            dedupe_key: None,
        }
    }

    /// A log file that qex could not complete must give a notice.
    ///
    /// A process of a job can hold the output open after the job stops. qex
    /// then writes the record and leaves the copy, and it cannot count what
    /// went. An earlier version wrote no count and no flag, so the record said
    /// that the file was complete. A program reads that field and not the text
    /// beside it.
    #[test]
    fn a_log_file_that_qex_could_not_complete_gives_a_notice() {
        use crate::job::LogsDropped;
        let mut s = status_with(JobState::Completed, Some(0));

        s.logs_dropped = Some(LogsDropped {
            incomplete: true,
            ..Default::default()
        });
        let notice = dropped_notice(&s, "stdout").expect("a file that is not complete says so");
        assert!(notice.contains("did not close"), "got: {notice}");

        // With a count as well, the reader gets both facts.
        s.logs_dropped = Some(LogsDropped {
            stdout_bytes: 4096,
            stdout_lines: 20,
            incomplete: true,
            ..Default::default()
        });
        let notice = dropped_notice(&s, "stdout").unwrap();
        assert!(notice.contains("qex removed"), "got: {notice}");
        assert!(notice.contains("did not close"), "got: {notice}");

        // A file that is complete, with nothing removed, gives no notice.
        s.logs_dropped = None;
        assert_eq!(dropped_notice(&s, "stdout"), None);
    }

    /// Makes one pipeline of three stages, with the given group and name.
    ///
    /// The stages come back in an order that is NOT the order of submission.
    /// A test of the order must fail when the sort goes away, and a fixture
    /// that is already in order tests nothing.
    fn a_pipeline_named(group: uuid::Uuid, group_name: &str) -> Vec<JobStatus> {
        let mut jobs = Vec::new();
        // build is stage 0, test is stage 1, ship is stage 2. The vector holds
        // them as ship, build, test.
        for (name, sequence) in [("ship", 2u64), ("build", 0), ("test", 1)] {
            let mut j = status_with(JobState::Completed, Some(0));
            j.name = name.into();
            j.group = Some(group);
            j.group_name = Some(group_name.to_string());
            // The three stages arrive in the same second, so the sequence is
            // the only thing that gives their order.
            j.submitted_at = 100;
            j.sequence = sequence;
            jobs.push(j);
        }
        jobs
    }

    /// Makes a pipeline of three stages, and one job that belongs to no
    /// pipeline.
    fn a_pipeline() -> (Vec<JobStatus>, uuid::Uuid) {
        let group = uuid::Uuid::new_v4();
        let mut jobs = a_pipeline_named(group, "release");

        let mut alone = status_with(JobState::Completed, Some(0));
        alone.name = "alone".into();
        alone.submitted_at = 50;
        jobs.push(alone);
        (jobs, group)
    }

    /// The id of a pipeline names every stage of it, in the order of
    /// submission.
    ///
    /// `qex pipeline` writes that id to stdout, so it is the value that a user
    /// keeps. Before this, `qex wait $GROUP` answered "there is no job with the
    /// id ..." with the code 127, and the documented way to use a pipeline
    /// ended with the user finding the last stage by hand.
    #[test]
    fn the_id_of_a_pipeline_names_every_stage() {
        let (jobs, group) = a_pipeline();
        let id_of = |name: &str| jobs.iter().find(|j| j.name == name).unwrap().id;
        // The order of the stages, and not the order of the vector.
        let want = vec![id_of("build"), id_of("test"), id_of("ship")];

        // The full id, the start of the id, and the name all give the stages.
        for raw in [
            group.to_string(),
            group.to_string()[..8].to_string(),
            "release".to_string(),
        ] {
            let found = resolve_targets_in(&jobs, &raw).unwrap();
            assert_eq!(found.ids, want, "`{raw}` must give every stage, in order");
            assert_eq!(
                found.group,
                Some(group),
                "`{raw}` must report that it named a pipeline"
            );
        }
    }

    /// One word must never reach two pipelines.
    ///
    /// A pipeline takes its name from its file, so a second run of the same
    /// file carries the same name. `qex kill ci` stopped the work of two
    /// separate runs, and the user named one. A short group id has the same
    /// fault, because two ids can start with the same characters.
    #[test]
    fn a_word_that_names_two_pipelines_gives_an_error() {
        let mut jobs = a_pipeline_named(uuid::Uuid::new_v4(), "ci");
        jobs.extend(a_pipeline_named(uuid::Uuid::new_v4(), "ci"));

        let err = resolve_targets_in(&jobs, "ci").unwrap_err().to_string();
        assert!(
            err.contains("2 pipelines"),
            "the message must say how many runs the word names: {err}"
        );
        assert!(
            err.contains("group id"),
            "the message must give the remedy: {err}"
        );

        // One of the two group ids still gives that one run only.
        let first = jobs[0].group.unwrap();
        let found = resolve_targets_in(&jobs, &first.to_string()).unwrap();
        assert_eq!(found.ids.len(), 3);
        assert_eq!(found.group, Some(first));
    }

    /// The message must give the stage count OF EACH run, beside the group id
    /// of that run.
    ///
    /// The count is how the reader separates the run that they want from the
    /// run that they do not want. A count that belongs to the other run reads
    /// as correct and sends the reader to the wrong group id. The two runs
    /// therefore hold a different number of stages here: a fixture in which
    /// both runs are the same size cannot see this fault.
    #[test]
    fn the_two_pipelines_each_give_their_own_stage_count() {
        let big = uuid::Uuid::new_v4();
        let small = uuid::Uuid::new_v4();
        let mut jobs = a_pipeline_named(big, "ci");
        let mut only = status_with(JobState::Completed, Some(0));
        only.name = "only".into();
        only.group = Some(small);
        only.group_name = Some("ci".into());
        jobs.push(only);

        let err = resolve_targets_in(&jobs, "ci").unwrap_err().to_string();
        assert!(
            err.contains(&format!("{big}  3 stage(s)")),
            "the run of three stages must show 3: {err}"
        );
        assert!(
            err.contains(&format!("{small}  1 stage(s)")),
            "the run of one stage must show 1: {err}"
        );
    }

    /// One job still gives one job, by id, by short id and by name.
    #[test]
    fn a_job_still_gives_one_job() {
        let (jobs, _) = a_pipeline();
        let build = jobs.iter().find(|j| j.name == "build").unwrap();
        for raw in [
            build.id.to_string(),
            build.id.to_string()[..8].to_string(),
            "build".to_string(),
        ] {
            let found = resolve_targets_in(&jobs, &raw).unwrap();
            assert_eq!(found.ids, vec![build.id]);
            // The caller uses this to choose the shape of its JSON, and to
            // decide whether a job that already stopped is a fault.
            assert_eq!(found.group, None, "`{raw}` names one job, not a pipeline");
        }
    }

    /// A pipeline of one stage is still a pipeline.
    ///
    /// `qex status --json` chooses an array or an object from this, so a shape
    /// that came from the NUMBER of stages would change on the day a pipeline
    /// has one stage, and `jq '.[0]'` would stop working.
    #[test]
    fn a_pipeline_of_one_stage_is_still_a_pipeline() {
        let group = uuid::Uuid::new_v4();
        let mut only = status_with(JobState::Completed, Some(0));
        only.name = "solo".into();
        only.group = Some(group);
        only.group_name = Some("one".into());
        let jobs = vec![only];

        let found = resolve_targets_in(&jobs, "one").unwrap();
        assert_eq!(found.ids.len(), 1);
        assert_eq!(found.group, Some(group));
    }

    /// A value that names nothing must say that it names no pipeline either.
    #[test]
    fn a_value_that_names_nothing_gives_an_error() {
        let (jobs, _) = a_pipeline();
        let err = resolve_targets_in(&jobs, "nothing")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pipeline"),
            "the message must say that a pipeline is also a handle: {err}"
        );

        // An empty value must not give every job. A command that deletes would
        // then delete everything.
        assert!(resolve_targets_in(&jobs, "").is_err());
    }

    /// A word that names a job AND a pipeline must give an error.
    ///
    /// qex must not choose one of the two for the user. A command that kills
    /// would kill the wrong work.
    #[test]
    fn a_word_that_names_a_job_and_a_pipeline_gives_an_error() {
        let (mut jobs, _) = a_pipeline();
        // The job that belongs to no pipeline takes the name of the pipeline.
        jobs.last_mut().unwrap().name = "release".into();

        let err = resolve_targets_in(&jobs, "release")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("job") && err.contains("pipeline"),
            "the message must name both readings: {err}"
        );

        // The pipeline is ONE run, so its group id appears one time. The three
        // stages share that id, and a list that repeats it for each stage
        // reads as three pipelines.
        let group = jobs[0].group.unwrap().to_string();
        assert_eq!(
            err.matches(&group).count(),
            1,
            "the group id of one run must appear one time: {err}"
        );
    }

    /// Makes a uuid whose text starts with these four bytes.
    fn id_starting(prefix: [u8; 4], last: u8) -> uuid::Uuid {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&prefix);
        bytes[15] = last;
        uuid::Uuid::from_bytes(bytes)
    }

    /// A short id that starts the id of a job AND the id of its own group is
    /// not an ambiguity.
    ///
    /// Both readings give the SAME job, so qex must answer with that job. An
    /// error here would refuse a value that has one meaning, and the user
    /// copied that value out of `qex list`.
    #[test]
    fn a_short_id_that_starts_a_job_and_its_own_group_gives_that_job() {
        let mut job = status_with(JobState::Completed, Some(0));
        job.id = id_starting([0x12, 0x34, 0x56, 0x78], 1);
        job.group = Some(id_starting([0x12, 0x34, 0x56, 0x78], 2));
        job.group_name = Some("shared".into());
        let want = job.id;
        let jobs = vec![job];

        let found = resolve_targets_in(&jobs, "12345678").expect("both readings give one job");
        assert_eq!(found.ids, vec![want]);
        // The user named a job, and not a pipeline. `qex status --json` reads
        // this to choose an object over an array.
        assert_eq!(found.group, None);
    }

    /// The same short id, where the pipeline holds a SECOND stage, is an
    /// ambiguity.
    ///
    /// The word then means "this one job" or "this pipeline of two stages",
    /// and those are different work. The two readings hold the same job, so a
    /// test that asks only whether one side is inside the other calls them the
    /// same and deletes or kills the second stage with no word to the user.
    #[test]
    fn a_short_id_that_starts_a_job_and_a_group_of_two_stages_gives_an_error() {
        let group = id_starting([0x12, 0x34, 0x56, 0x78], 2);

        let mut first = status_with(JobState::Completed, Some(0));
        first.id = id_starting([0x12, 0x34, 0x56, 0x78], 1);
        first.group = Some(group);
        first.group_name = Some("shared".into());

        // The second stage does NOT carry the short id in its own id.
        let mut second = status_with(JobState::Completed, Some(0));
        second.id = id_starting([0x77, 0x77, 0x77, 0x77], 3);
        second.group = Some(group);
        second.group_name = Some("shared".into());

        let jobs = vec![first, second];
        let err = resolve_targets_in(&jobs, "12345678")
            .expect_err("one job and a pipeline of two stages is an ambiguity")
            .to_string();
        assert!(
            err.contains("name of a job") && err.contains("name of a pipeline"),
            "the message must name both readings: {err}"
        );
    }

    /// A word that gives one job and a DIFFERENT pipeline of one stage is an
    /// ambiguity, and the count of each side is the same.
    ///
    /// The two readings must be compared by identity, and not by how many jobs
    /// each holds. A comparison of the counts alone calls this pair the same
    /// job, and qex would then kill or delete the wrong work.
    #[test]
    fn one_job_and_a_one_stage_pipeline_of_the_same_size_still_give_an_error() {
        let mut alone = status_with(JobState::Completed, Some(0));
        alone.id = id_starting([0xaa, 0xbb, 0xcc, 0xdd], 1);
        alone.name = "alone".into();

        let mut staged = status_with(JobState::Completed, Some(0));
        staged.id = id_starting([0x99, 0x99, 0x99, 0x99], 2);
        staged.group = Some(id_starting([0xaa, 0xbb, 0xcc, 0xdd], 3));
        staged.group_name = Some("staged".into());

        let jobs = vec![alone, staged];
        let err = resolve_targets_in(&jobs, "aabbccdd")
            .expect_err("one job and one pipeline is an ambiguity")
            .to_string();
        assert!(
            err.contains("name of a job") && err.contains("name of a pipeline"),
            "the message must name both readings: {err}"
        );
    }

    /// These codes are a contract with the agents. The help text gives them.
    ///
    /// Pin the LITERALS, and not the constants alone. The documents, the skill
    /// file and every script of a user name the numbers, so a change to a
    /// constant is a change to a published interface. A test that reads the
    /// constant on both sides agrees with itself and with nothing else.
    #[test]
    fn the_exit_codes_follow_the_documentation() {
        assert_eq!(EXIT_BAND_FLOOR, 97);
        assert_eq!(EXIT_JOB_RESERVED, 97);
        assert_eq!(EXIT_JOB_SIGNAL, 98);
        assert_eq!(EXIT_OOM, 99);
        assert_eq!(EXIT_NO_RESULT, 100);
        assert_eq!(EXIT_QEX_FAILED, 121);
        assert_eq!(EXIT_WAIT_BROKEN, 122);
        assert_eq!(EXIT_EXPIRED, 123);
        assert_eq!(EXIT_TIMEOUT, 124);
        assert_eq!(EXIT_KILLED, 125);
        assert_eq!(EXIT_SKIPPED, 126);
        assert_eq!(EXIT_NO_SUCH_JOB, 127);

        assert_eq!(exit_code_for(&status_with(JobState::Completed, Some(0))), 0);
        assert_eq!(exit_code_for(&status_with(JobState::Failed, Some(1))), 1);
        assert_eq!(
            exit_code_for(&status_with(JobState::Killed, None)),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Timeout, None)),
            EXIT_KILLED
        );
        // The kernel stopped the job for memory. The remedy is a larger claim,
        // so the code is not the 125 of a kill, a cancel or a time limit.
        assert_eq!(exit_code_for(&status_with(JobState::Oom, None)), EXIT_OOM);
        // A job that never started has its own code. It must not be 125: a job
        // with the code 125 ran and wrote output, and this job did neither.
        assert_eq!(exit_code_for(&status_with(JobState::Expired, None)), 123);
    }

    /// A job that ran gives its own exit code. Without this, an agent cannot
    /// separate the code 7 of its test tool from any other failure.
    #[test]
    fn a_job_that_ran_gives_its_own_exit_code() {
        assert_eq!(exit_code_for(&status_with(JobState::Completed, Some(0))), 0);
        assert_eq!(exit_code_for(&status_with(JobState::Failed, Some(7))), 7);
        assert_eq!(exit_code_for(&status_with(JobState::Failed, Some(1))), 1);
    }

    /// A code that the band keeps gives the sentinel, and the record keeps the
    /// true code.
    ///
    /// This test is the whole feature. Without the sentinel, a job that exits
    /// 124 of its own accord looks exactly like a wait that reached its time
    /// limit, and the two need different actions.
    #[test]
    fn a_job_code_in_the_band_gives_the_sentinel() {
        let s = status_with(JobState::Failed, Some(124));
        assert_eq!(exit_code_for(&s), EXIT_JOB_RESERVED);
        assert_eq!(
            s.exit_code,
            Some(124),
            "the record keeps the code of the job"
        );

        // The two boundaries. An error of one moves a job code into the band,
        // or a band code out of it.
        assert_eq!(exit_code_for(&status_with(JobState::Failed, Some(96))), 96);
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(97))),
            EXIT_JOB_RESERVED
        );
        // The sentinel value itself. The answer must be the sentinel, so the
        // rule stays true for every code in the band.
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(EXIT_JOB_RESERVED))),
            EXIT_JOB_RESERVED
        );
        // The usual code of a job that a signal stopped, 128 + N. It is in the
        // band, so it gives the sentinel as well. Only qex itself gives a code
        // above 127.
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(128))),
            EXIT_JOB_RESERVED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(137))),
            EXIT_JOB_RESERVED
        );
    }

    /// A job that something stopped gave no exit code of its own. `qex run`
    /// must give the code of the state, and not 1.
    ///
    /// 1 is the most common code of a program that failed. With 1, an agent
    /// cannot separate "my job failed" from "another agent on this machine
    /// stopped my job", so it starts the work again or it reports a fault that
    /// the work does not have.
    #[test]
    fn qex_run_gives_the_code_of_the_state_when_something_stopped_the_job() {
        assert_eq!(
            exit_code_for(&status_with(JobState::Killed, None)),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Cancelled, None)),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Timeout, None)),
            EXIT_KILLED
        );
        assert_eq!(exit_code_for(&status_with(JobState::Oom, None)), EXIT_OOM);
        assert_eq!(
            exit_code_for(&status_with(JobState::Skipped, None)),
            EXIT_SKIPPED
        );
    }

    /// Every code that qex gives is 0 to 96, or 97 to 127. A code of 128 or
    /// above says that the qex PROCESS died from a signal, so no answer of qex
    /// may use one.
    ///
    /// A dead process writes no exit code, so `128 + N` from a qex command can
    /// only mean that the command itself died. The job is then not described,
    /// and the caller must attach to it again.
    #[test]
    fn no_answer_of_qex_reaches_the_codes_of_a_signal() {
        for state in [
            JobState::Completed,
            JobState::Failed,
            JobState::Killed,
            JobState::Cancelled,
            JobState::Timeout,
            JobState::Oom,
            JobState::Skipped,
            JobState::Expired,
        ] {
            for code in [None, Some(0), Some(1), Some(96), Some(97), Some(255)] {
                let s = status_with(state, code);
                let answer = exit_code_for(&s);
                assert!(
                    (0..=127).contains(&answer),
                    "the state {state} with the code {code:?} gives {answer}"
                );
            }
        }
    }

    /// The trap of a wait gives the disposition back to the system, so a
    /// SECOND signal always stops the command.
    ///
    /// This is a unit test, and not an end-to-end test, because the property
    /// cannot be seen from outside: the first signal ends the wait in less
    /// than a millisecond, so no second signal can arrive while the command
    /// still waits. A test that sends two signals in one moment passes with a
    /// trap that a user CANNOT escape, which is the fault that the second
    /// signal exists to prevent.
    ///
    /// The test thus sends ONE signal to itself and reads the disposition that
    /// the system holds after it. A disposition of `SIG_DFL` is the whole of
    /// the rule: the next signal of that number stops this process.
    ///
    /// It reads the DISPOSITION and not the flags. macOS gives `SA_RESETHAND`
    /// its effect and does not report the flag back, so a test of the flags
    /// measures the system and not the behaviour of qex.
    #[test]
    fn the_trap_of_a_wait_gives_the_disposition_back_to_the_system() {
        use std::sync::atomic::Ordering;

        WAIT_INTERRUPTED.store(false, Ordering::SeqCst);
        catch_signals_while_waiting();

        for signal in [libc::SIGINT, libc::SIGTERM] {
            // The FIRST signal reaches the trap, and the trap holds it.
            WAIT_INTERRUPTED.store(false, Ordering::SeqCst);
            assert_eq!(unsafe { libc::raise(signal) }, 0);
            assert!(
                wait_was_interrupted(),
                "the trap must catch the first signal {signal}"
            );

            // The SECOND signal of that number now meets the system.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) },
                0
            );
            assert_eq!(
                action.sa_sigaction,
                libc::SIG_DFL,
                "the trap of the signal {signal} must give the disposition back at the \
                 delivery, so a second signal stops the command"
            );
        }

        // Leave the process as it was. A test that keeps a flag or a handler
        // changes the tests that come after it.
        WAIT_INTERRUPTED.store(false, Ordering::SeqCst);
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
    }

    /// A signal that is not a stop command leaves the job in the state `failed`
    /// with no exit code. The job then takes the code of a signal death, and
    /// the record names the signal.
    ///
    /// A signal that qex KNOWS about is different. A kill, a cancel, a time
    /// limit and an out-of-memory kill each give a state of their own, and the
    /// code is then 125: qex knows why the job stopped, and 125 says more than
    /// "a signal came".
    ///
    /// The usual form `128 + N` cannot serve here. A job that the
    /// out-of-memory killer stops gives 137, and a WAIT that the out-of-memory
    /// killer stops gives 137 as well. The two need different actions.
    #[test]
    fn a_signal_that_the_job_took_gives_the_code_of_a_signal_death() {
        let mut s = status_with(JobState::Failed, None);
        s.signal = Some(libc::SIGSEGV);
        assert_eq!(exit_code_for(&s), EXIT_JOB_SIGNAL);
        assert_eq!(s.signal, Some(libc::SIGSEGV), "the record names the signal");
    }

    #[test]
    fn the_result_text_names_the_cause() {
        let s = status_with(JobState::Failed, Some(3));
        assert!(describe_result(&s).contains('3'));

        let mut s = status_with(JobState::Oom, None);
        s.usage.max_rss = 2 << 30;
        let text = describe_result(&s);
        assert!(text.contains("memory"), "got: {text}");
        // The text must give the claim and the true use. An agent then corrects
        // its claim from this line.
        assert!(text.contains("1GB") && text.contains("2GB"), "got: {text}");
        // It must also name the CLAIM as the fault. The words "the machine ran
        // out of memory" sent the reader to the machine, and the fault was in
        // the claim.
        assert!(text.contains("too small"), "got: {text}");

        // qex writes the full story in the error field: the claim that failed,
        // the new claim, and the attempt. That text must win, because it says
        // more than the line above.
        s.error = Some("qex raised the claim to 2GB and starts the job again".into());
        assert!(
            describe_result(&s).contains("raised the claim"),
            "the record of qex must win"
        );

        // A job that never started has no log file, so this line is the only
        // text that the reader gets. It must carry the queue reason.
        let mut s = status_with(JobState::Expired, None);
        s.error = Some("the job did not start. It waited 5s in the queue".into());
        assert_eq!(
            describe_result(&s),
            "the job did not start. It waited 5s in the queue"
        );
        // A record with no text must still say what happened.
        let s = status_with(JobState::Expired, None);
        assert!(!describe_result(&s).is_empty());
    }

    #[test]
    fn a_short_id_has_eight_characters() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(short_id(&id).len(), 8);
        assert!(id.to_string().starts_with(&short_id(&id)));
    }
}
