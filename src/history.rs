//! This module keeps a short record of every job that qex accepted.
//!
//! The record of a job is the interface for an agent. When that record is gone,
//! `qex status` said only "there is no job with that id". An agent could then
//! not tell these two conditions apart:
//!
//! - the id is wrong, or the job never existed, so the agent must submit again;
//! - the job existed, and something deleted its record, so the work may have
//!   happened already and a second submission would repeat it.
//!
//! An agent that cannot tell them apart looks at the process list and at the
//! times of the output files. That is the archaeology that qex exists to
//! remove.
//!
//! This file holds one line for each job: the id, the name, the time, and the
//! time of the removal. It never holds the command, because a command line can
//! hold a token.

use crate::job::JobStatus;
use crate::paths;
#[cfg(test)]
use crate::spec::JobSpec;
use serde::{Deserialize, Serialize};

/// The number of lines to keep, whatever their age.
///
/// This limit is a guard only. The age is the usual rule; see
/// `[history] keep` in the config file.
const MAX_LINES: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: uuid::Uuid,
    pub name: String,
    pub submitted_at: u64,
    /// The time when qex deleted the record of this job.
    #[serde(default)]
    pub removed_at: Option<u64>,
    /// The final state of the job, when qex knew it at the removal.
    #[serde(default)]
    pub final_state: Option<String>,
}

fn path() -> anyhow::Result<std::path::PathBuf> {
    Ok(paths::state_dir()?.join("history.jsonl"))
}

/// Adds one line to the file.
///
/// A fault here never stops a command. This file helps a reader; it is not the
/// record of the job.
fn append(entry: &Entry) {
    let Ok(file_path) = path() else { return };
    let Ok(dir) = paths::state_dir() else { return };
    if paths::ensure_dir(&dir, 0o700).is_err() {
        return;
    }

    let Ok(mut line) = serde_json::to_string(entry) else {
        return;
    };
    line.push('\n');

    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // An append of one short line is atomic on a local file system, so two
    // processes can write here without a lock.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&file_path)
    {
        file.write_all(line.as_bytes()).ok();
    }
}

/// Records a job that qex accepted.
///
/// The tests use this form. The coordinator uses `record_submit_for`, because
/// it holds the parts and not the specification.
#[cfg(test)]
pub fn record_submit(spec: &JobSpec) {
    record_submit_for(&spec.id, &spec.name, spec.submitted_at);
}

/// Records a job that qex accepted, from its parts.
pub fn record_submit_for(id: &uuid::Uuid, name: &str, submitted_at: u64) {
    append(&Entry {
        id: *id,
        name: name.to_string(),
        submitted_at,
        removed_at: None,
        final_state: None,
    });
}

/// Records the removal of the record of a job.
pub fn record_removed(status: &JobStatus) {
    append(&Entry {
        id: status.id,
        name: status.name.clone(),
        submitted_at: status.submitted_at,
        removed_at: Some(crate::sys::now_secs()),
        final_state: Some(status.state.to_string()),
    });
}

