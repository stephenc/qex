//! This module runs the command that qex starts when a job stops.
//!
//! The hook is a property of the machine and of the person at it, and not a
//! property of the work. It is thus in the config file only. A job file has no
//! hook field: the same pipeline runs on a laptop with a desktop alert and on a
//! build machine with no screen, and the job must not know the difference.
//!
//! # One run for each job, and never two
//!
//! A person who receives the same notification two times learns to ignore it.
//! The hook must thus run one time for each job, and no more.
//!
//! Several processes can make a job terminal. The supervisor writes the usual
//! result. The coordinator writes `cancelled`, `skipped`, and `failed` for a
//! supervisor that left no result or that a restart found dead. There is no
//! single place in the program that all of these pass through.
//!
//! The claim file gives the guarantee instead of a code path. Each process that
//! makes a job terminal calls this module, and this module makes `hook.ran`
//! with `create_new`. That operation succeeds for one caller only, on this
//! machine and after a restart, because the file is beside the record of the
//! job and it lives as long as the record. The caller that loses does nothing.
//!
//! THE ORDER IS DELIBERATE, AND IT IS NOT SYMMETRICAL. qex makes the file and
//! then runs the hook, so a process that stops between those two steps loses
//! that one message. The other order would run the hook and then record it, and
//! a process that stopped between THOSE two steps would notify a second time
//! later. qex chooses the message that is lost, because a notification that
//! arrives two times teaches a person to ignore every notification.
//!
//! # A hook must not damage the queue
//!
//! The command comes from a user, so it can hang, fail, write a lot of output,
//! or not exist. qex gives these guarantees:
//!
//! - The hook starts AFTER the terminal state is on the disk. The job is thus
//!   in its final state before the hook runs. `qex wait` gives its answer, the
//!   budget is free, and the next job starts, whatever the hook does.
//! - The hook has a time limit. qex signals the process group of the hook at
//!   the limit, and it sends `KILL` a short time after that.
//! - The hook has a size limit. The time limit is NOT a limit on the output: a
//!   hook of three seconds that writes with no stop made a file of 3.7GB. qex
//!   stops a hook that goes above `OUTPUT_LIMIT`, and it cuts the file.
//! - A hook that fails changes no job. The result of the job is the result of
//!   the job, and a notification that did not arrive does not change it.
//! - The verdict of qex goes into `hook.log`, which `qex logs --hook` reads. A
//!   user whose notification did not arrive can thus learn the reason with a
//!   qex command, and does not read the log of a supervisor.

use crate::config::Config;
use crate::daemon::log;
use crate::job::JobStatus;
use std::path::Path;
use std::time::{Duration, Instant};

/// The name of the file that says that the hook of this job ran.
const CLAIM_FILE: &str = "hook.ran";

/// The name of the file that holds the output of the hook.
const LOG_FILE: &str = "hook.log";

/// The time that a hook gets after `TERM` and before `KILL`.
const GRACE: Duration = Duration::from_secs(2);

/// The maximum size of the log of the hook.
///
/// The time limit is not a limit on the output. A hook of three seconds that
/// writes with no stop made a file of 3.7GB in the state directory, for one
/// job. qex stops a hook that goes above this size, and it cuts the file to it.
/// A hook that writes a megabyte is not a hook that notifies a person.
const OUTPUT_LIMIT: u64 = 1 << 20;

/// Runs the stop hook for one job, and gives control back when it stops.
///
/// Call this function only after the terminal state of the job is on the disk.
/// The caller then waits for the hook and holds nothing: the job, the queue and
/// the coordinator do not need this process any more.
///
/// The function does nothing when the config file gives no hook, when the state
/// is not terminal, when the filter does not name the state, or when a
/// different process already ran the hook for this job.
pub fn fire(cfg: &Config, dir: &Path, status: &JobStatus) {
    if cfg.hooks.on_stop.is_empty() || !status.state.is_terminal() {
        return;
    }
    if !cfg.hooks.runs_on(status.state) {
        return;
    }
    if !claim(dir, status) {
        return;
    }

    let limit = cfg.hook_timeout().unwrap_or(Duration::from_secs(30));
    let verdict = match run(cfg, dir, status, limit) {
        Ok(text) => text,
        // A hook that qex could not start is a fault of the config file. Name
        // the program that failed, and say which file holds it. The job keeps
        // its result.
        Err(e) => format!(
            "did not start: {e}. Test the program `{}` in `[hooks] on_stop` of the config \
             file. The job keeps its result.",
            cfg.hooks.on_stop[0]
        ),
    };

    // Write the verdict of qex where a qex command can read it.
    //
    // Before this, the verdict went to the log of the supervisor or of the
    // coordinator, AND NO COMMAND READS THOSE FILES. A user whose notification
    // did not arrive had no way to learn the reason. `qex logs <id> --hook`
    // gives this file.
    note(dir, &format!("qex: the stop hook {verdict}"));
    log(&format!("the stop hook of the job {} {verdict}", status.id));
}

