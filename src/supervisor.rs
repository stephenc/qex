//! This module holds the supervisor. One supervisor controls one job.
//!
//! The supervisor is a separate process for one reason: the coordinator can
//! stop, fail or restart, and the job must continue and must still record its
//! result. The supervisor writes `status.json` when the job stops.
//!
//! The supervisor starts the job in a new session and a new process group. The
//! command `qex kill` can then signal every process of the job with one call,
//! and no process of the job can avoid the signal.

use crate::daemon::{log, Coordinator};
use crate::job::{self, JobState, Usage};
use crate::paths;
use crate::sys;
use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::time::Duration;

/// Starts the supervisor process for one job. Gives its process id.
///
/// The supervisor is a new `qex` process. It is not a copy of the coordinator,
/// so the coordinator does not fork its threads and its memory.
pub fn spawn(id: uuid::Uuid) -> Result<i32> {
    let exe = paths::program_path()?;
    let dir = paths::job_dir(&id)?;
    let log_path = dir.join("supervisor.log");

    use std::os::unix::fs::OpenOptionsExt;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log_err = log_file
        .try_clone()
        .context("copying the log file handle")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("supervise")
        .arg(id.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_err))
        .current_dir("/");

    unsafe {
        cmd.pre_exec(|| {
            // A new session. The job then continues after the terminal closes,
            // and the job has its own process group for `qex kill`.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("starting the supervisor")?;
    Ok(child.id() as i32)
}

/// Writes the pid of the supervisor of a job, in a file of its own.
///
/// The coordinator knows this pid at the fork, and the supervisor cannot write
/// it before it exists. A coordinator that starts again reads this file to learn
/// that a job continues; without it, that coordinator finds a job that says
/// `starting` with no process and it marks the job failed, while the supervisor
/// operates and the job runs.
///
/// This is a file of its own, and not a field of `status.json`, because the
/// supervisor owns that record from the moment that it starts. Two processes
/// that write one file give the fault that `a_job_that_operates_says_running_
/// and_gives_its_pid` holds.
pub fn record_supervisor_pid(id: &uuid::Uuid, pid: i32) {
    let Ok(dir) = paths::job_dir(id) else { return };
    crate::job::write_atomic(
        &dir.join("supervisor.pid"),
        pid.to_string().as_bytes(),
        0o600,
    )
    .ok();
}

/// Reads that pid.
pub fn supervisor_pid_of(dir: &std::path::Path) -> Option<i32> {
    std::fs::read_to_string(dir.join("supervisor.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Waits for one supervisor and puts its result in the coordinator.
///
/// This function operates in its own thread. It uses `waitpid` on the exact
/// process id. It does not read `/proc` and it does not search command lines.
pub fn reap(coord: Arc<Coordinator>, id: uuid::Uuid, pid: i32) {
    let mut wait_status: libc::c_int = 0;
    // This call blocks until the supervisor stops.
    let rc = unsafe { libc::waitpid(pid, &mut wait_status, 0) };

    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ECHILD) {
            // This supervisor is not a child of this process. A coordinator
            // that starts again finds the supervisors of the previous
            // coordinator, and the system gave them to the init process.
            //
            // `waitpid` cannot wait for such a process, so watch it instead.
            watch_until_gone(pid);
        } else {
            log(&format!(
                "qex could not wait for the supervisor {pid} of the job {id}: {e}"
            ));
        }
    }

    // The supervisor wrote the result. Read that file, because it holds the
    // exit code of the job and the measured use.
    let dir = match paths::job_dir(&id) {
        Ok(d) => d,
        Err(_) => return,
    };

    let mut state = coord.state.lock().unwrap();
    if let Some(job) = state.jobs.get_mut(&id) {
        job.supervisor_pid = None;

        match job::read_status(&dir) {
            Ok(status) if status.state.is_terminal() => {
                job.status = status;
            }
            other => {
                // The supervisor stopped before it wrote a result. Something
                // stopped it: a signal, or the out-of-memory killer.
                //
                // The job process can still operate. The system gives it to the
                // init process, and it continues to use memory and cores. qex
                // must stop it here. Without this step, the job continues, the
                // budget shows the memory as free, and no qex command can stop
                // the job, because its record says that it stopped.
                let job_pid = other.ok().and_then(|s| s.pid).or(job.status.pid);
                let mut note = "the supervisor stopped without a result".to_string();

                // Give the words of the supervisor itself.
                //
                // The supervisor writes each fault to its own log, and NO
                // COMMAND READ THAT FILE. A user thus met "the supervisor
                // stopped without a result", which names no cause and gives no
                // remedy, while the cause was on the disk beside the record.
                if let Some(text) = supervisor_log_tail(&dir) {
                    note.push_str(&format!(". The supervisor said: {text}"));
                }

                if let Some(pid) = job_pid {
                    if sys::pid_alive(pid) {
                        log(&format!(
                            "the supervisor of the job {id} stopped, and the job {pid} \
                             continues; qex stops the job now"
                        ));
                        stop_process_group(pid);
                        note.push_str("; qex stopped the job process");
                    }
                }

                job.status.state = JobState::Failed;
                job.status.finished_at = Some(sys::now_secs());
                // A job that failed waits for nothing, so this text belongs in
                // the error field.
                job.status.error = Some(note);
                job.status.blocked_reason = None;
                let status = job.status.clone();
                job::write_status(&dir, &status).ok();
                log(&format!("the supervisor of the job {id} left no result"));
            }
        }
    }
    drop(state);

    coord.notify();
}

/// Waits until a process stops, for a process that is not a child.
///
/// A parent uses `waitpid`. This function is for the other case: a coordinator
/// that starts again inherits no supervisor, so it tests the process instead.
fn watch_until_gone(pid: i32) {
    while sys::pid_alive(pid) {
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Stops each process of one process group.
///
/// This function sends `SIGTERM`, waits a short time, then sends `SIGKILL`.
/// A process cannot avoid the second signal.
fn stop_process_group(pid: i32) {
    unsafe {
        libc::killpg(pid, libc::SIGTERM);
    }
    // Give the job a short time to write its files and stop.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if !sys::pid_alive(pid) {
            return;
        }
    }
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

/// Runs one job. This function is the body of the `qex supervise` command.
///
/// The supervisor does not stop when the coordinator stops. It does not use
/// `PR_SET_PDEATHSIG`, because the job must continue in that case.
pub fn main(id: uuid::Uuid) -> Result<i32> {
    let dir = paths::job_dir(&id)?;
    let spec = job::read_spec(&dir).context("reading the job specification")?;
    let mut status = job::read_status(&dir).context("reading the job status")?;

    // Take the record, and say which process holds it.
    //
    // The coordinator knows this pid, and it deliberately does not write it:
    // a write from the coordinator would race the writes below. This process
    // writes it instead, before it does anything that can take time, so a
    // coordinator that starts again finds the supervisor of this job.
    status.supervisor_pid = Some(std::process::id() as i32);
    job::write_status(&dir, &status).context("writing the job status")?;

    // The output of a job holds secrets as frequently as its environment, so
    // these files use the same mode as the job specification.
    //
    // A second attempt adds to the file and does not replace it. The output of
    // the attempt that failed is the reason for the retry, and a reader needs
    // it. A mark separates the attempts.
    let again = status.attempts > 0;
    let stdout = create_private(&dir.join("stdout.log"), again)
        .context("opening the standard output file of the job")?;
    let stderr = create_private(&dir.join("stderr.log"), again)
        .context("opening the standard error file of the job")?;

    if again {
        use std::io::Write;
        let mark = format!("\n--- attempt {} ---\n", status.attempts + 1);
        (&stdout).write_all(mark.as_bytes()).ok();
        (&stderr).write_all(mark.as_bytes()).ok();
    }

    let mut cmd = std::process::Command::new(&spec.command[0]);
    cmd.args(&spec.command[1..])
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        // Give the job the environment that the CLI captured. Remove the
        // environment of this process, which came from the coordinator.
        .env_clear()
        .envs(&spec.env);

    unsafe {
        cmd.pre_exec(|| {
            // A new process group. `qex kill` then signals every process of the
            // job with one call to `killpg`.
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Apply the memory limit before the job starts.
    //
    // This code puts the supervisor itself in the cgroup, and the job then
    // inherits it. A job that starts first could allocate memory and fork
    // children before qex moved it, and those children would never meet the
    // limit.
    // A configuration that qex cannot read must never become the default
    // configuration in silence. The default has no enforcement, so a fault in
    // the file would turn `must enforce` into `no limit`, and the job would run
    // with no limit and no word to anybody.
    //
    // The job continues, because the work of the user is more important than the
    // file. The fault goes into the record of the job, where `qex status` shows
    // it, and into the log of the supervisor.
    //
    // This message goes into a record, so it takes the SHORT form of the fault.
    // `Config::load` gives a long message about an upgrade of the coordinator,
    // which is correct for a person whose command stopped, and wrong here: it
    // would fill the `error:` field of a job that ran with advice, and it would
    // hide the words that matter — that no limit operates.
    let mut config_fault: Option<String> = None;
    let cfg = match crate::config::Config::load_short() {
        Ok(cfg) => cfg,
        Err(e) => {
            let message = format!(
                "qex could not read the configuration ({e}). This job uses the default values, \
                 SO NO LIMIT OPERATES. Correct the file, and start the job again with \
                 `qex rerun {id}`. Run `qex config show` for the complete message."
            );
            log(&message);
            eprintln!("{message}");
            config_fault = Some(message);
            crate::config::Config::default()
        }
    };
    let mut cgroup_dir: Option<std::path::PathBuf> = None;
    let mut enforce_warning: Option<String> = None;
    if cfg.enforce.mode.is_on() {
        match crate::enforce::create_job_cgroup(&cfg, &id, spec.mem) {
            Ok(cgroup) => match crate::enforce::add_process(&cgroup, std::process::id() as i32) {
                Ok(()) => {
                    crate::enforce::record_cgroup_path(&dir, &cgroup);
                    cgroup_dir = Some(cgroup);
                }
                Err(e) => {
                    // Report the fault. A limit that qex did not apply must
                    // never look like a limit that operates.
                    enforce_warning = Some(e);
                    crate::enforce::remove_cgroup(&cgroup);
                }
            },
            Err(e) => {
                enforce_warning = Some(e);
            }
        }
    }

    // Put the fault in the record of the job.
    //
    // Before this, the message went to stderr, which this process writes to
    // `supervisor.log`. No command reads that file, so a user with
    // `mode = "hard"` was told that the limit was active while it was not for
    // this job.
    if let Some(warning) = &enforce_warning {
        eprintln!("qex: the memory limit is not active for this job: {warning}");
        status.error = Some(format!(
            "the memory limit is not active for this job: {warning}"
        ));
    }

    // A configuration that qex could not read is at least as important, and it
    // keeps its own words. It goes after the block above, because a fault in
    // the configuration is the cause of any limit fault that follows it.
    if let Some(fault) = &config_fault {
        status.error = Some(fault.clone());
    }
    let _ = &cgroup_dir;

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // A command that does not exist is a frequent error. Write a clear
            // message, and put it in the record of the job.
            let message = format!(
                "qex could not start `{}`: {e}. Test the program name and the PATH value.",
                spec.command[0]
            );
            eprintln!("{message}");
            status.state = JobState::Failed;
            status.finished_at = Some(sys::now_secs());
            // Use the error field. A job that failed waits for nothing, so this
            // text does not belong in `blocked_reason`.
            status.error = Some(message);
            status.blocked_reason = None;
            job::write_status(&dir, &status)?;
            return Ok(1);
        }
    };

    // Read the out-of-memory count before the job starts. An increase after
    // the job stops shows that the kernel stopped a process for memory.
    //
    // This measurement needs no limit from qex, so the state `oom` is now
    // available in the usual configuration.
    let watch_cgroup = cgroup_dir.clone().or_else(crate::enforce::own_cgroup);
    let oom_before = watch_cgroup
        .as_ref()
        .map(|c| crate::enforce::oom_count(c))
        .unwrap_or(0);

    let pid = child.id() as i32;
    status.state = JobState::Running;
    status.pid = Some(pid);
    // Record this process as the supervisor.
    //
    // The coordinator also writes this value, but this process writes the file
    // after that, from a copy that it read before. This process knows its own
    // process id, so it writes the correct value and no race is possible.
    status.supervisor_pid = Some(std::process::id() as i32);
    status.started_at = Some(sys::now_secs());
    status.attempts += 1;
    job::write_status(&dir, &status)?;

    // The job and the timer race each other. This value records the winner.
    //
    // A simple flag is not sufficient here. The timer can fire in the moment
    // between the exit of the job and the test of the flag. A job that
    // succeeded then gets the state `timeout`, and `qex wait` reports a failure
    // for a job that succeeded.
    //
    // Each side thus changes the value from RACE_OPEN with one atomic
    // operation. One side only can win.
    let outcome = Arc::new(std::sync::atomic::AtomicU8::new(RACE_OPEN));

    if let Some(limit) = spec.timeout {
        let outcome = Arc::clone(&outcome);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(limit));

            // Take the race. If the job already stopped, this operation fails
            // and the timer does nothing.
            if outcome
                .compare_exchange(
                    RACE_OPEN,
                    RACE_TIMER,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_err()
            {
                return;
            }

            // Signal the process group, so each child of the job stops.
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_secs(10));
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        });
    }

    // Wait for the job, but do not release its process id yet.
    //
    // `waitid` with `WNOWAIT` reports the result and keeps the process in the
    // table. The process id thus stays reserved, and the process group is still
    // the group of this job. The signals below cannot reach a different process.
    //
    // An error means that the process id is NOT reserved. The signals below
    // must then not go to that process group; see the note on the function.
    let reserved = match wait_without_reaping(pid) {
        Ok(()) => true,
        Err(e) => {
            log(&format!(
                "the wait for the job {id} (pid {pid}) failed: {e}. qex sends no signal to that \
                 process group, because the machine can give that pid to another process."
            ));
            false
        }
    };

    // Take the race before the last signals. The timer can no longer start.
    let _ = outcome.compare_exchange(
        RACE_OPEN,
        RACE_JOB,
        std::sync::atomic::Ordering::SeqCst,
        std::sync::atomic::Ordering::SeqCst,
    );

    // Stop each process that the job left. The job process is a zombie now, so
    // its process id is still reserved and this signal is safe.
    if reserved {
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }

    // If qex made a cgroup, stop each process in it. A process cannot leave a
    // cgroup, so this method finds a process that changed its process group.
    if let Some(cgroup) = crate::enforce::job_cgroup_path(&dir) {
        if crate::enforce::cgroup_had_oom(&cgroup) {
            crate::enforce::mark_oom(&dir);
        }
        crate::enforce::kill_cgroup(&cgroup);
    }

    // Test the out-of-memory count again. An increase during this job, with a
    // SIGKILL that no qex command sent, is the out-of-memory killer.
    if let Some(cgroup) = &watch_cgroup {
        if crate::enforce::oom_count(cgroup) > oom_before {
            crate::enforce::mark_oom(&dir);
        }
    }

    // Release the process id. Each signal above is complete.
    let exit = child.wait().context("waiting for the job")?;

    // Read the resources that the job used. The values include each child of
    // the job, so a job that forks gives a correct measurement.
    let usage = read_usage();

    if let Some(cgroup) = crate::enforce::job_cgroup_path(&dir) {
        crate::enforce::leave_cgroup(&cgroup);
        crate::enforce::remove_cgroup(&cgroup);
    }

    let signal = exit_signal(&exit);
    let code = exit.code();
    let timed_out = outcome.load(std::sync::atomic::Ordering::SeqCst) == RACE_TIMER;

    status.state = classify(&spec, code, signal, timed_out, &dir);
    status.exit_code = code;
    status.signal = signal;
    status.finished_at = Some(sys::now_secs());
    status.usage = usage;
    // The job stopped, so the pid stops being an identity: the machine can
    // give that number to another process at any moment. Keep it as history
    // only, where no code can act on it.
    status.pid = None;
    status.last_pid = Some(pid);

    // Run the job again when it failed and a retry is left.
    //
    // The job keeps one id and one record, so `qex wait` gives the final result
    // and an agent needs no extra command. A new job for each attempt would
    // give the agent an id that answers only for one attempt.
    //
    // The decision comes BEFORE the write, and the record goes to the disk one
    // time.
    //
    // An earlier version wrote `failed`, and then wrote `queued` a moment
    // later. A reader between the two writes saw a state that the job never
    // reached. The coordinator is such a reader: it reads the record of each
    // job, it keeps the state that it reads, and it stops reading a job that
    // stopped. It thus kept `failed` for a job that continued, and it kept it
    // for ever. `qex list` then showed `failed` for a job that was running,
    // `qex wait` gave the result of an attempt that was not the last one, and
    // every rule that asks "did this job stop?" received the wrong answer.
    let retrying = status.state == JobState::Failed && status.retries_left > 0;
    if retrying {
        status.retries_left -= 1;
        status.state = JobState::Queued;
        status.error = Some(format!(
            "attempt {} failed with the exit code {}; qex starts the job again",
            status.attempts,
            code.unwrap_or(-1)
        ));
        status.finished_at = None;
    }
    job::write_status(&dir, &status)?;

    if retrying {
        log(&format!(
            "job {id} failed and starts again; {} attempt(s) left",
            status.retries_left
        ));
        // Give the machine a moment. A task that fails at once, such as a
        // network that is not ready, needs the time more than the CPU.
        std::thread::sleep(Duration::from_secs(1));
        return main(id);
    }

    // Keep the measurement, so the next job of this command gets an accurate
    // claim with no effort from the agent.
    crate::usage::record(&spec, &status);

    Ok(code.unwrap_or(0))
}

/// Makes a file that the other users of the machine cannot read.
///
/// The output of a job frequently holds a token or a password, in the same way
/// as its environment.
fn create_private(path: &std::path::Path, append: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(!append)
        .append(append)
        .mode(0o600)
        .open(path)
}

/// No side won the race between the job and the timer.
const RACE_OPEN: u8 = 0;
/// The job stopped first.
const RACE_JOB: u8 = 1;
/// The timer fired first, so the job reached its time limit.
const RACE_TIMER: u8 = 2;

/// Gives the last words of the supervisor of a job.
///
/// The supervisor writes its faults to `supervisor.log`, and a supervisor that
/// stops before it writes a result has frequently written the reason there. The
/// coordinator puts this text in the record, so that the reason travels with
/// the job and a reader needs no second file.
///
/// The result holds the last lines only, and it is one line of text, because it
/// goes into a field that `qex status` shows.
fn supervisor_log_tail(dir: &std::path::Path) -> Option<String> {
    const KEEP: usize = 3;
    const LIMIT: usize = 400;

    let raw = std::fs::read(dir.join("supervisor.log")).ok()?;
    // The log of a job holds the output of a program, which is not always
    // valid text.
    let text = String::from_utf8_lossy(&raw);

    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }

    let start = lines.len().saturating_sub(KEEP);
    let mut joined = lines[start..].join(" / ");
    if joined.chars().count() > LIMIT {
        joined = joined.chars().take(LIMIT).collect::<String>() + "...";
    }
    Some(joined)
}

/// Waits for a process, but keeps its process id reserved.
///
/// The `WNOWAIT` option tells the kernel to report the result and keep the
/// process in the process table. The caller can then signal the process group
/// of that process without a risk: the system cannot give the process id to a
/// different process while the first process stays in the table.
///
/// The caller must call `wait` after this function, or the process stays in the
/// table as a zombie.
///
/// # Why an error here needs an answer
///
/// The caller signals a process group after this function. That is safe ONLY
/// while the process stays in the process table. A call that failed leaves no
/// such promise, and a caller that continues sends a signal to a process id
/// that the machine can have given to somebody else.
///
/// A signal that arrives interrupts this call and gives `EINTR`. That is common
/// in code that controls processes, and it is not an error: the process has not
/// stopped, so the call starts again.
fn wait_without_reaping(pid: i32) -> std::io::Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // This call blocks until the process stops.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(e);
    }
}

