//! This module applies the memory claim as a true limit.
//!
//! Enforcement is optional and it is off by default. A claim then controls the
//! queue only, and the behaviour is the same on Linux and on macOS.
//!
//! Linux can apply a memory limit with cgroup v2. macOS has no equivalent, so
//! this module does nothing there.
//!
//! qex does not apply a CPU limit. The `cpu` controller is not available to a
//! user on a usual Linux system, and macOS has no equivalent. The queue
//! controls the number of cores instead.
//!
//! # Why the coordinator needs a delegated cgroup
//!
//! A process can write a cgroup file only in a directory that it owns. The
//! login session of a user is owned by the root user, so a coordinator that
//! starts from a shell cannot make a cgroup for a job. A coordinator that
//! starts with `systemd-run --user --property=Delegate=yes` receives a
//! directory that it owns.
//!
//! `systemd-run` makes a temporary unit. systemd holds that unit in memory and
//! writes no file to the disk.

use crate::config::Config;
// The mode selects which cgroup file qex writes, and cgroups are Linux only.
// macOS therefore never reads this type, and an import of it there is an
// unused import, which CI treats as an error.
#[cfg(target_os = "linux")]
use crate::config::EnforceMode;
use std::path::{Path, PathBuf};

/// The result of the test for the enforcement of limits.
///
/// macOS makes the `Unavailable` value only, because macOS has no cgroup. The
/// type stays the same on both systems, so the code that reads it needs no
/// condition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Availability {
    /// qex can apply a memory limit. The path is the cgroup of the coordinator.
    Available(PathBuf),
    /// qex cannot apply a limit. The text gives the reason.
    Unavailable(String),
}

/// Tests if this process can make a cgroup for each job.
///
/// The test writes nothing. It reads the cgroup of this process, then tests the
/// owner and the controllers of that directory.
#[cfg(target_os = "linux")]
pub fn availability() -> Availability {
    use std::os::unix::fs::MetadataExt;

    let Ok(text) = std::fs::read_to_string("/proc/self/cgroup") else {
        return Availability::Unavailable("this system does not use cgroup v2".into());
    };

    // A cgroup v2 line has this form: "0::/user.slice/user-1000.slice/...".
    let Some(rel) = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|s| s.trim().to_string())
    else {
        return Availability::Unavailable("this system does not use cgroup v2".into());
    };

    let dir = PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    let Ok(meta) = std::fs::metadata(&dir) else {
        return Availability::Unavailable(format!("qex cannot read {}", dir.display()));
    };

    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Availability::Unavailable(format!(
            "the cgroup {} belongs to the user {}, so qex cannot make a cgroup for a job. \
             Start the coordinator with systemd, or set [enforce] mode = \"off\".",
            dir.display(),
            meta.uid()
        ));
    }

    // The parent must pass the memory controller to its children.
    let controllers = std::fs::read_to_string(dir.join("cgroup.controllers")).unwrap_or_default();
    if !controllers.split_whitespace().any(|c| c == "memory") {
        return Availability::Unavailable(format!(
            "the cgroup {} does not have the memory controller, so qex cannot limit the memory",
            dir.display()
        ));
    }

    Availability::Available(dir)
}

#[cfg(not(target_os = "linux"))]
pub fn availability() -> Availability {
    Availability::Unavailable(
        "this system cannot limit the memory of a job; qex uses the claims for the queue only"
            .into(),
    )
}

