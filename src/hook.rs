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
//!   stops a hook that goes above `OUTPUT_LIMIT` while it runs, AND it cuts the
//!   log after each hook. A hook that wrote 20MB and stopped between two tests
//!   of the size kept every byte before the second step was there.
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

/// Which process ran the hook.
///
/// THIS EXISTS TO MAKE THE REDUNDANCY TESTABLE, and it is not decoration.
///
/// Three paths reach the hook of an ordinary job, and each one alone satisfies
/// "the person received exactly one message": the supervisor at the end of its
/// work, the supervisor on the path of a command that does not exist, and the
/// coordinator when it reaps that supervisor. A test that counts the messages
/// therefore passes when any ONE of the three operates, so all three can rot
/// and no test says a word. A hand deletion of each of them, one at a time,
/// measured that: the whole suite stayed green for each.
///
/// The claim file names the process that took the claim. A test can then assert
/// WHICH path notified, which is the property that the design of this feature
/// rests on: the SUPERVISOR notifies, so a job still notifies when no
/// coordinator operates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The supervisor of the job. It exists for each job that ran, and it
    /// continues when the coordinator stops.
    Supervisor,
    /// The coordinator. It notifies for the jobs that have no supervisor —
    /// `cancelled`, `skipped` — and for a supervisor that left no result.
    Coordinator,
}

impl Origin {
    /// The word that goes in the claim file. A person reads that file too.
    fn as_str(self) -> &'static str {
        match self {
            Self::Supervisor => "supervisor",
            Self::Coordinator => "coordinator",
        }
    }
}

/// Runs the stop hook for one job, and gives control back when it stops.
///
/// Call this function only after the terminal state of the job is on the disk.
/// The caller then waits for the hook and holds nothing: the job, the queue and
/// the coordinator do not need this process any more.
///
/// THIS FUNCTION READS THE CONFIG FILE NOW, and it does not take the
/// configuration of the caller. The coordinator reads its configuration one
/// time, at its start, and it operates for hours. A user who deleted a hook
/// from the file thus met the hook again on each job, and a user who added one
/// received nothing — while `qex config show` gave the new value. A stale
/// configuration in this module does not make qex do nothing; it makes qex RUN
/// A COMMAND THAT THE USER DELETED. The file is small, one job stops one time,
/// and the read is thus not expensive.
pub fn fire(origin: Origin, dir: &Path, status: &JobStatus) {
    // A job that is not terminal never notifies. Test that first, because it
    // needs no file.
    if !status.state.is_terminal() {
        return;
    }

    // The SHORT form of a fault in the file. This message goes into a log line
    // and not in front of a person whose command stopped. `Config::load` gives
    // some 20 lines of advice about an upgrade of the coordinator, which would
    // hide the one line that this reader needs — that no notification came.
    // `supervisor::main` takes the short form for the same reason.
    match Config::load_short() {
        Ok(cfg) => fire_with(origin, &cfg, dir, status),
        // A configuration that qex cannot read must not run a hook. qex cannot
        // know if the command in memory is still the command in the file.
        Err(e) => log(&format!(
            "qex did not run the stop hook of the job {}: it could not read the \
             configuration ({e}). Correct the config file.",
            status.id
        )),
    }
}

