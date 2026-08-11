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

/// The two counts of `memory.events` that say what stopped a job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OomCounts {
    /// The times that this cgroup reached ITS OWN limit and the kernel could
    /// not free memory.
    pub oom: u64,
    /// The processes of this cgroup that ANY out-of-memory killer stopped.
    pub oom_kill: u64,
}

/// Reads the counts of a cgroup.
///
/// A count that the file does not name is zero. This function reads a file and
/// nothing else, so a test gives it a directory that the test made.
pub fn read_oom_counts(cgroup: &Path) -> Option<OomCounts> {
    let text = std::fs::read_to_string(cgroup.join("memory.events")).ok()?;
    let count = |name: &str| -> u64 {
        text.lines()
            .filter_map(|l| l.strip_prefix(name))
            .filter_map(|n| n.trim().parse::<u64>().ok())
            .next()
            .unwrap_or(0)
    };
    Some(OomCounts {
        oom: count("oom "),
        oom_kill: count("oom_kill "),
    })
}

/// Says whether the kernel stopped a job, and at which limit.
///
/// `memory.events` holds two counts, and they answer two different questions.
///
/// - `oom_kill` counts the processes of this cgroup that ANY out-of-memory
///   killer stopped. The killer of the whole machine raises this count as well.
/// - `oom` counts the times that this cgroup reached ITS OWN limit and the
///   kernel could not free memory. The limit of this cgroup alone raises it.
///
/// A NEW kill with a NEW rise in `oom` is thus a kill at the limit that qex
/// made from the claim: the claim was too small, and qex can act on that. A new
/// kill with NO new rise in `oom` came from outside this cgroup, so the claim
/// can be correct. Each answer is about a RISE, and never about the value that
/// a count holds: a count holds the events of every earlier attempt as well.
///
/// The limit of a PARENT cgroup gives the second answer as well. The parent
/// counts the event and this cgroup counts the kill. That answer is correct for
/// the parent too, because a larger claim for this job cannot move a limit that
/// belongs to a parent.
///
/// # Both counts need the value from the start of the attempt
///
/// `before` holds the counts that qex read before this attempt started. NEITHER
/// count starts at zero:
///
/// - qex reads the counter of the login session when it makes no cgroup, and
///   that counter holds the kills of every program of this user.
/// - qex names the cgroup of a job after the id of that job, so every attempt
///   of one job uses the SAME cgroup. The counts of an earlier attempt stay in
///   it. Without the value from the start, a job that raised `oom` on attempt 1
///   reads that rise again on attempt 2, and a kill from the machine then names
///   the claim.
///
/// `qex_made_cgroup` says whose counter this is. With no cgroup of its own, qex
/// cannot say which program the kernel stopped, so the answer is the weakest
/// one.
pub fn classify_oom(cgroup: &Path, qex_made_cgroup: bool, before: OomCounts) -> Option<OomScope> {
    let now = read_oom_counts(cgroup)?;

    // No new kill, no answer. A cgroup can reach its limit and free enough
    // memory after that, which raises `oom` and stops no process. The job then
    // stopped for a different reason, and qex must not report a kill for
    // memory.
    if now.oom_kill <= before.oom_kill {
        return None;
    }

    if !qex_made_cgroup {
        return Some(OomScope::Session);
    }

    if now.oom > before.oom {
        Some(OomScope::Job)
    } else {
        Some(OomScope::Machine)
    }
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

/// Watches the out-of-memory counts of ONE attempt of a job.
///
/// The supervisor makes this value BEFORE it starts the program of the attempt,
/// and it asks for the answer after that program stops. The counts from the
/// start live in here, so no caller passes them and no caller can pass the
/// wrong ones.
///
/// That matters because NEITHER count starts at zero. The counter of the login
/// session holds the kills of every program of this user, and qex names the
/// cgroup of a job after the id of the job, so every attempt of one job uses
/// the same cgroup and the counts of an earlier attempt stay in it.
pub struct OomWatch {
    /// The cgroup that qex reads for this attempt.
    cgroup: Option<PathBuf>,
    /// True when qex made that cgroup for THIS attempt.
    ///
    /// This value comes from the cgroup that this attempt made, and never from
    /// a path that an earlier attempt recorded. A cgroup of an earlier attempt
    /// survives, so a recorded path can name a cgroup that this attempt did not
    /// make, and the counts in it belong to that earlier attempt.
    qex_made_cgroup: bool,
    /// The counts at the start of this attempt.
    before: OomCounts,
}

impl OomWatch {
    /// Reads the counts before the program of the attempt starts.
    ///
    /// `job_cgroup` is the cgroup that THIS attempt made, and `None` when qex
    /// made none. With no cgroup of its own, qex reads the cgroup of the login
    /// session: it then still finds a kill, and it cannot name the victim.
    pub fn start(job_cgroup: Option<&Path>) -> Self {
        let cgroup = job_cgroup.map(|p| p.to_path_buf()).or_else(own_cgroup);
        let before = cgroup
            .as_deref()
            .and_then(read_oom_counts)
            .unwrap_or_default();
        Self {
            cgroup,
            qex_made_cgroup: job_cgroup.is_some(),
            before,
        }
    }

    /// Records what the counts say about this attempt.
    ///
    /// The supervisor calls this function more than one time, and `mark_oom`
    /// keeps the strongest answer, so a second call adds no fault.
    pub fn record(&self, job_dir: &Path) {
        let Some(cgroup) = self.cgroup.as_deref() else {
            return;
        };
        if let Some(scope) = classify_oom(cgroup, self.qex_made_cgroup, self.before) {
            mark_oom(job_dir, scope);
        }
    }
}

/// Tests if the out-of-memory killer stopped a job.
///
/// The kernel stops a process with `SIGKILL` for an out-of-memory event. That
/// signal is the signal that `qex kill` sends, so this test separates the two
/// causes. The states `oom` and `killed` need different corrections.
pub fn was_oom_killed(job_dir: &Path) -> bool {
    oom_evidence(job_dir).is_some()
}

/// How well qex knows that the kernel stopped a job for memory.
///
/// The values need different answers, and the difference between them is the
/// difference between a report and an action. qex ACTS on `Job` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomScope {
    /// The kernel stopped the job at the limit that qex made from the claim.
    ///
    /// qex made the cgroup of this job, so the counts belong to this job and to
    /// no other program, and the cgroup counted a NEW event at its own limit. The
    /// claim was therefore too small, and qex can act on that.
    Job,
    /// The kernel stopped the job, and NOT at the limit of this job.
    ///
    /// qex made the cgroup of this job, so the kill belongs to this job. The
    /// cgroup counted no NEW event at its own limit, so the memory of the machine,
    /// or a limit of a parent cgroup, stopped this job. The claim can be
    /// correct.
    ///
    /// This evidence REPORTS the state `oom`. It does not support a new attempt
    /// with a larger claim, and it teaches the learner nothing: the work can
    /// need no more memory than it asked for, and qex did not measure a need.
    Machine,
    /// qex read the counter of the session, because it made no cgroup.
    ///
    /// The counter of a cgroup counts the kills in each cgroup below it, so it
    /// also counts a kill in a different program of the same user. A machine
    /// that is short of memory is also the machine on which a person uses
    /// `kill -9`, so the two events arrive together.
    ///
    /// This evidence is sufficient to REPORT the state `oom`. It is not
    /// sufficient to run the job again with a larger claim, and it is not
    /// sufficient to teach the learner: the claim can be correct, and the
    /// machine full.
    Session,
}

