//! This module holds the `qex top` command.
//!
//! The command shows the queue and refreshes it. For each job it shows the
//! claim and the true use at this moment, so a person can see immediately that
//! a claim is much larger than the need.
//!
//! The command uses simple terminal codes and no library. It clears the screen
//! and writes the page again for each refresh.

use crate::client::Client;
use crate::job::{JobState, JobStatus};
use crate::proto::{Request, Response};
use crate::sys;
use crate::units::{format_duration, format_size};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Moves the cursor to the corner and clears the screen.
const CLEAR: &str = "\x1b[2J\x1b[H";

/// One measurement of a job, to calculate the CPU use between two refreshes.
struct Previous {
    cpu_secs: f64,
    at: Instant,
}

pub fn run(args: crate::cli::TopArgs) -> Result<i32> {
    let interval = Duration::from_secs_f64(args.interval.max(0.2));
    let mut previous: HashMap<uuid::Uuid, Previous> = HashMap::new();

    loop {
        // Read the queue from the coordinator when one operates, and from the
        // state directory when none does.
        //
        // This command never starts a coordinator. A command that watches must
        // not change the thing that it watches. It must also give an answer
        // when no coordinator operates: the supervisor of each job writes its
        // own record, so those records hold the truth at every moment, and a
        // job that operates can still be measured by its process group.
        let (jobs, info) = match Client::connect_existing() {
            Some(mut client) => {
                let Response::Jobs { mut jobs } = client.call(&Request::List)? else {
                    bail!("the coordinator did not give the job list");
                };
                jobs.sort_by_key(|j| (j.submitted_at, j.sequence));
                (jobs, Some(client.call(&Request::Info)?))
            }
            None => (crate::job::read_all_from_disk(), None),
        };

        if args.once {
            // The CPU column is the change in the CPU time between two
            // measurements, so one page needs two of them. Take the first
            // measurement, wait a short time, then write the page.
            render(&jobs, info.as_ref(), &mut previous);
            std::thread::sleep(Duration::from_millis(400));
            print!("{}", render(&jobs, info.as_ref(), &mut previous));
            return Ok(0);
        }

        let page = render(&jobs, info.as_ref(), &mut previous);

        print!("{CLEAR}{page}");
        use std::io::Write;
        std::io::stdout().flush().ok();
        std::thread::sleep(interval);
    }
}