/// Chooses the final state of a job.
fn classify(
    _spec: &crate::spec::JobSpec,
    code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    dir: &std::path::Path,
) -> JobState {
    // A job that stopped with the code 0 succeeded, whatever the timer did.
    //
    // The timer takes the result with one atomic operation, so it can win in
    // the very short moment between the exit of the job and the same operation
    // in the main thread. A record that says `timeout` with the exit code 0
    // contradicts itself, and a reader cannot tell what happened.
    //
    // A job that the timer stopped receives a signal, so it has no exit code.
    if code == Some(0) {
        return JobState::Completed;
    }

    if timed_out {
        return JobState::Timeout;
    }

    // The kernel stops a process with SIGKILL for an out-of-memory event. Read
    // the cgroup record to separate that event from a `qex kill` command.
    if signal == Some(libc::SIGKILL) && crate::enforce::was_oom_killed(dir) {
        return JobState::Oom;
    }

    match (code, signal) {
        (Some(0), _) => JobState::Completed,
        (Some(_), _) => JobState::Failed,
        (None, Some(libc::SIGTERM)) | (None, Some(libc::SIGKILL)) => JobState::Killed,
        (None, Some(_)) => JobState::Failed,
        (None, None) => JobState::Failed,
    }
}

fn exit_signal(exit: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    exit.signal()
}

