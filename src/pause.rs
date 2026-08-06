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
}

impl PauseRecord {
    pub fn new(by_pid: i32, reason: Option<String>, until: Option<u64>) -> Self {
        Self {
            paused_at: crate::sys::now_secs(),
            by_pid,
            reason,
            until,
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

    /// Reads the file. A file that qex cannot read gives no pause.
    ///
    /// A pause is a state that the user asked for, so qex must not invent one.
    /// An unreadable file thus gives the safe answer for the reader: the queue
    /// operates, and every command that lists jobs says so.
    pub fn read() -> Self {
        let Ok(path) = path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
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
    let mut text = String::from("the queue is paused, so qex starts no job.");
    if let Some(reason) = &record.reason {
        text.push_str(&format!(" Reason: {reason}."));
    }
    text.push_str(&format!(
        " A person or an agent paused it at {}. Run `qex resume queue` to start the queue again.",
        crate::sys::clock_text(record.paused_at)
    ));
    text
}

/// Gives the reason that a job waits for a lock that a person holds.
pub fn lock_reason(name: &str) -> String {
    format!("waits for the lock `{name}`, which a person holds")
}

/// Gives one line that says how long the queue has been paused.
///
/// Every command that shows the pause uses this function, so `qex info`,
/// `qex top` and `qex list` never disagree.
pub fn queue_line(record: &PauseRecord, now: u64) -> String {
    let mut text = format!(
        "paused since {} ({})",
        crate::sys::clock_text(record.paused_at),
        crate::units::format_duration(std::time::Duration::from_secs(
            now.saturating_sub(record.paused_at)
        ))
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
        text.push_str(&format!(" · reason: {reason}"));
    }
    text
}

/// Gives one line for a lock that a person holds.
pub fn lock_line(name: &str, record: &PauseRecord, held_by: Option<&str>, now: u64) -> String {
    let mut text = format!(
        "lock `{name}`: paused since {} ({})",
        crate::sys::clock_text(record.paused_at),
        crate::units::format_duration(std::time::Duration::from_secs(
            now.saturating_sub(record.paused_at)
        ))
    );
    match held_by {
        Some(job) => text.push_str(&format!(
            " · the job {job} still holds it · qex gives it to you when that job stops"
        )),
        None => text.push_str(" · it is yours now"),
    }
    if let Some(reason) = &record.reason {
        text.push_str(&format!(" · reason: {reason}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> PauseRecord {
        PauseRecord {
            paused_at: 1_000,
            by_pid: 42,
            reason: None,
            until: None,
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
