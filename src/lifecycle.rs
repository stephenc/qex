//! This module stops jobs and deletes job records.
//!
//! qex signals the process group of a job, and not the first process only. A
//! job that forks children thus stops completely, and no process stays and
//! holds memory that qex counted.

use crate::daemon::{log, Coordinator, State};
use crate::job::{self, JobState, JobStatus};
use crate::paths;
use crate::proto::{AbortedJob, ErrorKind, Response};
use std::path::Path;
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
    let (pid, pid_start) = {
        let mut state = coord.state.lock().unwrap();
        // Read the status file first. The supervisor writes the process id
        // there, and this command needs that value to signal the job.
        state.refresh_active();
        state.publish_changes();
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
            Some(p) => (p, job.status.pid_start_token),
            None => {
                return Response::error(
                    ErrorKind::WrongState,
                    format!("the job {id} starts now. Try the command again."),
                )
            }
        }
    };

    // Record the cause BEFORE the signal.
    //
    // The kernel uses SIGKILL for an out-of-memory kill, and this command uses
    // the same signal. The state `oom` names the memory of the machine as the
    // cause, so a job that a person stopped must never take it: the reader who
    // sent this command knows the cause, and qex must not give a different one.
    //
    // The mark goes to the disk first, because the supervisor can read the
    // record in the moment after the signal.
    if let Ok(dir) = paths::job_dir(&id) {
        crate::enforce::mark_user_kill(&dir);
    }

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

            // Read the pid again after the sleep. The grace time is long: the
            // job can stop in it and the machine can give its number to a new
            // process, and a job with `--retries` can start a new attempt
            // with a new pid. The KILL goes to the group of the recorded pid
            // only while that pid is still the pid of this job, only while it
            // still leads its group, and only while the process has the start
            // time that the record gave at the first signal.
            let pid_now = {
                let state = coord.state.lock().unwrap();
                state
                    .jobs
                    .get(&id)
                    .filter(|j| j.status.state.is_active())
                    .and_then(|j| j.status.pid)
            };

            if pid_now == Some(pid)
                && crate::sys::job_pid_alive(pid)
                && crate::sys::same_process_start(pid, pid_start)
            {
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
    // The dependency check and the removal from `state.jobs` are one critical
    // section. `handle_submit` validates dependencies under this same mutex,
    // so no submission can name this job in the window between the check and
    // the removal.
    let mut state = coord.state.lock().unwrap();

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
    //
    // This test is the LAST one, so every deletion meets it, whatever asked
    // for the deletion. It uses the rule of `crate::deps`, which `qex clean`
    // and `qex gc` use as well: one rule gives one answer, and a command
    // cannot say that it kept a record while this code deletes it.
    if let Some(hold) = crate::deps::hold_reason(&dep_nodes(&state), id) {
        // Each rule is a DIFFERENT relation, so each gets its own words. A
        // stage of a pipeline that no other stage waits for is not a job
        // that another job needs, and a message that says so is false.
        let text = match &hold {
            crate::deps::Hold::Needed(holders) => {
                let one = holders.len() == 1;
                format!(
                    "the job {id} is needed by {}. Wait for {}, or cancel {}.",
                    named(&state, holders),
                    if one { "that job" } else { "those jobs" },
                    if one { "it" } else { "them" }
                )
            }
            crate::deps::Hold::Pipeline(holders) => {
                let one = holders.len() == 1;
                format!(
                    "the job {id} belongs to a pipeline that has work left: {}. \
                         The record of a stage stays while the pipeline operates, so that \
                         a reader can see the whole pipeline. Wait for {}, or cancel {}.",
                    named(&state, holders),
                    if one { "that job" } else { "those jobs" },
                    if one { "it" } else { "them" }
                )
            }
        };

        return Response::error(ErrorKind::WrongState, text);
    }

    // Remove the job while the dependency check is still protected by the
    // same lock. A submit can run as soon as this lock goes, and it must then
    // see either the job throughout or no job throughout.
    let Some(removal) = remove_record(&mut state, id) else {
        return Response::error(
            ErrorKind::NoSuchJob,
            format!("there is no job with the id {id}"),
        );
    };
    state.publish_changes();
    drop(state);
    coord.notify();

    match finish_removal(removal) {
        Ok(()) => Response::Ok,
        Err(message) => Response::error(ErrorKind::Internal, message),
    }
}

