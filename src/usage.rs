//! This module remembers what a job really used, and gives the claim for the
//! next job of the same kind.
//!
//! A claim is an estimate. An agent that does not know the size of a task gives
//! `guess`, which claims one half of the budget. That claim is safe, and it is
//! also frequently far too large: a test suite that uses 165MB can hold 10GB of
//! the budget and stop other work for the length of the run.
//!
//! qex already measures each job. This module keeps those measurements and uses
//! them, so the claim becomes accurate without any effort from the agent.
//!
//! # What this module records
//!
//! The key is the directory and the command together, and not the name. A name
//! is for a person to read, and one program does very different work:
//!
//! - `cargo build` and `cargo test` have one program name and very different
//!   needs, so the command must be in the key.
//! - `cargo test` in a small library and `cargo test` in a large program have
//!   one command and very different needs, so the directory must be in the key
//!   as well.
//!
//! The record holds a hash of those two values, and never the values
//! themselves. A command line can hold a token or a password, and a directory
//! names the work of a user. This file must not become a second place that
//! holds either.
//!
//! # Which jobs qex records
//!
//! A job that completed only. A job that the out-of-memory killer stopped, or
//! that reached its time limit, gives a measurement that is too small: it shows
//! the memory that the job reached before something stopped it, and not the
//! memory that the job needs. A record from such a job would make the next
//! claim too small, and the next job would stop in the same way.

use crate::job::JobStatus;
use crate::paths;
use crate::spec::JobSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The number of measurements that qex keeps for one command.
const SAMPLES: usize = 5;

/// The smallest memory claim that this module gives.
///
/// A measurement holds the peak that qex saw. A job can use more on a different
/// day, with a larger input, so a very small claim is not useful.
const MIN_MEMORY: u64 = 64 << 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// The peak memory of the job, in bytes.
    pub max_rss: u64,
    /// The CPU time of the job, in seconds.
    pub cpu_secs: f64,
    /// The time that the job operated, in seconds.
    pub elapsed_secs: u64,
    pub at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    /// The name of the job, for a person to read. qex does not use it as a key.
    pub name: String,
    pub samples: Vec<Sample>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub commands: BTreeMap<String, Entry>,
}

/// The claim that this module gives, and the reason for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub cpu: u64,
    pub mem: u64,
    /// The number of measurements behind this claim.
    pub samples: usize,
}

