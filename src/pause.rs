//! This module holds what a person paused, and it keeps that on the disk.
//!
//! # Why the state is a file, and not a value in the coordinator
//!
//! A coordinator does not operate for ever. It stops when no job operates and
//! no command arrives, and it stops when a new build replaces the program file.
//! qex itself also tells a user to run `kill <pid>` on it: the capability
//! messages and the version warning each give that instruction.
//!
//! A pause that lived in the memory of the coordinator would thus disappear
//! while the person believes that the machine is quiet, and the next command
//! would start the queue again behind that person. The file removes that fault:
//! a new coordinator reads it at its start, and the pause continues.
//!
//! The coordinator is the one writer of this file, in the same way as it is the
//! one writer of a job record until the supervisor starts. The commands ask the
//! coordinator; they do not write the file.
//!
//! A retry does not read this file. That job already started, so it keeps its
//! slot and its locks and the next attempt starts, the same as any job that
//! still operates. The file is for the coordinator, including one that starts
//! again.

use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One pause: when it started, who asked for it, and when it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRecord {
    /// The moment of the request, in seconds since the epoch.
    pub paused_at: u64,
    /// The process that asked for the pause.
    ///
    /// This value says WHO to a person who reads the file after the command
    /// went away. A pause with no owner is a pause that nobody can explain.
    pub by_pid: i32,
    /// The text that the person gave with `--reason`.
    #[serde(default)]
    pub reason: Option<String>,
    /// The moment when the pause ends by itself, in seconds since the epoch.
    ///
    /// `None` means that the pause has no end. Such a pause needs a loud
    /// report, because a user who forgets it comes back to an empty queue.
    #[serde(default)]
    pub until: Option<u64>,
    /// True when qex made this pause because it could not read the file.
    ///
    /// No person asked for such a pause, so each message about it must say
    /// what happened and must not say that a person paused the queue.
    #[serde(default)]
    pub fault: bool,
}

impl PauseRecord {
    pub fn new(by_pid: i32, reason: Option<String>, until: Option<u64>) -> Self {
        Self {
            paused_at: crate::sys::now_secs(),
            by_pid,
            reason,
            until,
            fault: false,
        }
    }

    /// Tests if this pause reached its end.
    pub fn expired(&self, now: u64) -> bool {
        matches!(self.until, Some(end) if now >= end)
    }
}

/// Everything that a person paused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    /// The pause of the whole queue. A paused queue starts no job.
    #[serde(default)]
    pub queue: Option<PauseRecord>,
    /// The locks that a person holds. The key is the name of the lock.
    #[serde(default)]
    pub locks: BTreeMap<String, PauseRecord>,
}

impl Paused {
    pub fn is_empty(&self) -> bool {
        self.queue.is_none() && self.locks.is_empty()
    }

    /// Removes each pause that reached its end. Gives `true` if one went away.
    pub fn expire(&mut self, now: u64) -> bool {
        let mut changed = false;
        if let Some(record) = &self.queue {
            if record.expired(now) {
                self.queue = None;
                changed = true;
            }
        }
        let before = self.locks.len();
        self.locks.retain(|_, record| !record.expired(now));
        changed |= self.locks.len() != before;
        changed
    }

    /// Reads the file.
    ///
    /// # Why a file that qex cannot read PAUSES the queue
    ///
    /// A file that is absent is the usual state, and it means that nothing is
    /// paused. A file that EXISTS and that qex cannot read is different: it can
    /// hold a pause, and qex does not know.
    ///
    /// The two directions are not equal in cost. A queue that qex holds by
    /// mistake costs latency, and one command corrects it: `qex resume queue`
    /// writes a new file. A queue that operates by mistake gives the person the
    /// opposite of the one thing that person asked for, and no command corrects
    /// that after the work started. So qex holds the queue, and it says why.
    ///
    /// A file that a later version of qex writes is safe: an unknown FIELD is
    /// ignored, in the same way as every other record of qex. This rule covers
    /// a file that is truncated, empty, or of a shape that this version cannot
    /// read.
    pub fn read() -> Self {
        let Ok(path) = path() else {
            return Self::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // No file: nothing is paused. This is the usual state.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => return Self::held_by_fault(&path, &e.to_string()),
        };
        match serde_json::from_str(&text) {
            Ok(paused) => paused,
            Err(e) => Self::held_by_fault(&path, &e.to_string()),
        }
    }