/// Adds one line to the log of the hook.
fn note(dir: &Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(LOG_FILE))
    {
        writeln!(f, "{text}").ok();
    }
}

/// Runs the stop hook in a thread of its own.
///
/// The coordinator uses this form. It makes a job terminal while it holds the
/// lock of the queue, and a hook that hangs must never hold that lock.
///
/// THE TIME LIMIT STOPS WITH THE COORDINATOR. This thread applies the limit,
/// and a thread dies with its process. A coordinator that stops while the hook
/// operates thus leaves the hook to the init process with no limit, and a hook
/// that hangs then stays on the machine. The coordinator uses this form for the
/// jobs that have no supervisor only: `cancelled`, `skipped`, and a job whose
/// supervisor left no result. The supervisor path keeps its limit at all times,
/// because the supervisor waits for the hook itself.
pub fn fire_detached(cfg: &Config, dir: &Path, status: &JobStatus) {
    if cfg.hooks.on_stop.is_empty() || !status.state.is_terminal() {
        return;
    }
    let cfg = cfg.clone();
    let dir = dir.to_path_buf();
    let status = status.clone();
    std::thread::spawn(move || fire(&cfg, &dir, &status));
}

/// Takes the right to run the hook of this job. Gives `true` to the winner.
///
/// `create_new` is the whole mechanism. The operating system gives the file to
/// one caller, so two processes that stop the same job together cannot both
/// run the hook.
fn claim(dir: &Path, status: &JobStatus) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(CLAIM_FILE))
    {
        Ok(mut f) => {
            // The contents are for a person who reads the job directory. The
            // existence of the file is what qex acts on.
            writeln!(
                f,
                "{} {} {}",
                status.state,
                status.id,
                crate::sys::now_secs()
            )
            .ok();
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => {
            // qex cannot prove that the hook runs one time only, so it does not
            // run it. A directory that qex deleted is the usual cause.
            log(&format!(
                "qex did not run the stop hook of the job {}: it could not write {} ({e})",
                status.id,
                dir.join(CLAIM_FILE).display()
            ));
            false
        }
    }
}

