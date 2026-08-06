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
    let exe = std::env::current_exe().context("finding the qex program file")?;
    let dir = paths::job_dir(&id)?;
    let log_path = dir.join("supervisor.log");

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log_err = log_file.try_clone().context("copying the log file handle")?;

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

/// Waits for one supervisor and puts its result in the coordinator.
///
/// This function operates in its own thread. It uses `waitpid` on the exact
/// process id. It does not read `/proc` and it does not search command lines.
pub fn reap(coord: Arc<Coordinator>, id: uuid::Uuid, pid: i32) {
    let mut wait_status: libc::c_int = 0;
    // This call blocks until the supervisor stops.
    let rc = unsafe { libc::waitpid(pid, &mut wait_status, 0) };

    if rc < 0 {
        log(&format!(
            "qex could not wait for the supervisor {pid} of the job {id}: {}",
            std::io::Error::last_os_error()
        ));
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
            _ => {
                // The supervisor stopped without a record. The job result is
                // lost, so mark the job failed. This is better than a job that
                // stays in the state `running` for ever.
                job.status.state = JobState::Failed;
                job.status.finished_at = Some(sys::now_secs());
                job.status.blocked_reason =
                    Some("the supervisor stopped without a result".to_string());
                let status = job.status.clone();
                job::write_status(&dir, &status).ok();
                log(&format!("the supervisor of the job {id} left no result"));
            }
        }
    }
    drop(state);

    coord.notify();
}

/// Runs one job. This function is the body of the `qex supervise` command.
///
/// The supervisor does not stop when the coordinator stops. It does not use
/// `PR_SET_PDEATHSIG`, because the job must continue in that case.
pub fn main(id: uuid::Uuid) -> Result<i32> {
    let dir = paths::job_dir(&id)?;
    let spec = job::read_spec(&dir).context("reading the job specification")?;
    let mut status = job::read_status(&dir).context("reading the job status")?;

    let stdout = std::fs::File::create(dir.join("stdout.log"))
        .context("opening the standard output file of the job")?;
    let stderr = std::fs::File::create(dir.join("stderr.log"))
        .context("opening the standard error file of the job")?;

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
            status.blocked_reason = Some(message);
            job::write_status(&dir, &status)?;
            return Ok(1);
        }
    };

    let pid = child.id() as i32;

    // Apply the memory limit, if the config asks for one and the system can do
    // it. Put the job in its cgroup now, so each child of the job is also in
    // the cgroup and the limit covers the whole job.
    let cfg = crate::config::Config::load().unwrap_or_default();
    if let Some(cgroup) = crate::enforce::create_job_cgroup(&cfg, &id, spec.mem) {
        if crate::enforce::add_process(&cgroup, pid) {
            crate::enforce::record_cgroup_path(&dir, &cgroup);
        }
    }

    status.state = JobState::Running;
    status.pid = Some(pid);
    status.started_at = Some(sys::now_secs());
    job::write_status(&dir, &status)?;

    // Start the timer, if the job has a time limit.
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(limit) = spec.timeout {
        let flag = Arc::clone(&timed_out);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(limit));
            // Test the flag. The job can stop before the limit.
            if !flag.load(std::sync::atomic::Ordering::SeqCst) {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                // Signal the process group, so each child of the job stops.
                unsafe {
                    libc::killpg(pid, libc::SIGTERM);
                }
                std::thread::sleep(Duration::from_secs(10));
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
        });
    }

    let exit = child.wait().context("waiting for the job")?;

    // Read the resources that the job used. The values include each child of
    // the job, so a job that forks gives a correct measurement.
    let usage = read_usage();

    // The job stopped. Stop each process that the job left, so no process stays
    // and holds the memory that qex counted.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }

    // If qex made a cgroup, stop each process in it. A process cannot leave a
    // cgroup, so this method finds a process that changed its process group.
    if let Some(cgroup) = crate::enforce::job_cgroup_path(&dir) {
        if crate::enforce::cgroup_had_oom(&cgroup) {
            crate::enforce::mark_oom(&dir);
        }
        crate::enforce::kill_cgroup(&cgroup);
        crate::enforce::remove_cgroup(&cgroup);
    }

    let signal = exit_signal(&exit);
    let code = exit.code();

    status.state = classify(&spec, code, signal, timed_out.load(std::sync::atomic::Ordering::SeqCst), &dir);
    status.exit_code = code;
    status.signal = signal;
    status.finished_at = Some(sys::now_secs());
    status.usage = usage;
    status.pid = Some(pid);
    job::write_status(&dir, &status)?;

    Ok(code.unwrap_or(0))
}

/// Chooses the final state of a job.
fn classify(
    _spec: &crate::spec::JobSpec,
    code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    dir: &std::path::Path,
) -> JobState {
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
            submitted_at: 0,
        }
    }

    #[test]
    fn the_exit_code_gives_the_final_state() {
        let dir = std::path::Path::new("/nonexistent");
        assert_eq!(classify(&spec(), Some(0), None, false, dir), JobState::Completed);
        assert_eq!(classify(&spec(), Some(1), None, false, dir), JobState::Failed);
        assert_eq!(classify(&spec(), Some(127), None, false, dir), JobState::Failed);
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