    /// Gives the pause that qex holds when it cannot read the file.
    fn held_by_fault(path: &std::path::Path, fault: &str) -> Self {
        Self {
            queue: Some(PauseRecord {
                paused_at: crate::sys::now_secs(),
                by_pid: 0,
                reason: Some(format!("{}: {fault}", path.display())),
                until: None,
                fault: true,
            }),
            locks: BTreeMap::new(),
        }
    }

    /// Writes the file, or deletes it when nothing is paused.
    pub fn write(&self) -> Result<()> {
        let path = path()?;
        if self.is_empty() {
            std::fs::remove_file(&path).ok();
            return Ok(());
        }
        paths::ensure_dir(&paths::runtime_dir()?, 0o700)?;
        let bytes = serde_json::to_vec_pretty(self).context("writing the pause record")?;
        crate::job::write_atomic(&path, &bytes, 0o600)
    }
}

/// Ends a pause of the QUEUE, whatever ended it.
///
/// A pause of the queue ends in THREE places, and all three are the same event:
/// `qex resume queue`, a `--for` that reached its time while the coordinator
/// operates, and a `--for` that reached its time while NO coordinator operates
/// — `daemon::recover` finds that one at the next start. They were separate
/// blocks of code, and separate blocks drift: the deletion of the credit in the
/// second one left the whole test suite green, and the third one never had it
/// at all, so a `kill <pid>` brought the queue-deleting behaviour straight
/// back. This function is the one place, so a later change cannot correct one
/// path and forget the others.
///
/// `recover` calls it LAST, after the job records are read: the queue is empty
/// until then, so there is nobody to credit.
///
/// The caller has ALREADY removed the record from `state.paused`, and it gives
/// the record here. `now` is the moment when the pause ended.
///
/// The two things:
///
///   1. Give back the time that the pause took from `--max-queue-time`. See
///      `credit_paused_wait`.
///   2. Start the settle timer again. `idle_since` says how long no job has
///      operated, and a paused queue is idle by construction, so at the end of
///      the pause that timer is already satisfied. Without this the FIRST job
///      to start would be an OVERSIZED job, alone, in front of everything that
///      waited. The person who ends a pause asked for the queue, and not for
///      that.
pub fn end_queue_pause(state: &mut crate::daemon::State, record: &PauseRecord, now: u64) {
    credit_paused_wait(state, record.paused_at, now);
    state.idle_since = Some(std::time::Instant::now());
}