fn render(
    jobs: &[JobStatus],
    info: Option<&Response>,
    previous: &mut HashMap<uuid::Uuid, Previous>,
) -> String {
    let mut out = String::new();

    if let Some(Response::Info {
        version,
        program_replaced,
        cpu_budget,
        mem_budget,
        cpu_claimed,
        mem_claimed,
        jobs_running,
        jobs_queued,
        ..
    }) = info
    {
        out.push_str(&format!(
            "qex   budget {cpu_claimed}/{cpu_budget} cores, {}/{} memory   \
             {jobs_running} running, {jobs_queued} queued\n",
            format_size(*mem_claimed),
            format_size(*mem_budget),
        ));
        // Give both versions. A coordinator can hold the code of an earlier
        // build, and that difference caused a fault that named no cause.
        let mine = env!("CARGO_PKG_VERSION");
        if version != mine {
            out.push_str(&format!(
                "      WARNING: the coordinator is version {version} and this command is \
                 {mine}\n"
            ));
        } else {
            out.push_str(&format!("      version {version}\n"));
        }
        if *program_replaced {
            out.push_str(
                "      the qex program changed; this coordinator stops when no job operates\n",
            );
        }
    }

    if info.is_none() {
        // No coordinator. Give the budget from the config file, and count the
        // jobs from their records.
        let cfg = crate::config::Config::load().unwrap_or_default();
        let active: Vec<&JobStatus> = jobs.iter().filter(|j| j.state.is_active()).collect();
        let queued = jobs.iter().filter(|j| j.state == JobState::Queued).count();
        let cpu: u64 = active.iter().map(|j| j.cpu).sum();
        let mem: u64 = active.iter().map(|j| j.mem).sum();

        out.push_str(&format!(
            "qex   budget {cpu}/{} cores, {}/{} memory   {} running, {queued} queued\n",
            cfg.budget_cpu().unwrap_or(0),
            format_size(mem),
            format_size(cfg.budget_mem().unwrap_or(0)),
            active.len(),
        ));
        out.push_str(
            "      no coordinator operates. These records come from the state directory.\n\
             \x20     qex starts a coordinator when you submit a job.\n",
        );
    }

    out.push_str(&format!(
        "machine  {} cores, {} free of {}\n\n",
        sys::cpu_count(),
        format_size(sys::available_memory()),
        format_size(sys::total_memory()),
    ));

    out.push_str(&format!(
        "{:<8}  {:<9}  {:<14}  {:>9}  {:>9}  {:>17}  {:>6}  {}\n",
        "ID", "STATE", "NAME", "CPU CLAIM", "CPU NOW", "MEMORY CLAIM/NOW", "TIME", "NOTE"
    ));

    if jobs.is_empty() {
        out.push_str("\nno jobs\n");
        return out;
    }

    for job in jobs {
        // Measure the job now, for a job that operates.
        let (cpu_now, mem_now) = match (job.state.is_active(), job.pid) {
            (true, Some(pid)) => {
                let usage = sys::group_usage(pid);
                let now = Instant::now();
                // The CPU use is the change in the CPU time, divided by the
                // time between the two measurements. The result is a number of
                // cores, so 2.0 means two cores in full use.
                let cores = previous.get(&job.id).map(|p| {
                    let seconds = now.duration_since(p.at).as_secs_f64();
                    if seconds > 0.0 {
                        ((usage.cpu_secs - p.cpu_secs) / seconds).max(0.0)
                    } else {
                        0.0
                    }
                });
                previous.insert(
                    job.id,
                    Previous {
                        cpu_secs: usage.cpu_secs,
                        at: now,
                    },
                );
                (cores, Some(usage.rss))
            }
            _ => {
                previous.remove(&job.id);
                (None, None)
            }
        };

        let cpu_text = match cpu_now {
            // The first refresh has no earlier measurement to compare with.
            None if job.state.is_active() => "...".to_string(),
            None => "-".to_string(),
            Some(c) => format!("{c:.1}"),
        };

        let mem_text = match mem_now {
            Some(rss) => format!("{} / {}", format_size(job.mem), format_size(rss)),
            None if job.usage.max_rss > 0 => {
                format!("{} / {}", format_size(job.mem), format_size(job.usage.max_rss))
            }
            None => format!("{} / -", format_size(job.mem)),
        };

        let elapsed = job
            .elapsed()
            .map(format_duration)
            .unwrap_or_else(|| "-".to_string());

        let note = note_for(job);

        out.push_str(&format!(
            "{:<8}  {:<9}  {:<14.14}  {:>9}  {:>9}  {:>17}  {:>6}  {:.40}\n",
            &job.id.to_string()[..8],
            job.state.as_str(),
            job.name,
            job.cpu,
            cpu_text,
            mem_text,
            elapsed,
            note
        ));
    }

    out.push_str("\nThe CPU column gives cores in use. Press Ctrl-C to stop.\n");
    out
}

