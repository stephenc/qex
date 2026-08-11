//! This module reports a kill for memory. It applies no limit.
//!
//! qex does not limit a job. A claim decides what STARTS and when, and a job
//! that claims two gigabytes and uses twenty still fills the machine.
//!
//! # What qex can still see
//!
//! Linux counts the processes that an out-of-memory killer stopped, in
//! `memory.events` of each cgroup. qex reads the cgroup of ITS OWN PROCESS, so
//! a RISE in that count says that the kernel stopped something for memory while
//! an attempt ran.
//!
//! That counter holds every program of this user below that cgroup, so it does
//! not name the victim. A machine that is short of memory is also the machine
//! on which a person uses `kill -9`, and the two arrive together. qex therefore
//! REPORTS the state `oom` and says what it cannot prove.
//!
//! macOS gives no such counter, so a kill for memory there reports the state
//! `killed`. `oom_evidence_is_available` says which system this is.

use std::path::{Path, PathBuf};

/// The out-of-memory counts of a cgroup.
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
/// A count is ZERO in two cases: the file does not name that count, and the
/// file names it with a value that qex cannot read as a number.
///
/// Zero is the correct answer for both, because zero is the answer that ACTS
/// LEAST. A count that qex cannot read is a count that qex cannot use as
/// evidence, and a count of zero starts no attempt and records no bound. A
/// count that qex guessed would raise a claim from a value that qex did not
/// measure.
///
/// A file that qex cannot read at all gives `None`, which is a different
/// answer: there is then no cgroup to ask, and qex reports no kill for memory.
///
/// This function reads a file and nothing else, so a test gives it a directory
/// that the test made.
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

/// Tests whether the kernel stopped a process for memory during this attempt.
///
/// `memory.events` holds `oom_kill`, which counts the processes that ANY
/// out-of-memory killer stopped below this cgroup. A RISE says that the kernel
/// stopped something for memory while this attempt ran.
///
/// It does NOT say that the job of this attempt was the victim. qex reads the
/// cgroup of its own process, and every program of this user below that cgroup
/// raises the same count. The caller pairs this answer with the signal that
/// stopped the job.
///
/// # The count is a RISE, and never a total
///
/// `before` holds the count that qex read before this attempt started. The
/// count cannot be assumed to start at zero: the cgroup of this process holds
/// the kills of every program that ran below it, including the earlier
/// attempts of this job.
pub fn classify_oom(cgroup: &Path, before: OomCounts) -> bool {
    let Some(now) = read_oom_counts(cgroup) else {
        return false;
    };
    // A cgroup can reach its limit and free enough memory after that, which
    // raises `oom` and stops no process. The job then stopped for a different
    // reason, and qex must not report a kill for memory.
    now.oom_kill > before.oom_kill
}

/// Gives the cgroup of THIS process.
///
/// qex makes no cgroup for a job, so this is the cgroup that qex reads for a
/// kill. It holds every program of this user below it, which is why qex cannot
/// name the victim of a kill.
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
/// That matters because NEITHER count can be assumed to start at zero. qex
/// reads the cgroup of its own process, and that cgroup holds the counts of
/// every program that ran below it.
pub struct OomWatch {
    /// The cgroup that qex reads for this attempt.
    cgroup: Option<PathBuf>,
    /// The counts at the start of this attempt.
    before: OomCounts,
}

impl OomWatch {
    /// Reads the counts before the program of the attempt starts.
    ///
    /// qex makes no cgroup for a job, so it reads the cgroup of THIS PROCESS,
    /// whatever made that cgroup. It then still finds a kill, and it cannot
    /// name the victim.
    pub fn start() -> Self {
        let cgroup = own_cgroup();
        let before = cgroup
            .as_deref()
            .and_then(read_oom_counts)
            .unwrap_or_default();
        Self { cgroup, before }
    }

    /// Records what the counts say about this attempt.
    ///
    /// The supervisor calls this function more than one time, and `mark_oom`
    /// writes the same answer each time, so a second call adds no fault.
    pub fn record(&self, job_dir: &Path) {
        let Some(cgroup) = self.cgroup.as_deref() else {
            return;
        };
        if classify_oom(cgroup, self.before) {
            mark_oom(job_dir);
        }
    }
}

/// Tests if the out-of-memory killer stopped a job.
///
/// The kernel stops a process with `SIGKILL` for an out-of-memory event. That
/// signal is the signal that `qex kill` sends, so this test separates the two
/// causes. The states `oom` and `killed` need different corrections.
pub fn was_oom_killed(job_dir: &Path) -> bool {
    oom_evidence(job_dir)
}

/// Gives the evidence that qex holds for a kill for memory.
///
/// # One answer, and it is the weakest one
///
/// qex reads the cgroup of ITS OWN PROCESS. That counter counts every program
/// of this user below that cgroup, so a kill in a different program raises it
/// as well, and a machine that is short of memory is also the machine on which
/// a person uses `kill -9`. The two arrive together.
///
/// So this evidence is sufficient to REPORT the state `oom`. It is not
/// sufficient to say that the claim of this job was too small.
///
/// A record of an EARLIER qex can name a scope: `job` when that version made a
/// cgroup for the job and the counter of that cgroup rose at its own limit, and
/// `machine` when it did not. This version makes no such cgroup and cannot
/// distinguish those cases, so it reads every record the same way. A record
/// that claims more than this version can prove must not be believed.
pub fn oom_evidence(job_dir: &Path) -> bool {
    // THE RECORD IS THE ONLY SOURCE. This function does not read the cgroup.
    //
    // A count in a cgroup means nothing without the value from the start of the
    // attempt, and the supervisor alone holds that value. The supervisor
    // records what it found, and this function reads that record.
    job_dir.join("oom").exists()
}