/// Adds the length of a pause of the QUEUE to each job that waited through it.
///
/// # The fault that this function prevents
///
/// `--max-queue-time` means "the work has no value after this much time in the
/// queue". Without this function a pause of 30 minutes killed every job that
/// carried a limit below 30 minutes: the person came back to an empty queue,
/// a set of `expired` records and a stop hook for each one. A pause exists to
/// give the machine to the person, and not to delete the queue.
///
/// The clock of the limit therefore stops while the queue is paused. This
/// function is the whole of that rule, and the two places that end a pause of
/// the queue — `qex resume queue`, and a `--for` that reached its end — each
/// call it while they hold the lock, before any job can expire.
///
/// `paused_at` is the moment when the pause began, and `now` is the moment when
/// it ended. A job that was submitted DURING the pause takes the part of the
/// pause after its submission, and no more.
///
/// The value is added one time for each pause, and not each second: a number
/// that counted up would write the record of every job in the queue twice a
/// second for the whole length of the pause.
pub fn credit_paused_wait(state: &mut crate::daemon::State, paused_at: u64, now: u64) {
    let Some(length) = now.checked_sub(paused_at) else {
        // The clock of the machine moved back. Give no credit; a wrong credit
        // would keep a job in the queue after its limit, which is the fault
        // that `--max-queue-time` exists to prevent.
        return;
    };
    if length == 0 {
        return;
    }
    for id in state.queue.clone() {
        let Some(job) = state.jobs.get_mut(&id) else {
            continue;
        };
        if job.status.state != crate::job::JobState::Queued {
            continue;
        }
        let start = job.status.submitted_at.max(paused_at);
        let credit = now.saturating_sub(start);
        if credit == 0 {
            continue;
        }
        job.status.queue_pause_secs += credit;
        let status = job.status.clone();
        if let Ok(dir) = paths::job_dir(&id) {
            crate::job::write_status(&dir, &status).ok();
        }
    }
}

/// Gives the location of the file: `<state>/run/paused.json`.
pub fn path() -> Result<std::path::PathBuf> {
    Ok(paths::runtime_dir()?.join("paused.json"))
}

/// Names the process that asked for a pause.
///
/// A pid of 0 means "this answer does not say". No process has the pid 0, and
/// an earlier coordinator gives no pid at all in `Response::Info`, so the two
/// readers of that answer must be able to say so. They must NEVER print a pid
/// that they invented: `qex info` printed the pid of the COORDINATOR, and a
/// person who read it and ran `kill <pid>` stopped the one process that must
/// keep operating.
fn who(by_pid: i32) -> String {
    if by_pid <= 0 {
        return "an unknown process".to_string();
    }
    format!("pid {by_pid}")
}

/// Gives the form of a `--reason` that a terminal may print.
///
/// The reason is text that a person or an agent typed, and every function below
/// puts it in a SENTENCE that `qex info`, `qex top`, `qex list` and the log of
/// the coordinator write to a terminal. A reason that held an ESC byte would
/// thus clear the screen of the next reader, or move the cursor over the lines
/// above. `job::printable` is the rule for a sentence: it changes a control
/// byte into a space and it keeps every other character.
///
/// The record on the disk keeps the text that the person gave. This function is
/// for the reader, in the same way as `job::safe_name` is for a job name.
fn shown_reason(reason: &str) -> String {
    crate::job::printable(reason)
}

/// Gives the form of a lock name that a terminal may print.
///
/// A lock name is a NAME, and not a sentence, so it takes `job::safe_name`.
/// `qex pause lock` accepts any text, and the name goes into `blocked_reason`,
/// which every job of the queue carries and every reader prints.
fn shown_lock(name: &str) -> String {
    crate::job::safe_name(name)
}

/// True when a word names this stored lock.
///
/// qex shows the safe form, so a word that a reader copies from `qex status`
/// must find the lock that the job holds.
pub fn lock_matches(stored: &str, word: &str) -> bool {
    stored == word || crate::job::safe_name(stored) == word
}

/// True when two lock words name the same lock.
///
/// A pause that a person took under the shown form must still hold a job
/// that asked for the stored form.
pub fn lock_same(a: &str, b: &str) -> bool {
    a == b || crate::job::safe_name(a) == crate::job::safe_name(b)
}

/// Gives the stored lock name that a word names.
///
/// When no known lock matches, the word is a new lock that the person is
/// taking. When two stored names give one safe form, qex cannot choose.
pub fn resolve_lock_name<'a>(
    word: &str,
    known: impl IntoIterator<Item = &'a str>,
) -> Result<String, String> {
    let mut hits: Vec<String> = known
        .into_iter()
        .filter(|n| lock_matches(n, word))
        .map(|s| s.to_string())
        .collect();
    hits.sort();
    hits.dedup();
    match hits.len() {
        0 => Ok(word.to_string()),
        1 => Ok(hits.pop().unwrap()),
        n => {
            let shown = crate::job::visible(word);
            let list = hits
                .iter()
                .map(|name| format!("  {}", crate::job::visible(name)))
                .collect::<Vec<_>>()
                .join("\n");
            Err(format!(
                "`{shown}` names {n} locks.\n\n\
                 Two lock names give one safe form. qex cannot choose.\n\n\
                 Use one of these names:\n{list}"
            ))
        }
    }
}

