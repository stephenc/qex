//! This module holds the commands that talk to the coordinator.

use crate::cli::{self, StateFilter};
use crate::client::Client;
use crate::config::{Config, EnvCapture};
use crate::job::{JobState, JobStatus};
use crate::paths;
use crate::proto::{ErrorKind, Request, Response};
use crate::spec::{JobSpec, SubmitOptions};
use crate::units::{format_duration, format_size, parse_duration};
use anyhow::{bail, Result};
use std::time::{Duration, Instant};

/// The exit code of `qex wait` when the wait reached its time limit.
/// The command `timeout` uses the same code.
pub const EXIT_TIMEOUT: i32 = 124;
/// The exit code of `qex wait` when something stopped the job.
pub const EXIT_KILLED: i32 = 125;
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
    };

    let spec = JobSpec::resolve(&opts, &cfg)?;

    let mut client = Client::connect()?;
    match client.call(&Request::Submit {
        spec: Box::new(spec),
    })? {
        Response::Submitted { id, warning } => {
            // The warning goes to stderr. The id stays alone on stdout, so the
            // command `ID=$(qex submit ...)` continues to operate.
            if let Some(text) = warning {
                eprintln!("qex: {text}");
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
    let Response::Jobs { mut jobs } = client.call(&Request::List)? else {
        return report(client.call(&Request::List)?);
    };

    if let Some(f) = &filter {
        jobs.retain(|j| f.matches(j.state));
    }
    if let Some(tag) = &args.tag {
        jobs.retain(|j| j.tags.iter().any(|t| t == tag));
    }
    jobs.sort_by_key(|j| j.submitted_at);

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
    let id = resolve_id(&mut client, &args.id)?;

    match client.call(&Request::Status { id })? {
        Response::Status { status } => {
            if args.json {
                let mut value = serde_json::to_value(&*status)?;
                if args.show_env {
                    // The environment can hold secrets, so qex adds it only
                    // when the user asks for it.
                    if let Ok(spec) = crate::job::read_spec(&paths::job_dir(&id)?) {
                        value["env"] = serde_json::to_value(&spec.env)?;
                    }
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_status(&status, args.show_env)?;
            }
            Ok(0)
        }
        other => report(other),
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
    println!("claim:     {} core(s), {}", s.cpu, format_size(s.mem));

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
            println!("{} {} {}", short_id(&s.id), s.state, describe_result(s));
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
        return match client.recv() {
            Ok(Response::Status { status }) => Ok(WaitOutcome::Finished(*status)),
            Ok(Response::Error {
                kind: ErrorKind::NoSuchJob,
                ..
            }) => Ok(WaitOutcome::NoSuchJob),
            Ok(other) => {
                report(other)?;
                bail!("the coordinator gave an answer that qex did not expect")
            }
            Err(_) => Ok(WaitOutcome::TimedOut),
        };
    }

    // There is no coordinator. Read the file of the job.
    wait_on_file(raw_id, deadline)
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
    let id = match Client::connect_existing() {
        Some(mut c) => resolve_id(&mut c, &args.id)?,
        None => match find_id_on_disk(&args.id)? {
            Some(id) => id,
            None => {
                eprintln!("qex: there is no job with the id {}", args.id);
                return Ok(EXIT_NO_SUCH_JOB);
            }
        },
    };

    let dir = paths::job_dir(&id)?;
    let files: Vec<(&str, std::path::PathBuf)> = if args.stdout {
        vec![("", dir.join("stdout.log"))]
    } else if args.stderr {
        vec![("", dir.join("stderr.log"))]
    } else {
        vec![
            ("stdout", dir.join("stdout.log")),
            ("stderr", dir.join("stderr.log")),
        ]
    };

    if args.follow {
        return follow(&dir, &files);
    }

    for (label, path) in &files {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let text = match args.tail {
            Some(n) => {
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(n);
                lines[start..].join("\n")
            }
            None => text,
        };
        if text.is_empty() {
            continue;
        }
        if !label.is_empty() && files.len() > 1 {
            println!("==> {label} <==");
        }
        println!("{}", text.trim_end_matches('\n'));
    }
    Ok(0)
}

/// Writes the output of a job while the job operates.
fn follow(dir: &std::path::Path, files: &[(&str, std::path::PathBuf)]) -> Result<i32> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut handles = Vec::new();
    for (label, path) in files {
        let mut f = std::fs::File::open(path)
            .or_else(|_| std::fs::File::create(path).and_then(|_| std::fs::File::open(path)))?;
        f.seek(SeekFrom::Start(0))?;
        handles.push((label.to_string(), f));
    }

    let stdout = std::io::stdout();
    loop {
        let mut moved = false;
        for (_label, f) in handles.iter_mut() {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                stdout.lock().write_all(&buf)?;
                stdout.lock().flush()?;
                moved = true;
            }
        }

        // Stop when the job stops and the files hold no more data.
        if let Ok(status) = crate::job::read_status(dir) {
            if status.state.is_terminal() && !moved {
                return Ok(0);
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
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
        let id = resolve_id(&mut client, raw)?;
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
        let id = resolve_id(&mut client, raw)?;
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
             \x20   qex clean --state done\n\
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

    for raw in &args.ids {
        targets.push(resolve_id(&mut client, raw)?);
    }

    for j in &jobs {
        if !j.state.is_terminal() {
            continue;
        }
        let by_state = filter.as_ref().map(|f| f.matches(j.state)).unwrap_or(false);
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
        return status.exit_code.unwrap_or(match status.state {
            JobState::Completed => 0,
            _ => 1,
        });
    }

    match status.state {
        JobState::Completed => 0,
        JobState::Killed | JobState::Timeout | JobState::Oom | JobState::Cancelled => EXIT_KILLED,
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
    if let Ok(id) = raw.parse::<uuid::Uuid>() {
        return Ok(id);
    }

    let Response::Jobs { jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };

    let matches: Vec<&JobStatus> = jobs
        .iter()
        .filter(|j| j.id.to_string().starts_with(raw) || j.name == raw)
        .collect();

    match matches.len() {
        1 => Ok(matches[0].id),
        0 => bail!("there is no job with the id or the name `{raw}`"),
        n => bail!(
            "`{raw}` names {n} jobs. Write more characters of the id.\n{}",
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
    let mut client = Client::connect()?;
    match client.call(&Request::Info)? {
        Response::Info {
            pid,
            version,
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
            println!("version:         {version}");
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
            state,
            pid: Some(1),
            exit_code: code,
            signal: None,
            submitted_at: 0,
            started_at: Some(0),
            finished_at: Some(1),
            cpu: 1,
            mem: 1 << 30,
            usage: Usage::default(),
            forced: false,
            forced_reason: None,
            blocked_reason: None,
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