/// Makes a cgroup for one job and sets the memory limits.
///
/// Every step reports its fault. A limit that qex could not apply must never
/// look like a limit that operates.
#[cfg(target_os = "linux")]
pub fn create_job_cgroup(cfg: &Config, id: &uuid::Uuid, mem_claim: u64) -> Result<PathBuf, String> {
    if !cfg.enforce.mode.is_on() {
        return Err("the config file sets [enforce] mode = \"off\"".into());
    }
    let base = match availability() {
        Availability::Available(b) => b,
        Availability::Unavailable(reason) => return Err(reason),
    };

    // Give the memory controller to the child directories.
    //
    // This write fails with EBUSY while a process is in this directory, because
    // cgroup v2 refuses a directory that holds both processes and children.
    // Move this process to a child directory first, then the write succeeds.
    let leaf = base.join("qex-main");
    if std::fs::read_to_string(base.join("cgroup.procs"))
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        std::fs::create_dir_all(&leaf)
            .map_err(|e| format!("qex could not make {}: {e}", leaf.display()))?;
        move_processes(&base, &leaf)?;
    }

    std::fs::write(base.join("cgroup.subtree_control"), b"+memory").map_err(|e| {
        format!(
            "qex could not give the memory controller to {}: {e}",
            base.display()
        )
    })?;

    let dir = base.join(format!("qex-{id}"));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("qex could not make {}: {e}", dir.display()))?;

    // Test that the controller arrived. Without this test, qex writes to a file
    // that does not exist and reports a limit that it did not apply.
    if !dir.join("memory.max").exists() {
        std::fs::remove_dir(&dir).ok();
        return Err(format!(
            "the cgroup {} has no memory.max file, so this system cannot limit the memory",
            dir.display()
        ));
    }

    match cfg.enforce.mode {
        EnforceMode::Soft => {
            // `memory.high` slows the job and reclaims its memory. The job
            // continues. `memory.max` stops the job, and it is the second limit.
            let max = (mem_claim as f64 * cfg.enforce.mem_overcommit) as u64;
            write_limit(&dir, "memory.high", mem_claim)?;
            write_limit(&dir, "memory.max", max)?;
        }
        EnforceMode::Hard => {
            write_limit(&dir, "memory.max", mem_claim)?;
        }
        EnforceMode::Off => unreachable!("this function tests the mode above"),
    }

    Ok(dir)
}

#[cfg(target_os = "linux")]
fn write_limit(dir: &Path, file: &str, value: u64) -> Result<(), String> {
    std::fs::write(dir.join(file), value.to_string())
        .map_err(|e| format!("qex could not write {}/{file}: {e}", dir.display()))
}

