//! This module stops jobs and deletes job records.
//!
//! qex signals the process group of a job, and not the first process only. A
//! job that forks children thus stops completely, and no process stays and
//! holds memory that qex counted.

use crate::daemon::{log, Coordinator};
use crate::job::{self, JobState};
use crate::paths;
use crate::proto::{ErrorKind, Response};
use std::sync::Arc;
use std::time::Duration;

/// Reads a signal name such as `TERM`, `SIGTERM`, `KILL` or `9`.
pub fn parse_signal(s: &str) -> Result<i32, String> {
    let t = s.trim().to_ascii_uppercase();
    let t = t.strip_prefix("SIG").unwrap_or(&t);

    if let Ok(n) = t.parse::<i32>() {
        if (1..=64).contains(&n) {
            return Ok(n);
        }
        return Err(format!("the signal number {n} is not in the range 1 to 64"));
    }

    match t {
        "TERM" => Ok(libc::SIGTERM),
        "KILL" => Ok(libc::SIGKILL),
        "INT" => Ok(libc::SIGINT),
        "HUP" => Ok(libc::SIGHUP),
        "QUIT" => Ok(libc::SIGQUIT),
        "USR1" => Ok(libc::SIGUSR1),
        "USR2" => Ok(libc::SIGUSR2),
        other => Err(format!(
            "unknown signal `{other}`. Use TERM, KILL, INT, HUP, QUIT, USR1, USR2, or a number."
        )),
    }
}

/// Stops one job.
///
/// qex sends the first signal, waits for the grace time, then sends `KILL`.
/// A job that handles `SIGTERM` can thus write its files before it stops.
pub fn kill(coord: &Arc<Coordinator>, id: uuid::Uuid, signal: i32, grace_secs: u64) -> Response {
    let pid = {
        let mut state = coord.state.lock().unwrap();
        // Read the status file first. The supervisor writes the process id
        // there, and this command needs that value to signal the job.
        state.refresh_active();
        let Some(job) = state.jobs.get(&id) else {
            return Response::error(
                ErrorKind::NoSuchJob,
                format!("there is no job with the id {id}"),
            );
        };

        match job.status.state {
            JobState::Queued => {
                return Response::error(
                    ErrorKind::WrongState,
                    format!("the job {id} waits in the queue. Use `qex cancel {id}`."),
                )
            }
            s if s.is_terminal() => {
                return Response::error(
                    ErrorKind::WrongState,
                    format!("the job {id} stopped. Its state is `{s}`."),
                )
            }
            _ => {}
        }

        match job.status.pid {
            Some(p) => p,
            None => {
                return Response::error(
                    ErrorKind::WrongState,
                    format!("the job {id} starts now. Try the command again."),
                )
            }
        }
    };

    // Signal the process group. The supervisor put the job in its own group,
    // so this call reaches each child of the job.
    let sent = unsafe { libc::killpg(pid, signal) };
    if sent != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) {
            // There is no process in that group. The job stopped in the moment
            // before this command. Report that result. A message that says
            // "the job received the signal" would be false.
            log(&format!("job {id} has no process; it stopped already"));
            return Response::error(
                ErrorKind::WrongState,
                format!(
                    "the job {id} has no process. It stopped in the moment before this \
                     command. Read `qex status {id}` for the result."
                ),
            );
        }
        return Response::error(
            ErrorKind::Internal,
            format!("qex could not signal the job {id}: {e}"),
        );
    }

    log(&format!("job {id} received the signal {signal}"));

    // Send KILL after the grace time. The job cannot avoid that signal.
    if signal != libc::SIGKILL && grace_secs > 0 {
        let coord = Arc::clone(coord);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(grace_secs));

            let still_active = {
                let state = coord.state.lock().unwrap();
                state
                    .jobs
                    .get(&id)
                    .map(|j| j.status.state.is_active())
                    .unwrap_or(false)
            };

            if still_active {
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
                log(&format!(
                    "job {id} did not stop in {grace_secs} seconds; qex sent KILL"
                ));
            }
        });
    }

    Response::Ok
}

