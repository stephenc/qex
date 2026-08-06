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

use crate::config::{Config, EnforceMode};
use std::path::{Path, PathBuf};

/// The result of the test for the enforcement of limits.
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// Gives the path of the new cgroup, or `None` if qex did not make one.
#[cfg(target_os = "linux")]
pub fn create_job_cgroup(cfg: &Config, id: &uuid::Uuid, mem_claim: u64) -> Option<PathBuf> {
    if !cfg.enforce.mode.is_on() {
        return None;
    }
    let Availability::Available(base) = availability() else {
        return None;
    };

    // Give the memory controller to the child directories.
    std::fs::write(base.join("cgroup.subtree_control"), b"+memory").ok();

    let dir = base.join(format!("qex-{id}"));
    std::fs::create_dir_all(&dir).ok()?;

    match cfg.enforce.mode {
        EnforceMode::Soft => {
            // `memory.high` slows the job and reclaims its memory. The job
            // continues. `memory.max` stops the job, and it is the second limit.
            let max = (mem_claim as f64 * cfg.enforce.mem_overcommit) as u64;
            std::fs::write(dir.join("memory.high"), mem_claim.to_string()).ok();
            std::fs::write(dir.join("memory.max"), max.to_string()).ok();
        }
        EnforceMode::Hard => {
            std::fs::write(dir.join("memory.max"), mem_claim.to_string()).ok();
        }
        EnforceMode::Off => return None,
    }

    Some(dir)
}

#[cfg(not(target_os = "linux"))]
pub fn create_job_cgroup(_cfg: &Config, _id: &uuid::Uuid, _mem: u64) -> Option<PathBuf> {
    None
}

/// Puts a process in a cgroup.
pub fn add_process(cgroup: &Path, pid: i32) -> bool {
    std::fs::write(cgroup.join("cgroup.procs"), pid.to_string()).is_ok()
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

        assert!(!was_oom_killed(&dir), "a new job has no out-of-memory record");
        mark_oom(&dir);
        assert!(was_oom_killed(&dir));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The default mode gives no warning, because it promises no limit.
    #[test]
    fn the_default_mode_gives_no_warning() {
        let cfg = Config::default();
        assert_eq!(cfg.enforce.mode, EnforceMode::Off);
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
                let warning = startup_warning(&cfg).expect("qex must warn about a limit it cannot apply");
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
                assert!(path.exists(), "the cgroup path must exist: {}", path.display());
            }
            Availability::Unavailable(reason) => {
                assert!(!reason.is_empty(), "the reason must not be empty");
            }
        }
    }
}