/// Starts the hook, waits for it, and applies the time limit.
///
/// Gives the words for the log, or the error of a command that did not start.
fn run(cfg: &Config, dir: &Path, status: &JobStatus, limit: Duration) -> std::io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;

    // The output goes beside the record of the job. `qex clean` and `qex gc`
    // then delete it with the job, and the mode is the mode of the other files
    // of the job, because a hook can write a token to its output.
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(LOG_FILE))?;
    let err = out.try_clone()?;

    let mut cmd = std::process::Command::new(&cfg.hooks.on_stop[0]);
    cmd.args(&cfg.hooks.on_stop[1..])
        // The directory of the job, so a hook that reads a file of the job
        // needs no path.
        //
        // A job can delete its own directory. `spawn` gives an error for a
        // directory that is not there, and the hook would then never run, so
        // qex uses the root directory in that case.
        .current_dir(match std::path::Path::new(&status.cwd) {
            p if !status.cwd.is_empty() && p.is_dir() => p.to_path_buf(),
            _ => std::path::PathBuf::from("/"),
        })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err));

    // The data of the job goes in the environment, and NEVER in the command
    // line. A job name comes from the user of the queue, and a name such as
    // `; rm -rf ~` must be a name and never a command. qex thus builds no text
    // that a shell reads: it starts the program of `on_stop` directly, and a
    // shell that the user names in `on_stop` reads these values as variables.
    for (key, value) in variables(dir, status) {
        cmd.env(key, value);
    }

    unsafe {
        cmd.pre_exec(|| {
            // A process group of its own, so the time limit below reaches each
            // child of the hook. A hook that starts `sleep 300 &` must not
            // leave that process on the machine.
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;

    // Wait for the hook. Stop it at the time limit, and stop it also when its
    // output goes above the size limit.
    let deadline = start + limit;
    let log_path = dir.join(LOG_FILE);
    let mut too_slow = false;
    let mut too_large = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                // qex cannot tell if the hook stopped, so it stops the hook.
                // A process that qex cannot see must not stay on the machine.
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
                return Ok(format!("gave an error at a test of its state: {e}"));
            }
        }

        if Instant::now() >= deadline {
            too_slow = true;
            break;
        }
        if std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0) > OUTPUT_LIMIT {
            too_large = true;
            break;
        }
        // A short interval, because a hook can write a large quantity between
        // two tests of the size.
        std::thread::sleep(Duration::from_millis(20));
    }

    if too_slow || too_large {
        stop(pid, &mut child);

        if too_large {
            // Cut the file. The state directory must not hold gigabytes because
            // one hook wrote with no stop.
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&log_path) {
                f.set_len(OUTPUT_LIMIT).ok();
            }
            return Ok(format!(
                "wrote more than {} and qex stopped it. qex cut {}. Write less in the hook.",
                crate::units::format_size(OUTPUT_LIMIT),
                log_path.display()
            ));
        }

        return Ok(format!(
            "used more than its time limit of {} and qex stopped it. \
             Make the hook faster, or increase `[hooks] timeout`.",
            crate::units::format_duration(limit)
        ));
    }

    let exit = child.wait()?;
    if exit.success() {
        return Ok(format!(
            "ran in {}",
            crate::units::format_duration(start.elapsed())
        ));
    }
    Ok(format!(
        "stopped with {exit}. Read {} for its output.",
        dir.join(LOG_FILE).display()
    ))
}