/// Gives the records in the form that the rule of `crate::deps` reads.
fn dep_nodes(state: &State) -> Vec<crate::deps::Node<'_>> {
    state
        .jobs
        .values()
        .map(|j| crate::deps::Node {
            id: j.status.id,
            group: j.status.group,
            terminal: j.status.state.is_terminal(),
            needs: &j.spec.needs,
            after: &j.spec.after,
        })
        .collect()
}

/// Names jobs for a sentence, as `a1b2c3d4 (train), e5f6a7b8 (test)`.
fn named(state: &State, ids: &[uuid::Uuid]) -> String {
    ids.iter()
        .filter_map(|holder| {
            state.jobs.get(holder).map(|j| {
                format!(
                    "{} ({})",
                    &j.status.id.to_string()[..8],
                    j.status.display_name()
                )
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The removal of one record, in two parts.
///
/// `remove_record` changes the memory of the coordinator, and it runs under the
/// lock. `finish_removal` does the disk work, and it must NOT run under the
/// lock: the deletion of a large log is slow, and a lock held for it stops the
/// scheduler. This structure carries what the second part needs from the
/// first.
pub struct Removal {
    status: JobStatus,
    /// The jobs that named the removed job as their cause, with the record
    /// that each one holds now.
    dependents: Vec<(uuid::Uuid, JobStatus)>,
}

/// Takes one record out of the memory of the coordinator.
///
/// The caller holds the lock, and it made the checks: the job stopped, and no
/// job needs its record. Gives `None` when there is no such job.
fn remove_record(state: &mut State, id: uuid::Uuid) -> Option<Removal> {
    let removed = state.jobs.remove(&id)?;
    // The SAFE name: this value goes into a sentence that a reader sees,
    // through `status.error`. See `job::safe_name`.
    let cause_name = removed.status.display_name();
    let cause_state = removed.status.state.to_string();

    state.queue.retain(|q| *q != id);

    // Free the dedupe key of this job with its record.
    //
    // The key must go at the same moment as the record. A key that names a job
    // with no record would give an id that `qex status` cannot answer, and the
    // caller could not learn anything about the work.
    crate::daemon::release_dedupe(state, id);

    // Make each job that names this job as its cause self-contained.
    //
    // A skipped job holds `caused_by` and a text that says "Read `qex logs X`
    // for the cause". After the deletion of X, that text sends the reader to a
    // job that does not exist. The one thing that `caused_by` exists to give
    // would then be lost.
    //
    // Write the name and the state of the deleted job into the text of each
    // dependent, so the record still answers the question.
    let mut dependents = Vec::new();
    for job in state.jobs.values_mut() {
        if job.status.caused_by != Some(id) {
            continue;
        }
        job.status.error = Some(format!(
            "the job `{}` ({}) did not succeed, so this job did not run. \
             Its record is deleted, so there is no log to read.",
            cause_name, cause_state
        ));
        job.status.caused_by = None;
        dependents.push((job.status.id, job.status.clone()));
    }

    Some(Removal {
        status: removed.status,
        dependents,
    })
}

/// Does the disk work of one removal.
///
/// Gives the fault when the directory stays. Such a record is not deleted: the
/// next coordinator reads the directory and holds the job again.
fn finish_removal(removal: Removal) -> Result<(), String> {
    for (dep, status) in removal.dependents {
        if let Ok(dir) = paths::job_dir(&dep) {
            job::write_status(&dir, &status).ok();
        }
    }

    // Record the removal before deleting the directory. A reader of history
    // can then learn that this job existed and that its work happened.
    crate::history::record_removed(&removal.status);

    let dir = paths::job_dir(&removal.status.id).map_err(|e| e.to_string())?;
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("qex could not delete {}: {e}", dir.display())),
    }
}

/// What the part of an abort that runs under the lock decided.
///
/// The lists name jobs, and the counts come from the lists. A number that a
/// later change computed from the request would say what the reader asked
/// for, and the answer must say what qex did.
pub struct AbortPlan {
    /// The jobs that waited and that the plan cancelled, with the record that
    /// each one holds now. The caller writes each record to the disk and runs
    /// the stop hook, as `qex cancel` does.
    pub cancelled: Vec<(uuid::Uuid, JobStatus)>,
    /// The queued jobs whose record qex could not write. Each one stays in
    /// the queue, and the answer names it.
    pub not_cancelled: Vec<AbortedJob>,
    /// The jobs of the scope that operate: each one is a job to signal.
    pub running: Vec<(uuid::Uuid, String)>,
    /// The jobs that wait or operate outside the scope.
    pub outside: usize,
}

/// Tests one job against the scope of an abort.
///
/// Every part of the scope narrows it, and a part that is `None` narrows
/// nothing. The directory is the directory OF THE JOB, as `qex status` shows
/// it, and not the directory of the process that submitted it: a reader can
/// see the first one on every record.
///
/// `boot` is the identifier of the current start of the machine. The start
/// time of a process counts from that start, and a queued record survives a
/// restart of the machine, so a record from an earlier start could name a
/// process of this start by accident. Such a record is outside every context.
fn in_scope(job: &crate::daemon::Job, scope: &crate::proto::AbortScope, boot: &str) -> bool {
    if let Some(dir) = &scope.cwd {
        if Path::new(&job.status.cwd) != Path::new(dir) {
            return false;
        }
    }
    if let Some(caller) = &scope.submitter {
        if job.status.boot_id.as_deref() != Some(boot) {
            return false;
        }
        if !crate::context::shared(&job.status.submitter, caller) {
            return false;
        }
    }
    scope.tags.is_empty() || scope.tags.iter().any(|t| job.spec.tags.contains(t))
}

/// The part of an abort that runs under the lock: the pause, the cancel, and
/// the list of the jobs to signal.
///
/// THE THREE STEPS ARE ONE LOCK HOLD, AND THAT IS THE WHOLE GUARANTEE. The
/// scheduler moves a job from the queue to a process under this same lock,
/// and it reads the pause there. So after this function returns, every job
/// that waited in the scope is cancelled, every job of the scope that left the
/// queue is in `running`, and no job of the queue can start until a resume.
/// A command that paused, released the lock, and then cancelled would let the
/// scheduler start a job between the two steps, which is the fault that a
/// cancel loop in a script has.
///
/// THE RECORD OF A CANCELLED JOB STAYS, in the state `cancelled`, exactly as
/// after `qex cancel`. A reader that waits for the job, a reader that asks
/// `qex status`, and the stop hook each need that record, and each must get
/// the answer that a cancel gives. `qex clean cancelled` deletes the records.
pub fn plan_abort(
    state: &mut State,
    scope: &crate::proto::AbortScope,
    by_pid: i32,
    boot: &str,
) -> AbortPlan {
    // The process id of each job that operates comes from the disk. Without
    // this read, a job that started a moment ago has no pid and no signal
    // reaches it.
    state.refresh_active();

    // The pause comes FIRST. A pause that a person made earlier keeps its end
    // and its reason; see `keep_the_end`.
    let reason = state
        .paused
        .queue
        .is_none()
        .then(|| String::from("qex abort"));
    let record = crate::daemon::keep_the_end(state.paused.queue.take(), by_pid, reason, None);
    state.paused.queue = Some(record);
    state.save_pause();

    let now = crate::sys::now_secs();
    let mut cancelled = Vec::new();
    let mut not_cancelled = Vec::new();
    let mut running = Vec::new();
    let mut outside = 0usize;
    for job in state.jobs.values_mut() {
        if !in_scope(job, scope, boot) {
            if !job.status.state.is_terminal() {
                outside += 1;
            }
            continue;
        }
        match job.status.state {
            JobState::Queued => {
                let mut next = job.status.clone();
                next.state = JobState::Cancelled;
                next.finished_at = Some(now);
                next.blocked_reason = None;
                // A cancel that a person asked for leaves no error text. See
                // `cancel_queued` in the daemon module.
                next.error = None;
                // THE RECORD GOES TO THE DISK FIRST, UNDER THE LOCK, AND THE
                // MEMORY FOLLOWS. A coordinator can die at any moment, and
                // the next coordinator reads the disk. A record that this
                // coordinator cancelled in memory and did not write would
                // come back as a queued job, and one `qex resume queue` would
                // start the work that the abort stopped. A record that qex
                // could not write therefore stays queued in memory as well,
                // and the answer names it: the count of cancelled jobs is the
                // count of records on the disk that say so.
                //
                // The write does not wait for the disk. Every other request
                // waits for this lock, and a wait for the disk on each of
                // thousands of records would hold them all for the whole
                // abort. The page cache survives the death of the
                // coordinator, which is the death that this write protects
                // against.
                let written = paths::job_dir(&job.status.id)
                    .map_err(|e| e.to_string())
                    .and_then(|dir| {
                        job::write_status_unsynced(&dir, &next).map_err(|e| format!("{e:#}"))
                    });
                match written {
                    Ok(()) => {
                        job.status = next;
                        cancelled.push((job.status.id, job.status.clone()));
                    }
                    Err(why) => {
                        log(&format!(
                            "qex abort could not write the record of the job {}: {why}",
                            job.status.id
                        ));
                        not_cancelled.push(AbortedJob {
                            id: job.status.id,
                            name: job.status.display_name(),
                            why,
                        });
                    }
                }
            }
            JobState::Starting | JobState::Running => {
                running.push((job.status.id, job.status.display_name()));
            }
            _ => {}
        }
    }

    // One pass over the queue for every cancelled job. A pass for each job
    // would cost the square of the queue, and the queue is large exactly when
    // this command matters.
    let gone: std::collections::HashSet<uuid::Uuid> = cancelled.iter().map(|(id, _)| *id).collect();
    state.queue.retain(|q| !gone.contains(q));
    state.publish_changes();

    AbortPlan {
        cancelled,
        not_cancelled,
        running,
        outside,
    }
}

/// Names the variable that makes the coordinator stop after the lock section
/// of an abort. **This variable exists for the tests of qex**, and a release
/// build does not read it.
#[cfg(debug_assertions)]
const CRASH_AFTER_PLAN: &str = "QEX_TEST_CRASH_AFTER_ABORT_PLAN";

/// How long an abort waits for a job that is between the queue and its first
/// process.
///
/// The scheduler took such a job out of the queue before the abort, so the
/// abort must signal it, and the signal needs the pid that the supervisor
/// writes a moment after the fork. A job that has no pid after this time is
/// reported, with the command that stops it.
const STARTING_WAIT: Duration = Duration::from_secs(5);

/// Tests if a job left the queue and has no process yet.
fn starts_now(coord: &Arc<Coordinator>, id: uuid::Uuid) -> bool {
    let state = coord.state.lock().unwrap();
    state
        .jobs
        .get(&id)
        .map(|j| j.status.state.is_active() && j.status.pid.is_none())
        .unwrap_or(false)
}

/// Stops the jobs of one scope, and empties their part of the queue.
///
/// See `plan_abort` for the part that gives the guarantee. This function does
/// the disk work outside the lock, and it then stops each job that operates
/// with `kill`, which is the one way that qex stops a process tree.
pub fn abort(
    coord: &Arc<Coordinator>,
    scope: crate::proto::AbortScope,
    keep_running: bool,
    grace_secs: u64,
    by_pid: i32,
) -> Response {
    let boot = crate::sys::boot_id();
    let plan = {
        let mut state = coord.state.lock().unwrap();
        plan_abort(&mut state, &scope, by_pid, &boot)
    };
    // Wake the scheduler, so the jobs that wait get the pause as their reason.
    coord.notify();

    // A test of qex stops the coordinator here, in the moment after the lock
    // and before any answer. The next coordinator must then hold every
    // cancelled job as cancelled. A release build has no such switch: a
    // coordinator inherits the environment of the shell that started it, and
    // a variable that a person left set must not stop a real queue.
    #[cfg(debug_assertions)]
    if std::env::var_os(CRASH_AFTER_PLAN).is_some() {
        log("qex stops here because QEX_TEST_CRASH_AFTER_ABORT_PLAN is set");
        std::process::abort();
    }

    // The stop hook, for each cancelled job, outside the lock: a reader whose
    // filter names `cancelled` gets one line for each job, as after
    // `qex cancel`. The record itself went to the disk under the lock.
    for (id, status) in &plan.cancelled {
        if let Ok(dir) = paths::job_dir(id) {
            crate::hook::fire_detached(&dir, status);
        }
    }

    let mut signalled = Vec::new();
    let mut not_stopped = Vec::new();
    let mut continues = Vec::new();

    if keep_running {
        continues.extend(plan.running.into_iter().map(|(id, _)| id));
    } else {
        let deadline = std::time::Instant::now() + STARTING_WAIT;
        let mut pending = plan.running;
        loop {
            let mut later = Vec::new();
            for (id, name) in pending {
                match kill(coord, id, libc::SIGTERM, grace_secs) {
                    Response::Ok => signalled.push(id),
                    Response::Error { message, .. } => {
                        if starts_now(coord, id) && std::time::Instant::now() < deadline {
                            later.push((id, name));
                        } else {
                            not_stopped.push(AbortedJob {
                                id,
                                name,
                                why: message,
                            });
                        }
                    }
                    other => not_stopped.push(AbortedJob {
                        id,
                        name,
                        why: format!("qex gave an answer that this command cannot read: {other:?}"),
                    }),
                }
            }
            if later.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            pending = later;
        }
    }

    log(&format!(
        "qex abort: {} cancelled, {} not cancelled, {} signalled, {} not stopped, \
         {} continue, {} outside the scope; the queue is paused",
        plan.cancelled.len(),
        plan.not_cancelled.len(),
        signalled.len(),
        not_stopped.len(),
        continues.len(),
        plan.outside,
    ));

    Response::Aborted {
        cancelled: plan.cancelled.len(),
        not_cancelled: plan.not_cancelled,
        signalled,
        not_stopped,
        continues,
        outside: plan.outside,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued_job(boot: Option<&str>, chain: Vec<crate::job::Ancestor>) -> crate::daemon::Job {
        let spec = crate::spec::JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "t".into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu: 1,
            mem: 1 << 20,
            timeout: None,
            max_queue_time: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            locks: vec![],
            claims: Default::default(),
            retries: 0,
            nice: None,
            needs: vec![],
            after: vec![],
            dedupe_key: None,
            dedupe_window: 0,
            learn_key: None,
            submitted_at: 0,
        };
        let mut status = JobStatus::new(&spec);
        status.boot_id = boot.map(String::from);
        status.submitter = chain;
        crate::daemon::Job {
            spec,
            status,
            supervisor_pid: None,
        }
    }

    fn chain() -> Vec<crate::job::Ancestor> {
        vec![
            crate::job::Ancestor {
                pid: 50,
                ppid: 1,
                start: Some(7),
                name: "claude".into(),
                cwd: None,
                terminal: true,
            },
            crate::job::Ancestor {
                pid: 1,
                ppid: 0,
                start: Some(0),
                name: "systemd".into(),
                cwd: None,
                terminal: false,
            },
        ]
    }

    /// Points the state directory at a directory of this test.
    ///
    /// `plan_abort` writes the pause file, and a unit test inherits the
    /// environment of the person who runs it. Without this guard the test
    /// would pause the real queue of that person.
    fn isolated_state() -> (
        std::sync::MutexGuard<'static, ()>,
        crate::testutil::EnvVar,
        std::path::PathBuf,
    ) {
        let lock = crate::testutil::env_lock();
        let dir = std::env::temp_dir().join(format!(
            "qex-lifecycle-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let var = crate::testutil::EnvVar::set("XDG_STATE_HOME", dir.to_str().unwrap());
        (lock, var, dir)
    }

    /// A record from an earlier start of the machine is outside the context,
    /// whatever its chain says: the start time of a process counts from the
    /// start of the machine, so the chain can name a process of this start by
    /// accident.
    #[test]
    fn a_record_from_an_earlier_start_of_the_machine_is_outside_the_context() {
        let (_lock, _var, dir) = isolated_state();
        let coord = Arc::new(Coordinator::new(crate::config::Config::default(), 0));
        let mut state = coord.state.lock().unwrap();
        let this_start = queued_job(Some("boot-now"), chain());
        let earlier_start = queued_job(Some("boot-before"), chain());
        let ids = [this_start.status.id, earlier_start.status.id];
        for job in [this_start, earlier_start] {
            // The cancel goes to the disk first, so each job needs its
            // directory, as a submission makes it.
            std::fs::create_dir_all(paths::job_dir(&job.status.id).unwrap()).unwrap();
            state.queue.push(job.status.id);
            state.jobs.insert(job.status.id, job);
        }

        let scope = crate::proto::AbortScope {
            cwd: Some("/".into()),
            submitter: Some(chain()),
            tags: vec![],
        };
        let plan = plan_abort(&mut state, &scope, 1, "boot-now");

        assert_eq!(plan.cancelled.len(), 1);
        assert_eq!(plan.cancelled[0].0, ids[0]);
        assert_eq!(plan.outside, 1);
        assert_eq!(state.jobs[&ids[1]].status.state, JobState::Queued);
        drop(state);
        std::fs::remove_dir_all(dir).ok();
    }

    /// An abort waits for the pid of a job that is between the queue and its
    /// first process, and then signals it.
    ///
    /// WHAT THIS TEST OBSERVES: the wait, with a pid that arrives late. The
    /// job is `starting` with no pid, as the coordinator holds it in the
    /// moment after the scheduler took it and before the supervisor wrote the
    /// pid. A thread of the test then gives it the pid of a process that the
    /// test owns, in its own process group, as the supervisor would. A race
    /// against a real supervisor cannot hold that window open, so this test
    /// makes the state instead of racing for it.
    #[test]
    fn an_abort_waits_for_the_pid_of_a_job_that_starts() {
        use std::os::unix::process::CommandExt;

        let (_lock, _var, dir) = isolated_state();
        let coord = Arc::new(Coordinator::new(crate::config::Config::default(), 0));
        let mut job = queued_job(Some("boot"), chain());
        job.status.state = JobState::Starting;
        job.status.pid = None;
        let id = job.status.id;
        coord.state.lock().unwrap().jobs.insert(id, job);

        // The process that the job "starts": a child of this test, in its own
        // group, so that the signal reaches it and nothing else.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("the test cannot start sleep");
        let pid = child.id() as i32;

        let late = Arc::clone(&coord);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let mut state = late.state.lock().unwrap();
            if let Some(job) = state.jobs.get_mut(&id) {
                job.status.state = JobState::Running;
                job.status.pid = Some(pid);
            }
        });

        let answer = abort(&coord, crate::proto::AbortScope::default(), false, 0, 1);
        writer.join().unwrap();

        let (signalled, not_stopped) = match answer {
            Response::Aborted {
                signalled,
                not_stopped,
                ..
            } => (signalled, not_stopped),
            other => panic!("the abort gave {other:?}"),
        };
        assert_eq!(
            not_stopped,
            Vec::<AbortedJob>::new(),
            "the abort must wait for the pid and then signal the job"
        );
        assert_eq!(signalled, vec![id]);

        let status = child.wait().expect("the test cannot reap its child");
        assert!(
            !status.success(),
            "the signal must reach the process of the job: {status:?}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

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