/// Records an out-of-memory event for a job.
pub fn mark_oom(job_dir: &Path) {
    std::fs::write(job_dir.join("oom"), b"1").ok();
}

/// Deletes the out-of-memory record of a job.
///
/// The record belongs to ONE attempt. A job with `--retries` runs again after a
/// failure, and a record that stays would make the next attempt an
/// out-of-memory kill as well, whatever stopped it.
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
/// a person stopped must NEVER look like an out-of-memory kill, because the two
/// states give the reader two different causes. `oom` sends the reader to the
/// memory of the machine and to the claim of the job, and the person who sent
/// the kill knows that neither one is the cause. A wrong cause costs that
/// reader the time to disprove it, and qex must give no cause that it cannot
/// support.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The record of a kill for memory is written and read back.
    ///
    /// A record of an EARLIER qex can name a scope — `job` or `machine` — from
    /// a version that made a cgroup for the job. This version makes no such
    /// cgroup, so it cannot distinguish those cases and it reads every record
    /// the same way: the kernel stopped something for memory. A record that
    /// claims more than this version can prove must not be believed.
    #[test]
    fn the_out_of_memory_record_is_read_back_whatever_an_earlier_qex_wrote() {
        let dir = std::env::temp_dir().join(format!("qex-oom-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        assert!(
            !was_oom_killed(&dir),
            "a new job has no out-of-memory record"
        );
        mark_oom(&dir);
        assert!(was_oom_killed(&dir));

        clear_oom(&dir);
        assert!(!was_oom_killed(&dir));

        // Each of these is a record that an earlier qex wrote. Every one of
        // them says the same thing to this version.
        for text in ["1", "job", "machine", "session"] {
            std::fs::write(dir.join("oom"), text.as_bytes()).unwrap();
            assert!(
                was_oom_killed(&dir),
                "a record of an earlier qex that holds `{text}` names a kill for memory"
            );
        }

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

    /// Makes a directory that holds a `memory.events` file of a test.
    ///
    /// The test reads files and nothing else, so a test gives it a directory
    /// that the test made. NO test of this group needs a cgroup, a limit, or
    /// memory pressure of any kind.
    fn a_cgroup_with_events(name: &str, events: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qex-events-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("memory.events"), events.as_bytes()).unwrap();
        dir
    }

    /// A RISE in the count of kills says that the kernel stopped something.
    #[test]
    fn a_new_kill_names_a_kill_for_memory() {
        let dir = a_cgroup_with_events("kill", "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n");
        assert!(classify_oom(&dir, OomCounts::default()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// No new kill gives no answer.
    #[test]
    fn a_cgroup_with_no_kill_gives_no_answer() {
        let dir = a_cgroup_with_events("nokill", "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n");
        assert!(!classify_oom(&dir, OomCounts::default()));
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
        assert!(!classify_oom(&dir, OomCounts::default()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file that is not there gives no answer.
    #[test]
    fn a_cgroup_with_no_events_file_gives_no_answer() {
        let dir = std::env::temp_dir().join(format!("qex-events-{}-none", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!classify_oom(&dir, OomCounts::default()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A count that qex cannot read as a number is zero.
    ///
    /// Zero is the answer that ACTS LEAST: it reports no kill. A count that qex
    /// guessed would name a cause that qex did not measure.
    #[test]
    fn a_count_that_qex_cannot_read_is_zero() {
        let dir = a_cgroup_with_events("unread", "oom what\noom_kill later\n");
        assert_eq!(
            read_oom_counts(&dir),
            Some(OomCounts {
                oom: 0,
                oom_kill: 0
            }),
            "a count that qex cannot read must not become evidence"
        );
        assert!(!classify_oom(&dir, OomCounts::default()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The count is a RISE, and never a total.
    ///
    /// The cgroup that qex reads holds the kills of every program of this user
    /// below it, including the earlier attempts of this job. A total would name
    /// a kill that happened before this attempt started.
    #[test]
    fn a_count_from_before_the_attempt_gives_no_answer() {
        let dir = a_cgroup_with_events("before", "low 0\nhigh 0\nmax 0\noom 0\noom_kill 4\n");
        assert!(
            !classify_oom(
                &dir,
                OomCounts {
                    oom: 0,
                    oom_kill: 4
                }
            ),
            "a count that this attempt did not raise says nothing about it"
        );
        assert!(classify_oom(
            &dir,
            OomCounts {
                oom: 0,
                oom_kill: 3
            }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The supervisor must WRITE the answer that it found.
    ///
    /// A test of `oom_evidence` alone could pass while nothing is written, if
    /// that function read the cgroup as well. This test requires the FILE.
    #[test]
    fn the_answer_of_an_attempt_reaches_the_record() {
        let job = std::env::temp_dir().join(format!("qex-record-{}", std::process::id()));
        std::fs::remove_dir_all(&job).ok();
        std::fs::create_dir_all(&job).unwrap();
        let cgroup = a_cgroup_with_events("record", "oom 0\noom_kill 0\n");

        // The supervisor reads the counts BEFORE the program of the attempt
        // runs, so a count that an earlier program left is a fact from before.
        let before = read_oom_counts(&cgroup).unwrap();

        // The kernel stops something during the attempt.
        std::fs::write(cgroup.join("memory.events"), b"oom 0\noom_kill 1\n").unwrap();
        if classify_oom(&cgroup, before) {
            mark_oom(&job);
        }

        assert!(
            job.join("oom").exists(),
            "the supervisor must write the record, and not leave the answer in the cgroup"
        );
        assert!(was_oom_killed(&job));

        std::fs::remove_dir_all(&job).ok();
        std::fs::remove_dir_all(&cgroup).ok();
    }
}