/// Gives the reason that a job in the queue waits, while the queue is paused.
///
/// # Why this text holds no elapsed time
///
/// The scheduler writes `status.json` for every job whose reason changed, and
/// each write does two `fsync` calls. The scheduler tests the queue every
/// 500ms. A reason that said "6 minutes ago" would thus change and write the
/// record of every job in the queue, for the whole length of the pause.
///
/// The clock time does not change, so this text is written one time. The
/// elapsed time belongs to `qex info` and `qex top`, which calculate it when a
/// person reads them.
pub fn queue_reason(record: &PauseRecord) -> String {
    if record.fault {
        return format!(
            "the queue is paused, so qex starts no job. qex could not read its pause record, and \
             a record that qex cannot read can hold a pause, so qex holds the queue. The fault: \
             {}. Correct that file, or run `qex resume queue` to write a new one.",
            record
                .reason
                .as_deref()
                .map(shown_reason)
                .unwrap_or_else(|| "unknown".into())
        );
    }

    let mut text = String::from("the queue is paused, so qex starts no job.");
    if let Some(reason) = &record.reason {
        text.push_str(&format!(" Reason: {}.", shown_reason(reason)));
    }
    text.push_str(&format!(
        " {} paused it at {}. Run `qex resume queue` to start the queue again.",
        {
            let w = who(record.by_pid);
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => w,
            }
        },
        crate::sys::clock_text(record.paused_at)
    ));
    text
}

/// Gives the reason that a job waits for a lock that a person holds.
pub fn lock_reason(name: &str) -> String {
    format!(
        "waits for the lock `{}`, which a person holds",
        shown_lock(name)
    )
}

/// Gives one line that says how long the queue has been paused.
///
/// Every command that shows the pause uses this function, so `qex info`,
/// `qex top` and `qex list` never disagree.
pub fn queue_line(record: &PauseRecord, now: u64) -> String {
    if record.fault {
        return format!(
            "PAUSED BY A FAULT: qex could not read its pause record, so it holds the queue · {} · \
             correct that file, or run `qex resume queue` to write a new one",
            record
                .reason
                .as_deref()
                .map(shown_reason)
                .unwrap_or_else(|| "unknown".into())
        );
    }

    // Name WHO asked. A second person who finds a paused queue must be able to
    // tell an agent that paused it from a colleague who paused it, before that
    // person types `qex resume queue` over the work of somebody else. The pid
    // is the only "who" that qex holds, and a pid that no longer exists is
    // itself an answer: the command that paused the queue has gone away.
    let mut text = format!(
        "paused since {} ({}) by {}",
        crate::sys::clock_text(record.paused_at),
        crate::units::format_duration(std::time::Duration::from_secs(
            now.saturating_sub(record.paused_at)
        )),
        who(record.by_pid)
    );
    match record.until {
        Some(end) => text.push_str(&format!(
            " · ends at {} (in {})",
            crate::sys::clock_text(end),
            crate::units::format_duration(std::time::Duration::from_secs(end.saturating_sub(now)))
        )),
        // Say this loudly. A pause with no end is the pause that a person
        // forgets, and an empty queue in the morning is the result.
        None => text.push_str(" · NO END: it continues until `qex resume queue`"),
    }
    if let Some(reason) = &record.reason {
        text.push_str(&format!(" · reason: {}", shown_reason(reason)));
    }
    text
}

