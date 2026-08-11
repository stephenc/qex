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
                job.status = status.clone();

                // Run the hook here as well.
                //
                // The supervisor writes the terminal record and then runs the
                // hook. A signal in the moment between those two steps leaves a
                // job with a correct result and no notification. This call
                // closes that moment. It costs nothing when the supervisor did
                // its work: the claim file stops the second run.
                //
                // NO TEST HOLDS THIS LINE, and none can. It covers a window of
                // a few microseconds inside another process, and a test cannot
                // put a signal there. A hand deletion of it leaves the whole
                // suite green — measured, after the tests that hold the two
                // calls in the supervisor were written. Do not read that green
                // as "this line does nothing": read it as "the fault that this
                // line stops is one that a test cannot make happen".
                crate::hook::fire_detached(&dir, &status);
            }
            // The supervisor gave the job back to the queue.
            //
            // It does this after the kernel stopped the job for memory: the
            // claim is now larger, and the queue never admitted that claim. The
            // coordinator must test the new claim against the budget, in the
            // same way as a new job. Without this branch the job would go to
            // the state `failed` with "the supervisor stopped without a
            // result", and the work would stop.
            Ok(status) if status.state == JobState::Queued => {
                job.status = status;
                job.status.supervisor_pid = None;
                let claim = job.status.mem;
                // Use the rule of the submission, so a job that qex corrects
                // does not go in front of the jobs that waited for it.
                state.enqueue(id);
                log(&format!(
                    "job {id} is in the queue again, and it waits for capacity for {}",
                    crate::units::format_size(claim)
                ));
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

                // The supervisor of this job stopped before it could run the
                // hook, so the coordinator runs it. The claim file stops a
                // second run if the supervisor already started one.
                crate::hook::fire_detached(&dir, &status);
            }
        }
        state.publish_changes();
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

    // Delete the marks of the attempt before this one.
    //
    // Each mark says what stopped ONE attempt. This point is safe for both: the
    // job has no process id yet, so `qex kill` refuses the job and can write no
    // mark, and no job process operates that the kernel can stop.
    //
    // Without this step, a job with `--retries` that a command stopped on the
    // first attempt kept that mark for ever. A second attempt that the kernel
    // stopped for memory then said that a command stopped it, and the lesson of
    // the kill for memory went away.
    crate::enforce::clear_user_kill(&dir);
    crate::enforce::clear_oom(&dir);

    // Keep what an earlier attempt of this job removed from the output. The
    // limit belongs to the stream and not to one attempt, so the count of the
    // job is the sum of the attempts.
    let earlier_drops = status.logs_dropped.unwrap_or_default();

    // Read the configuration before anything else uses it.
    //
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

    // The limit on the output of the job. A fault in this field must not stop
    // the job, so an incorrect value gives the default limit and a warning.
    let log_limit = match cfg.log_max_bytes() {
        Ok(limit) => limit,
        Err(e) => {
            let message = format!("{e}. This job uses the default limit.");
            log(&message);
            eprintln!("qex: {message}");
            crate::config::Config::default()
                .log_max_bytes()
                .ok()
                .flatten()
        }
    };

    // The output of a job holds secrets as frequently as its environment, so
    // these files use the same mode as the job specification.
    //
    // A second attempt adds to the file and does not replace it. The output of
    // the attempt that failed is the reason for the retry, and a reader needs
    // it. A mark separates the attempts.
    let again = status.attempts > 0;
    let out_path = dir.join("stdout.log");
    let err_path = dir.join("stderr.log");
    let stdout =
        create_private(&out_path, again).context("opening the standard output file of the job")?;
    let stderr =
        create_private(&err_path, again).context("opening the standard error file of the job")?;

    if again {
        use std::io::Write;
        let mark = format!("\n--- attempt {} ---\n", status.attempts + 1);
        (&stdout).write_all(mark.as_bytes()).ok();
        (&stderr).write_all(mark.as_bytes()).ok();
    }

    let out_len = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    let err_len = std::fs::metadata(&err_path).map(|m| m.len()).unwrap_or(0);
    let out_cap = crate::logcap::CapWriter::new(&out_path, stdout, out_len, log_limit);
    let err_cap = crate::logcap::CapWriter::new(&err_path, stderr, err_len, log_limit);

    // The environment of THIS attempt.
    //
    // The claim in the record is the claim in force, and it is larger than the
    // claim in the specification after qex answered a kill for memory. The job
    // must hear the raised claim: a Go job that keeps `GOMEMLIMIT` at the claim
    // that failed collects at that size again, and the new attempt dies in the
    // same place with a larger claim held for it. See `spec::reexport_claim`.
    let mut job_env = spec.env.clone();
    crate::spec::reexport_claim(
        &mut job_env,
        status.cpu,
        spec.mem,
        status.mem,
        &cfg.claims.also,
    );

    let mut cmd = std::process::Command::new(&spec.command[0]);
    cmd.args(&spec.command[1..])
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::null())
        // The job writes into a pipe, and this process writes the file. The
        // limit on the output thus operates while the job writes. A job that
        // writes into the file itself can fill the disk before anybody looks,
        // and the same disk holds the record of each job.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Give the job the environment that the CLI captured. Remove the
        // environment of this process, which came from the coordinator.
        .env_clear()
        .envs(&job_env);

    // Tell the job which devices it received.
    //
    // These values REPLACE the captured environment. The coordinator owns the
    // assignment, so a value that came from the shell of the user would send
    // the job to a device that qex gave to a different job.
    //
    // The job sees the record as well as the environment. The environment is
    // what a framework reads with no change to its code; the record is what an
    // agent reads AFTERWARDS to explain a failure, and the record survives.
    for (name, given) in &status.assigned {
        let upper = name.to_ascii_uppercase().replace('-', "_");
        if given.devices.is_empty() {
            cmd.env(format!("QEX_CLAIM_{upper}"), given.units.to_string());
            continue;
        }
        let list = given
            .devices
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(",");
        cmd.env(format!("QEX_{upper}_DEVICES"), &list);
        if let Some(pool) = cfg.pool(name) {
            if let Some(var) = &pool.env {
                cmd.env(var, &list);
            }
            // The quantity that the job may use on EACH device. A claim with
            // no size takes the whole device, so the value is the capacity of
            // the smallest device that the job received.
            let size = given.size.or_else(|| {
                given
                    .devices
                    .iter()
                    .filter_map(|i| pool.devices.get(*i as usize).copied())
                    .min()
            });
            if let Some(size) = size {
                let quantity = pool
                    .size_name
                    .as_deref()
                    .unwrap_or("size")
                    .to_ascii_uppercase();
                cmd.env(format!("QEX_{upper}_{quantity}"), size.to_string());
            }
        }
    }

    // The new process group goes in the SAME `pre_exec` as the politeness of
    // the job, below. That call needs the configuration, which this function
    // reads above, so one closure does both and the job forks once.

    // Apply the memory limit before the job starts.
    //
    // This code puts the supervisor itself in the cgroup, and the job then
    // inherits it. A job that starts first could allocate memory and fork
    // children before qex moved it, and those children would never meet the
    // limit.
    let mut cgroup_dir: Option<std::path::PathBuf> = None;
    let mut enforce_warning: Option<String> = None;
    if cfg.enforce.mode.is_on() {
        // Use the claim in the RECORD, and not the claim in the specification.
        //
        // qex raises the claim in the record after the kernel stops the job for
        // memory. A limit from the specification would hold the first claim,
        // and the kernel would stop each new attempt at the size that already
        // failed.
        match crate::enforce::create_job_cgroup(&cfg, &id, status.mem) {
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

    // How politely this job uses the machine. See `PolitenessConfig`.
    //
    // THE SUPERVISOR TESTS THESE VALUES AGAIN, and it does not trust the test
    // that `qex submit` made. `load_short` above parses the file and does not
    // validate it, and the file can change between the submission and the
    // start: `qex rerun` needs no config file, and a job can wait in the queue
    // while somebody edits the file. Measured with `[politeness] nice = 100` in
    // the file at the start of the job: the job ran at nice 19, because
    // `setpriority` takes 19 for any number above the range and reports
    // success, and nothing said so. The coordinator refuses such a file and
    // keeps the values that it had. The supervisor holds no earlier values, so
    // it takes the DEFAULT values and puts the fault in the record of the job.
    let politeness = match cfg.politeness_values() {
        Ok(()) => cfg.politeness.clone(),
        Err(e) => {
            let message = format!(
                "{e} This job uses the default politeness values, so it gives way as a job \
                 of qex did before."
            );
            log(&message);
            add_fault(&mut status.error, message);
            crate::config::PolitenessConfig::default()
        }
    };
    let nice = spec.nice.unwrap_or(politeness.nice);
    let io_class = politeness.io.clone();
    let oom_adj = politeness.oom_score_adj;

    unsafe {
        cmd.pre_exec(move || {
            // A new process group. `qex kill` then signals every process of the
            // job with one call to `killpg`.
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }

            // The steps below make the job give way. NOT ONE OF THEM CAN STOP
            // THE JOB: a machine that refuses them gives a job that runs at the
            // usual priority, which is what qex did before. A failure here must
            // never take the work away from the user.
            apply_politeness(nice, &io_class, oom_adj);
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
            // Use the error field. A job that failed waits for nothing, so this
            // text does not belong in `blocked_reason`.
            status.error = Some(message);
            status.blocked_reason = None;
            job::write_status(&dir, &status)?;

            // Leave the cgroup of the job before the hook runs.
            //
            // This process put itself in that cgroup for the job, and the job
            // never started. The hook is not the job, so it must not receive
            // the memory limit of the job. Without this step, a hook in a
            // `hard` mode cgroup meets a limit that belongs to other work.
            if let Some(cgroup) = crate::enforce::job_cgroup_path(&dir) {
                crate::enforce::leave_cgroup(&cgroup);
                crate::enforce::remove_cgroup(&cgroup);
            }

            crate::hook::fire(crate::hook::Origin::Supervisor, &dir, &status);
            return Ok(1);
        }
    };

    // Copy each stream of the job through the limit, in a thread of its own.
    //
    // The threads report through a channel and not through `join`. A process
    // that left the process group can hold the pipe open, and a `join` would
    // then wait for ever. The supervisor must write the result of a job that
    // stopped, whatever a process of that job still holds.
    let (tx, rx) = std::sync::mpsc::channel::<(bool, crate::logcap::Report)>();
    let mut copies = 0;
    if let Some(pipe) = child.stdout.take() {
        let done = tx.clone();
        let eof = tx.clone();
        copies += 1;
        std::thread::spawn(move || {
            let dropped = crate::logcap::pump(pipe, out_cap, || {
                eof.send((false, crate::logcap::Report::Eof)).ok();
            });
            done.send((false, crate::logcap::Report::Done(dropped)))
                .ok();
        });
    }
    if let Some(pipe) = child.stderr.take() {
        let done = tx.clone();
        let eof = tx.clone();
        copies += 1;
        std::thread::spawn(move || {
            let dropped = crate::logcap::pump(pipe, err_cap, || {
                eof.send((true, crate::logcap::Report::Eof)).ok();
            });
            done.send((true, crate::logcap::Report::Done(dropped))).ok();
        });
    }
    drop(tx);

    // Read BOTH out-of-memory counts before this attempt starts. A rise after
    // the job stops shows that the kernel stopped a process for memory, and a
    // rise in `oom` shows that the limit of this cgroup was the cause.
    //
    // NEITHER count starts at zero. The counter of the login session holds the
    // kills of every program of this user. The cgroup of a job takes its name
    // from the id of the job, so every attempt of one job uses the same cgroup
    // and the counts of an earlier attempt stay in it.
    //
    // This measurement needs no limit from qex, so the state `oom` is now
    // available in the usual configuration.
    let watch_cgroup = cgroup_dir.clone().or_else(crate::enforce::own_cgroup);
    let oom_before = watch_cgroup
        .as_ref()
        .and_then(|c| crate::enforce::read_oom_counts(c))
        .unwrap_or_default();

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
        // This cgroup belongs to this job. It does NOT start at zero: every
        // attempt of one job uses it, so only a rise above `oom_before`
        // describes this attempt.
        crate::enforce::record_oom_evidence(&dir, &cgroup, true, oom_before);
        crate::enforce::kill_cgroup(&cgroup);
    }

    // Test the counts again. A kill during this job, with a SIGKILL that no qex
    // command sent, is the out-of-memory killer.
    //
    // The answer says WHICH limit stopped the job. With a cgroup of its own,
    // the counts are about this job, and the count `oom` separates the limit of
    // this job from the memory of the whole machine. Without a cgroup, qex
    // reads the cgroup of the session: that counter counts the kills in each
    // cgroup below it, so a kill in a different program of this user raises the
    // same number and qex cannot name the victim.
    if let Some(cgroup) = &watch_cgroup {
        crate::enforce::record_oom_evidence(&dir, cgroup, cgroup_dir.is_some(), oom_before);
    }

    // Release the process id. Each signal above is complete.
    let exit = child.wait().context("waiting for the job")?;

    // Complete the copy of each stream. Each process of the job stopped, so
    // the pipes close and each thread writes the last part of its file.
    //
    // The two events have different times, and that difference is deliberate:
    //
    // 1. The END OF THE OUTPUT has a limit of 30 seconds. A process that left
    //    the process group can hold the pipe open for ever, and the record of
    //    a job that stopped must not wait for it.
    // 2. The COPY OF THE TAIL that follows has a long limit. That work is
    //    local, and its time grows with `max_bytes`. A short limit here would
    //    cut the log file of a job that did nothing wrong, on a machine where
    //    the disk is slow or the limit is some gigabytes.
    //
    // The decision itself is in `drain_copies`, so that a test can drive it.
    let mut drops = crate::job::LogsDropped {
        limit: log_limit.unwrap_or(0),
        ..earlier_drops
    };
    let incomplete = drain_copies(
        &rx,
        copies,
        &mut drops,
        std::time::Instant::now() + EOF_LIMIT,
        COPY_LIMIT,
    );

    if incomplete {
        // Something still holds a pipe of this job, or the copy did not
        // complete. Write the result, and say that a log file is not complete.
        // A record that arrives is worth more than a wait that has no end.
        log(&format!(
            "the output of the job {id} did not close; qex writes the result now, and the \
             last part of a log file can be missing"
        ));
        // The file that holds the tail must not stay. Nothing reads it, and it
        // holds disk space that `qex du` cannot explain. A copy that continues
        // keeps its open file, so this operation stops no work.
        for log_file in [&out_path, &err_path] {
            std::fs::remove_file(crate::logcap::tail_path(log_file)).ok();
        }
        let note = "the output of this job did not close, so a log file can be missing its \
                    last part. A process of the job kept the pipe open. Read the log file, \
                    and start the job again if you need the full output.";
        add_fault(&mut status.error, note.to_string());
        // Say it in the record as well, and not in the text only.
        //
        // The counts here are the counts that arrived. A copy that did not
        // report can have removed much more, and a field that says `null` tells
        // a program that the file is complete. That is not true, and a program
        // reads this field and not the text.
        drops.incomplete = true;
    }

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
    // Keep an earlier message. The two messages that this field can already
    // hold say that the memory limit is NOT active, or that qex could not read
    // the configuration. Each of those is the cause of the note below, and not
    // a smaller fact than it.
    if status.error.is_none() {
        if let Some(note) = unexplained_kill_note(status.state, signal, &dir) {
            status.error = Some(note);
        }
    }
    status.exit_code = code;
    status.signal = signal;
    status.finished_at = Some(sys::now_secs());
    status.usage = usage;
    // Say what qex removed from the output. `qex status` and `qex logs` read
    // this value, so a reader never takes a part of the output for the whole.
    status.logs_dropped = drops.any().then_some(drops);
    // The job stopped, so the pid stops being an identity: the machine can
    // give that number to another process at any moment. Keep it as history
    // only, where no code can act on it.
    status.pid = None;
    status.last_pid = Some(pid);

    // Answer an out-of-memory kill with a larger claim and a new attempt.
    //
    // This is the case that the README describes: a training run with
    // `--mem guess` that the kernel stops at hour four. The claim was too
    // small, the claim came from qex, and qex can correct it.
    if status.state == JobState::Oom {
        // Act on the evidence of THIS JOB only.
        //
        // qex reads a cgroup counter to find a kill for memory. With a cgroup
        // of its own for the job, that counter counts this job and nothing
        // else, and the kernel stopped the job at the limit that qex made from
        // the claim: the claim was too small, and that is a fact.
        //
        // With no cgroup of its own, qex reads the counter of the session. That
        // counter also counts a kill in a different program of this user, and a
        // machine that is short of memory is the machine on which a person uses
        // `kill -9`. The two arrive together, so the count alone does not say
        // that THIS job was the victim, and it does not say that the claim was
        // too small: the machine can be full while the claim is correct.
        //
        // A cgroup of its own is not sufficient by itself. The count `oom_kill`
        // counts the processes that ANY out-of-memory killer stopped, and the
        // killer of the whole machine raises it as well. The count `oom` rises
        // at the limit of THIS cgroup alone, so it is the count that says the
        // claim was too small. `[politeness] oom_score_adj` raises the score of
        // a qex job on purpose, so a qex job is the job that the killer of the
        // machine takes first.
        //
        // qex therefore REPORTS the state `oom` on the weaker evidence, and it
        // ACTS on the stronger evidence only. A new attempt with a larger claim
        // repeats work, holds more of the machine, and teaches the learner a
        // number that no measurement supports.
        let scope = crate::enforce::oom_evidence(&dir);
        if scope != Some(crate::enforce::OomScope::Job) {
            let note = note_for_a_kill_that_qex_cannot_act_on(scope, status.mem);
            log(&format!("job {id}: {note}"));
            // JOIN, and do not replace. This field can already hold "the
            // memory limit is not active for this job", which is the reason
            // that qex has the weaker evidence and the fact that the reader
            // needs most.
            add_fault(&mut status.error, note);
            job::write_status(&dir, &status)?;
            // `oom` is a final state here, so the stop hook must run. A hook
            // fires one time for each job that STOPS, and not for each attempt.
            crate::hook::fire(crate::hook::Origin::Supervisor, &dir, &status);
            return Ok(code.unwrap_or(1));
        }

        // Keep the lesson NOW, and not at the end of this function.
        //
        // The new attempt does not come back to this point: this process gives
        // the job to the coordinator and stops. A record at the end would thus
        // lose the measurement of every attempt except the last, and the
        // measurement of an attempt that the kernel stopped is the most
        // valuable measurement that qex holds.
        crate::usage::record_lower_bound(&spec, &status);

        match raise_claim(&cfg, &status, spec.mem) {
            Raise::To(next) => {
                let message = format!(
                    "the kernel stopped attempt {} of this job, because the job used more \
                     memory than its claim of {}. THE CLAIM WAS TOO SMALL. qex raised the \
                     claim to {} and starts the job again.",
                    status.attempts,
                    crate::units::format_size(status.mem),
                    crate::units::format_size(next)
                );
                log(&format!("job {id}: {message}"));

                status.oom_raises += 1;
                status.mem = next;
                // Say where this claim came from. A reader of `qex status` then
                // sees that qex made the number, and not the agent.
                status.claim_source = "raised".to_string();
                status.state = JobState::Queued;
                // JOIN, and do not replace. The field can hold a fault of the
                // configuration or of the memory limit, and it holds the raise
                // of the attempt before this one. A ladder of three attempts
                // thus gives the reader each step of it, in order.
                add_fault(&mut status.error, message);
                status.pid = None;
                status.finished_at = None;

                // `--max-queue-time` DOES NOT EXPIRE THIS JOB, and this code
                // needs no line to make that true.
                //
                // `sched::expire` refuses every job that already ran, from the
                // start time in the record, and this job holds the start time
                // of the attempt that the kernel stopped. That rule already
                // covers a job between two attempts of `--retries`, and it
                // covers this job in the same way and for the same reason: a
                // limit on the WAIT must not delete a job that RAN. The e2e
                // test `a_retry_after_a_kill_for_memory_is_never_expired_by_the_wait_limit`
                // holds that behaviour.
                // This process stops now, so it holds the record no longer.
                status.supervisor_pid = None;
                job::write_status(&dir, &status)?;

                // The out-of-memory record belongs to the attempt that stopped.
                // A record that stays would make the next attempt an
                // out-of-memory kill as well, whatever stopped it.
                crate::enforce::clear_oom(&dir);

                // GIVE THE JOB BACK TO THE COORDINATOR, and do not start it here.
                //
                // A retry after a failure keeps the same claim, so this process
                // can start the job again itself: the budget that the queue gave
                // to this job is still the correct budget.
                //
                // A retry after a kill for memory has a LARGER claim, and the
                // queue never saw that claim. A start from this process would
                // put a job of 1GB in a budget that admitted 600MB, beside a
                // job that holds the rest, and the sum would be above the
                // budget. Stopping exactly that is the work of the queue. With
                // `[enforce] mode = "hard"` the kernel would also receive the
                // sum of the limits, and the machine would meet the load that
                // the budget exists to prevent.
                //
                // The record says `queued` now. The coordinator reads that
                // record when this process stops, puts the job in the queue
                // again, and starts it when the machine has capacity for the
                // NEW claim.
                log(&format!(
                    "job {id} waits for the queue again, with the claim {}",
                    crate::units::format_size(next)
                ));
                return Ok(0);
            }
            Raise::Stop(reason) => {
                log(&format!("job {id}: {reason}"));
                add_fault(&mut status.error, reason);
                job::write_status(&dir, &status)?;
                // The ladder stopped, so `oom` is the final state of this job
                // and the stop hook must run. The hook fires one time, for the
                // job, and not one time for each attempt.
                crate::hook::fire(crate::hook::Origin::Supervisor, &dir, &status);
                return Ok(code.unwrap_or(1));
            }
        }
    }

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
        wait_while_paused(&dir, &spec, &mut status);
        return main(id);
    }

    // Keep the measurement, so the next job of this command gets an accurate
    // claim with no effort from the agent.
    crate::usage::record(&spec, &status);

    // Tell the person that the job stopped.
    //
    // THE SUPERVISOR RUNS THE HOOK, and it runs it here, at the end.
    //
    // The supervisor is the process that knows the result first, and it exists
    // for each job that ran. The coordinator can stop and start again while a
    // job runs, so a coordinator that ran the hook would miss the jobs of the
    // period in which it did not operate.
    //
    // The record on the disk already says that the job stopped, so this process
    // holds nothing now: `qex wait` gives its answer, the budget is free, and
    // the next job starts. A hook that hangs thus makes this process live
    // longer and does no other damage.
    crate::hook::fire(crate::hook::Origin::Supervisor, &dir, &status);

    Ok(code.unwrap_or(0))
}