/// Moves every process of one cgroup to a different cgroup.
#[cfg(target_os = "linux")]
fn move_processes(from: &Path, to: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(from.join("cgroup.procs"))
        .map_err(|e| format!("qex could not read {}/cgroup.procs: {e}", from.display()))?;
    for line in text.lines() {
        let pid = line.trim();
        if pid.is_empty() {
            continue;
        }
        // A process that stops between the read and the write gives an error.
        // That error is not a fault of qex, so this code continues.
        std::fs::write(to.join("cgroup.procs"), pid).ok();
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn create_job_cgroup(_cfg: &Config, _id: &uuid::Uuid, _mem: u64) -> Result<PathBuf, String> {
    Err("this system cannot limit the memory of a job".into())
}

/// Puts a process in a cgroup.
pub fn add_process(cgroup: &Path, pid: i32) -> Result<(), String> {
    std::fs::write(cgroup.join("cgroup.procs"), pid.to_string())
        .map_err(|e| format!("qex could not put the process {pid} in the cgroup: {e}"))
}

/// Moves this process out of a cgroup, back to the parent cgroup.
///
/// A cgroup directory holds processes or child directories, and a directory
/// with a process in it cannot be deleted.
pub fn leave_cgroup(cgroup: &Path) {
    if let Some(parent) = cgroup.parent() {
        let pid = std::process::id().to_string();
        std::fs::write(parent.join("cgroup.procs"), &pid).ok();
    }
}

/// Tests if the kernel stopped a job because it reached its memory limit.
///
/// The file `memory.events` counts the events of the cgroup. A value above zero
/// in `oom_kill` shows that the kernel stopped a process of this job.
#[cfg(target_os = "linux")]
pub fn cgroup_had_oom(cgroup: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(cgroup.join("memory.events")) else {
        return false;
    };
    text.lines()
        .filter_map(|l| l.strip_prefix("oom_kill "))
        .filter_map(|n| n.trim().parse::<u64>().ok())
        .any(|n| n > 0)
}

#[cfg(not(target_os = "linux"))]
pub fn cgroup_had_oom(_cgroup: &Path) -> bool {
    false
}

/// Stops every process of a cgroup with one write.
///
/// No process can avoid this method. A process cannot leave the cgroup, and it
/// cannot start a child outside the cgroup.
#[cfg(target_os = "linux")]
pub fn kill_cgroup(cgroup: &Path) -> bool {
    std::fs::write(cgroup.join("cgroup.kill"), b"1").is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn kill_cgroup(_cgroup: &Path) -> bool {
    false
}

/// Deletes the cgroup of a job.
pub fn remove_cgroup(cgroup: &Path) {
    // A cgroup directory is removed with `rmdir`. It must be empty of processes.
    std::fs::remove_dir(cgroup).ok();
}

/// Gives the cgroup path of one job, if qex made one.
pub fn job_cgroup_path(job_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(job_dir.join("cgroup")).ok()?;
    let path = PathBuf::from(text.trim());
    path.exists().then_some(path)
}

/// Records the cgroup path of a job, so the supervisor can find it again.
pub fn record_cgroup_path(job_dir: &Path, cgroup: &Path) {
    std::fs::write(job_dir.join("cgroup"), cgroup.to_string_lossy().as_bytes()).ok();
}

/// Gives the cgroup of this process, whatever made that cgroup.
///
/// This function does not need enforcement. Every process on a Linux system
/// with cgroup v2 is in a cgroup, and the session of the user has one. qex can
/// thus read the out-of-memory count for a job even when it sets no limit.
#[cfg(target_os = "linux")]
pub fn own_cgroup() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let dir = PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    dir.exists().then_some(dir)
}

#[cfg(not(target_os = "linux"))]
pub fn own_cgroup() -> Option<PathBuf> {
    None
}

/// Gives the number of processes that the out-of-memory killer stopped in a
/// cgroup.
///
/// Read this value before the job starts and after it stops. An increase shows
/// that the kernel stopped a process for memory.
///
/// This measurement needs no limit from qex. Without it, the state `oom` is not
/// available in the usual configuration, and qex would report `killed` for a
/// job that no command stopped.
#[cfg(target_os = "linux")]
pub fn oom_count(cgroup: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(cgroup.join("memory.events")) else {
        return 0;
    };
    text.lines()
        .filter_map(|l| l.strip_prefix("oom_kill "))
        .filter_map(|n| n.trim().parse::<u64>().ok())
        .next()
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
pub fn oom_count(_cgroup: &Path) -> u64 {
    0
}

/// Tests if the out-of-memory killer stopped a job.
///
/// The kernel stops a process with `SIGKILL` for an out-of-memory event. That
/// signal is the signal that `qex kill` sends, so this test separates the two
/// causes. The states `oom` and `killed` need different corrections.
pub fn was_oom_killed(job_dir: &Path) -> bool {
    if job_dir.join("oom").exists() {
        return true;
    }
    match job_cgroup_path(job_dir) {
        Some(cgroup) => cgroup_had_oom(&cgroup),
        None => false,
    }
}

/// Records an out-of-memory event for a job.
pub fn mark_oom(job_dir: &Path) {
    std::fs::write(job_dir.join("oom"), b"1").ok();
}

/// Deletes the out-of-memory record of a job.
///
/// The record belongs to ONE attempt. qex starts the job again with a larger
/// claim after such a kill, and a record that stays would make the next attempt
/// an out-of-memory kill as well, whatever stopped it.
pub fn clear_oom(job_dir: &Path) {
    std::fs::remove_file(job_dir.join("oom")).ok();
}

/// Records that a command stopped this job.
///
/// `qex kill` writes this mark BEFORE it sends the signal.
///
/// The kernel and `qex kill` both use `SIGKILL`, and the cgroup counter is not
/// exact when qex applies no limit: qex then reads the counter of the session,
/// which also counts a kill in a different program of the same user. A job that
/// a person stopped must NEVER look like an out-of-memory kill, because qex
/// answers an out-of-memory kill with a larger claim and a new attempt. It must
/// not repeat work that somebody stopped on purpose.
///
/// This mark is thus the first evidence, and it wins against the counter.
pub fn mark_user_kill(job_dir: &Path) {
    std::fs::write(job_dir.join("killed-by-user"), b"1").ok();
}

/// Tests if a command stopped this job.
pub fn was_user_killed(job_dir: &Path) -> bool {
    job_dir.join("killed-by-user").exists()
}

/// Tests if this machine can tell an out-of-memory kill from another kill.
///
/// Linux counts the kills of the out-of-memory killer in `memory.events`, and
/// every process is in a cgroup, so the evidence is there whether qex applies a
/// limit or not. macOS has no cgroup and no equivalent counter.
///
/// qex uses this test to say what it does not know. A kill with no evidence
/// gives the state `killed`, which is the safe answer: qex starts no new
/// attempt for it. A reader must learn that qex could not tell, and not believe
/// that qex knew.
pub fn oom_evidence_is_available() -> bool {
    own_cgroup().is_some()
}

/// The name of the variable that stops a second start with systemd.
///
/// systemd is on Linux only, and macOS thus never reads this name.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const REEXEC_VAR: &str = "QEX_SYSTEMD_STARTED";

/// Starts the coordinator again in a temporary systemd unit, if that step gives
/// it a cgroup that it owns.
///
/// A coordinator that starts from a login shell is in a cgroup of the root
/// user, and it cannot make a cgroup for a job. `systemd-run --user` with
/// `Delegate=yes` gives the coordinator a cgroup that it owns.
///
/// systemd holds a temporary unit in memory. It writes no file to the disk, so
/// this step leaves nothing in `/etc` and nothing in the home directory.
///
/// Gives `true` if this process must stop, because the new process continues
/// the work.
#[cfg(target_os = "linux")]
pub fn restart_with_systemd(cfg: &Config) -> bool {
    if !cfg.enforce.mode.is_on() || !cfg.enforce.use_systemd {
        return false;
    }
    // A limit is possible already, so this step is not necessary.
    if matches!(availability(), Availability::Available(_)) {
        return false;
    }
    // This process is the second start. A third start would repeat for ever.
    if std::env::var_os(REEXEC_VAR).is_some() {
        return false;
    }
    if !systemd_is_available() {
        return false;
    }

    let Ok(exe) = crate::paths::program_path() else {
        return false;
    };

    let result = std::process::Command::new("systemd-run")
        .args([
            "--user",
            "--quiet",
            "--collect",
            "--property=Delegate=yes",
            "--property=Description=qex coordinator",
            "--unit",
        ])
        .arg(format!("qex-{}", std::process::id()))
        .arg(&exe)
        .arg("daemon")
        .env(REEXEC_VAR, "1")
        .status();

    match result {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!(
                "qex: systemd-run gave the code {:?}, so qex continues without a memory limit",
                status.code()
            );
            false
        }
        Err(e) => {
            eprintln!(
                "qex: qex could not run systemd-run ({e}), so it continues without a memory limit"
            );
            false
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn restart_with_systemd(_cfg: &Config) -> bool {
    false
}

/// Tests that a systemd user manager operates.
#[cfg(target_os = "linux")]
fn systemd_is_available() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Tests the configuration and gives a warning if enforcement cannot operate.
///
/// The coordinator writes this warning at its start. A silent failure here is
/// dangerous: the user reads `mode = "hard"` in the config file and believes
/// that a limit is active.
pub fn startup_warning(cfg: &Config) -> Option<String> {
    if !cfg.enforce.mode.is_on() {
        return None;
    }
    match availability() {
        Availability::Available(_) => None,
        Availability::Unavailable(reason) => Some(format!(
            "the config file sets [enforce] mode = \"{:?}\", but qex cannot apply a memory \
             limit: {reason}. qex continues, and it uses the claims for the queue only.",
            cfg.enforce.mode
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_out_of_memory_record_is_read_back() {
        let dir = std::env::temp_dir().join(format!("qex-oom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(
            !was_oom_killed(&dir),
            "a new job has no out-of-memory record"
        );
        mark_oom(&dir);
        assert!(was_oom_killed(&dir));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The default mode gives no warning, because it promises no limit.
    #[test]
    fn the_default_mode_gives_no_warning() {
        let cfg = Config::default();
        assert_eq!(cfg.enforce.mode, crate::config::EnforceMode::Off);
        assert!(startup_warning(&cfg).is_none());
    }

    /// A mode that qex cannot apply must give a warning. Without the warning,
    /// the user believes that a limit is active when it is not.
    #[test]
    fn a_mode_that_cannot_operate_gives_a_warning() {
        let cfg: Config = toml::from_str("[enforce]\nmode = \"hard\"\n").unwrap();
        match availability() {
            Availability::Available(_) => {
                assert!(startup_warning(&cfg).is_none());
            }
            Availability::Unavailable(_) => {
                let warning =
                    startup_warning(&cfg).expect("qex must warn about a limit it cannot apply");
                assert!(warning.contains("mode"), "got: {warning}");
                assert!(
                    warning.contains("queue only"),
                    "the warning must say what qex does instead: {warning}"
                );
            }
        }
    }

    /// The test must give a clear answer on this machine, and it must not stop.
    #[test]
    fn the_availability_test_gives_an_answer() {
        match availability() {
            Availability::Available(path) => {
                assert!(
                    path.exists(),
                    "the cgroup path must exist: {}",
                    path.display()
                );
            }
            Availability::Unavailable(reason) => {
                assert!(!reason.is_empty(), "the reason must not be empty");
            }
        }
    }
}