impl OomScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Job => "job",
            Self::Machine => "machine",
            Self::Session => "session",
        }
    }

    /// How much this evidence supports an ACTION.
    ///
    /// qex keeps the strongest evidence that it holds for one attempt, because
    /// the supervisor tests more than one time. `Job` is the only value that
    /// starts a new attempt. `Machine` names the job that the kernel stopped,
    /// and `Session` cannot name it, so `Machine` tells a reader more.
    fn strength(self) -> u8 {
        match self {
            Self::Job => 2,
            Self::Machine => 1,
            Self::Session => 0,
        }
    }
}

/// Gives the evidence that qex holds for a kill for memory.
pub fn oom_evidence(job_dir: &Path) -> Option<OomScope> {
    if let Ok(text) = std::fs::read_to_string(job_dir.join("oom")) {
        return Some(match text.trim() {
            "job" => OomScope::Job,
            "machine" => OomScope::Machine,
            // A record from an earlier version of qex holds `1` and names no
            // scope. Read it as the weakest evidence: qex then reports the
            // state and starts no new attempt, which is the safe answer.
            _ => OomScope::Session,
        });
    }
    // THE RECORD IS THE ONLY SOURCE. This function does not read the cgroup.
    //
    // A count in a cgroup means nothing without the value from the start of the
    // attempt, and the supervisor alone holds that value: qex names the cgroup
    // of a job after the id of the job, so every attempt of one job uses the
    // same cgroup and the counts of an earlier attempt stay in it. The
    // supervisor records what it found, and this function reads that record.
    None
}

