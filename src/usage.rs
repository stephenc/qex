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
//! A job that completed, and a job that the kernel stopped for memory. Each of
//! the two gives a different kind of evidence, and this module keeps them
//! apart:
//!
//! - A job that COMPLETED gives a peak. The job did all its work, so the peak
//!   is the memory that the job needs.
//! - A job that the kernel STOPPED FOR MEMORY gives a lower bound. The job did
//!   not finish, so the true need is ABOVE this value. This is the most
//!   valuable sample that qex holds: it costs a whole run to obtain, and it is
//!   the answer to the question that the next claim asks.
//!
//! qex records nothing else. A job that somebody stopped, or that reached its
//! time limit, gives a measurement that is too small and no bound: something
//! outside the memory stopped it, and the memory that it reached says nothing
//! about the memory that it needs. A sample from such a job would make the next
//! claim too small, and the next job would stop in the same way.
//!
//! A lower bound is never averaged with a peak. `suggest` takes the largest
//! peak AND the largest lower bound, and the claim is above both.

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

/// What one measurement says about the memory that a command needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Measurement {
    /// The job completed, and this value is the memory that it used.
    ///
    /// This is the value of every sample that qex wrote before it learned from
    /// an out-of-memory kill, so it is the value for a file with no `kind`
    /// field. An old file thus keeps its meaning.
    #[default]
    Peak,
    /// The kernel stopped the job for memory. The true need is ABOVE this value.
    LowerBound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// What this measurement says. See [`Measurement`].
    #[serde(default)]
    pub kind: Measurement,
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
    let (kind, bytes) = match status.state {
        crate::job::JobState::Completed => {
            // A job with no measurement gives nothing.
            if status.usage.max_rss == 0 {
                return;
            }
            (Measurement::Peak, status.usage.max_rss)
        }
        crate::job::JobState::Oom => {
            // Take the LARGER of the claim and the measured peak.
            //
            // The two numbers answer the same question from two sides. With a
            // memory limit, the kernel stops the job at the claim, so the need
            // is above the claim. With no limit, the kernel stops the job while
            // the machine is full, and the peak of the job is then the evidence
            // that qex has. The larger of the two is the value that does not
            // repeat the failure, and a claim that is a little too large costs
            // capacity only.
            //
            // The measurement can also be zero here: the kernel can stop a job
            // before any child of the supervisor ends. The claim is then the
            // only evidence, and it is sufficient.
            (
                Measurement::LowerBound,
                status.usage.max_rss.max(status.mem),
            )
        }
        // A job that somebody stopped, or that reached its time limit, says
        // nothing about the memory that it needs.
        _ => return,
    };
    if bytes == 0 {
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
        kind,
        max_rss: bytes,
        cpu_secs: status.usage.cpu_secs,
        elapsed_secs: status.elapsed().map(|d| d.as_secs()).unwrap_or(0),
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
    let peak_mem = entry
        .samples
        .iter()
        .filter(|s| s.kind == Measurement::Peak)
        .map(|s| s.max_rss)
        .max()
        .unwrap_or(0);
    let mut mem = (peak_mem as f64 * margin) as u64;

    // A lower bound comes from a job that the kernel stopped for memory. It is
    // a claim that FAILED, so the next claim must be above it. An average with
    // the peaks of the smaller runs would lose that lesson, and the next job
    // would stop in the same way and cost a second whole run.
    let bound = entry
        .samples
        .iter()
        .filter(|s| s.kind == Measurement::LowerBound)
        .map(|s| s.max_rss)
        .max()
        .unwrap_or(0);
    if bound > 0 {
        // The margin is the usual step above a measurement. A margin of exactly
        // 1.0 is permitted in the config file, and it would give here the claim
        // that the kernel already stopped, so the claim goes at least one tenth
        // above the bound.
        let above = ((bound as f64 * margin) as u64).max(bound + bound / 10 + 1);
        mem = mem.max(above);
    }

    let mem = mem.max(MIN_MEMORY);

    // Calculate the cores from the CPU time and the time that the job operated.
    //
    // The CPU time of a job that used two cores for 10 seconds is 20 seconds.
    // The division thus gives the number of cores that the job used together.
    //
    // Each sample counts here, and a lower bound also. The memory of a job that
    // the kernel stopped is not the memory that the job needs, but the cores
    // that it used in the time that it ran are a true measurement.
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
            kind: Measurement::Peak,
            max_rss,
            cpu_secs,
            elapsed_secs,
            at: 0,
        }
    }

    /// A measurement from a job that the kernel stopped for memory. The true
    /// need is above this value.
    fn lower_bound(max_rss: u64) -> Sample {
        Sample {
            kind: Measurement::LowerBound,
            max_rss,
            cpu_secs: 1.0,
            elapsed_secs: 1,
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

    /// A job that the kernel stopped for memory gives a LOWER BOUND, and the
    /// next claim must be above it.
    ///
    /// This measurement costs a whole run to obtain. qex threw it away before,
    /// so the same claim died in the same way on the next run.
    #[test]
    fn the_next_claim_is_above_a_lower_bound() {
        let cmd: Vec<String> = vec!["train".into()];
        let store = store_with(&["train"], vec![lower_bound(8 << 30)]);
        let s = suggest(&store, &dir(), &cmd, 1.5).unwrap();
        assert!(
            s.mem > (8 << 30),
            "the claim must be above a claim that failed, and it was {}",
            crate::units::format_size(s.mem)
        );
        assert_eq!(s.mem, 12 << 30);

        // A margin of exactly 1.0 is permitted in the config file. It must
        // still give a claim ABOVE the value that the kernel stopped.
        let s = suggest(&store, &dir(), &cmd, 1.0).unwrap();
        assert!(
            s.mem > (8 << 30),
            "a margin of 1.0 gave the claim that already failed"
        );
    }

    /// A small run that succeeds must not hide the lesson of a run that the
    /// kernel stopped.
    ///
    /// The kill says that the command needs more than 8GB. Three later runs of
    /// 1GB do not answer that: they had a smaller input. A claim from those
    /// three would stop the next large run in the same way, and that costs a
    /// whole run.
    #[test]
    fn a_lower_bound_is_not_averaged_away_by_the_smaller_runs() {
        let cmd: Vec<String> = vec!["train".into()];
        let store = store_with(
            &["train"],
            vec![
                sample(1 << 30, 1.0, 10),
                lower_bound(8 << 30),
                sample(1 << 30, 1.0, 10),
                sample(1 << 30, 1.0, 10),
            ],
        );
        let s = suggest(&store, &dir(), &cmd, 1.5).unwrap();
        assert!(
            s.mem > (8 << 30),
            "the lower bound went away, and the claim is {}",
            crate::units::format_size(s.mem)
        );
    }

    /// A peak that is larger than a lower bound must win. The bound says "more
    /// than 2GB", and a run that completed with 6GB says "6GB is sufficient".
    #[test]
    fn the_largest_evidence_wins_whatever_its_kind() {
        let cmd: Vec<String> = vec!["train".into()];
        let store = store_with(
            &["train"],
            vec![lower_bound(2 << 30), sample(6 << 30, 1.0, 10)],
        );
        let s = suggest(&store, &dir(), &cmd, 1.5).unwrap();
        assert_eq!(s.mem, 9 << 30, "6GB and one half");
    }

    /// A file that qex wrote before this feature holds no `kind` field. Each of
    /// those samples is a peak, so an old file must give the claim that it gave
    /// before. Without this rule, every earlier measurement would become a
    /// lower bound, and every claim would go up.
    #[test]
    fn an_earlier_file_keeps_its_meaning() {
        let text = r#"{"commands":{"x":{"name":"t","samples":[
            {"max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}]}}}"#;
        let store: Store = serde_json::from_str(text).unwrap();
        let sample = &store.commands["x"].samples[0];
        assert_eq!(sample.kind, Measurement::Peak);
        assert_eq!(sample.max_rss, 1 << 30);
    }

    /// The record of a job that the kernel stopped must hold the CLAIM when the
    /// claim is the larger number. With a memory limit, the kernel stops the
    /// job at the claim, so the need is above the claim and not at the peak
    /// that qex measured.
    #[test]
    fn a_kill_for_memory_records_the_claim_when_it_is_larger() {
        use crate::testutil::{env_lock, EnvVar};
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("qex-usage-oom-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let _env = EnvVar::set("XDG_STATE_HOME", dir.to_str().unwrap());

        let mut spec = crate::spec::JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "train".into(),
            cwd: "/project".into(),
            command: vec!["train".into()],
            env: Default::default(),
            cpu: 1,
            mem: 4 << 30,
            timeout: None,
            tags: vec![],
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
        };

        let mut status = crate::job::JobStatus::new(&spec);
        status.state = crate::job::JobState::Oom;
        status.usage.max_rss = 1 << 30;
        record(&spec, &status);

        let store = load();
        let entry = &store.commands[&key(&spec.cwd, &spec.command)];
        assert_eq!(entry.samples.len(), 1);
        assert_eq!(entry.samples[0].kind, Measurement::LowerBound);
        assert_eq!(
            entry.samples[0].max_rss,
            4 << 30,
            "the claim is the larger evidence"
        );

        // A job that somebody stopped teaches nothing. The memory that it
        // reached says nothing about the memory that it needs.
        spec.command = vec!["other".into()];
        let mut killed = crate::job::JobStatus::new(&spec);
        killed.state = crate::job::JobState::Killed;
        killed.usage.max_rss = 3 << 30;
        record(&spec, &killed);
        assert!(
            !load().commands.contains_key(&key(&spec.cwd, &spec.command)),
            "a job that a command stopped must teach the learner nothing"
        );

        std::fs::remove_dir_all(&dir).ok();
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