/// How long the supervisor waits for the output of the job to close.
///
/// A pipe closes when the last process that holds it stops. A job that leaves a
/// process behind (`setsid`, `nohup ... &`, a daemon that a test starts) thus
/// keeps its output open after the job itself ends. The record of a job that
/// stopped must not wait for such a process, so the wait has a limit.
const EOF_LIMIT: Duration = Duration::from_secs(30);

/// How long the supervisor waits for the copy of the last part to complete.
///
/// This work is local and it follows the end of the output. Its time grows with
/// `[logs] max_bytes`, so this limit is long: a short one would cut the log file
/// of a job that did nothing wrong, on a machine where the disk is slow or the
/// limit is some gigabytes.
const COPY_LIMIT: Duration = Duration::from_secs(600);

/// Waits for each copy of a stream to report, and says if one did not.
///
/// The result is `true` when the supervisor gave up waiting. The counts that DID
/// arrive stay in `drops`, because a count that arrived is true even when
/// another one is missing.
///
/// # Why the two limits are different
///
/// The END OF THE OUTPUT has a short limit ([`EOF_LIMIT`]), because a process
/// that left the process group can hold the pipe open for ever. The COPY OF THE
/// LAST PART that follows has a long limit ([`COPY_LIMIT`]), because that work
/// is local. A copy that already reached the end of the output is thus never cut
/// short, and a job that never closes its output never blocks the record.
///
/// # Why this is a function
///
/// A test of the limit through a real job needs a process that holds the pipe
/// after the job stops, and the supervisor stops each such process on purpose:
/// `killpg` reaches the process group of the job, and `kill_cgroup` reaches a
/// process that left that group. A test built on `setsid` passed on one machine
/// and failed on the ubuntu-24.04 runner, where the holder does not survive, and
/// a test that measures nothing on a machine reports a pass there.
///
/// Whether such a process survives is a property of the operating system. The
/// DECISION is the property of qex, it is this loop, and a test drives it with a
/// channel that it makes itself.
fn drain_copies(
    rx: &std::sync::mpsc::Receiver<(bool, crate::logcap::Report)>,
    copies: usize,
    drops: &mut crate::job::LogsDropped,
    eof_limit: std::time::Instant,
    copy_limit: Duration,
) -> bool {
    let mut open = copies;
    let mut waiting_for_eof = copies;
    while open > 0 {
        let wait = if waiting_for_eof > 0 {
            eof_limit.saturating_duration_since(std::time::Instant::now())
        } else {
            copy_limit
        };
        match rx.recv_timeout(wait) {
            Ok((_, crate::logcap::Report::Eof)) => waiting_for_eof -= 1,
            Ok((is_err, crate::logcap::Report::Done(d))) => {
                open -= 1;
                if is_err {
                    drops.stderr_bytes += d.bytes;
                    drops.stderr_lines += d.lines;
                } else {
                    drops.stdout_bytes += d.bytes;
                    drops.stdout_lines += d.lines;
                }
            }
            Err(_) => return true,
        }
    }
    false
}