/// Reads the resources that the child processes used.
fn read_usage() -> Usage {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    if rc != 0 {
        return Usage::default();
    }

    // On Linux `ru_maxrss` is in kilobytes. On macOS it is in bytes.
    #[cfg(target_os = "linux")]
    let max_rss = (ru.ru_maxrss as u64).saturating_mul(1024);
    #[cfg(not(target_os = "linux"))]
    let max_rss = ru.ru_maxrss as u64;

    let cpu_secs = ru.ru_utime.tv_sec as f64
        + ru.ru_utime.tv_usec as f64 / 1e6
        + ru.ru_stime.tv_sec as f64
        + ru.ru_stime.tv_usec as f64 / 1e6;

    Usage { max_rss, cpu_secs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::JobSpec;

    /// The words of the supervisor must reach the record of the job.
    ///
    /// A user met "the supervisor stopped without a result", which names no
    /// cause and gives no remedy, while the cause was in `supervisor.log` beside
    /// the record. No command read that file.
    #[test]
    fn the_last_words_of_the_supervisor_reach_the_record() {
        let dir = std::env::temp_dir().join(format!("qex-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // No file, and an empty file, both give nothing. A note that says
        // "the supervisor said:" and then nothing is worse than no note.
        assert_eq!(supervisor_log_tail(&dir), None);
        std::fs::write(dir.join("supervisor.log"), b"\n  \n").unwrap();
        assert_eq!(supervisor_log_tail(&dir), None);

        // The LAST lines, because the fault that stopped the supervisor is the
        // last thing that it wrote.
        std::fs::write(
            dir.join("supervisor.log"),
            b"one\ntwo\nthree\nfour\nError: renaming status.json into place\n",
        )
        .unwrap();
        let tail = supervisor_log_tail(&dir).unwrap();
        assert!(tail.contains("renaming status.json"), "got: {tail}");
        assert!(!tail.contains("one"), "the oldest lines must go: {tail}");

        // The text goes into a field that `qex status` shows, so it stays one
        // line and it has a limit.
        std::fs::write(dir.join("supervisor.log"), "x".repeat(5000).as_bytes()).unwrap();
        let tail = supervisor_log_tail(&dir).unwrap();
        assert!(tail.chars().count() <= 405, "the text must have a limit");
        assert!(!tail.contains('\n'), "the text must be one line");

        // Output that is not valid text must not lose the message.
        std::fs::write(dir.join("supervisor.log"), b"bad \xff\xfe byte").unwrap();
        assert!(supervisor_log_tail(&dir).unwrap().contains("bad"));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn spec() -> JobSpec {
        JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "t".into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu: 1,
            mem: 1 << 30,
            timeout: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            locks: vec![],
            retries: 0,
            needs: vec![],
            after: vec![],
            submitted_at: 0,
        }
    }

    #[test]
    fn the_exit_code_gives_the_final_state() {
        let dir = std::path::Path::new("/nonexistent");
        assert_eq!(
            classify(&spec(), Some(0), None, false, dir),
            JobState::Completed
        );
        assert_eq!(
            classify(&spec(), Some(1), None, false, dir),
            JobState::Failed
        );
        assert_eq!(
            classify(&spec(), Some(127), None, false, dir),
            JobState::Failed
        );
    }

    #[test]
    fn a_signal_gives_the_state_killed() {
        let dir = std::path::Path::new("/nonexistent");
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGTERM), false, dir),
            JobState::Killed
        );
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGKILL), false, dir),
            JobState::Killed
        );
    }

    /// A time limit gives the state `timeout`, and not the state `killed`. The
    /// two states need different corrections, so they must stay separate.
    #[test]
    fn a_time_limit_gives_the_state_timeout() {
        let dir = std::path::Path::new("/nonexistent");
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGTERM), true, dir),
            JobState::Timeout
        );
    }

    /// A fault in the program gives the state `failed`.
    #[test]
    fn a_fault_signal_gives_the_state_failed() {
        let dir = std::path::Path::new("/nonexistent");
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGSEGV), false, dir),
            JobState::Failed
        );
    }

    /// The measurement reads `RUSAGE_CHILDREN`, which counts the child
    /// processes that this process waited for. It gives zero before the first
    /// child stops, so the test starts a child first.
    #[test]
    fn the_use_measurement_gives_a_value_after_a_child_stops() {
        std::process::Command::new("sh")
            .args(["-c", "head -c 4000000 /dev/zero > /dev/null"])
            .status()
            .expect("the test could not start a child process");

        let usage = read_usage();
        // A zero value here shows an incorrect call or an incorrect unit.
        assert!(usage.max_rss > 0, "the memory measurement gave zero");
        // The value must be a plausible quantity of memory, and not a value in
        // the wrong unit. Linux gives kilobytes and macOS gives bytes, so an
        // error of 1024 in either direction is possible.
        assert!(
            usage.max_rss > 64 * 1024 && usage.max_rss < 8 * (1 << 30),
            "the memory measurement {} is not plausible; test the unit",
            crate::units::format_size(usage.max_rss)
        );
        assert!(usage.cpu_secs >= 0.0);
    }
}