/// Gives the key for one command in one directory.
///
/// The key is a hash, so this file never holds a command line or a path. A
/// command line can hold a token, and a path names the work of a user.
pub fn key(cwd: &std::path::Path, command: &[String]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;

    // The directory comes first. `cargo test` in a small library and the same
    // command in a large program need very different claims.
    for byte in cwd.to_string_lossy().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash ^= 0xfe;
    hash = hash.wrapping_mul(0x100000001b3);

    for part in command {
        for byte in part.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Separate the arguments, so ["a b"] and ["a", "b"] give two keys.
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn store_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(paths::state_dir()?.join("usage.json"))
}

/// Reads the store. A file that qex cannot read gives an empty store.
pub fn load() -> Store {
    let Ok(path) = store_path() else {
        return Store::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Store::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Adds one measurement for a command.
///
/// Two supervisors can stop at the same time, so this function holds a lock on
/// the file while it reads and writes. Without the lock, one measurement would
/// replace the other.
pub fn record(spec: &JobSpec, status: &JobStatus) {
    // A job that did not complete gives a measurement that is too small.
    if status.state != crate::job::JobState::Completed {
        return;
    }
    // A job with no measurement gives nothing.
    if status.usage.max_rss == 0 {
        return;
    }

    let Ok(path) = store_path() else { return };
    let Ok(dir) = paths::state_dir() else { return };
    if paths::ensure_dir(&dir, 0o700).is_err() {
        return;
    }

    let lock_path = dir.join("usage.lock");
    let Ok(lock) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    else {
        return;
    };
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::flock(lock.as_raw_fd(), libc::LOCK_EX);
    }

    let mut store = load();
    let entry = store
        .commands
        .entry(key(&spec.cwd, &spec.command))
        .or_default();
    entry.name = spec.name.clone();
    entry.samples.push(Sample {
        max_rss: status.usage.max_rss,
        cpu_secs: status.usage.cpu_secs,
        elapsed_secs: status
            .elapsed()
            .map(|d| d.as_secs())
            .unwrap_or(0),
        at: crate::sys::now_secs(),
    });

    // Keep the most recent measurements only. A task changes over time, and an
    // old measurement is not evidence about the task of today.
    let extra = entry.samples.len().saturating_sub(SAMPLES);
    entry.samples.drain(..extra);

    if let Ok(bytes) = serde_json::to_vec_pretty(&store) {
        // Mode 0600: this file names the jobs of this user.
        crate::job::write_atomic(&path, &bytes, 0o600).ok();
    }

    unsafe {
        libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
    }
}

/// Gives the claim for a command, from the measurements of the earlier jobs.
///
/// The result is `None` when qex has no measurement for this command.
pub fn suggest(
    store: &Store,
    cwd: &std::path::Path,
    command: &[String],
    margin: f64,
) -> Option<Suggestion> {
    let entry = store.commands.get(&key(cwd, command))?;
    if entry.samples.is_empty() {
        return None;
    }

    // Use the largest measurement, and not the average.
    //
    // A claim that is too small stops the job, and a claim that is a little too
    // large costs some capacity only. The two faults are not equal.
    let peak_mem = entry.samples.iter().map(|s| s.max_rss).max().unwrap_or(0);
    let mem = ((peak_mem as f64 * margin) as u64).max(MIN_MEMORY);

    // Calculate the cores from the CPU time and the time that the job operated.
    //
    // The CPU time of a job that used two cores for 10 seconds is 20 seconds.
    // The division thus gives the number of cores that the job used together.
    let cores = entry
        .samples
        .iter()
        .map(|s| {
            // A job shorter than one second gives an elapsed time of 0. Use one
            // second for it. The CPU time then gives the cores directly, and a
            // very short job gives a value near zero, which becomes one core
            // below.
            s.cpu_secs / s.elapsed_secs.max(1) as f64
        })
        .fold(0.0f64, f64::max);

    let cpu = (cores * margin).ceil().max(1.0) as u64;

    Some(Suggestion {
        cpu,
        mem,
        samples: entry.samples.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(max_rss: u64, cpu_secs: f64, elapsed_secs: u64) -> Sample {
        Sample {
            max_rss,
            cpu_secs,
            elapsed_secs,
            at: 0,
        }
    }

    fn dir() -> std::path::PathBuf {
        std::path::PathBuf::from("/project")
    }

    fn store_with(command: &[&str], samples: Vec<Sample>) -> Store {
        let cmd: Vec<String> = command.iter().map(|s| s.to_string()).collect();
        let mut store = Store::default();
        store.commands.insert(
            key(&dir(), &cmd),
            Entry {
                name: "test".into(),
                samples,
            },
        );
        store
    }

    #[test]
    fn with_no_measurement_there_is_no_claim() {
        let store = Store::default();
        assert_eq!(suggest(&store, &dir(), &["cargo".into()], 1.5), None);
    }

    /// The claim comes from the largest measurement, and not from the average.
    /// A claim that is too small stops the job.
    #[test]
    fn the_claim_uses_the_largest_measurement() {
        let store = store_with(
            &["cargo", "test"],
            vec![
                sample(100 << 20, 1.0, 10),
                sample(400 << 20, 1.0, 10),
                sample(200 << 20, 1.0, 10),
            ],
        );
        let cmd: Vec<String> = vec!["cargo".into(), "test".into()];
        let s = suggest(&store, &dir(), &cmd, 1.5).unwrap();
        assert_eq!(s.mem, (400 << 20) * 3 / 2, "400MB and one half");
        assert_eq!(s.samples, 3);
    }

    /// The cores come from the CPU time and the time that the job operated.
    /// A job that used two cores for 10 seconds has 20 seconds of CPU time.
    #[test]
    fn the_cores_come_from_the_cpu_time_and_the_elapsed_time() {
        let cmd: Vec<String> = vec!["make".into()];

        // Two cores for 10 seconds.
        let store = store_with(&["make"], vec![sample(1 << 20, 20.0, 10)]);
        assert_eq!(suggest(&store, &dir(), &cmd, 1.0).unwrap().cpu, 2);

        // A test suite that waits: 1.9 seconds of CPU time in 19 seconds. This
        // is the measurement of the qex test suite itself.
        let store = store_with(&["make"], vec![sample(165 << 20, 1.9, 19)]);
        let s = suggest(&store, &dir(), &cmd, 1.5).unwrap();
        assert_eq!(s.cpu, 1, "a job that waits needs one core");
        assert!(
            s.mem < (300 << 20),
            "the claim must be near the measurement, and it was {}",
            crate::units::format_size(s.mem)
        );
    }

    /// A very small measurement must not give a claim that is too small to be
    /// useful. A job can use more memory with a larger input.
    #[test]
    fn a_small_measurement_gives_the_smallest_useful_claim() {
        let cmd: Vec<String> = vec!["true".into()];
        let store = store_with(&["true"], vec![sample(1 << 20, 0.0, 0)]);
        let s = suggest(&store, &dir(), &cmd, 1.5).unwrap();
        assert_eq!(s.mem, MIN_MEMORY);
        assert_eq!(s.cpu, 1);
    }

    /// Two commands must not share a record. One program does very different
    /// work with different arguments.
    #[test]
    fn two_commands_have_two_records() {
        let build: Vec<String> = vec!["cargo".into(), "build".into()];
        let test: Vec<String> = vec!["cargo".into(), "test".into()];
        assert_ne!(key(&dir(), &build), key(&dir(), &test));

        let store = store_with(&["cargo", "build"], vec![sample(1 << 30, 1.0, 1)]);
        assert!(suggest(&store, &dir(), &build, 1.5).is_some());
        assert!(
            suggest(&store, &dir(), &test, 1.5).is_none(),
            "`cargo test` must not use the record of `cargo build`"
        );
    }

    /// The key must separate the arguments. Without that, `["a b"]` and
    /// `["a", "b"]` would give one key for two different commands.
    #[test]
    fn the_key_separates_the_arguments() {
        let joined: Vec<String> = vec!["a b".into()];
        let split: Vec<String> = vec!["a".into(), "b".into()];
        assert_ne!(key(&dir(), &joined), key(&dir(), &split));
    }

    /// One command in two directories must have two records. `cargo test` in a
    /// small library and the same command in a large program need very
    /// different claims.
    #[test]
    fn one_command_in_two_directories_has_two_records() {
        let cmd: Vec<String> = vec!["cargo".into(), "test".into()];
        let small = std::path::PathBuf::from("/home/me/small-library");
        let large = std::path::PathBuf::from("/home/me/large-program");
        assert_ne!(key(&small, &cmd), key(&large, &cmd));

        let mut store = Store::default();
        store.commands.insert(
            key(&small, &cmd),
            Entry {
                name: "test".into(),
                samples: vec![sample(100 << 20, 1.0, 10)],
            },
        );
        assert!(suggest(&store, &small, &cmd, 1.5).is_some());
        assert!(
            suggest(&store, &large, &cmd, 1.5).is_none(),
            "a different directory must not use this record"
        );
    }

    /// The key must not hold the command or the directory. A command line can
    /// hold a token, and a directory names the work of a user.
    #[test]
    fn the_key_does_not_hold_the_command() {
        let secret: Vec<String> = vec!["deploy".into(), "--token=SECRET123".into()];
        let k = key(&dir(), &secret);
        assert!(!k.contains("SECRET"), "the key holds the command: {k}");
        assert!(!k.contains("project"), "the key holds the directory: {k}");
        assert_eq!(k.len(), 16, "the key is a hash of a fixed length");
    }
}