/// Records an out-of-memory event for a job, with the evidence for it.
pub fn mark_oom(job_dir: &Path, scope: OomScope) {
    // Keep the stronger evidence. The supervisor makes more than one test, and
    // a later test can read the counter of the session.
    if let Some(held) = oom_evidence(job_dir) {
        if held.strength() > scope.strength() {
            return;
        }
    }
    std::fs::write(job_dir.join("oom"), scope.as_str().as_bytes()).ok();
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

/// Deletes that mark.
///
/// The mark belongs to ONE attempt, in the same way as the out-of-memory
/// record. A job with `--retries` can stop with `qex kill` on the first attempt
/// and stop for memory on the second, and a mark that stayed would say that a
/// command stopped an attempt that no command touched. The record would then
/// lose the lesson of the kill for memory.
pub fn clear_user_kill(job_dir: &Path) {
    std::fs::remove_file(job_dir.join("killed-by-user")).ok();
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
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        assert!(
            !was_oom_killed(&dir),
            "a new job has no out-of-memory record"
        );
        mark_oom(&dir, OomScope::Session);
        assert!(was_oom_killed(&dir));
        assert_eq!(oom_evidence(&dir), Some(OomScope::Session));

        // The stronger evidence replaces the weaker evidence, and the weaker
        // evidence never replaces the stronger. qex acts on `Job` only, so a
        // second test that overwrote the first would decide the behaviour.
        mark_oom(&dir, OomScope::Job);
        assert_eq!(oom_evidence(&dir), Some(OomScope::Job));
        mark_oom(&dir, OomScope::Session);
        assert_eq!(oom_evidence(&dir), Some(OomScope::Job));

        clear_oom(&dir);
        assert_eq!(oom_evidence(&dir), None);

        // A record from an earlier version of qex names no scope. Read it as
        // the weaker evidence: qex then reports the state and starts no new
        // attempt, which is the safe answer.
        std::fs::write(dir.join("oom"), b"1").unwrap();
        assert_eq!(oom_evidence(&dir), Some(OomScope::Session));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The mark of a kill by a command must go away with the attempt that it
    /// belongs to. A mark that stayed said that a command stopped an attempt
    /// that no command touched.
    #[test]
    fn the_mark_of_a_kill_by_a_command_can_be_cleared() {
        let dir = std::env::temp_dir().join(format!("qex-userkill-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        assert!(!was_user_killed(&dir));
        mark_user_kill(&dir);
        assert!(was_user_killed(&dir));
        clear_user_kill(&dir);
        assert!(!was_user_killed(&dir));

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

    /// Makes a directory that holds a `memory.events` file of a test.
    ///
    /// The classification reads files and nothing else, so a test gives it a
    /// directory that the test made. No test of this group needs a cgroup, a
    /// limit, or memory pressure of any kind.
    fn a_cgroup_with_events(name: &str, events: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qex-events-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("memory.events"), events.as_bytes()).unwrap();
        dir
    }

    /// A kill AT the limit of this job says the claim was too small.
    ///
    /// The count `oom` rises at the limit of this cgroup alone, so it is the
    /// evidence that qex acts on.
    #[test]
    fn a_kill_at_the_limit_of_the_job_names_the_job() {
        let dir = a_cgroup_with_events("atlimit", "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n");
        assert_eq!(
            classify_oom(&dir, true, OomCounts::default()),
            Some(OomScope::Job)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A kill with NO event at the limit of this job names the machine.
    ///
    /// The count `oom_kill` counts the processes that any out-of-memory killer
    /// stopped, and the killer of the whole machine raises it. The claim of
    /// this job can be correct, so qex must not act on this count.
    ///
    /// A limit of a PARENT cgroup gives these same counts, and the answer is
    /// correct for that case as well.
    #[test]
    fn a_kill_that_the_machine_made_names_the_machine() {
        let dir = a_cgroup_with_events("machine", "low 0\nhigh 0\nmax 0\noom 0\noom_kill 1\n");
        assert_eq!(
            classify_oom(&dir, true, OomCounts::default()),
            Some(OomScope::Machine)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// No kill gives no answer.
    #[test]
    fn a_cgroup_with_no_kill_gives_no_answer() {
        let dir = a_cgroup_with_events("nokill", "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n");
        assert_eq!(classify_oom(&dir, true, OomCounts::default()), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A limit that the kernel reached, with no kill, gives NO answer.
    ///
    /// A cgroup can reach its limit and free enough memory after that. The
    /// count `oom` rises and the kernel stops no process. The job then stopped
    /// for a different reason, and a report of a kill for memory would name a
    /// cause that did not happen.
    #[test]
    fn a_limit_that_stopped_no_process_gives_no_answer() {
        let dir = a_cgroup_with_events("noproc", "low 0\nhigh 0\nmax 5\noom 2\noom_kill 0\n");
        assert_eq!(classify_oom(&dir, true, OomCounts::default()), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that is not there gives no answer.
    #[test]
    fn a_cgroup_with_no_events_file_gives_no_answer() {
        let dir = std::env::temp_dir().join(format!("qex-events-{}-none", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(classify_oom(&dir, true, OomCounts::default()), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that holds no count gives no answer, and it does not stop.
    #[test]
    fn a_file_with_no_counts_gives_no_answer() {
        let dir = a_cgroup_with_events("odd", "this file holds no count\n\noom_kill\n");
        assert_eq!(classify_oom(&dir, true, OomCounts::default()), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// With no cgroup of its own, qex cannot name the job that the kernel
    /// stopped, whatever the counts say.
    #[test]
    fn a_counter_of_the_session_names_the_session() {
        let dir = a_cgroup_with_events("session", "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n");
        assert_eq!(
            classify_oom(&dir, false, OomCounts::default()),
            Some(OomScope::Session)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An increase alone belongs to this job.
    ///
    /// The counter of the session holds the kills of every program of this
    /// user, so a count that the job did not raise says nothing about the job.
    #[test]
    fn a_count_from_before_the_job_gives_no_answer() {
        let dir = a_cgroup_with_events("before", "low 0\nhigh 0\nmax 0\noom 0\noom_kill 4\n");
        assert_eq!(
            classify_oom(
                &dir,
                false,
                OomCounts {
                    oom: 0,
                    oom_kill: 4
                }
            ),
            None
        );
        assert_eq!(
            classify_oom(
                &dir,
                false,
                OomCounts {
                    oom: 0,
                    oom_kill: 3
                }
            ),
            Some(OomScope::Session)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// qex keeps the strongest evidence of one attempt.
    ///
    /// The supervisor tests more than one time, and a later test can read the
    /// weaker counter. The answer that ACTS must win.
    #[test]
    fn the_strongest_evidence_of_an_attempt_stays() {
        let dir = std::env::temp_dir().join(format!("qex-strength-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        mark_oom(&dir, OomScope::Machine);
        assert_eq!(oom_evidence(&dir), Some(OomScope::Machine));
        // The weaker answer must not replace the stronger one.
        mark_oom(&dir, OomScope::Session);
        assert_eq!(oom_evidence(&dir), Some(OomScope::Machine));
        // The stronger answer must win.
        mark_oom(&dir, OomScope::Job);
        assert_eq!(oom_evidence(&dir), Some(OomScope::Job));
        mark_oom(&dir, OomScope::Machine);
        assert_eq!(oom_evidence(&dir), Some(OomScope::Job));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The cgroup of a job holds the counts of every attempt of that job.
    ///
    /// qex names the cgroup of a job after the id of the job, so attempt 2 uses
    /// the cgroup of attempt 1. A raise on attempt 1 leaves `oom` above zero.
    /// Without the value from the start of attempt 2, a kill of the MACHINE on
    /// attempt 2 reads that earlier rise and names the claim.
    ///
    /// This is the answer that decides whether qex repeats hours of work.
    #[test]
    fn a_count_of_an_earlier_attempt_does_not_name_the_claim() {
        let dir = a_cgroup_with_events("reused", "oom 1\noom_kill 1\n");

        // Attempt 1 raised both counts, and qex acted on that.
        assert_eq!(
            classify_oom(&dir, true, OomCounts::default()),
            Some(OomScope::Job)
        );

        // Attempt 2 starts with those counts already in the cgroup.
        let before = read_oom_counts(&dir).unwrap();

        // The machine stops attempt 2. Only `oom_kill` rises.
        std::fs::write(dir.join("memory.events"), b"oom 1\noom_kill 2\n").unwrap();
        assert_eq!(
            classify_oom(&dir, true, before),
            Some(OomScope::Machine),
            "a count of an earlier attempt must not name the claim of this attempt"
        );

        // The limit of the job stops attempt 2. Both counts rise.
        std::fs::write(dir.join("memory.events"), b"oom 2\noom_kill 3\n").unwrap();
        assert_eq!(
            classify_oom(&dir, true, before),
            Some(OomScope::Job),
            "a rise above the count of the earlier attempt must name the claim"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The supervisor must WRITE the answer that it found.
    ///
    /// This test drives the same value that the supervisor drives, and it makes
    /// it at the same moment: before the counts of the attempt rise. Without
    /// that, a test can pass while the answer stays in the cgroup and nothing
    /// records it.
    #[test]
    fn the_answer_of_an_attempt_reaches_the_record() {
        let job = std::env::temp_dir().join(format!("qex-record-{}", std::process::id()));
        std::fs::remove_dir_all(&job).ok();
        std::fs::create_dir_all(&job).unwrap();
        let cgroup = a_cgroup_with_events("record", "oom 0\noom_kill 0\n");
        // The supervisor records this path, so the job dir names the cgroup
        // exactly as it does in a real run.
        record_cgroup_path(&job, &cgroup);

        // The supervisor makes the watch BEFORE the program of the attempt runs.
        let watch = OomWatch::start(Some(&cgroup));

        // The machine stops the job during the attempt.
        std::fs::write(cgroup.join("memory.events"), b"oom 0\noom_kill 1\n").unwrap();
        watch.record(&job);

        // The FILE must exist. A test of `oom_evidence` alone can pass while
        // nothing is written, if that function reads the cgroup as well.
        assert!(
            job.join("oom").exists(),
            "the supervisor must write the record, and not leave the answer in the cgroup"
        );
        assert_eq!(
            oom_evidence(&job),
            Some(OomScope::Machine),
            "the answer must reach the record of the job"
        );

        // The limit of the job then stops it, and the stronger answer must win.
        std::fs::write(cgroup.join("memory.events"), b"oom 1\noom_kill 2\n").unwrap();
        watch.record(&job);
        assert_eq!(oom_evidence(&job), Some(OomScope::Job));

        std::fs::remove_dir_all(&job).ok();
        std::fs::remove_dir_all(&cgroup).ok();
    }

    /// The watch reads the counts of the START of the attempt.
    ///
    /// The supervisor makes the watch before the program runs, so a count that
    /// an earlier attempt left is a fact from before this attempt. Without
    /// that, attempt 2 of a job reads the rise of attempt 1 and names the claim.
    #[test]
    fn the_watch_holds_the_counts_of_the_start_of_the_attempt() {
        let job = std::env::temp_dir().join(format!("qex-watch2-{}", std::process::id()));
        std::fs::remove_dir_all(&job).ok();
        std::fs::create_dir_all(&job).unwrap();
        // Attempt 1 raised both counts and qex acted on that.
        let cgroup = a_cgroup_with_events("watch2", "oom 1\noom_kill 1\n");

        // Attempt 2 makes its own watch against the same cgroup.
        let watch = OomWatch::start(Some(&cgroup));

        // The machine stops attempt 2: `oom_kill` rises and `oom` does not.
        std::fs::write(cgroup.join("memory.events"), b"oom 1\noom_kill 2\n").unwrap();
        watch.record(&job);
        assert_eq!(
            oom_evidence(&job),
            Some(OomScope::Machine),
            "a rise of an earlier attempt must not name the claim of this attempt"
        );

        std::fs::remove_dir_all(&job).ok();
        std::fs::remove_dir_all(&cgroup).ok();
    }

    /// The watch says qex made the cgroup only when THIS attempt made it.
    ///
    /// A recorded path can name the cgroup of an earlier attempt, because the
    /// removal of a cgroup fails while the supervisor is inside it. A watch
    /// that read such a path would give the counts of that earlier attempt the
    /// weight of a measurement of this one.
    #[test]
    fn the_watch_owns_a_cgroup_only_when_this_attempt_made_it() {
        let dir = a_cgroup_with_events("owned", "oom 0\noom_kill 0\n");

        let made = OomWatch::start(Some(&dir));
        assert!(
            made.qex_made_cgroup,
            "a cgroup that this attempt made belongs to this attempt"
        );

        let not_made = OomWatch::start(None);
        assert!(
            !not_made.qex_made_cgroup,
            "with no cgroup of its own, qex cannot name the job that the kernel stopped"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A cgroup with a count says nothing by itself.
    ///
    /// Only the supervisor holds the counts from the start of the attempt, so
    /// the record that the supervisor writes is the one source of this answer.
    #[test]
    fn the_evidence_comes_from_the_record_and_not_from_the_cgroup() {
        let job = std::env::temp_dir().join(format!("qex-nofall-{}", std::process::id()));
        std::fs::remove_dir_all(&job).ok();
        std::fs::create_dir_all(&job).unwrap();
        let cgroup = a_cgroup_with_events("nofall", "oom 1\noom_kill 1\n");
        record_cgroup_path(&job, &cgroup);

        assert_eq!(
            oom_evidence(&job),
            None,
            "a count with no record must give no answer"
        );

        std::fs::remove_dir_all(&job).ok();
        std::fs::remove_dir_all(&cgroup).ok();
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