/// Gives the short note for one job.
fn note_for(job: &JobStatus) -> String {
    if let Some(reason) = &job.blocked_reason {
        return reason.clone();
    }
    if job.forced {
        return "FORCED: larger than the budget".to_string();
    }
    match job.state {
        JobState::Completed => "ok".to_string(),
        JobState::Failed => match job.exit_code {
            Some(c) => format!("exit code {c}"),
            None => "failed".to_string(),
        },
        JobState::Skipped => "a job that it needed did not succeed".to_string(),
        JobState::Oom => "out of memory".to_string(),
        JobState::Timeout => "reached its time limit".to_string(),
        JobState::Killed => "stopped by a command".to_string(),
        JobState::Cancelled => "left the queue".to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Usage;

    fn job(state: JobState, cpu: u64, mem: u64) -> JobStatus {
        JobStatus {
            id: uuid::Uuid::new_v4(),
            name: "example".into(),
            command: vec!["true".into()],
            cwd: "/".into(),
            state,
            pid: Some(std::process::id() as i32),
            supervisor_pid: None,
            exit_code: None,
            signal: None,
            submitted_at: 0,
            sequence: 1,
            started_at: Some(0),
            finished_at: None,
            cpu,
            mem,
            claim_source: "explicit".into(),
            usage: Usage::default(),
            forced: false,
            forced_reason: None,
            blocked_reason: None,
            error: None,
            needs: vec![],
            after: vec![],
            caused_by: None,
            tags: vec![],
        }
    }

    fn info() -> Response {
        Response::Info {
            pid: 1,
            version: "test".into(),
            started_at: 0,
            program_replaced: false,
            jobs_running: 1,
            jobs_queued: 0,
            cpu_budget: 12,
            mem_budget: 20 << 30,
            cpu_claimed: 2,
            mem_claimed: 4 << 30,
        }
    }

    #[test]
    fn the_page_holds_the_budget_and_the_jobs() {
        let jobs = vec![job(JobState::Running, 2, 4 << 30)];
        let mut previous = HashMap::new();
        let page = render(&jobs, Some(&info()), &mut previous);

        assert!(page.contains("2/12 cores"), "the budget is missing: {page}");
        assert!(page.contains("example"), "the job name is missing");
        assert!(page.contains("4GB"), "the memory claim is missing");
    }

    /// The first refresh cannot give a CPU value, because there is no earlier
    /// measurement to compare with. The second refresh gives one.
    #[test]
    fn the_cpu_column_needs_two_measurements() {
        let jobs = vec![job(JobState::Running, 1, 1 << 30)];
        let mut previous = HashMap::new();

        let first = render(&jobs, Some(&info()), &mut previous);
        assert!(first.contains("..."), "the first page has no earlier value");

        std::thread::sleep(Duration::from_millis(50));
        let second = render(&jobs, Some(&info()), &mut previous);
        assert!(
            !second.contains("..."),
            "the second page must give a number: {second}"
        );
    }

    /// A job that stopped shows its measured peak, and not a live value.
    #[test]
    fn a_job_that_stopped_shows_its_measurement() {
        let mut j = job(JobState::Completed, 1, 1 << 30);
        j.usage.max_rss = 500 << 20;
        j.finished_at = Some(10);

        let mut previous = HashMap::new();
        let page = render(&[j], Some(&info()), &mut previous);
        assert!(page.contains("500MB"), "the measurement is missing: {page}");
        assert!(page.contains("ok"), "the result is missing");
    }

    /// The command must give the jobs when no coordinator operates.
    ///
    /// A command that watches must not depend on the thing that it watches, and
    /// it must not start it either.
    #[test]
    fn the_page_holds_the_jobs_with_no_coordinator() {
        let mut j = job(JobState::Completed, 2, 1 << 30);
        j.usage.max_rss = 100 << 20;
        j.finished_at = Some(5);

        let mut previous = HashMap::new();
        let page = render(&[j], None, &mut previous);

        assert!(page.contains("example"), "the job is missing: {page}");
        assert!(
            page.contains("no coordinator"),
            "the page must say that no coordinator operates: {page}"
        );
        assert!(
            page.contains("state directory"),
            "the page must say where the records come from: {page}"
        );
        // The budget still comes from the config file.
        assert!(page.contains("cores"), "the budget is missing: {page}");
    }

    #[test]
    fn an_empty_queue_says_so() {
        let mut previous = HashMap::new();
        let page = render(&[], Some(&info()), &mut previous);
        assert!(page.contains("no jobs"));
    }

    /// A job that waits must show the reason in the note.
    #[test]
    fn a_job_that_waits_shows_the_reason() {
        let mut j = job(JobState::Queued, 4, 1 << 30);
        j.blocked_reason = Some("waits for cores: 12 of 12 are in use".into());
        j.started_at = None;

        let mut previous = HashMap::new();
        let page = render(&[j], Some(&info()), &mut previous);
        assert!(page.contains("waits for cores"), "got: {page}");
    }
}