/// Gives one line for a lock that a person holds.
pub fn lock_line(name: &str, record: &PauseRecord, held_by: Option<&str>, now: u64) -> String {
    let mut text = format!(
        "lock `{}`: paused since {} ({}) by {}",
        shown_lock(name),
        crate::sys::clock_text(record.paused_at),
        crate::units::format_duration(std::time::Duration::from_secs(
            now.saturating_sub(record.paused_at)
        )),
        who(record.by_pid)
    );
    match held_by {
        Some(job) => text.push_str(&format!(
            " · the job {job} still holds it · qex gives it to you when that job stops"
        )),
        None => text.push_str(" · it is yours now"),
    }
    if let Some(reason) = &record.reason {
        text.push_str(&format!(" · reason: {}", shown_reason(reason)));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_safe_lock_word_finds_the_stored_name() {
        let known = ["deploy prod", "gpu0"];
        assert_eq!(
            resolve_lock_name("deploy_prod", known).unwrap(),
            "deploy prod"
        );
        assert_eq!(
            resolve_lock_name("deploy prod", known).unwrap(),
            "deploy prod"
        );
        assert_eq!(resolve_lock_name("new", known).unwrap(), "new");
        let err = resolve_lock_name("a_b", ["a b", "a_b"]).unwrap_err();
        assert!(err.contains("2 locks"), "{err}");
        assert!(lock_same("lk\u{1b}[2J", "lk_2J"));
        assert!(!lock_same("gpu0", "gpu1"));
    }

    fn record() -> PauseRecord {
        PauseRecord {
            paused_at: 1_000,
            by_pid: 42,
            reason: None,
            until: None,
            fault: false,
        }
    }

    /// A pause with `--for` must end by itself. Without this test a queue that
    /// a person paused for 30 minutes would stay paused for ever.
    #[test]
    fn a_pause_with_an_end_goes_away_by_itself() {
        let mut p = Paused {
            queue: Some(PauseRecord {
                until: Some(1_100),
                ..record()
            }),
            ..Default::default()
        };
        p.locks.insert(
            "gpu0".into(),
            PauseRecord {
                until: Some(2_000),
                ..record()
            },
        );

        assert!(!p.expire(1_099), "the pause must stay before its end");
        assert!(p.queue.is_some());

        assert!(p.expire(1_100), "the pause must go away at its end");
        assert!(p.queue.is_none(), "the queue must operate again");
        assert!(
            p.locks.contains_key("gpu0"),
            "a lock with a later end must stay"
        );

        assert!(p.expire(2_000));
        assert!(p.is_empty());
    }

    /// A pause with no end never goes away by itself.
    #[test]
    fn a_pause_with_no_end_stays() {
        let mut p = Paused {
            queue: Some(record()),
            ..Default::default()
        };
        assert!(!p.expire(9_999_999));
        assert!(p.queue.is_some());
    }

    /// The reason of a queued job must not hold a number that changes.
    ///
    /// The scheduler writes `status.json` with two `fsync` calls for every job
    /// whose reason changed, and it tests the queue every 500ms. A reason with
    /// an elapsed time would rewrite every record of the queue, twice a second,
    /// for the whole length of the pause.
    #[test]
    fn the_reason_of_a_paused_job_does_not_change_with_time() {
        let r = record();
        assert_eq!(queue_reason(&r), queue_reason(&r));
        assert!(queue_reason(&r).contains("the queue is paused"));
        assert!(
            queue_reason(&r).contains("qex resume queue"),
            "the reason must give the remedy"
        );
        assert!(
            !queue_reason(&r).contains("ago"),
            "the reason must hold no elapsed time"
        );
    }

    /// A pause with no end must say so wherever a person reads it.
    #[test]
    fn a_pause_with_no_end_says_so() {
        let line = queue_line(&record(), 1_360);
        assert!(line.contains("NO END"), "got: {line}");
        assert!(line.contains("6m"), "the line must give the length: {line}");
    }

    /// Every place that reports a pause must name WHO asked for it.
    ///
    /// # The fault that this test prevents
    ///
    /// A queue is shared. The second person finds a queue that starts nothing,
    /// and the only safe next step is to find the person or the agent that
    /// paused it — a colleague on a call needs the machine, and an agent that
    /// paused itself does not. A line that gave no owner leaves one choice:
    /// type `qex resume queue` over the work of somebody else, and learn
    /// nothing either way.
    ///
    /// The pid is the only "who" that qex holds, and it is in the record for
    /// this purpose. It was in the record and in no output.
    #[test]
    fn every_report_of_a_pause_names_who_asked_for_it() {
        let r = record();
        assert_eq!(r.by_pid, 42, "the helper must give a pid to look for");

        let line = queue_line(&r, 1_360);
        assert!(
            line.contains("pid 42"),
            "the queue line must say who: {line}"
        );

        let reason = queue_reason(&r);
        assert!(
            reason.contains("42"),
            "the reason of each queued job must say who: {reason}"
        );

        let lock = lock_line("gpu0", &r, None, 1_360);
        assert!(
            lock.contains("pid 42"),
            "the lock line must say who: {lock}"
        );
    }

    /// A file that qex cannot read must PAUSE the queue, and say why.
    ///
    /// # The fault that this test prevents
    ///
    /// A parse fault that gave "nothing is paused" would start the work while
    /// the person believes that the machine is quiet, with no line in any log
    /// and no word in any command. The two directions are not equal: a queue
    /// that qex holds by mistake costs latency and `qex resume queue` corrects
    /// it, and a queue that operates by mistake cannot be corrected after the
    /// work started.
    #[test]
    fn a_record_that_qex_cannot_read_holds_the_queue() {
        let paused =
            Paused::held_by_fault(std::path::Path::new("/x/paused.json"), "expected value");

        let record = paused.queue.as_ref().expect("the queue must be paused");
        assert!(record.fault);
        assert!(!record.expired(9_999_999), "such a pause has no end");

        // The words must say what happened, and must not say that a person
        // paused the queue.
        let reason = queue_reason(record);
        assert!(reason.contains("could not read"), "got: {reason}");
        assert!(reason.contains("/x/paused.json"), "got: {reason}");
        assert!(reason.contains("qex resume queue"), "got: {reason}");
        assert!(
            !reason.contains("A person or an agent paused it"),
            "no person asked for this pause: {reason}"
        );

        let line = queue_line(record, 0);
        assert!(line.contains("PAUSED BY A FAULT"), "got: {line}");
    }

    /// An unknown field must not stop the file from parsing.
    ///
    /// A later version of qex adds fields to this record. A parse that refused
    /// them would turn every such file into a fault, and the two versions could
    /// not share one machine.
    #[test]
    fn a_field_that_this_version_does_not_know_is_ignored() {
        let text = r#"{"queue":{"paused_at":10,"by_pid":7,"reason":null,"until":null,
                       "paused_by_user":"someone"},"locks":{},"maintenance":true}"#;
        let back: Paused = serde_json::from_str(text).expect("an unknown field must be ignored");
        let record = back.queue.expect("the pause must hold");
        assert_eq!(record.paused_at, 10);
        assert!(!record.fault);
    }

    /// The record must survive the JSON, or a pause is lost at a restart.
    #[test]
    fn the_record_survives_the_json() {
        let mut p = Paused {
            queue: Some(PauseRecord {
                reason: Some("recording a demo".into()),
                until: Some(2_000),
                ..record()
            }),
            ..Default::default()
        };
        p.locks.insert("gpu0".into(), record());

        let text = serde_json::to_string(&p).unwrap();
        let back: Paused = serde_json::from_str(&text).unwrap();
        assert_eq!(back, p);
    }
}