/// Waits while a person holds the queue, or a lock of this job.
///
/// # The fault that this function prevents
///
/// A retry starts the next attempt IN THIS PROCESS. It does not give the job
/// back to `sched::choose`, so the pause test of the scheduler never sees it.
/// Without this wait, a job with `--retries` starts a new process minutes after
/// `qex pause queue` answered "paused" and after `qex info` reported it. A job
/// that fails quickly repeats that for the whole length of the pause.
///
/// This is also why the pause is a FILE. The supervisor is a separate process
/// with no connection to the coordinator, and a file is a state that every
/// process can read.
///
/// The lock has the same rule: a person who holds `gpu0` must not lose it to
/// the second attempt of a job that held it before.
fn wait_while_paused(
    dir: &std::path::Path,
    spec: &crate::spec::JobSpec,
    status: &mut job::JobStatus,
) {
    let mut said = false;
    loop {
        let mut paused = crate::pause::Paused::read();
        paused.expire(sys::now_secs());

        let reason = match &paused.queue {
            Some(record) => Some(crate::pause::queue_reason(record)),
            None => spec
                .locks
                .iter()
                .find(|name| paused.locks.contains_key(*name))
                .map(|name| crate::pause::lock_reason(name)),
        };

        let Some(reason) = reason else {
            if said {
                status.blocked_reason = None;
                job::write_status(dir, status).ok();
                log("the pause ended; the next attempt of this job starts");
            }
            return;
        };

        // Write the reason one time only. The text holds no number that
        // changes, so a second write would give the same bytes and two more
        // `fsync` calls, twice a second, for the whole length of the pause.
        if !said {
            said = true;
            status.blocked_reason = Some(reason.clone());
            job::write_status(dir, status).ok();
            log(&format!("the next attempt of this job waits: {reason}"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Makes a file that the other users of the machine cannot read.
///
/// The output of a job frequently holds a token or a password, in the same way
/// as its environment.
fn create_private(path: &std::path::Path, append: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        // The limit on the output reads this file to count the lines that it
        // removes. Without this permission, the count of a second attempt is
        // zero, and the file then says that no line went.
        .read(true)
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

/// Adds a fault to the record of a job, and keeps the faults that are there.
///
/// # The fault that this removes
///
/// A job can meet more than one fault before it starts. `error` held ONE of
/// them, because each writer replaced the field. Measured with
/// `[enforce] mode = "hard"` on a machine with no cgroup delegation AND
/// `[politeness] nice = 100`: `qex status --json` gave the memory-limit fault
/// only. The politeness fault reached `supervisor.log`, which no command reads,
/// so a user saw a job that ran at a priority nobody asked for and had nothing
/// to read about it.
///
/// The reader needs every fault, so this function joins them.
///
/// The mark between two faults is `; `, which is the mark that the output limit
/// already used for the same purpose. One mark, and one place that joins.
fn add_fault(error: &mut Option<String>, message: String) {
    match error {
        Some(already) => {
            already.push_str("; ");
            already.push_str(&message);
        }
        None => *error = Some(message),
    }
}

/// Makes a job give way to the work of a person.
///
/// This function operates in the child, between the fork and the exec. It must
/// therefore call the system only: no allocation, and no lock. Each step gives
/// up in silence, because a job that runs at the usual priority is the
/// behaviour that qex had before, and it is far better than no job at all.
fn apply_politeness(nice: i32, io_class: &str, oom_score_adj: i32) {
    // The processor. A larger number gives way to everything else.
    //
    // A user cannot ask for a number below the number that the process has,
    // without privilege. qex still makes the call: it fails, the job continues
    // at the priority that it had, and no other step is lost. Measured on
    // Linux with the usual `RLIMIT_NICE` of 0: `setpriority(PRIO_PROCESS, 0,
    // -5)` from nice 0 gives EACCES and leaves the process at 0.
    //
    // The call happens for 0 as well. `--nice 0` asks that the job does not
    // give way, so qex asks for 0 and does not leave the priority that the job
    // received from the supervisor. On a machine with no privilege that ASK
    // frequently gives nothing. Measured with a coordinator started under
    // `nice 5`: `--nice 0` gave a job at nice 5, and `--nice 10` and
    // `--nice 19` gave 10 and 19. qex makes the call so that it obeys the user
    // where the machine permits it, and the help text and the documentation say
    // that qex can only make a job give way MORE than the coordinator does.
    //
    // A number ABOVE the range is worse than a number below it. `setpriority`
    // moves such a number INTO the range and reports success, so it never
    // reaches this code as a fault. `Config::validate` and `JobSpec::resolve`
    // refuse it, and the supervisor tests the configuration again above.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }

    // The disk, on Linux. A build that reads the whole source tree makes an
    // editor wait for its own file without this.
    #[cfg(target_os = "linux")]
    {
        // From <linux/ioprio.h>. The class is in the top three bits.
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
        const CLASS_BEST_EFFORT: libc::c_int = 2;
        const CLASS_IDLE: libc::c_int = 3;

        let value = match io_class {
            // Level 4, the middle of the eight levels of the class.
            //
            // THIS IS NOT NECESSARILY MORE POLITE THAN `none`. The man page of
            // `ionice` gives the level of a process that asked for nothing as
            // `(cpu_nice + 20) / 5`, so a job at the default `nice = 10` gets
            // level 6 with no call at all, and level 4 asks for MORE of the
            // disk than that. `none` is the default of qex for this reason, and
            // `idle` is the value that makes a job give way.
            "best-effort" => Some((CLASS_BEST_EFFORT << IOPRIO_CLASS_SHIFT) | 4),
            "idle" => Some(CLASS_IDLE << IOPRIO_CLASS_SHIFT),
            _ => None,
        };
        if let Some(value) = value {
            unsafe {
                libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, value);
            }
        }

        // The out-of-memory score. A background job should lose that
        // competition before an editor that holds an hour of work.
        //
        // This writes a file, and a write in a child between the fork and the
        // exec must not allocate. `write` on a fixed buffer is safe here.
        if oom_score_adj != 0 {
            write_oom_score(oom_score_adj);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        // macOS has no equivalent of either, and `nice` above covers the
        // processor. The values are read and ignored, which the configuration
        // says.
        let _ = (io_class, oom_score_adj);
    }
}

/// Writes the out-of-memory score of this process.
///
/// This operates between the fork and the exec, so it uses the system calls
/// only and it allocates nothing.
#[cfg(target_os = "linux")]
fn write_oom_score(value: i32) {
    let mut out = [0u8; OOM_TEXT];
    let len = write_i32(value, &mut out);

    unsafe {
        let path = c"/proc/self/oom_score_adj";
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
        if fd >= 0 {
            libc::write(fd, out.as_ptr() as *const libc::c_void, len);
            libc::close(fd);
        }
    }
}

/// The space that the text of any `i32` needs.
///
/// `-2147483648` is 11 characters: 10 digits and the sign.
#[cfg(target_os = "linux")]
const OOM_TEXT: usize = 11;

/// Writes the text of a whole number into a buffer, and gives its length.
///
/// # Why the buffer holds ANY `i32`
///
/// The kernel takes -1000 to 1000, and `Config::validate` refuses anything
/// else, so a larger number does not arrive here. The buffer still holds one.
///
/// This code runs in the child, between the fork and the exec. An index outside
/// a buffer is a panic; a panic formats a message; and that allocates and takes
/// two locks in a process where no lock is safe. The job would then die. A
/// buffer that fits the values that qex EXPECTS puts the life of the job behind
/// a test in another file. A buffer that fits every value it can RECEIVE does
/// not.
#[cfg(target_os = "linux")]
fn write_i32(value: i32, out: &mut [u8; OOM_TEXT]) -> usize {
    // The digits, in the wrong order. `unsigned_abs` and not `-value`, because
    // `-i32::MIN` is not an `i32` and it would stop here in a debug build.
    let mut digits = [0u8; 10];
    let mut n = 0;
    let mut v = value.unsigned_abs();
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 {
            break;
        }
    }

    let mut len = 0;
    if value < 0 {
        out[0] = b'-';
        len = 1;
    }
    for i in (0..n).rev() {
        out[len] = digits[i];
        len += 1;
    }
    len
}

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

    // A kill from a command wins against every other test.
    //
    // `qex kill` writes a mark before it sends the signal. qex answers an
    // out-of-memory kill with a larger claim and a NEW ATTEMPT, so a job that a
    // person stopped must never look like one: qex would repeat work that
    // somebody stopped on purpose, at a larger size.
    if signal.is_some() && crate::enforce::was_user_killed(dir) {
        return JobState::Killed;
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

/// Says why qex started no new attempt after a kill for memory.
///
/// This function is separate so that a test reads the answer directly. The
/// state that produces each answer needs a cgroup and a machine that is short
/// of memory, and a test must make neither.
///
/// Each answer names what the reader must DO, because an agent reads the record
/// to decide whether to run the work again.
fn note_for_a_kill_that_qex_cannot_act_on(
    scope: Option<crate::enforce::OomScope>,
    claim: u64,
) -> String {
    if scope == Some(crate::enforce::OomScope::Machine) {
        // SAY ONLY WHAT THE COUNTS PROVED. qex made a cgroup for this job and
        // that cgroup counted no out-of-memory event of its own, so the limit
        // of this job did not stop it. qex does NOT know which limit did: the
        // memory of the whole machine and the limit of a parent cgroup give
        // these same counts, and they need different answers from the reader.
        //
        // The claim is a fact of the job, so this text names it. The limit that
        // could stop the job is NOT always the claim: with `[enforce] mode =
        // "soft"` the claim is `memory.high` and the limit that kills is
        // `memory.max`, which is larger.
        format!(
            "the kernel stopped this job for memory, and NOT at the limit of this job. The claim              of this job was {}, and the cgroup of this job counted no out-of-memory event of              its own. Some memory outside this job was short: the machine, or a limit of a              parent cgroup. qex cannot say which, so THE CLAIM CAN BE CORRECT: qex started no              new attempt and it learned nothing from this attempt. Read `qex info` and the load              of the machine. Run the same work again with the same claim when the memory that              was short is free. A larger claim for this job cannot move a limit that belongs to              a parent.",
            crate::units::format_size(claim)
        )
    } else {
        // qex could not name the program that the kernel stopped. Two states
        // reach this text: qex made no cgroup for the job, and a record from an
        // earlier version of qex that names no scope. The words must hold for
        // both, so they say what qex COULD NOT DO, and not what the
        // configuration is.
        format!(
            "the kernel stopped this job for memory, and its claim was {}. qex holds no count              that belongs to this job alone: it read the counter of the whole login session,              which also counts a kill in a different program of this user. qex thus cannot              prove that the claim of this job was too small, because the machine can be full              while the claim is correct. qex therefore did NOT start the job again. Compare the              `usage` field with the claim. Give a larger `--mem` value, or set `[enforce] mode`              in the config file, and qex then holds a count for this job alone.",
            crate::units::format_size(claim)
        )
    }
}

/// The decision about the claim for the next attempt after a kill for memory.
enum Raise {
    /// qex raises the claim to this value and starts the job again.
    To(u64),
    /// qex does not start the job again. The text says why, and what to do.
    Stop(String),
}

/// Chooses the claim for the next attempt after the kernel stopped the job.
///
/// # Why the ladder has a limit
///
/// A claim that doubles for ever finishes with the whole machine, and each
/// attempt costs the full time of the job. Two rules stop the ladder:
///
/// 1. A number of raises, from `[retry] on_oom`. Two raises give four times the
///    first claim, which corrects the usual error of an estimate.
/// 2. The memory budget of qex. qex must not claim memory that it does not
///    have, and a claim above the budget makes the job an oversized job, which
///    the queue then starts alone. A job that already claims the full budget
///    has no larger claim available, and the answer for the user is a different
///    machine, and not another attempt.
fn raise_claim(cfg: &crate::config::Config, status: &job::JobStatus, first_claim: u64) -> Raise {
    let claim = crate::units::format_size(status.mem);

    if cfg.retry.on_oom == 0 {
        return Raise::Stop(format!(
            "the kernel stopped this job, because the job used more memory than its claim of \
             {claim}. THE CLAIM WAS TOO SMALL. The config file sets `[retry] on_oom = 0`, so qex \
             did not start the job again. Give a larger claim with `--mem`."
        ));
    }

    if status.oom_raises >= cfg.retry.on_oom {
        return Raise::Stop(format!(
            "the kernel stopped this job {} times, because the job used more memory than its \
             claim. THE CLAIM WAS TOO SMALL. qex raised the claim from {} to {claim}, and that \
             claim was also too small. Give a larger claim with `--mem`, or use a machine with \
             more memory.",
            status.attempts,
            crate::units::format_size(first_claim)
        ));
    }

    // A budget that qex cannot read must not stop the correction, so a fault
    // here gives the claim of this attempt and the rules below then stop the
    // ladder.
    let budget = cfg.budget_mem().unwrap_or(status.mem);

    // A job that already claims the budget or more has no larger claim.
    //
    // Say that in its own words. An oversized job is a supported case: the
    // queue starts such a job alone. Its claim is ABOVE the budget, so a
    // sentence that calls that claim "the whole budget" contradicts itself and
    // gives the reader a number that is not the number in the record.
    if status.mem >= budget {
        return Raise::Stop(format!(
            "the kernel stopped this job, because the job used more memory than its claim of \
             {claim}. THE CLAIM WAS TOO SMALL. The memory budget of qex on this machine is {}, \
             so qex has no larger claim to give. Use a machine with more memory, or raise \
             `[budget] mem` in the config file.",
            crate::units::format_size(budget)
        ));
    }

    // Never above the budget. qex must not claim memory that it does not have.
    let next = ((status.mem as f64 * cfg.retry.growth) as u64).min(budget);

    // A multiplier a little above 1.0 can give the claim that already failed,
    // because the calculation gives whole bytes. A new attempt at that claim
    // would stop in the same way and cost a whole run.
    if next <= status.mem {
        return Raise::Stop(format!(
            "the kernel stopped this job, because the job used more memory than its claim of \
             {claim}. THE CLAIM WAS TOO SMALL. The config file sets `[retry] growth = {}`, which \
             gives the same claim again, so qex did not start the job again. Give a larger claim \
             with `--mem`, or raise `growth` in the config file.",
            cfg.retry.growth
        ));
    }

    Raise::To(next)
}

/// Gives a note for a kill that qex cannot explain.
///
/// The kernel uses `SIGKILL` for an out-of-memory kill, and `qex kill` uses the
/// same signal. Linux counts the out-of-memory kills in the cgroup, so qex has
/// evidence there. A machine with no cgroup gives none.
///
/// qex then gives the state `killed`, which is the safe answer: qex starts no
/// new attempt for it. The reader must learn that qex GUESSED, and must not
/// believe that a command stopped the job.
fn unexplained_kill_note(
    state: JobState,
    signal: Option<i32>,
    dir: &std::path::Path,
) -> Option<String> {
    if state != JobState::Killed || signal != Some(libc::SIGKILL) {
        return None;
    }
    if crate::enforce::was_user_killed(dir) || crate::enforce::oom_evidence_is_available() {
        return None;
    }
    Some(
        "the signal KILL stopped this job, and no qex command sent it. This machine keeps no \
         count of the kills for memory, so qex cannot say if the kernel stopped the job for \
         memory or if a different program stopped it. qex gave the state `killed` and did not \
         raise the claim. Compare the `usage` field with the claim, and give a larger `--mem` \
         value if the two are near."
            .to_string(),
    )
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

    /// A kill that the machine made must not read as a kill at the claim.
    ///
    /// The two answers send a reader in opposite directions. One says the claim
    /// was too small, and the other says the claim can be correct.
    #[test]
    fn the_answer_for_a_kill_of_the_machine_keeps_the_claim() {
        let note = note_for_a_kill_that_qex_cannot_act_on(
            Some(crate::enforce::OomScope::Machine),
            2 << 30,
        );
        assert!(
            note.contains("NOT at the limit of this job"),
            "the answer must say that the limit of the job did not stop it: {note}"
        );
        assert!(
            note.contains("THE CLAIM CAN BE CORRECT"),
            "the answer must not tell a reader that the claim was too small: {note}"
        );
        assert!(
            note.contains("with the same claim"),
            "the answer must name what the reader does next: {note}"
        );
        assert!(
            note.contains("parent cgroup"),
            "the answer must cover the limit of a parent, which gives these same counts: {note}"
        );
        assert!(
            note.contains("qex cannot say which"),
            "the answer must not name one cause, because the counts name none: {note}"
        );
    }

    /// With no cgroup of its own, qex cannot name the job that the kernel
    /// stopped, and the answer must say so.
    #[test]
    fn the_answer_for_the_counter_of_the_session_names_its_limit() {
        let note = note_for_a_kill_that_qex_cannot_act_on(
            Some(crate::enforce::OomScope::Session),
            2 << 30,
        );
        assert!(
            note.contains("whole login session"),
            "the answer must say which counter qex read: {note}"
        );
        assert!(
            note.contains("[enforce] mode"),
            "the answer must name the setting that gives qex the stronger evidence: {note}"
        );
        assert!(
            !note.contains("applies no memory limit"),
            "an old record reaches this text as well, so it must not name a configuration: \
             {note}"
        );
    }

    /// A second fault must not push the first one out of the record.
    ///
    /// A job can meet more than one fault before it starts. Measured with
    /// `[enforce] mode = "hard"` on a machine with no cgroup delegation AND
    /// `[politeness] nice = 100`: `qex status --json` gave the memory-limit
    /// fault only, and the politeness fault reached `supervisor.log`, which no
    /// command reads. The user then had a job at a priority that nobody asked
    /// for and nothing to read about it.
    #[test]
    fn a_job_with_two_faults_keeps_both_of_them() {
        let mut error = None;
        add_fault(&mut error, "the memory limit is not active.".into());
        assert_eq!(error.as_deref(), Some("the memory limit is not active."));

        add_fault(&mut error, "the politeness values have a fault.".into());
        let both = error.unwrap();
        assert!(
            both.contains("memory limit") && both.contains("politeness"),
            "the record must keep both faults, and it said: {both}"
        );
    }

    /// The text of the OOM score must fit the buffer for EVERY `i32`.
    ///
    /// This code runs in the child, between the fork and the exec. An index
    /// outside the buffer is a panic; a panic formats a message; and that
    /// allocates and takes two locks in a process where no lock is safe. The
    /// job would then die, and this step must never be able to stop a job.
    ///
    /// An earlier form of this code held a buffer for -1000 to 1000 and nothing
    /// held the value inside that range. This test therefore uses the two ends
    /// of the type, and not the two ends of the range that the kernel accepts.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_oom_score_text_fits_the_buffer_for_every_number() {
        for value in [
            i32::MIN,
            i32::MIN + 1,
            -100000,
            -1000,
            -1,
            0,
            1,
            9,
            10,
            500,
            1000,
            999999,
            i32::MAX,
        ] {
            let mut out = [0u8; OOM_TEXT];
            let len = write_i32(value, &mut out);
            assert_eq!(
                std::str::from_utf8(&out[..len]).unwrap(),
                value.to_string(),
                "the text of {value} is wrong"
            );
        }
    }

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

    /// A copy that reports both events gives a complete record and no flag.
    #[test]
    fn a_copy_that_completes_gives_a_complete_record() {
        use crate::logcap::{Dropped, Report};
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send((false, Report::Eof)).unwrap();
        tx.send((
            false,
            Report::Done(Dropped {
                bytes: 4096,
                lines: 20,
            }),
        ))
        .unwrap();
        tx.send((true, Report::Eof)).unwrap();
        tx.send((
            true,
            Report::Done(Dropped {
                bytes: 16,
                lines: 1,
            }),
        ))
        .unwrap();

        let mut drops = crate::job::LogsDropped::default();
        let incomplete = drain_copies(
            &rx,
            2,
            &mut drops,
            std::time::Instant::now() + Duration::from_secs(30),
            Duration::from_secs(600),
        );

        assert!(!incomplete, "each copy reported, so the record is complete");
        assert_eq!(drops.stdout_bytes, 4096);
        assert_eq!(drops.stdout_lines, 20);
        assert_eq!(drops.stderr_bytes, 16);
        assert_eq!(drops.stderr_lines, 1);
    }

    /// AN OUTPUT THAT NEVER CLOSES MUST NOT HOLD THE RECORD OF THE JOB.
    ///
    /// A pipe closes when the last process that holds it stops, so a job that
    /// leaves a process behind keeps its output open for as long as that process
    /// lives. Without the limit the supervisor waits with it: `qex wait` blocks,
    /// and every rule that asks "did this job stop?" receives no answer.
    ///
    /// The counts that DID arrive must stay. A count that arrived is true even
    /// when another one is missing, and `qex status` shows it.
    #[test]
    fn an_output_that_never_closes_stops_the_wait_and_keeps_what_arrived() {
        use crate::logcap::{Dropped, Report};
        let (tx, rx) = std::sync::mpsc::channel();

        // One stream completed. The other never reports at all, in the same way
        // as a stream that a process of the job still holds.
        tx.send((false, Report::Eof)).unwrap();
        tx.send((
            false,
            Report::Done(Dropped {
                bytes: 1024,
                lines: 8,
            }),
        ))
        .unwrap();
        // The sender stays alive, so the channel does not close and the wait
        // ends because of the limit and not because of a broken channel.

        // The limit for the COPY is 10 seconds here, and not the 600 seconds of
        // the supervisor. A change that uses the copy limit for the end of the
        // output must FAIL this test and not hold it: with 600 seconds such a
        // change gives a test that waits ten minutes and reports nothing, which
        // a reader takes for a machine that stopped.
        let mut drops = crate::job::LogsDropped::default();
        let start = std::time::Instant::now();
        let incomplete = drain_copies(
            &rx,
            2,
            &mut drops,
            start + Duration::from_millis(50),
            Duration::from_secs(10),
        );
        let took = start.elapsed();

        assert!(
            incomplete,
            "the record must say that a log file is not complete"
        );
        assert!(
            took < Duration::from_secs(5),
            "the wait took {took:?}; the limit did not operate"
        );
        assert_eq!(drops.stdout_bytes, 1024, "a count that arrived must stay");
        assert_eq!(drops.stdout_lines, 8, "a count that arrived must stay");
        drop(tx);
    }

    /// A copy that reached the end of the output must never be cut short.
    ///
    /// The limit on the END of the output is short, because a process can hold
    /// the pipe for ever. The copy of the last part that follows is local work,
    /// and its time grows with `[logs] max_bytes`. A limit that used one time
    /// for both would cut the log file of a job that did nothing wrong, on a
    /// machine where the disk is slow or the limit is some gigabytes.
    ///
    /// The deadline for the end of the output is already past here, and the copy
    /// reports after it. The record must still be complete.
    #[test]
    fn a_copy_that_reached_the_end_of_the_output_is_not_cut_short() {
        use crate::logcap::{Dropped, Report};
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send((false, Report::Eof)).unwrap();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            tx.send((
                false,
                Report::Done(Dropped {
                    bytes: 77,
                    lines: 3,
                }),
            ))
            .ok();
        });

        let mut drops = crate::job::LogsDropped::default();
        let incomplete = drain_copies(
            &rx,
            1,
            &mut drops,
            // The end of the output already arrived, so this deadline is spent.
            std::time::Instant::now(),
            Duration::from_secs(600),
        );

        assert!(
            !incomplete,
            "the copy reached the end of the output, so the long limit applies to it"
        );
        assert_eq!(drops.stdout_bytes, 77);
        assert_eq!(drops.stdout_lines, 3);
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
            max_queue_time: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            learn_key: None,
            group: None,
            group_name: None,
            locks: vec![],
            claims: Default::default(),
            retries: 0,
            nice: None,
            needs: vec![],
            after: vec![],
            submitted_at: 0,
            dedupe_key: None,
            dedupe_window: 0,
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

    /// Makes an empty job directory for a test of the classification.
    fn job_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qex-sv-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A kill for memory gives the state `oom`, and not the state `killed`.
    ///
    /// The kernel and `qex kill` both use SIGKILL. Without the record in the
    /// job directory, the two causes look the same, and qex would tell the user
    /// to correct a claim that was correct.
    #[test]
    fn a_kill_for_memory_gives_the_state_oom() {
        let dir = job_dir("oom");
        crate::enforce::mark_oom(&dir, crate::enforce::OomScope::Job);
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGKILL), false, &dir),
            JobState::Oom
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A job that a USER stopped must never look like a kill for memory.
    ///
    /// This test protects the feature from doing harm. qex answers a kill for
    /// memory with a larger claim and a NEW ATTEMPT. A job that somebody
    /// stopped on purpose must not run again, and it must not teach the learner
    /// that the command needs more memory.
    ///
    /// The two marks can both exist: the out-of-memory count of a session also
    /// counts a kill in a different program of the same user. The mark from the
    /// command wins.
    #[test]
    fn a_job_that_a_user_stopped_is_never_an_out_of_memory_kill() {
        let dir = job_dir("userkill");
        crate::enforce::mark_user_kill(&dir);
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGKILL), false, &dir),
            JobState::Killed
        );

        crate::enforce::mark_oom(&dir, crate::enforce::OomScope::Session);
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGKILL), false, &dir),
            JobState::Killed,
            "a kill from a command must win against the count of the session"
        );
        assert_eq!(
            classify(&spec(), None, Some(libc::SIGTERM), false, &dir),
            JobState::Killed
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A kill that qex cannot explain gives the safe answer, and it says that
    /// qex could not tell. A machine with no cgroup counts no kill for memory,
    /// so a guess there would send the reader to the wrong correction.
    #[test]
    fn a_kill_that_qex_cannot_explain_says_so() {
        let dir = job_dir("unexplained");

        // A job that a command stopped needs no note: qex knows the cause.
        crate::enforce::mark_user_kill(&dir);
        assert_eq!(
            unexplained_kill_note(JobState::Killed, Some(libc::SIGKILL), &dir),
            None
        );
        std::fs::remove_file(dir.join("killed-by-user")).unwrap();

        let note = unexplained_kill_note(JobState::Killed, Some(libc::SIGKILL), &dir);
        if crate::enforce::oom_evidence_is_available() {
            // This machine counts the kills for memory, so qex knows that the
            // kernel did not stop this job.
            assert_eq!(note, None);
        } else {
            let note = note.expect("qex must say that it could not tell the cause");
            assert!(note.contains("cannot say"), "got: {note}");
            assert!(
                note.contains("--mem"),
                "the note must say what to do: {note}"
            );
        }

        // No note for a state that qex did not guess.
        assert_eq!(
            unexplained_kill_note(JobState::Completed, Some(libc::SIGKILL), &dir),
            None
        );
        assert_eq!(unexplained_kill_note(JobState::Killed, None, &dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Gives a configuration with a budget and a retry rule.
    fn cfg_with(budget: &str, on_oom: u32) -> crate::config::Config {
        toml::from_str(&format!(
            "[budget]\nmem = \"{budget}\"\n[retry]\non_oom = {on_oom}\ngrowth = 2.0\n"
        ))
        .unwrap()
    }

    fn oom_status(claim: u64, raises: u32, attempts: u32) -> job::JobStatus {
        let mut s = job::JobStatus::new(&spec());
        s.state = JobState::Oom;
        s.mem = claim;
        s.oom_raises = raises;
        s.attempts = attempts;
        s
    }

    /// The claim doubles after a kill for memory. The next attempt then has a
    /// chance to succeed; an attempt with the same claim has none.
    #[test]
    fn the_claim_doubles_after_a_kill_for_memory() {
        let cfg = cfg_with("8GB", 2);
        let Raise::To(next) = raise_claim(&cfg, &oom_status(1 << 30, 0, 1), 1 << 30) else {
            panic!("the first kill for memory must raise the claim");
        };
        assert_eq!(next, 2 << 30);

        let Raise::To(next) = raise_claim(&cfg, &oom_status(2 << 30, 1, 2), 1 << 30) else {
            panic!("the second kill for memory must raise the claim");
        };
        assert_eq!(next, 4 << 30);
    }

    /// The ladder has a limit. A claim that doubles for ever finishes with the
    /// whole machine, and each attempt costs the full time of the job.
    #[test]
    fn the_claim_stops_growing_at_the_limit() {
        let cfg = cfg_with("64GB", 2);
        let Raise::Stop(reason) = raise_claim(&cfg, &oom_status(4 << 30, 2, 3), 1 << 30) else {
            panic!("a job must not double for ever");
        };
        assert!(reason.contains("too small"), "got: {reason}");
        assert!(
            reason.contains("1GB") && reason.contains("4GB"),
            "the reason must give the first claim and the last one: {reason}"
        );
        assert!(
            reason.contains("--mem"),
            "the reason must say what to do: {reason}"
        );
    }

    /// qex must not claim memory that it does not have. A claim that is already
    /// the whole budget has no larger claim available, and the answer for the
    /// user is a different machine.
    #[test]
    fn the_claim_never_goes_above_the_memory_budget() {
        let cfg = cfg_with("6GB", 3);

        // The double is above the budget, so the claim stops at the budget.
        let Raise::To(next) = raise_claim(&cfg, &oom_status(4 << 30, 0, 1), 4 << 30) else {
            panic!("a claim below the budget must still grow");
        };
        assert_eq!(next, 6 << 30, "the claim must stop at the budget");

        // The claim is the whole budget. There is no larger claim.
        let Raise::Stop(reason) = raise_claim(&cfg, &oom_status(6 << 30, 1, 2), 4 << 30) else {
            panic!("qex must not claim more memory than its budget");
        };
        assert!(reason.contains("budget"), "got: {reason}");
        assert!(
            reason.contains("machine with more memory"),
            "the reason must say what to do: {reason}"
        );

        // A job that is larger than the budget already keeps its own claim.
        //
        // The message must not call that claim "the whole budget". An
        // oversized job is a supported case, and the text would then give a
        // number that is not the number in the record.
        let Raise::Stop(reason) = raise_claim(&cfg, &oom_status(20 << 30, 0, 1), 20 << 30) else {
            panic!("an oversized job has no larger claim");
        };
        assert!(
            reason.contains("20GB") && reason.contains("6GB"),
            "the reason must give the claim and the budget: {reason}"
        );
        assert!(
            !reason.contains("claim of 20GB. THE CLAIM WAS TOO SMALL. That claim is already"),
            "the reason must not call a claim above the budget the whole budget: {reason}"
        );
    }

    /// A multiplier a little above 1.0 can give the claim that already failed.
    /// A new attempt at that claim costs a whole run and stops in the same way.
    #[test]
    fn a_multiplier_that_gives_the_same_claim_stops_the_ladder() {
        let cfg: crate::config::Config =
            toml::from_str("[budget]\nmem = \"64GB\"\n[retry]\non_oom = 3\ngrowth = 1.0000001\n")
                .unwrap();
        cfg.validate().expect("the config file is valid");

        let Raise::Stop(reason) = raise_claim(&cfg, &oom_status(1024, 0, 1), 1024) else {
            panic!("a multiplier that gives the same claim must stop the ladder");
        };
        assert!(reason.contains("growth"), "got: {reason}");
        assert!(
            reason.contains("--mem"),
            "the reason must say what to do: {reason}"
        );
    }

    /// The config file can turn the correction off. qex must then say why it
    /// did not start the job again.
    #[test]
    fn the_config_file_can_stop_the_correction() {
        let cfg = cfg_with("8GB", 0);
        let Raise::Stop(reason) = raise_claim(&cfg, &oom_status(1 << 30, 0, 1), 1 << 30) else {
            panic!("`on_oom = 0` must stop the correction");
        };
        assert!(reason.contains("on_oom"), "got: {reason}");
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