/// Deletes the record of one job.
///
/// This command does not stop a job. A job that operates keeps its record.
pub fn clean(coord: &Arc<Coordinator>, id: uuid::Uuid) -> Response {
    // Keep the name and the state of this job. The dependents need them after
    // this job leaves the list.
    let (cause_name, cause_state) = {
        let state = coord.state.lock().unwrap();
        match state.jobs.get(&id) {
            // The SAFE name: this value goes into a sentence that a reader
            // sees, through `status.error`. See `job::safe_name`.
            Some(job) => (job.status.display_name(), job.status.state.to_string()),
            None => (String::from("unknown"), String::from("unknown")),
        }
    };

    {
        let state = coord.state.lock().unwrap();
        match state.jobs.get(&id) {
            None => {
                return Response::error(
                    ErrorKind::NoSuchJob,
                    format!("there is no job with the id {id}"),
                )
            }
            Some(job) if !job.status.state.is_terminal() => {
                return Response::error(
                    ErrorKind::WrongState,
                    format!(
                        "the job {id} is in the state `{}`. Stop it first with `qex kill {id}`.",
                        job.status.state
                    ),
                )
            }
            Some(_) => {}
        }

        // Keep a job that a job in the queue needs.
        //
        // Without this rule, the record of the cause disappears, and a job that
        // waits for it cannot report why it did not run.
        let waiting: Vec<String> = state
            .jobs
            .values()
            .filter(|j| !j.status.state.is_terminal())
            .filter(|j| j.spec.needs.contains(&id) || j.spec.after.contains(&id))
            .map(|j| {
                format!(
                    "{} ({})",
                    &j.status.id.to_string()[..8],
                    j.status.display_name()
                )
            })
            .collect();

        if !waiting.is_empty() {
            return Response::error(
                ErrorKind::WrongState,
                format!(
                    "the job {id} is needed by {}. Wait for {}, or cancel {}.",
                    waiting.join(", "),
                    if waiting.len() == 1 {
                        "that job"
                    } else {
                        "those jobs"
                    },
                    if waiting.len() == 1 { "it" } else { "them" }
                ),
            );
        }
    }

    // Record the removal before the deletion. A reader of `qex status` can then
    // learn that this job existed and that its work happened.
    {
        let state = coord.state.lock().unwrap();
        if let Some(job) = state.jobs.get(&id) {
            let status = job.status.clone();
            drop(state);
            crate::history::record_removed(&status);
        }
    }

    let dir = match paths::job_dir(&id) {
        Ok(d) => d,
        Err(e) => return Response::error(ErrorKind::Internal, e.to_string()),
    };

    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Response::error(
                ErrorKind::Internal,
                format!("qex could not delete {}: {e}", dir.display()),
            );
        }
    }

    let mut state = coord.state.lock().unwrap();
    state.jobs.remove(&id);
    state.queue.retain(|q| *q != id);

    // Free the dedupe key of this job with its record.
    //
    // The key must go at the same moment as the record. A key that names a job
    // with no record would give an id that `qex status` cannot answer, and the
    // caller could not learn anything about the work.
    crate::daemon::release_dedupe(&mut state, id);

    // Make each job that names this job as its cause self-contained.
    //
    // A skipped job holds `caused_by` and a text that says "Read `qex logs X`
    // for the cause". After the deletion of X, that text sends the reader to a
    // job that does not exist. The one thing that `caused_by` exists to give
    // would then be lost.
    //
    // Write the name and the state of the deleted job into the text of each
    // dependent, so the record still answers the question.
    let dependents: Vec<uuid::Uuid> = state
        .jobs
        .values()
        .filter(|j| j.status.caused_by == Some(id))
        .map(|j| j.status.id)
        .collect();

    for dep in dependents {
        if let Some(job) = state.jobs.get_mut(&dep) {
            job.status.error = Some(format!(
                "the job `{}` ({}) did not succeed, so this job did not run. \
                 Its record is deleted, so there is no log to read.",
                cause_name, cause_state
            ));
            job.status.caused_by = None;
            let status = job.status.clone();
            if let Ok(dir) = paths::job_dir(&dep) {
                job::write_status(&dir, &status).ok();
            }
        }
    }
    drop(state);

    coord.notify();
    Response::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_parse_in_each_usual_form() {
        assert_eq!(parse_signal("TERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("SIGTERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("term").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("KILL").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("9").unwrap(), 9);
        assert_eq!(parse_signal("INT").unwrap(), libc::SIGINT);
    }

    #[test]
    fn an_unknown_signal_gives_a_message_with_the_permitted_names() {
        let err = parse_signal("BANANA").unwrap_err();
        assert!(err.contains("TERM"), "the error must list the names: {err}");
        assert!(
            parse_signal("0").is_err(),
            "the signal 0 tests a process only"
        );
        assert!(parse_signal("99").is_err());
    }
}
