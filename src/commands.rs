//! This module holds the commands that talk to the coordinator.

use crate::cli::{self, StateFilter};
use crate::client::Client;
use crate::config::{Config, EnvCapture};
use crate::job::{safe_name, JobState, JobStatus};
use crate::paths;
use crate::proto::{ErrorKind, Request, Response};
use crate::spec::{JobSpec, SubmitOptions};
use crate::units::{format_duration, format_size, parse_duration};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

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
            if let Some(path) = &args.id_file {
                write_id_file(path, &format!("{id}\n"))?;
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
        // The deadline is one moment, and not a limit for each job, so a
        // pipeline of ten stages does not get ten times the time of the user.
        for id in &ids {
            match wait_one(&id.to_string(), deadline)? {
                WaitOutcome::Finished(s) => {
                    // Report the FIRST fault. A later stage that qex skipped
                    // would otherwise hide the stage that failed.
                    let code = exit_code_for(&s, ExitMode::State);
                    if code != 0 && wait_code == 0 {
                        wait_code = code;
                    }
                }
                WaitOutcome::TimedOut => {
                    eprintln!("qex: the wait for {id} reached its time limit. The job continues.");
                    wait_code = EXIT_TIMEOUT;
                    break;
                }
                WaitOutcome::NoSuchJob => {
                    eprintln!("qex: there is no job with the id {id}");
                    return Ok(EXIT_NO_SUCH_JOB);
                }
            }
        }
    }

    let mut values: Vec<serde_json::Value> = Vec::new();
    for (n, id) in ids.iter().enumerate() {
        let id = *id;
        let status = match client.call(&Request::Status { id })? {
            Response::Status { status } => status,
            other => return report(other),
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
            // Keep the silence of `--json`, in the same way as the two answers
            // below. A reader that asked for JSON reads the exit code.
            if !args.json {
                eprintln!("qex: {e}");
            }
            return Ok(EXIT_NO_SUCH_JOB);
        }
    };

    // With `--any`, give control back when the FIRST job stops. An agent that
    // started several jobs can then read a result as soon as it arrives, in
    // place of the order of submission.
    if args.any {
        return wait_for_any(&args, &ids, deadline);
    }

    let mut results: Vec<JobStatus> = Vec::new();
    let mut worst = 0i32;

    for raw_id in &ids {
        let status = match wait_one(raw_id, deadline)? {
            WaitOutcome::Finished(s) => s,
            WaitOutcome::TimedOut => {
                if !args.json {
                    eprintln!(
                        "qex: the wait for {raw_id} reached its time limit. The job continues."
                    );
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

        let code = exit_code_for(&status, wait_mode(args.passthrough));
        if code != 0 && worst == 0 {
            worst = code;
        }
        // From here the records are for a READER. See `for_display`.
        results.push(for_display(*status));
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

/// Waits until the first job of a set stops.
///
/// This function tests each job in turn with a short limit, so no job holds the
/// wait while a different job is ready. It is the one place where qex polls,
/// and it polls its own records, which always answer.
fn wait_for_any(args: &cli::WaitArgs, ids: &[String], deadline: Option<Instant>) -> Result<i32> {
    let mut delay = Duration::from_millis(50);

    loop {
        for raw in ids {
            let status = match Client::connect_existing() {
                Some(mut client) => match resolve_id(&mut client, raw) {
                    Ok(id) => match client.call(&Request::Status { id })? {
                        Response::Status { status } => Some(*status),
                        _ => None,
                    },
                    Err(_) => None,
                },
                None => find_id_on_disk(raw)?
                    .and_then(|id| paths::job_dir(&id).ok())
                    .and_then(|dir| crate::job::read_status(&dir).ok()),
            };

            let Some(status) = status else {
                eprintln!("qex: there is no job with the id {raw}");
                return Ok(EXIT_NO_SUCH_JOB);
            };

            if status.state.is_terminal() {
                // From here the record is for a READER. See `for_display`.
                let status = for_display(status);
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&vec![&status])?);
                } else {
                    println!(
                        "{}: {} — {}",
                        short_id(&status.id),
                        status.state,
                        describe_result(&status)
                    );
                }
                return Ok(exit_code_for(&status, wait_mode(args.passthrough)));
            }
        }

        if let Some(d) = deadline {
            if Instant::now() >= d {
                if !args.json {
                    eprintln!("qex: no job stopped before the time limit. They continue.");
                }
                return Ok(EXIT_TIMEOUT);
            }
        }

        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

enum WaitOutcome {
    // `JobStatus` is large, and the other two answers hold nothing. A box keeps
    // this type small, because each value of it would otherwise take the space
    // of the largest one.
    Finished(Box<JobStatus>),
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
            Ok(Response::Status { status }) => return Ok(WaitOutcome::Finished(status)),
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
fn wait_on_file(raw_id: &str, deadline: Option<Instant>) -> Result<WaitOutcome> {
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

/// How a command turns the result of a job into its own exit code.
///
/// Every command that waits for a job uses this one function. A second rule in
/// a second command is how two commands start to answer one question two ways.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExitMode {
    /// The code names the STATE of the job. `qex wait` uses this mode.
    State,
    /// The code is the exit code of the job. `qex wait --passthrough` uses it.
    Passthrough,
    /// The code is the exit code of the job when the job RAN, and the code of
    /// the state when the job gave no code of its own. `qex run` uses it.
    Run,
}

/// Gives the mode of `qex wait`, with or without `--passthrough`.
fn wait_mode(passthrough: bool) -> ExitMode {
    if passthrough {
        ExitMode::Passthrough
    } else {
        ExitMode::State
    }
}

/// Gives the exit code for a job result.
fn exit_code_for(status: &JobStatus, mode: ExitMode) -> i32 {
    // `qex run` needs a mode between the other two, because neither of them
    // alone is correct for it:
    //
    // - `qex run` writes the output of the job, so a caller expects the exit
    //   code of the job when the job ran. `ExitMode::State` gives 1 for every
    //   job that failed, and it thus loses the code 7 of `sh -c 'exit 7'`.
    // - A job that something stopped gave no exit code, and
    //   `ExitMode::Passthrough` gives 1 for it. The caller reads 1, which is
    //   the most common code of a program that failed, and concludes that its
    //   own command failed. The true answer is that ANOTHER command on the
    //   machine stopped the job, and that is an ordinary event on a machine
    //   that several agents share.
    //
    // `ExitMode::Run` thus passes the code of the job through for the two
    // states in which the job ran to its own end, and it gives the code of the
    // state for every other state. Those are the codes of `qex wait`.
    let passthrough = match mode {
        ExitMode::State => false,
        ExitMode::Passthrough => true,
        ExitMode::Run => matches!(status.state, JobState::Completed | JobState::Failed),
    };

    if passthrough {
        // A job that did not run has no exit code of its own. Give the code for
        // the state instead, so the caller still learns that an earlier job is
        // the cause and this job never ran.
        if status.state == JobState::Skipped {
            return EXIT_SKIPPED;
        }
        if status.state == JobState::Expired {
            return EXIT_EXPIRED;
        }
        return status.exit_code.unwrap_or(match status.state {
            JobState::Completed => 0,
            _ => 1,
        });
    }

    match status.state {
        JobState::Completed => 0,
        // A job that `qex cancel` removed from the queue never ran and wrote
        // nothing, so it has the code of a job that something stopped. The
        // caller must not read it as a fault in the work: the work did not
        // start. The code is the same for `qex run` and for `qex wait`,
        // because one state gives one answer.
        JobState::Killed | JobState::Timeout | JobState::Oom | JobState::Cancelled => EXIT_KILLED,
        // A job that never started has its own code. A script can then separate
        // "my job ran too long" from "my job never got the machine".
        JobState::Expired => EXIT_EXPIRED,
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
        // Give the queue reason here. The reader learns what the job waited for
        // with no other command, and there is no log file to read.
        JobState::Expired => s
            .error
            .clone()
            .unwrap_or_else(|| "the job waited more time than its queue limit".to_string()),
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
            config_error,
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
                        "cli_version": crate::version::VERSION,
                        "jobs_running": jobs_running,
                        "jobs_queued": jobs_queued,
                        "cpu_budget": cpu_budget,
                        "mem_budget": mem_budget,
                        "cpu_claimed": cpu_claimed,
                        "mem_claimed": mem_claimed,
                        "config_error": config_error,
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
            Ok(0)
        }
        other => report(other),
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
    let cfg = Config::load()?;
    cfg.validate()?;

    // `qex run` gives the output of the job on stdout. A JSON object there
    // would mix with that output, and neither part could be read.
    if args.submit.json {
        bail!(
            "`qex run` does not accept --json.\n\n\
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
        other => return report(other),
    };

    if let Some(path) = &args.submit.id_file {
        write_id_file(path, &format!("{id}\n"))?;
    }
    if args.show_id {
        eprintln!("qex: job {id}");
    }

    // Catch Ctrl-C, and stop the job with it.
    //
    // Without this, Ctrl-C would stop this command and leave the job in the
    // queue. A user expects Ctrl-C to stop the work, because `qex run` looks
    // like the command that it replaces.
    unsafe {
        let handler = on_interrupt as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

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
    stream_until_done(&mut client, id, &dir, !deduplicated)
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
) -> Result<i32> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut handles: Vec<(bool, Option<std::fs::File>)> = vec![(false, None), (true, None)];
    let mut announced_wait = false;
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
                     To wait again, run `qex status {id} --wait`."
                );
                return Ok(EXIT_TIMEOUT);
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

        // Name the job in a transport failure.
        //
        // A coordinator that stops while `qex run` waits gives an error here,
        // and `qex run` then exits with the code 1. The job can still operate,
        // so the message must give the reader the id and the next command.
        // Without them the reader has the code of a job that failed and no
        // way to learn that the job continues.
        // Put the cause INSIDE the message, and do not add a context that
        // anyhow writes before it. A context that ends in a full stop gives
        // `...answers again.: Broken pipe`, and the remedy then stands before
        // the fault that it corrects.
        let status = match client.call(&Request::Status { id }) {
            Ok(Response::Status { status }) => status,
            Ok(other) => return report(other),
            Err(e) => bail!(
                "the coordinator did not give the state of the job {id}: {e:#}. The job can \
                 still operate. Use `qex status {id}` when the coordinator answers again."
            ),
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
            let code = exit_code_for(&status, ExitMode::Run);
            report_run_stop(&status, stopped_here);
            return Ok(code);
        }

        std::thread::sleep(Duration::from_millis(80));
    }
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
        JobState::Oom => format!(
            "the machine ran out of memory, and the job {id} stopped. The job claimed {} and \
             used {}. Give a larger `--mem`, or make the work smaller.",
            format_size(status.mem),
            format_size(status.usage.max_rss)
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
            blocked_reason: None,
            error: None,
            needs: vec![],
            after: vec![],
            locks: vec![],
            attempts: 1,
            retries_left: 0,
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
    #[test]
    fn the_exit_codes_follow_the_documentation() {
        assert_eq!(
            exit_code_for(&status_with(JobState::Completed, Some(0)), ExitMode::State),
            0
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(1)), ExitMode::State),
            1
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(42)), ExitMode::State),
            1
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Killed, None), ExitMode::State),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Timeout, None), ExitMode::State),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Oom, None), ExitMode::State),
            EXIT_KILLED
        );
        // A job that never started has its own code. It must not be 125: a job
        // with the code 125 ran and wrote output, and this job did neither.
        //
        // Pin the LITERAL, and not the constant alone. The documents, the skill
        // file and every script of a user name the number 123, so a change to
        // the constant is a change to a published interface. A test that reads
        // the constant on both sides agrees with itself and with nothing else.
        assert_eq!(EXIT_EXPIRED, 123);
        assert_eq!(
            exit_code_for(&status_with(JobState::Expired, None), ExitMode::State),
            123
        );
        // `--passthrough` gives the code of the state here as well. A job that
        // never started has no exit code of its own, so the alternative is 1,
        // and 1 says that the work ran and failed.
        assert_eq!(
            exit_code_for(&status_with(JobState::Expired, None), ExitMode::Passthrough),
            123
        );
    }

    /// The option `--passthrough` gives the exit code of the job.
    #[test]
    fn the_passthrough_option_gives_the_exit_code_of_the_job() {
        assert_eq!(
            exit_code_for(
                &status_with(JobState::Failed, Some(42)),
                ExitMode::Passthrough
            ),
            42
        );
        assert_eq!(
            exit_code_for(
                &status_with(JobState::Completed, Some(0)),
                ExitMode::Passthrough
            ),
            0
        );
        // A signal gives no exit code. The result must still show a failure.
        assert_eq!(
            exit_code_for(&status_with(JobState::Killed, None), ExitMode::Passthrough),
            1
        );
    }

    /// `qex run` writes the output of the job, so it gives the exit code of the
    /// job. Without this, `qex run -- sh -c 'exit 7'` would give 1, and the
    /// command that `qex run` goes in front of would change its result.
    #[test]
    fn qex_run_gives_the_exit_code_of_a_job_that_ran() {
        assert_eq!(
            exit_code_for(&status_with(JobState::Completed, Some(0)), ExitMode::Run),
            0
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(7)), ExitMode::Run),
            7
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Failed, Some(1)), ExitMode::Run),
            1
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
            exit_code_for(&status_with(JobState::Killed, None), ExitMode::Run),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Cancelled, None), ExitMode::Run),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Timeout, None), ExitMode::Run),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Oom, None), ExitMode::Run),
            EXIT_KILLED
        );
        assert_eq!(
            exit_code_for(&status_with(JobState::Skipped, None), ExitMode::Run),
            EXIT_SKIPPED
        );
    }

    /// `qex run` and `qex wait` must never answer one question two ways. For
    /// each state in which the job gave no exit code, the two commands give one
    /// code. The test compares them against each other, so they cannot drift.
    #[test]
    fn qex_run_and_qex_wait_agree_for_a_job_that_did_not_give_a_code() {
        for state in [
            JobState::Killed,
            JobState::Cancelled,
            JobState::Timeout,
            JobState::Oom,
            JobState::Skipped,
            JobState::Expired,
        ] {
            let s = status_with(state, None);
            assert_eq!(
                exit_code_for(&s, ExitMode::Run),
                exit_code_for(&s, ExitMode::State),
                "the state {state} gives two codes"
            );
        }
    }

    /// A signal that is not a stop command leaves the job in the state `failed`
    /// with no exit code. `qex run` gives 1 there, and `qex wait` gives 1 too.
    #[test]
    fn a_signal_that_the_job_took_gives_one_from_both_commands() {
        let mut s = status_with(JobState::Failed, None);
        s.signal = Some(libc::SIGSEGV);
        assert_eq!(exit_code_for(&s, ExitMode::Run), 1);
        assert_eq!(exit_code_for(&s, ExitMode::State), 1);
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