/// Reads every line, and gives the newest line for each job.
pub fn load() -> Vec<Entry> {
    let Ok(file_path) = path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(file_path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .collect()
}

/// Gives what qex knows about one job that has no record now.
///
/// The result joins the lines for that job, so a job that qex accepted and then
/// deleted gives one entry that holds both times.
pub fn lookup(id: uuid::Uuid) -> Option<Entry> {
    let mut found: Option<Entry> = None;
    for entry in load() {
        if entry.id != id {
            continue;
        }
        found = Some(match found {
            None => entry,
            Some(mut earlier) => {
                // A later line holds the removal. Keep both facts.
                if entry.removed_at.is_some() {
                    earlier.removed_at = entry.removed_at;
                    earlier.final_state = entry.final_state;
                }
                earlier
            }
        });
    }
    found
}

/// Writes a message for a job that has no record now.
///
/// The message tells an agent what to do next, which is the reason for this
/// module.
pub fn describe_missing(id: uuid::Uuid) -> String {
    match lookup(id) {
        None => format!(
            "there is no job with the id {id}, and qex has no record of one. \
             Test the id, or submit the job."
        ),
        Some(entry) => match (entry.removed_at, entry.final_state) {
            (Some(when), Some(state)) => format!(
                "the job {id} ({}) existed and its state was `{state}`. Something deleted \
                 its record {} ago, with `qex clean` or by a deletion of the state \
                 directory. The work of that job HAPPENED. Do not submit it again unless \
                 you want to repeat the work.",
                entry.name,
                crate::units::format_duration(std::time::Duration::from_secs(
                    crate::sys::now_secs().saturating_sub(when)
                ))
            ),
            (Some(when), None) => format!(
                "the job {id} ({}) existed. Something deleted its record {} ago. qex does \
                 not know how that job ended.",
                entry.name,
                crate::units::format_duration(std::time::Duration::from_secs(
                    crate::sys::now_secs().saturating_sub(when)
                ))
            ),
            (None, _) => format!(
                "the job {id} ({}) existed, and qex accepted it {} ago, but its record is \
                 gone and qex did not delete it. Something removed the state directory. \
                 The job may have run.",
                entry.name,
                crate::units::format_duration(std::time::Duration::from_secs(
                    crate::sys::now_secs().saturating_sub(entry.submitted_at)
                ))
            ),
        },
    }
}

/// Deletes the lines that are old.
///
/// The coordinator calls this function at its start.
///
/// An agent asks about a job of the last minutes or hours. An id of last month
/// answers no question, and a file that grows for ever is a fault of its own.
/// The config file gives the time to keep, in `[history] keep`, and the default
/// is one day.
pub fn prune(cfg: &crate::config::Config) {
    let keep_for = cfg
        .history_keep()
        .unwrap_or(std::time::Duration::from_secs(86400))
        .as_secs();
    let now = crate::sys::now_secs();
    let entries = load();

    // Keep a line while it is young. Use the later of the two times, so a line
    // that records a removal stays for its own period.
    let kept: Vec<Entry> = entries
        .iter()
        .filter(|e| {
            let at = e.removed_at.unwrap_or(e.submitted_at).max(e.submitted_at);
            now.saturating_sub(at) <= keep_for
        })
        .cloned()
        .collect();

    // A very busy machine can hold many young lines. The count is a guard.
    let start = kept.len().saturating_sub(MAX_LINES);
    let kept = &kept[start..];

    if kept.len() == entries.len() {
        return;
    }

    let Ok(file_path) = path() else { return };
    let mut text = String::new();
    for entry in kept {
        if let Ok(line) = serde_json::to_string(entry) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    crate::job::write_atomic(&file_path, text.as_bytes(), 0o600).ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{env_lock, EnvVar};

    fn temp_state(tag: &str) -> (std::path::PathBuf, EnvVar) {
        let dir = std::env::temp_dir().join(format!("qex-hist-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let guard = EnvVar::set("XDG_STATE_HOME", dir.to_str().unwrap());
        (dir, guard)
    }

    fn spec(id: uuid::Uuid, name: &str) -> JobSpec {
        JobSpec {
            id,
            name: name.into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu: 1,
            mem: 1 << 20,
            timeout: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            locks: vec![],
            claims: Default::default(),
            retries: 0,
            needs: vec![],
            after: vec![],
            submitted_at: crate::sys::now_secs(),
        }
    }

    /// An id that qex never saw must say so, so an agent submits the job.
    #[test]
    fn an_unknown_id_says_that_qex_never_saw_it() {
        let _guard = env_lock();
        let (dir, _env) = temp_state("unknown");

        let text = describe_missing(uuid::Uuid::new_v4());
        assert!(text.contains("no record"), "got: {text}");
        assert!(
            text.contains("submit"),
            "the message must say what to do: {text}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A job whose record qex deleted must say so, and it must say that the
    /// work happened. An agent must not repeat work that already succeeded.
    #[test]
    fn a_deleted_record_says_that_the_work_happened() {
        let _guard = env_lock();
        let (dir, _env) = temp_state("deleted");

        let id = uuid::Uuid::new_v4();
        record_submit(&spec(id, "build"));

        let mut status = crate::job::JobStatus::new(&spec(id, "build"));
        status.state = crate::job::JobState::Completed;
        record_removed(&status);

        let text = describe_missing(id);
        assert!(text.contains("existed"), "got: {text}");
        assert!(text.contains("completed"), "the state is missing: {text}");
        assert!(
            text.contains("HAPPENED"),
            "the message must say that the work happened: {text}"
        );
        assert!(
            text.contains("Do not submit it again"),
            "the message must say what to do: {text}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// qex must delete an old line. An id of last month answers no question,
    /// and a file that grows for ever is a fault of its own.
    #[test]
    fn an_old_line_goes_away() {
        let _guard = env_lock();
        let (dir, _env) = temp_state("prune");

        let old_id = uuid::Uuid::new_v4();
        let new_id = uuid::Uuid::new_v4();

        let mut old = spec(old_id, "old");
        old.submitted_at = crate::sys::now_secs() - 3 * 86400;
        record_submit(&old);
        record_submit(&spec(new_id, "new"));

        let cfg = crate::config::Config::default();
        assert_eq!(cfg.history.keep, "1d", "the default is one day");
        prune(&cfg);

        assert!(lookup(old_id).is_none(), "the old line must go away");
        assert!(lookup(new_id).is_some(), "the young line must stay");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A record that disappeared without `qex clean` must be reported as that.
    /// Something removed the state directory, and the job may have run.
    #[test]
    fn a_record_that_vanished_is_different_from_one_that_qex_deleted() {
        let _guard = env_lock();
        let (dir, _env) = temp_state("vanished");

        let id = uuid::Uuid::new_v4();
        record_submit(&spec(id, "train"));

        let text = describe_missing(id);
        assert!(text.contains("qex did not delete it"), "got: {text}");
        assert!(text.contains("may have run"), "got: {text}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_file_never_holds_the_command() {
        let _guard = env_lock();
        let (dir, _env) = temp_state("secret");

        let mut s = spec(uuid::Uuid::new_v4(), "deploy");
        s.command = vec!["deploy".into(), "--token=SECRET123".into()];
        record_submit(&s);

        let text = std::fs::read_to_string(dir.join("qex/history.jsonl")).unwrap();
        assert!(
            !text.contains("SECRET123"),
            "the file holds the command: {text}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