/// Stops each process of the hook, and waits for it.
///
/// The signal goes to the process group, so a hook that started children stops
/// completely. `KILL` follows `TERM`, and a process cannot avoid `KILL`.
fn stop(pid: i32, child: &mut std::process::Child) {
    unsafe {
        libc::killpg(pid, libc::SIGTERM);
    }
    let grace = Instant::now() + GRACE;
    while Instant::now() < grace {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
    child.wait().ok();
}

/// Gives the variables that the hook receives.
///
/// The set answers the questions that a person asks when the notification
/// arrives: which job is this, what happened to it, how long did it take, and
/// where do I look now. A hook that needs more reads `spec.json` and
/// `status.json` in `QEX_JOB_DIR`, so this list stays short.
///
/// A variable that has no value is an empty text and not an absent variable. A
/// shell line such as `echo "$QEX_EXIT_CODE"` thus works for each job, and the
/// author of the hook writes no test for a variable that does not exist.
fn variables(dir: &Path, status: &JobStatus) -> Vec<(String, String)> {
    let text = |v: Option<i32>| v.map(|n| n.to_string()).unwrap_or_default();
    let mut set = vec![
        ("QEX_JOB_ID".into(), status.id.to_string()),
        ("QEX_JOB_NAME".into(), status.name.clone()),
        ("QEX_STATE".into(), status.state.to_string()),
        ("QEX_EXIT_CODE".into(), text(status.exit_code)),
        ("QEX_SIGNAL".into(), text(status.signal)),
        (
            "QEX_ELAPSED_SECS".into(),
            status
                .elapsed()
                .map(|d| d.as_secs().to_string())
                .unwrap_or_default(),
        ),
        ("QEX_CWD".into(), status.cwd.clone()),
        ("QEX_JOB_DIR".into(), dir.display().to_string()),
        ("QEX_ATTEMPTS".into(), status.attempts.to_string()),
        ("QEX_MAX_RSS".into(), status.usage.max_rss.to_string()),
        ("QEX_TAGS".into(), status.tags.join(" ")),
    ];
    for (_, value) in set.iter_mut() {
        *value = printable(value);
    }
    set
}

/// Replaces each control character with a space.
///
/// A NUL byte in a value stops the start of the hook: the system cannot receive
/// a variable with a NUL in it, and `spawn` gives "nul byte found in provided
/// data". A job NAME can hold that byte, because the name comes from the person
/// or the agent that submitted the job and not from the config file. The
/// notification of that job was then lost for ever, and the message named the
/// config file, which was correct.
///
/// The other control characters go for the same reason in a smaller degree: a
/// notification on a screen must not receive an escape sequence from a job name.
fn printable(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobState;

    fn status(state: JobState) -> JobStatus {
        let mut s = JobStatus::new(&crate::spec::JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "build".into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu: 1,
            mem: 1 << 30,
            timeout: None,
            tags: vec!["ci".into()],
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
        });
        s.state = state;
        s.exit_code = Some(3);
        s.started_at = Some(100);
        s.finished_at = Some(112);
        s
    }

    /// Gives an empty directory for one test.
    ///
    /// The name holds letters, numbers and hyphens only. A test that puts this
    /// path in a shell line with a bracket in it tests the shell and not qex.
    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qex-hook-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg_with(hook: &str) -> Config {
        toml::from_str(hook).unwrap()
    }

    /// The hook must run one time for each job. A person who receives the same
    /// notification two times learns to ignore every notification.
    #[test]
    fn the_hook_of_one_job_runs_one_time_only() {
        let dir = temp("once");
        let mark = dir.join("count");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"sh\", \"-c\", \"echo x >> {}\"]\n",
            mark.display()
        ));
        let status = status(JobState::Completed);

        // Two processes can make one job terminal. Both call this module.
        fire(&cfg, &dir, &status);
        fire(&cfg, &dir, &status);
        fire(&cfg, &dir, &status);

        let text = std::fs::read_to_string(&mark).unwrap();
        assert_eq!(text.lines().count(), 1, "the hook ran more than one time");
        assert!(dir.join(CLAIM_FILE).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hook receives the values that a reader of the notification needs.
    #[test]
    fn the_hook_receives_the_id_the_state_and_the_exit_code() {
        let dir = temp("env");
        let out = dir.join("env.txt");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"sh\", \"-c\", \"env | grep ^QEX_ > {}\"]\n",
            out.display()
        ));
        let status = status(JobState::Failed);
        fire(&cfg, &dir, &status);

        let text = std::fs::read_to_string(&out).unwrap();
        assert!(
            text.contains(&format!("QEX_JOB_ID={}", status.id)),
            "{text}"
        );
        assert!(text.contains("QEX_JOB_NAME=build"), "{text}");
        assert!(text.contains("QEX_STATE=failed"), "{text}");
        assert!(text.contains("QEX_EXIT_CODE=3"), "{text}");
        assert!(text.contains("QEX_ELAPSED_SECS=12"), "{text}");
        assert!(text.contains("QEX_JOB_DIR="), "{text}");
        assert!(text.contains("QEX_TAGS=ci"), "{text}");
        // A value that the job has not got is an empty text, and not an absent
        // variable. A shell line then needs no test.
        assert!(text.lines().any(|l| l == "QEX_SIGNAL="), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A name from a job must be a name, and never a command. The name goes in
    /// the environment, so qex builds no text that a shell reads.
    #[test]
    fn a_job_name_with_shell_characters_does_not_become_a_command() {
        let dir = temp("inject");
        let mark = dir.join("owned");
        let out = dir.join("name.txt");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"sh\", \"-c\", \"printf %s \\\"$QEX_JOB_NAME\\\" > {}\"]\n",
            out.display()
        ));
        let mut status = status(JobState::Completed);
        status.name = format!("x; touch {}", mark.display());
        fire(&cfg, &dir, &status);

        assert!(
            !mark.exists(),
            "a job name became a command; the name must stay in the environment"
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), status.name);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A hook that hangs must not hold the caller for ever. The caller runs the
    /// hook after the terminal state is on the disk, so the limit is the time
    /// that a job directory keeps a process, and no more.
    #[test]
    fn a_hook_that_hangs_stops_at_its_time_limit() {
        let dir = temp("hang");
        let cfg = cfg_with("[hooks]\non_stop = [\"sleep\", \"60\"]\ntimeout = \"1s\"\n");
        let start = Instant::now();
        fire(&cfg, &dir, &status(JobState::Completed));
        let took = start.elapsed();
        assert!(
            took < Duration::from_secs(10),
            "the hook held the caller for {took:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A command that does not exist must not stop the caller, and it must not
    /// change the job.
    #[test]
    fn a_hook_that_does_not_exist_is_reported_and_changes_nothing() {
        let dir = temp("missing");
        let cfg = cfg_with("[hooks]\non_stop = [\"qex-no-such-program\"]\n");
        let status = status(JobState::Completed);
        fire(&cfg, &dir, &status);
        // The claim stays: qex tried, and a second try would notify two times.
        assert!(dir.join(CLAIM_FILE).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The filter decides which jobs give a notification. A queue with many
    /// jobs and a notification for each one is a notification that a person
    /// turns off.
    #[test]
    fn the_filter_selects_the_states_that_notify() {
        let dir = temp("filter");
        let mark = dir.join("ran");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"touch\", \"{}\"]\non_stop_states = [\"failed\"]\n",
            mark.display()
        ));

        fire(&cfg, &dir, &status(JobState::Completed));
        assert!(!mark.exists(), "the filter must stop this state");
        assert!(
            !dir.join(CLAIM_FILE).exists(),
            "a state that the filter stops must not take the claim"
        );

        fire(&cfg, &dir, &status(JobState::Failed));
        assert!(mark.exists(), "the filter must permit this state");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A job name that holds a NUL byte must not stop the notification.
    ///
    /// A job file can give the name `a\0b`. qex accepts that name, and
    /// `Command::env` then refuses the value: `spawn` gives "nul byte found in
    /// provided data". The hook never started, the claim was already taken, and
    /// the message of that job was lost for ever — while the log named the
    /// config file, which was correct. The name comes from the person who
    /// submitted the job, so job data decided that a notification did not
    /// arrive.
    #[test]
    fn a_job_name_with_a_control_byte_still_runs_the_hook() {
        let dir = temp("nul");
        let out = dir.join("name.txt");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"sh\", \"-c\", \"printf %s \\\"$QEX_JOB_NAME\\\" > {}\"]\n",
            out.display()
        ));
        let mut status = status(JobState::Completed);
        status.name = "a\0b\u{1b}[31mc\nd".to_string();
        status.tags = vec!["x\0y".to_string()];
        fire(&cfg, &dir, &status);

        let text = std::fs::read_to_string(&out).unwrap_or_default();
        assert_eq!(text, "a b [31mc d", "each control byte must become a space");

        // The verdict of qex must not say that the hook did not start.
        let log = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert!(!log.contains("did not start"), "got: {log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The verdict of qex must reach a file that a qex command reads.
    ///
    /// Before this, the verdict went to the log of the supervisor or of the
    /// coordinator, and no command reads those files. A user whose notification
    /// did not arrive had no way to learn the reason.
    #[test]
    fn the_verdict_of_qex_goes_into_the_log_of_the_hook() {
        let dir = temp("verdict");
        let cfg = cfg_with("[hooks]\non_stop = [\"qex-no-such-program\"]\n");
        fire(&cfg, &dir, &status(JobState::Completed));

        let log = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert!(log.contains("qex: the stop hook"), "got: {log}");
        assert!(log.contains("qex-no-such-program"), "got: {log}");
        assert!(
            log.contains("on_stop"),
            "the remedy must name the field: {log}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A hook that writes with no stop must not fill the disk.
    ///
    /// The time limit is not a limit on the output: a hook of three seconds
    /// wrote 3.7GB into the state directory, for one job.
    #[test]
    fn a_hook_that_writes_without_a_stop_is_stopped_and_its_log_is_cut() {
        let dir = temp("flood");
        let cfg =
            cfg_with("[hooks]\non_stop = [\"yes\", \"aaaaaaaaaaaaaaaa\"]\ntimeout = \"60s\"\n");
        let start = Instant::now();
        fire(&cfg, &dir, &status(JobState::Completed));

        // The size limit, and not the time limit, stopped this hook.
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the size limit must stop the hook before the time limit"
        );
        let size = std::fs::metadata(dir.join(LOG_FILE)).unwrap().len();
        assert!(
            size <= OUTPUT_LIMIT + 4096,
            "the log of the hook is {size} bytes and the limit is {OUTPUT_LIMIT}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A job that still operates must never notify. The state on the disk is
    /// the state that the hook reports.
    #[test]
    fn a_job_that_did_not_stop_does_not_run_the_hook() {
        let dir = temp("running");
        let mark = dir.join("ran");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"touch\", \"{}\"]\n",
            mark.display()
        ));
        fire(&cfg, &dir, &status(JobState::Running));
        assert!(!mark.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