/// Runs the stop hook with a configuration that the caller supplies.
///
/// The tests use this form. Each other caller uses [`fire`], which reads the
/// file, because a configuration in memory can be older than the file.
fn fire_with(origin: Origin, cfg: &Config, dir: &Path, status: &JobStatus) {
    if cfg.hooks.on_stop.is_empty() || !status.state.is_terminal() {
        return;
    }
    if !cfg.hooks.runs_on(status.state) {
        return;
    }
    if !claim(origin, dir, status) {
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
///
/// The mode below is belt AND braces, and a deletion of it leaves the test
/// suite green. [`run`] opens the same file with the same mode before it starts
/// the hook, so on every path that a test can drive, the file already exists and
/// `create` does nothing. This call makes the file only when THAT open failed.
/// The mode stays: the two places that make this file must not disagree, and a
/// reader who sees one of them must find the same rule in the other.
fn note(dir: &Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(LOG_FILE))
    {
        // A newline before the text.
        //
        // qex cuts a log that is too large at the size limit, which is
        // frequently the middle of a line. Without this newline, the verdict
        // joins that line, and `qex logs --hook --tail 1` gives a megabyte.
        write!(f, "\n{text}\n").ok();
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
pub fn fire_detached(dir: &Path, status: &JobStatus) {
    if !status.state.is_terminal() {
        return;
    }
    let dir = dir.to_path_buf();
    let status = status.clone();
    // The thread reads the config file. The caller frequently holds the lock of
    // the queue, and a read of a file must not happen there.
    std::thread::spawn(move || fire(Origin::Coordinator, &dir, &status));
}

/// Takes the right to run the hook of this job. Gives `true` to the winner.
///
/// `create_new` is the whole mechanism. The operating system gives the file to
/// one caller, so two processes that stop the same job together cannot both
/// run the hook.
fn claim(origin: Origin, dir: &Path, status: &JobStatus) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(CLAIM_FILE))
    {
        Ok(mut f) => {
            // The contents are for a person who reads the job directory, and
            // for a test. The EXISTENCE of the file is what qex acts on; the
            // last word names the process that took it, so a test can assert
            // WHICH of the redundant paths notified. See [`Origin`].
            //
            //     completed 6f1c8f2e-… 1786171234 supervisor
            writeln!(
                f,
                "{} {} {} {}",
                status.state,
                status.id,
                crate::sys::now_secs(),
                origin.as_str()
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
                // TAKE THE DEAD PROCESS OUT OF THE PROCESS TABLE.
                //
                // `Child` does not do this when it is dropped, so a return
                // here left a zombie. On the path of the supervisor that costs
                // nothing, because the process stops a moment later. On the
                // path of the COORDINATOR it is a leak: that process operates
                // for hours, and each such hook leaves one entry for as long as
                // it operates.
                //
                // The `wait` is safe HERE and not in `stop`. There, a second
                // signal follows, and a `wait` before it would let the machine
                // give the same number to a different process. Here the signal
                // is already sent and no other follows.
                child.wait().ok();
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
    } else {
        child.wait().ok();
    }

    // Test the size again, whatever ended the loop.
    //
    // The loop tests the size between two sleeps, and it leaves at once when
    // the hook stops. A hook that wrote 20MB and then stopped inside one
    // interval thus kept every byte, while the documentation said that the
    // limit holds. The limit is on the file, and not on the speed of the hook.
    let cut = cut_log(&log_path);

    if too_large {
        return Ok(format!(
            "wrote more than {} and qex stopped it. qex cut {}. Write less in the hook.",
            crate::units::format_size(OUTPUT_LIMIT),
            log_path.display()
        ));
    }

    if too_slow {
        let mut text = format!(
            "used more than its time limit of {} and qex stopped it. \
             Make the hook faster, or increase `[hooks] timeout`.",
            crate::units::format_duration(limit)
        );
        if cut {
            text.push_str(&format!(
                " It also wrote more than {}, so qex cut its log.",
                crate::units::format_size(OUTPUT_LIMIT)
            ));
        }
        return Ok(text);
    }

    if cut {
        return Ok(format!(
            "wrote more than {} before it stopped. qex cut {}. Write less in the hook.",
            crate::units::format_size(OUTPUT_LIMIT),
            log_path.display()
        ));
    }

    match exit_of(&mut child) {
        Some(exit) if exit.success() => Ok(format!(
            "ran in {}",
            crate::units::format_duration(start.elapsed())
        )),
        Some(exit) => Ok(format!(
            "stopped with {exit}. Read `qex logs {} --hook` for its output.",
            status.id
        )),
        None => Ok("stopped, and qex could not read its result".to_string()),
    }
}

/// Cuts the log of the hook to the size limit. Gives `true` if it cut the file.
fn cut_log(path: &Path) -> bool {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size <= OUTPUT_LIMIT {
        return false;
    }
    // The state directory must not hold gigabytes because one hook wrote with
    // no stop.
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        f.set_len(OUTPUT_LIMIT).ok();
    }
    true
}

/// Gives the result of a hook that this code already waited for.
fn exit_of(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    child.try_wait().ok().flatten()
}

/// Stops each process of the hook, and waits for it.
///
/// The signal goes to the process group, so a hook that started children stops
/// completely. `KILL` follows `TERM`, and a process cannot avoid `KILL`.
fn stop(pid: i32, child: &mut std::process::Child) {
    unsafe {
        libc::killpg(pid, libc::SIGTERM);
    }

    // THIS CODE MUST NOT WAIT FOR THE HOOK DURING THE GRACE TIME.
    //
    // The second signal goes to the process group, because the hook can have
    // children and they must also stop. That signal is safe while the first
    // process of the group stays in the process table, and a `wait` takes it
    // out of that table. A `wait` here would thus let the machine give the same
    // number to a different process, and the signal below would reach the work
    // of somebody else. The supervisor has the same rule; see
    // `wait_without_reaping` there.
    //
    // The cost is the full grace time on this path, which is the path of a hook
    // that qex must stop.
    std::thread::sleep(GRACE);

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
        // THE SAFE NAME, which is the one form of a name that qex shows.
        //
        // A hook exists to put a name in front of a person: `notify-send
        // "$QEX_JOB_NAME"`, a line in a file, a message in a chat. That is the
        // same act as `qex list`, and it takes the same rule. A name is text
        // that another agent chose, and a raw name with an ESC byte, written to
        // a terminal by a hook of two words, moves the cursor and writes over
        // the text around it.
        //
        // The name that arrives here therefore goes back into a qex command as
        // it stands, because `resolve_id` finds a job by its safe name as well.
        // A hook that needs the name that the submitter typed reads
        // `status.json` in `QEX_JOB_DIR`.
        ("QEX_JOB_NAME".into(), crate::job::safe_name(&status.name)),
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
/// data". The values that carry that risk come from the person or the agent
/// that submitted the job, and not from the config file: the TAGS and the
/// DIRECTORY. The notification of such a job was lost for ever, and the message
/// named the config file, which was correct and no help at all.
///
/// The other control characters go for the same reason in a smaller degree: a
/// notification on a screen must not receive an escape sequence from a tag.
///
/// The job NAME does not depend on this function. It goes through
/// [`crate::job::safe_name`] first, which is stricter, and which is the rule for
/// every name that qex puts in front of a reader.
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
            nice: None,
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
        fire_with(Origin::Supervisor, &cfg, &dir, &status);
        fire_with(Origin::Supervisor, &cfg, &dir, &status);
        fire_with(Origin::Supervisor, &cfg, &dir, &status);

        let text = std::fs::read_to_string(&mark).unwrap();
        assert_eq!(text.lines().count(), 1, "the hook ran more than one time");
        assert!(dir.join(CLAIM_FILE).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The claim file names the process that ran the hook.
    ///
    /// Three paths reach the hook, and each one alone gives the person exactly
    /// one message. A test that counts the messages thus passes when any ONE of
    /// them operates, and all three can rot in silence — measured, by deleting
    /// each of them by hand. This word is what lets a test name the path that
    /// must operate.
    #[test]
    fn the_claim_file_names_the_process_that_ran_the_hook() {
        for (origin, word) in [
            (Origin::Supervisor, "supervisor"),
            (Origin::Coordinator, "coordinator"),
        ] {
            let dir = temp(word);
            let cfg = cfg_with("[hooks]\non_stop = [\"true\"]\n");
            let s = status(JobState::Completed);
            fire_with(origin, &cfg, &dir, &s);

            let text = std::fs::read_to_string(dir.join(CLAIM_FILE)).unwrap();
            assert_eq!(
                text.split_whitespace().next_back(),
                Some(word),
                "the claim file must name the process: {text:?}"
            );
            // The state and the id stay in front of it, for a person who reads
            // the job directory.
            assert!(text.starts_with("completed "), "got: {text:?}");
            assert!(text.contains(&s.id.to_string()), "got: {text:?}");
            std::fs::remove_dir_all(&dir).ok();
        }
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
        fire_with(Origin::Supervisor, &cfg, &dir, &status);

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
        fire_with(Origin::Supervisor, &cfg, &dir, &status);

        assert!(
            !mark.exists(),
            "a job name became a command; the name must stay in the environment"
        );
        // The SAFE name arrives, which is the name that `qex list` shows. Two
        // rules hold this test up, and each one alone is sufficient: the value
        // travels in the environment and never in a command line, and the name
        // itself holds no shell character when it gets there.
        //
        // THE EXPECTATION IS A LITERAL, and it does not call `safe_name`. A
        // test that builds its expectation with the function that it tests
        // passes for every result that both sides give, so it would pass with
        // the RAW name on both sides and measure nothing.
        let got = std::fs::read_to_string(&out).unwrap();
        // `; ` is a RUN of two characters that the safe form does not keep, and
        // a run becomes ONE `_`.
        assert!(
            got.starts_with("x_touch_"),
            "the safe form of the name must arrive: {got}"
        );
        assert!(
            !got.contains(';') && !got.contains(' ') && !got.contains('/'),
            "the name must carry no shell character and no path: {got}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The files of the hook hold the output of a command that a user wrote, so
    /// they take the mode of the other files of a job: the owner, and nobody
    /// else. `docs/security.md` states 0600 for `hook.log`.
    ///
    /// A hook writes a token as easily as a job does. `notify-send "$(cat
    /// ~/.netrc)"` is one line of a config file, and its output lands here.
    #[test]
    fn the_files_of_the_hook_are_readable_by_the_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let mode_of = |path: std::path::PathBuf| {
            std::fs::metadata(&path)
                .unwrap_or_else(|e| panic!("{} is not there: {e}", path.display()))
                .permissions()
                .mode()
                & 0o777
        };

        // A hook that RAN. `run` makes both files.
        let dir = temp("mode");
        let cfg = cfg_with("[hooks]\non_stop = [\"sh\", \"-c\", \"echo a secret\"]\n");
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));
        for name in [LOG_FILE, CLAIM_FILE] {
            let mode = mode_of(dir.join(name));
            assert_eq!(
                mode, 0o600,
                "{name} has the mode {mode:o}, and another user can read it"
            );
        }
        std::fs::remove_dir_all(&dir).ok();

        // A hook that DID NOT START. `run` opened no file, so `note` makes
        // `hook.log` itself, and it must give the same mode. This path holds
        // the name of the program that failed, which can name a directory of
        // the owner.
        let dir = temp("mode2");
        let cfg = cfg_with("[hooks]\non_stop = [\"qex-no-such-program\"]\n");
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));
        let mode = mode_of(dir.join(LOG_FILE));
        assert_eq!(
            mode, 0o600,
            "the log of a hook that did not start has the mode {mode:o}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hook gets no standard input.
    ///
    /// The hook is a child of the supervisor or of the coordinator, and it
    /// would take the standard input of that process. A hook that reads its
    /// input then waits for the whole of its time limit and gives no
    /// notification, and a hook that a person starts from a terminal takes the
    /// keys of that person. `qex run` is such a terminal.
    ///
    /// THE TEST ASKS THE SYSTEM WHAT THE INPUT IS, and it does not measure the
    /// time. A test that gave `cat` to the hook and waited passed with no
    /// `Stdio::null()` at all, because the input of `cargo test` is already
    /// closed on the machine that runs the suite. It measured the harness.
    ///
    /// THIS TEST STILL CANNOT PROVE THE LINE ON EVERY MACHINE. Where the suite
    /// itself runs with `/dev/null` on its input — measured here — a hook that
    /// INHERITED that input gives the same answer, so a deletion of
    /// `Stdio::null()` passes. The test holds the documented property, and the
    /// property matters most where qex runs from a terminal, which is where no
    /// automated suite runs. Do not delete the line because this test is green
    /// without it.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_hook_reads_no_standard_input() {
        let dir = temp("stdin");
        let out = dir.join("in.txt");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"sh\", \"-c\", \"readlink /proc/self/fd/0 > {}\"]\n",
            out.display()
        ));
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));

        assert_eq!(
            std::fs::read_to_string(&out).unwrap().trim(),
            "/dev/null",
            "the standard input of the hook must be /dev/null"
        );
        let log = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert!(log.contains("ran in"), "the hook must succeed: {log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The hook starts in the directory of the job, and a directory that is
    /// gone does not stop it.
    ///
    /// A job can delete its own directory — `rm -rf` in a build tree is
    /// ordinary work. `spawn` gives an error for a working directory that is
    /// not there, so the hook of such a job never ran and the message named the
    /// config file, which was correct and no help at all.
    #[test]
    fn the_hook_starts_in_the_directory_of_the_job_or_in_the_root() {
        let dir = temp("cwd");
        let out = dir.join("where.txt");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"sh\", \"-c\", \"pwd > {}\"]\n",
            out.display()
        ));

        // The directory of the job is there.
        let mut s = status(JobState::Completed);
        s.cwd = dir.display().to_string();
        fire_with(Origin::Supervisor, &cfg, &dir, &s);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap().trim(),
            dir.display().to_string(),
            "the hook must start in the directory of the job"
        );

        // The directory of the job is gone. The hook must still run.
        let second = temp("cwd2");
        let mut s = status(JobState::Completed);
        s.cwd = second.join("this-directory-is-gone").display().to_string();
        fire_with(Origin::Supervisor, &cfg, &second, &s);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap().trim(),
            "/",
            "a directory that is gone must not stop the hook"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&second).ok();
    }

    /// A hook that hangs must not hold the caller for ever. The caller runs the
    /// hook after the terminal state is on the disk, so the limit is the time
    /// that a job directory keeps a process, and no more.
    #[test]
    fn a_hook_that_hangs_stops_at_its_time_limit() {
        let dir = temp("hang");
        let cfg = cfg_with("[hooks]\non_stop = [\"sleep\", \"60\"]\ntimeout = \"1s\"\n");
        let start = Instant::now();
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));
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
        fire_with(Origin::Supervisor, &cfg, &dir, &status);
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

        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));
        assert!(!mark.exists(), "the filter must stop this state");
        assert!(
            !dir.join(CLAIM_FILE).exists(),
            "a state that the filter stops must not take the claim"
        );

        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Failed));
        assert!(mark.exists(), "the filter must permit this state");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A control byte in the data of a job must not stop the notification.
    ///
    /// A job file can give the name `a\0b` and the tag `x\0y`. qex accepts
    /// both, and `Command::env` then refuses the value: `spawn` gives "nul byte
    /// found in provided data". The hook never started, the claim was already
    /// taken, and the message of that job was lost for ever — while the log
    /// named the config file, which was correct and no help at all. The data
    /// comes from the person who submitted the job, so JOB DATA decided that a
    /// notification did not arrive.
    ///
    /// The name and the tags take different roads to the same guarantee. The
    /// name goes through `safe_name`, which keeps the letters, the numbers and
    /// `-_.` and nothing else. A tag has no such rule, so `printable` carries
    /// it, and this test asserts both.
    #[test]
    fn a_control_byte_in_the_data_of_a_job_still_runs_the_hook() {
        let dir = temp("nul");
        let out = dir.join("name.txt");
        let cfg = cfg_with(&format!(
            "[hooks]\non_stop = [\"sh\", \"-c\", \
             \"printf '%s|%s' \\\"$QEX_JOB_NAME\\\" \\\"$QEX_TAGS\\\" > {}\"]\n",
            out.display()
        ));
        let mut status = status(JobState::Completed);
        status.name = "a\0b\u{1b}[31mc\nd".to_string();
        status.tags = vec!["x\0y".to_string()];
        fire_with(Origin::Supervisor, &cfg, &dir, &status);

        let text = std::fs::read_to_string(&out).unwrap_or_default();
        let (name, tags) = text.split_once('|').unwrap_or(("", ""));
        assert_eq!(name, "a_b_31mc_d", "the name must take its safe form");
        assert_eq!(tags, "x y", "each control byte of a tag must become a space");
        assert!(
            !name.contains('\u{1b}') && !tags.contains('\u{1b}'),
            "no value may carry an escape byte to a screen: {text:?}"
        );

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
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));

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
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));

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

    /// A hook that writes a large file and then stops at once must also meet
    /// the size limit.
    ///
    /// The loop tests the size between two sleeps, and it left at once when the
    /// hook stopped. A hook that wrote 20MB inside one interval thus kept every
    /// byte, and the documentation said that the limit holds for each hook.
    #[test]
    fn a_hook_that_writes_a_large_file_quickly_also_meets_the_size_limit() {
        let dir = temp("fastflood");
        // This command writes 3MB and stops. It does not hang, so the time
        // limit and the loop have no part in the result.
        let cfg = cfg_with("[hooks]\non_stop = [\"head\", \"-c\", \"3000000\", \"/dev/zero\"]\n");
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Completed));

        let size = std::fs::metadata(dir.join(LOG_FILE)).unwrap().len();
        assert!(
            size <= OUTPUT_LIMIT + 4096,
            "the log of the hook is {size} bytes and the limit is {OUTPUT_LIMIT}"
        );
        let log = std::fs::read_to_string(dir.join(LOG_FILE)).unwrap_or_default();
        assert!(
            log.contains("wrote more than"),
            "the verdict must say that qex cut the file"
        );

        // The verdict must be a line of its own. qex cuts the file in the
        // middle of a line, so a verdict with no newline before it joins that
        // line, and `qex logs --hook --tail 1` then gives a megabyte.
        let last = log.lines().next_back().unwrap_or("");
        assert!(
            last.starts_with("qex: "),
            "the last line is {} bytes",
            last.len()
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
        fire_with(Origin::Supervisor, &cfg, &dir, &status(JobState::Running));
        assert!(!mark.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
