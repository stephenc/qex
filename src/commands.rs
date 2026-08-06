//! This module holds the commands that talk to the coordinator.

use crate::cli::{self, StateFilter};
use crate::client::Client;
use crate::config::{Config, EnvCapture};
use crate::job::{JobState, JobStatus};
use crate::paths;
use crate::proto::{ErrorKind, Request, Response};
use crate::spec::{JobSpec, SubmitOptions};
use crate::units::{format_duration, format_size, parse_duration};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

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

pub fn submit(args: cli::SubmitArgs) -> Result<i32> {
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
        tags: args.tags,
        priority: args.priority,
        env: args.env,
        env_capture,
        command: args.command,
        job_file: args.job_file,
        needs: args.needs,
        after: args.after,
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
    match client.call(&Request::Submit {
        spec: Box::new(spec),
    })? {
        Response::Submitted { id, warning } => {
            // The warning goes to stderr. The id stays alone on stdout, so the
            // command `ID=$(qex submit ...)` continues to operate.
            if let Some(text) = warning {
                eprintln!("qex: {text}");
            }
            if let Some(path) = &args.id_file {
                write_id_file(path, &format!("{id}\n"))?;
            }
            println!("{id}");
            Ok(0)
        }
        other => report(other),
    }
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
    if let Some(group) = &args.group {
        // A group takes its id or its name. A name is easier to type, and the
        // names of a pipeline belong to one submission.
        jobs.retain(|j| {
            j.group.map(|g| g.to_string().starts_with(group)).unwrap_or(false)
                || j.group_name.as_deref() == Some(group.as_str())
        });
    }
    // Show the jobs in the order of submission. A pipeline then reads from the
    // first stage to the last stage.
    jobs.sort_by_key(|j| (j.submitted_at, j.sequence));

    if args.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(0);
    }

    if jobs.is_empty() {
        println!("no jobs");
        return Ok(0);
    }

    println!(
        "{:<8}  {:<10}  {:<16}  {:>5}  {:>8}  {:>8}  {}",
        "ID", "STATE", "NAME", "CPU", "MEM", "TIME", "NOTE"
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
    let id = match resolve_id(&mut client, &args.id) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("qex: {e}");
            return Ok(EXIT_NO_SUCH_JOB);
        }
    };

    // Wait for the job first, if the user asked for that.
    //
    // An agent runs this command in the background of its harness. The harness
    // then reports the end of the command, and this output holds the state, the
    // exit code and the cause of a failure. One command gives everything.
    let mut wait_code = 0;
    if args.wait {
        let deadline = match &args.timeout {
            Some(t) => parse_duration(t)
                .map_err(|e| anyhow::anyhow!("--timeout: {e}"))?
                .map(|d| Instant::now() + d),
            None => None,
        };
        match wait_one(&args.id, deadline)? {
            WaitOutcome::Finished(s) => wait_code = exit_code_for(&s, false),
            WaitOutcome::TimedOut => {
                eprintln!(
                    "qex: the wait for {} reached its time limit. The job continues.",
                    args.id
                );
                wait_code = EXIT_TIMEOUT;
            }
            WaitOutcome::NoSuchJob => {
                eprintln!("qex: there is no job with the id {}", args.id);
                return Ok(EXIT_NO_SUCH_JOB);
            }
        }
    }

    match client.call(&Request::Status { id })? {
        Response::Status { status } => {
            // Read the output of the job in the same call.
            //
            // A reader of a job that failed always wants the last lines of its
            // standard error. Without this, every failure costs two commands
            // and two answers, and the reader is frequently an agent with a
            // limited context.
            let excerpt = job_excerpt(&status, &args)?;

            if args.json {
                let mut value = serde_json::to_value(&*status)?;
                if args.show_env {
                    // The environment can hold secrets, so qex adds it only
                    // when the user asks for it.
                    if let Ok(spec) = crate::job::read_spec(&paths::job_dir(&id)?) {
                        value["env"] = serde_json::to_value(&spec.env)?;
                    }
                }
                if !excerpt.is_empty() {
                    // One field for each stream. A reader that wants the result
                    // of a test program needs the standard output, and a reader
                    // that wants the cause needs the standard error.
                    let mut logs = serde_json::Map::new();
                    for (name, selected) in &excerpt {
                        let mut one = serde_json::Map::new();
                        one.insert("text".into(), serde_json::Value::from(selected.text.clone()));
                        if let Some(found) = selected.matches {
                            one.insert("matches".into(), serde_json::Value::from(found));
                        }
                        if selected.truncated {
                            one.insert(
                                "hidden_lines".into(),
                                serde_json::Value::from(selected.hidden),
                            );
                        }
                        logs.insert(name.clone(), serde_json::Value::Object(one));
                    }
                    value["logs"] = serde_json::Value::Object(logs);
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_status(&status, args.show_env)?;
                for (name, selected) in &excerpt {
                    println!();
                    println!("--- {name} ---");
                    if let Some(notice) = selected.notice() {
                        println!("{notice}");
                    }
                    print!("{}", selected.text);
                }
            }
            Ok(wait_code)
        }
        other => report(other),
    }
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
    if let Some(pid) = s.pid {
        println!("pid:       {pid}");
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
            "default" => "  (the default; give --cpu and --mem to change it)",
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
        println!("error:     {e}");
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

    let mut results: Vec<JobStatus> = Vec::new();
    let mut worst = 0i32;

    for raw_id in &args.ids {
        let status = match wait_one(raw_id, deadline)? {
            WaitOutcome::Finished(s) => s,
            WaitOutcome::TimedOut => {
                if !args.json {
                    eprintln!("qex: the wait for {raw_id} reached its time limit. The job continues.");
                }
                return Ok(EXIT_TIMEOUT);
            }
            WaitOutcome::NoSuchJob => {
                if !args.json {
                    eprintln!("qex: there is no job with the id {raw_id}");
                }
                return Ok(EXIT_NO_SUCH_JOB);
            }
        };

        let code = exit_code_for(&status, args.passthrough);
        if code != 0 && worst == 0 {
            worst = code;
        }
        results.push(status);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for s in &results {
            // Use punctuation. Without it, the line reads as one sentence:
            // "a1b2c3d4 completed the job succeeded".
            println!("{}: {} — {}", short_id(&s.id), s.state, describe_result(s));
        }
    }

    Ok(worst)
}

enum WaitOutcome {
    Finished(JobStatus),
    TimedOut,
    NoSuchJob,
}

/// Waits for one job.
///
/// If a coordinator operates, this function sends one request and sleeps. If no
/// coordinator operates, it reads the status file of the job. The second path
/// is necessary because the supervisor continues after the coordinator stops.
fn wait_one(raw_id: &str, deadline: Option<Instant>) -> Result<WaitOutcome> {
    if let Some(mut client) = Client::connect_existing() {
        let id = match resolve_id(&mut client, raw_id) {
            Ok(id) => id,
            Err(_) => return Ok(WaitOutcome::NoSuchJob),
        };

        // Give the socket the time limit of the user. The coordinator answers
        // when the job stops, so this call blocks and uses no CPU time.
        let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if remaining == Some(Duration::ZERO) {
            return Ok(WaitOutcome::TimedOut);
        }
        client.set_read_timeout(remaining)?;

        client.send(&Request::Wait { id })?;
        match client.recv() {
            Ok(Response::Status { status }) => return Ok(WaitOutcome::Finished(*status)),
            Ok(Response::Error {
                kind: ErrorKind::NoSuchJob,
                ..
            }) => return Ok(WaitOutcome::NoSuchJob),
            Ok(other) => {
                report(other)?;
                bail!("the coordinator gave an answer that qex did not expect")
            }
            Err(e) => {
                // Separate the two causes. A read that reaches its limit is the
                // time limit of the user. Every other fault means that the
                // coordinator stopped.
                //
                // Without this test, `qex wait` reports the code 124 when the
                // coordinator fails, and the user reads "your wait reached its
                // time limit" for a wait that had no time limit.
                if is_read_timeout(&e) {
                    return Ok(WaitOutcome::TimedOut);
                }
                // The coordinator stopped. The supervisor of the job continues
                // and writes the result, so read that file instead.
            }
        }
    }

    // There is no coordinator. Read the file of the job.
    wait_on_file(raw_id, deadline)
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
    let mine = env!("CARGO_PKG_VERSION");
    if let Ok(Response::Info {
        version,
        pid,
        program_replaced,
        ..
    }) = client.call(&Request::Info)
    {
        if version != mine {
            eprintln!(
                "qex: the coordinator (pid {pid}) is version {version}, and this command is \
                 version {mine}. The coordinator stops when no job operates, and the next \
                 command starts one with this version. Stop it now with `kill {pid}` if you \
                 need this version immediately."
            );
        } else if program_replaced {
            eprintln!(
                "qex: something replaced the qex program after the coordinator (pid {pid}) \
                 started. The coordinator stops when no job operates."
            );
        }
    }
}

/// Changes each dependency name into a job id.
///
/// Each name must give a job that exists now. A name that gives no job is an
/// error at the submission.
fn resolve_dependencies(
    client: &mut Client,
    names: &[String],
    option: &str,
) -> Result<Vec<uuid::Uuid>> {
    let mut ids = Vec::new();
    for name in names {
        let id = resolve_id(client, name).map_err(|e| {
            anyhow::anyhow!(
                "{option}: {e}\n\n\
                 A job can wait for the jobs that you started before it. Start the \
                 first job, keep its id, then give that id here."
            )
        })?;

        // A dependency given by NAME must still be in the queue or operate.
        //
        // A name is the value that can be wrong in silence. An agent runs a
        // script a second time and writes `--needs test`, but it forgot to
        // start a new test job. The name gives the test job of the FIRST run,
        // which already stopped. The new stage then waits for nothing, and the
        // pipeline reports success although the order was wrong.
        //
        // An id does not have that risk. An id names one job for ever, and the
        // agent read it from the `qex submit` of this run. An id thus needs the
        // existence test only, which `resolve_id` already made.
        //
        // This difference also keeps a pipeline script correct. A script that
        // keeps each id can submit its last stage even when the first stage
        // already failed, and that stage then becomes `skipped` with the
        // correct cause.
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
    }
    Ok(ids)
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
fn wait_on_file(raw_id: &str, deadline: Option<Instant>) -> Result<WaitOutcome> {
    let Some(id) = find_id_on_disk(raw_id)? else {
        return Ok(WaitOutcome::NoSuchJob);
    };
    let dir = paths::job_dir(&id)?;
    let mut delay = Duration::from_millis(20);

    loop {
        if let Ok(status) = crate::job::read_status(&dir) {
            if status.state.is_terminal() {
                return Ok(WaitOutcome::Finished(status));
            }
        }

        if let Some(d) = deadline {
            if Instant::now() >= d {
                return Ok(WaitOutcome::TimedOut);
            }
        }

        std::thread::sleep(delay);
        // The delay grows to one second. A short job thus gives a fast answer,
        // and a long job does not use CPU time.
        delay = (delay * 2).min(Duration::from_secs(1));
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

    let streams = chosen_streams(&args.select);

    if args.json {
        let mut out = serde_json::Map::new();
        out.insert("id".into(), serde_json::Value::String(id.to_string()));
        for (name, file) in &streams {
            let selected = select_log(&dir, file, &args.select, crate::logsel::DEFAULT_LINES)?;
            out.insert((*name).into(), serde_json::Value::String(selected.text));
            if let Some(found) = selected.matches {
                out.insert(
                    format!("{name}_matches"),
                    serde_json::Value::from(found),
                );
            }
            if selected.truncated {
                out.insert(
                    format!("{name}_hidden_lines"),
                    serde_json::Value::from(selected.hidden),
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
    let lead = select.tail.unwrap_or(crate::logsel::FOLLOW_LEAD_LINES);
    let stdout = std::io::stdout();
    let mut handles = Vec::new();

    for (name, file) in &streams {
        let path = dir.join(file);
        // The supervisor can make this file after the command starts.
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .create(true)
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
        f.seek(SeekFrom::End(0))?;
        handles.push((name.to_string(), f, partial));
    }

    loop {
        let mut moved = false;
        for (name, file, partial) in handles.iter_mut() {
            let mut buf = Vec::new();
            if file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                moved = true;
                partial.push_str(&String::from_utf8_lossy(&buf));

                while let Some(end) = partial.find('\n') {
                    let line: String = partial.drain(..=end).collect();
                    let line = line.trim_end_matches('\n');
                    if keep_line(select, line) {
                        let mut out = stdout.lock();
                        if streams.len() > 1 {
                            write!(out, "[{name}] ")?;
                        }
                        writeln!(out, "{line}")?;
                        out.flush()?;
                    }
                }
            }
        }

        match crate::job::read_status(dir) {
            Ok(status) => {
                if status.state.is_terminal() && !moved {
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
    let signal =
        crate::lifecycle::parse_signal(&args.signal).map_err(|e| anyhow::anyhow!("--signal: {e}"))?;
    let grace = parse_duration(&args.grace)
        .map_err(|e| anyhow::anyhow!("--grace: {e}"))?
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut client = Client::connect()?;
    let mut worst = 0;
    for raw in &args.ids {
        let id = match resolve_id(&mut client, raw) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("qex: {e}");
                worst = EXIT_NO_SUCH_JOB;
                continue;
            }
        };
        match client.call(&Request::Kill {
            id,
            signal,
            grace_secs: grace,
        })? {
            Response::Ok => println!("{id} received the signal"),
            other => worst = report(other)?,
        }
    }
    Ok(worst)
}

pub fn cancel(args: cli::CancelArgs) -> Result<i32> {
    let mut client = Client::connect()?;
    let mut worst = 0;
    for raw in &args.ids {
        let id = match resolve_id(&mut client, raw) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("qex: {e}");
                worst = EXIT_NO_SUCH_JOB;
                continue;
            }
        };
        match client.call(&Request::Cancel { id })? {
            Response::Ok => println!("{id} left the queue"),
            other => worst = report(other)?,
        }
    }
    Ok(worst)
}

pub fn clean(args: cli::CleanArgs) -> Result<i32> {
    if args.ids.is_empty() && !args.all && args.state.is_none() && args.older_than.is_none() {
        bail!(
            "name the jobs to delete.\n\n\
             Examples:\n\
             \x20   qex clean <id>\n\
             \x20   qex clean completed        # or: qex clean --state completed\n\
             \x20   qex clean done             # every job that stopped\n\
             \x20   qex clean --older-than 7d\n\
             \x20   qex clean --all"
        );
    }

    let older_than = match &args.older_than {
        Some(t) => parse_duration(t)
            .map_err(|e| anyhow::anyhow!("--older-than: {e}"))?
            .map(|d| d.as_secs()),
        None => None,
    };
    let filter = match args.state.as_deref() {
        Some(s) => Some(StateFilter::parse(s).map_err(|e| anyhow::anyhow!("--state: {e}"))?),
        None => None,
    };

    let mut client = Client::connect()?;
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };

    let now = crate::sys::now_secs();
    let mut targets: Vec<uuid::Uuid> = Vec::new();
    let mut word_filters: Vec<StateFilter> = Vec::new();

    for raw in &args.ids {
        // Accept a state name in place of a job id, so `qex clean completed`
        // operates in the same way as `qex clean --state completed`.
        //
        // A job can have the name of a state. Test the jobs first, and give an
        // error when the word gives both a job and a state.
        let is_job = jobs
            .iter()
            .any(|j| j.id.to_string().starts_with(raw) || j.name == *raw);
        match (is_job, StateFilter::parse(raw)) {
            (true, Ok(_)) => bail!(
                "`{raw}` is the name of a job and the name of a state. \
                 Use the job id, or use `--state {raw}`."
            ),
            (false, Ok(f)) => word_filters.push(f),
            _ => targets.push(resolve_id(&mut client, raw)?),
        }
    }

    for j in &jobs {
        if !j.state.is_terminal() {
            continue;
        }
        let by_state = filter.as_ref().map(|f| f.matches(j.state)).unwrap_or(false)
            || word_filters.iter().any(|f| f.matches(j.state));
        let by_age = older_than
            .map(|limit| now.saturating_sub(j.finished_at.unwrap_or(j.submitted_at)) >= limit)
            .unwrap_or(false);
        if args.all || by_state || by_age {
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
    Ok(worst)
}

/// Gives the exit code for a job result.
fn exit_code_for(status: &JobStatus, passthrough: bool) -> i32 {
    if passthrough {
        // A job that did not run has no exit code of its own. Give the code for
        // the state instead, so the caller still learns that an earlier job is
        // the cause and this job never ran.
        if status.state == JobState::Skipped {
            return EXIT_SKIPPED;
        }
        return status.exit_code.unwrap_or(match status.state {
            JobState::Completed => 0,
            _ => 1,
        });
    }

    match status.state {
        JobState::Completed => 0,
        JobState::Killed | JobState::Timeout | JobState::Oom | JobState::Cancelled => EXIT_KILLED,
        // A job that did not run has its own code. A script can then separate
        // "my job failed" from "a job before mine failed", and it does not read
        // the JSON output.
        JobState::Skipped => EXIT_SKIPPED,
        _ => 1,
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
        JobState::Oom => {
            format!(
                "the machine ran out of memory. The job claimed {} and used {}.",
                format_size(s.mem),
                format_size(s.usage.max_rss)
            )
        }
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

/// Reads a job id from the text that the user wrote.
///
/// The user can write the full id, or the start of the id. A short id is easier
/// to copy from the output of `qex list`.
fn resolve_id(client: &mut Client, raw: &str) -> Result<uuid::Uuid> {
    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };

    // Test each name in the same way, including a full id.
    //
    // An earlier version gave back each value with the form of a UUID without
    // a test. A `--needs` value with one incorrect character was then accepted,
    // the dependency did not exist, and the job started immediately with no
    // warning. `qex logs` with such a value also wrote nothing and gave the
    // code 0, so a reader could not separate "this job wrote nothing" from
    // "this job does not exist".
    if let Ok(id) = raw.parse::<uuid::Uuid>() {
        return if jobs.iter().any(|j| j.id == id) {
            Ok(id)
        } else {
            // Say whether qex ever saw this id. An agent must be able to tell
            // "the record was deleted, and the work happened" from "this job
            // never existed, so submit it".
            bail!("{}", crate::history::describe_missing(id))
        };
    }

    let matches: Vec<&JobStatus> = jobs
        .iter()
        .filter(|j| j.id.to_string().starts_with(raw) || j.name == raw)
        .collect();

    match matches.len() {
        1 => Ok(matches[0].id),
        0 => bail!("there is no job with the id or the name `{raw}`"),
        n => bail!(
            "`{raw}` names {n} jobs. Give the id of the job that you want, or delete \
             the old jobs with `qex clean done` and start again.\n{}",
            matches
                .iter()
                .map(|j| format!("  {} {}", j.id, j.name))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
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
    match client.call(&Request::Info)? {
        Response::Info {
            pid,
            version,
            started_at,
            program_replaced,
            jobs_running,
            jobs_queued,
            cpu_budget,
            mem_budget,
            cpu_claimed,
            mem_claimed,
        } => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "pid": pid,
                        "version": version,
                        "started_at": started_at,
                        "program_replaced": program_replaced,
                        "cli_version": env!("CARGO_PKG_VERSION"),
                        "jobs_running": jobs_running,
                        "jobs_queued": jobs_queued,
                        "cpu_budget": cpu_budget,
                        "mem_budget": mem_budget,
                        "cpu_claimed": cpu_claimed,
                        "mem_claimed": mem_claimed,
                    }))?
                );
                return Ok(0);
            }
            println!("coordinator pid: {pid}");
            println!("version:         {version} (this command: {})", env!("CARGO_PKG_VERSION"));
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
            println!(
                "cores:           {cpu_claimed} of {cpu_budget} in use",
            );
            println!(
                "memory:          {} of {} in use",
                format_size(mem_claimed),
                format_size(mem_budget)
            );
            Ok(0)
        }
        other => report(other),
    }
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
            blocked_reason: None,
            error: None,
            needs: vec![],
            after: vec![],
            caused_by: None,
            tags: vec![],
        }
    }

    /// These codes are a contract with the agents. The help text gives them.
    #[test]
    fn the_exit_codes_follow_the_documentation() {
        assert_eq!(exit_code_for(&status_with(JobState::Completed, Some(0)), false), 0);
        assert_eq!(exit_code_for(&status_with(JobState::Failed, Some(1)), false), 1);
        assert_eq!(exit_code_for(&status_with(JobState::Failed, Some(42)), false), 1);
        assert_eq!(
            exit_code_for(&status_with(JobState::Killed, None), false),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Timeout, None), false),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Oom, None), false),
            EXIT_KILLED
        );
    }

    /// The option `--passthrough` gives the exit code of the job.
    #[test]
    fn the_passthrough_option_gives_the_exit_code_of_the_job() {
        assert_eq!(exit_code_for(&status_with(JobState::Failed, Some(42)), true), 42);
        assert_eq!(exit_code_for(&status_with(JobState::Completed, Some(0)), true), 0);
        // A signal gives no exit code. The result must still show a failure.
        assert_eq!(exit_code_for(&status_with(JobState::Killed, None), true), 1);
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
    }

    #[test]
    fn a_short_id_has_eight_characters() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(short_id(&id).len(), 8);
        assert!(id.to_string().starts_with(&short_id(&id)));
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
                None => bail!("the stage `{}` waits for `{name}`, which qex did not submit", stage.name),
            }
        }
        for name in &stage.after {
            match ids.get(name) {
                Some(id) => spec.after.push(*id),
                None => bail!("the stage `{}` waits for `{name}`, which qex did not submit", stage.name),
            }
        }

        let id = spec.id;
        match client.call(&Request::Submit {
            spec: Box::new(spec),
        })? {
            Response::Submitted { id: given, warning } => {
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
                "group_name": group_name,
                "jobs": jobs,
            }))?
        );
    } else {
        // The group id goes on stdout alone, so `GROUP=$(qex pipeline f.toml)`
        // operates in the same way as `ID=$(qex submit ...)`.
        for (name, id) in &submitted {
            eprintln!("{name}: {id}");
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
    let mine = env!("CARGO_PKG_VERSION");

    // Do not start a coordinator. A question about a version must not change
    // the machine.
    let coordinator = Client::connect_existing().and_then(|mut c| {
        match c.call(&Request::Info) {
            Ok(Response::Info {
                version,
                pid,
                program_replaced,
                ..
            }) => Some((version, pid, program_replaced)),
            _ => None,
        }
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
    match coordinator {
        None => println!("coordinator: none operates"),
        Some((version, pid, replaced)) => {
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

/// Writes the id of a job to a file.
///
/// A shell variable does not last between the commands of an agent, and an
/// agent that loses an id must search for it. A file holds the id.
fn write_id_file(path: &std::path::Path, text: &str) -> Result<()> {
    // A job id gives no access to anything, so this file uses the usual mode.
    crate::job::write_atomic(path, text.as_bytes(), 0o644)
        .with_context(|| format!("writing the id file {}", path.display()))
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
            "group_name": group_name,
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
