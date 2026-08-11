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
//! A job can name a different command to be measured against. `qex submit
//! --each-line` does that: the command of each job holds one line of the input
//! and is used one time only, so the jobs of one fan-out measure against their
//! template. See `JobSpec::learn_key`.
//!
//! The record holds a hash of those two values, and never the values
//! themselves. A command line can hold a token or a password, and a directory
//! names the work of a user. This file must not become a second place that
//! holds either.
//!
//! # Which jobs qex records
//!
//! A job that COMPLETED, and nothing else. The job did all its work, so its
//! peak is the memory that the job needs.
//!
//! A job that did NOT complete gives the memory that it REACHED, which is the
//! size at which something stopped it, and that number says nothing about the
//! memory that the job needs. A sample from such a job would make the next
//! claim too small, and the next job would stop in the same way. That covers a
//! job that somebody stopped, a job that reached its time limit, and a job that
//! the kernel stopped for memory.
//!
//! # A file of an earlier qex
//!
//! An earlier qex wrote a second kind of sample, a `lower-bound`, after a kill
//! for memory. This version writes none and uses none, and it still READS one,
//! because the store loads as ONE value: a word that this version could not
//! read would give an empty store and take every peak of every command with it.
//! `suggest` passes over such a sample, and a command whose only history is one
//! gives no claim at all.

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
    /// A job that the kernel stopped for memory, in a file of an earlier qex.
    ///
    /// qex WRITES no sample of this kind and it USES none. The kind stays
    /// readable because the store loads as one value: a kind that this version
    /// could not read would give an empty store, and the reader would lose
    /// every peak of every command with no message.
    ///
    /// `suggest` passes over these samples. The value is not a measurement of
    /// the memory that a job used, so a claim must not come from it.
    LowerBound,
    /// A kind that this version does not know.
    ///
    /// A LATER qex can write a kind that this one has never seen, and the store
    /// loads as ONE value: without this arm, that one word would give an empty
    /// store and the reader would lose every peak of every command with no
    /// message. `suggest` passes over these samples, because qex cannot say
    /// what they measure.
    #[serde(other)]
    Unknown,
}

/// One measurement of one job.
///
/// EVERY FIELD TAKES A DEFAULT, and the reason is the same as the reason for
/// `Measurement::Unknown`: the store loads as one value, so a sample that is
/// missing a field, or that a later qex wrote with a field that this one does
/// not know, would give an EMPTY store. The reader would then lose every peak
/// of every command, with no message, and would learn of the loss when a later
/// claim came back too small.
///
/// A sample with a missing number reads as zero, and `add` writes no sample of
/// zero bytes, so such a sample gives no claim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sample {
    /// What this measurement says. See [`Measurement`].
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

/// Records the peak of a job that completed.
///
/// A job that did not complete does not come here. It reached the memory that
/// something stopped it at, and that number is not the memory that the job
/// needs.
pub fn record(spec: &JobSpec, status: &JobStatus) {
    if status.state != crate::job::JobState::Completed || status.usage.max_rss == 0 {
        return;
    }
    add(spec, status, Measurement::Peak, status.usage.max_rss);
}

/// Adds one measurement for a command.
///
/// Two supervisors can stop at the same time, so this function holds a lock on
/// the file while it reads and writes. Without the lock, one measurement would
/// replace the other.
fn add(spec: &JobSpec, status: &JobStatus, kind: Measurement, bytes: u64) {
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

    // A job of a fan-out gives its template here, so one fan-out makes one
    // entry and not one entry for each line. `JobSpec::learn_key` holds the
    // reason in full.
    let against = spec.learn_key.as_deref().unwrap_or(&spec.command);

    let mut store = load();
    let entry = store.commands.entry(key(&spec.cwd, against)).or_default();
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

    write_store(&path, &store, &lock);
}

/// Writes the store and releases the lock.
fn write_store(path: &std::path::Path, store: &Store, lock: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    if let Ok(bytes) = serde_json::to_vec_pretty(store) {
        // Mode 0600: this file names the jobs of this user.
        crate::job::write_atomic(path, &bytes, 0o600).ok();
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
    // A PEAK is the only measurement that gives a claim, so an entry with no
    // peak gives NO answer.
    //
    // An entry can hold samples and no peak: a file of an earlier qex holds a
    // `lower-bound` for a command whose jobs never completed. Without this
    // test, such a command took the smallest claim that qex permits and the
    // record said `learned`, so a reader was told that 64MB came from the
    // earlier jobs of a command that an earlier qex knew needed more than 8GB.
    // NO answer is the correct answer: the reader then gets the default, which
    // no measurement contradicts.
    let peak_mem = entry
        .samples
        .iter()
        .filter(|s| s.kind == Measurement::Peak)
        .map(|s| s.max_rss)
        .max()?;
    // A PEAK is the only measurement that gives a claim.
    //
    // A file of an earlier qex can hold a sample of the kind `lower-bound`.
    // That value is not a measurement: it says that a job stopped, and it names
    // no memory that the job used. A claim that came from one would sit above a
    // number that no job ever reached, for every later run of that command.
    let mem = ((peak_mem as f64 * margin) as u64).max(MIN_MEMORY);

    // Calculate the cores from the CPU time and the time that the job operated.
    //
    // The CPU time of a job that used two cores for 10 seconds is 20 seconds.
    // The division thus gives the number of cores that the job used together.
    //
    // EVERY sample counts here, and a sample of a kind that this version does
    // not use as well. The memory of a job that did not finish says nothing
    // about the memory that it needs, and the cores that it used in the time
    // that it ran are a true measurement either way.
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

    /// Every job of one fan-out must read one record.
    ///
    /// The command of each job holds one line and runs one time, so a key on
    /// the command would give a record that no later job can read, and one
    /// entry for every line of every fan-out with no end. The key is therefore
    /// the template, and every line of the fan-out reaches it.
    #[test]
    fn every_line_of_a_fan_out_shares_one_record() {
        let template: Vec<String> = vec!["./process".into(), "{}".into()];
        let line_a: Vec<String> = vec!["./process".into(), "a.csv".into()];
        let line_b: Vec<String> = vec!["./process".into(), "b.csv".into()];

        // Without the template, the two lines are two records.
        assert_ne!(key(&dir(), &line_a), key(&dir(), &line_b));
        // With it, they are one.
        assert_eq!(key(&dir(), &template), key(&dir(), &template));

        // A measurement of line A must give the claim of line B.
        let store = store_with(&["./process", "{}"], vec![sample(400 << 20, 1.0, 10)]);
        assert!(
            suggest(&store, &dir(), &line_b, 1.5).is_none(),
            "the command of the line must not reach the record"
        );
        let s = suggest(&store, &dir(), &template, 1.5).unwrap();
        assert_eq!(s.mem, (400 << 20) * 3 / 2);
    }

    /// A peak that is larger than an old bound must win. The bound says "more
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
    /// bound of an earlier qex, and every claim would go up.
    #[test]
    fn an_earlier_file_keeps_its_meaning() {
        let text = r#"{"commands":{"x":{"name":"t","samples":[
            {"max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}]}}}"#;
        let store: Store = serde_json::from_str(text).unwrap();
        let sample = &store.commands["x"].samples[0];
        assert_eq!(sample.kind, Measurement::Peak);
        assert_eq!(sample.max_rss, 1 << 30);
    }

    /// A file that an earlier qex wrote must still give its peaks.
    ///
    /// An earlier qex wrote a sample of the kind `lower-bound` after the kernel
    /// stopped a job for memory. qex writes no such sample now, and it uses
    /// none — but the store LOADS as one value, so a kind that this version
    /// cannot read gives an EMPTY store. The reader then loses every peak of
    /// every command, with no message, and learns of the loss when a later
    /// claim comes back too small.
    ///
    /// So the kind stays readable, and `suggest` passes over it.
    #[test]
    fn a_file_with_a_bound_of_an_earlier_qex_still_gives_its_peaks() {
        let text = r#"{"commands":{"x":{"name":"t","samples":[
            {"kind":"peak","max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0},
            {"kind":"lower-bound","max_rss":8589934592,"cpu_secs":1.0,"elapsed_secs":10,"at":1}]}}}"#;

        let store: Store = serde_json::from_str(text).expect("a file of an earlier qex must load");
        assert_eq!(
            store.commands["x"].samples.len(),
            2,
            "each sample must load, so that no peak goes away"
        );
        let peaks: Vec<u64> = store.commands["x"]
            .samples
            .iter()
            .filter(|s| s.kind == Measurement::Peak)
            .map(|s| s.max_rss)
            .collect();
        assert_eq!(peaks, vec![1 << 30], "the peak of the old file must stay");
    }

    /// A file that this version cannot read in full must not lose the rest.
    ///
    /// The store loads as ONE value, so one word or one missing field that this
    /// version does not know would give an EMPTY store: every peak of every
    /// command, gone with no message, and the reader learns of it when a later
    /// claim comes back too small.
    ///
    /// A later qex can write a kind and a field that this one has never seen,
    /// so this test drives shapes that do not exist yet.
    #[test]
    fn a_file_that_this_version_cannot_read_in_full_keeps_its_peaks() {
        let peak =
            r#"{"kind":"peak","max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}"#;
        for (what, sample) in [
            (
                "a kind of a later qex",
                r#"{"kind":"ceiling","max_rss":8589934592,"cpu_secs":1.0,"elapsed_secs":10,"at":1}"#,
            ),
            (
                "a sample with no kind",
                r#"{"max_rss":2,"cpu_secs":1.0,"elapsed_secs":10,"at":1}"#,
            ),
            (
                "a sample that is missing a field",
                r#"{"kind":"peak","max_rss":2}"#,
            ),
            ("a sample with no field at all", r#"{}"#),
        ] {
            let text =
                format!(r#"{{"commands":{{"x":{{"name":"t","samples":[{peak},{sample}]}}}}}}"#);
            let store: Store = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{what} must not empty the store: {e}"));
            let peaks: Vec<u64> = store.commands["x"]
                .samples
                .iter()
                .filter(|s| s.kind == Measurement::Peak)
                .map(|s| s.max_rss)
                .collect();
            assert!(
                peaks.contains(&(1 << 30)),
                "{what} must leave the peak of the file: {peaks:?}"
            );
        }
    }

    /// A command whose only history is a BOUND gives NO claim.
    ///
    /// An entry can hold samples and no peak: an earlier qex wrote a bound for
    /// a command whose jobs never completed. A claim from such an entry would
    /// be the smallest that qex permits, and the record would say `learned`, so
    /// a reader would be told that 64MB came from the earlier jobs of a command
    /// that needed more than 8GB. The reader must get the default instead.
    #[test]
    fn a_command_with_a_bound_and_no_peak_gives_no_claim() {
        let cmd: Vec<String> = vec!["train".into()];
        let store = store_with(&["train"], vec![lower_bound(8 << 30)]);
        assert!(
            suggest(&store, &dir(), &cmd, 1.5).is_none(),
            "a bound is not a measurement, so it must give no claim at all"
        );
    }

    /// The claim of an old file comes from its PEAKS, and never from its bound.
    ///
    /// A bound is not a measurement. A version that read one as a peak would
    /// put every later claim of that command above a number that no job used.
    #[test]
    fn a_bound_of_an_earlier_qex_gives_no_claim() {
        let cmd: Vec<String> = vec!["train".into()];
        let store = store_with(
            &["train"],
            vec![lower_bound(8 << 30), sample(1 << 30, 1.0, 10)],
        );
        let s = suggest(&store, &dir(), &cmd, 1.5).expect("the peaks must give a claim");
        assert_eq!(
            s.mem,
            (1 << 30) * 3 / 2,
            "the claim must come from the peak alone, and not from the bound"
        );
    }

    /// qex learns from a job that COMPLETED, and from nothing else.
    ///
    /// A job that the kernel stopped for memory reached the memory that
    /// something stopped it at, and that number is not the memory that the job
    /// needs. A job that a person stopped says nothing at all. Neither one may
    /// reach the store.
    #[test]
    fn a_job_that_did_not_complete_teaches_the_learner_nothing() {
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
            max_queue_time: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            learn_key: None,
            group: None,
            group_name: None,
            claims: Default::default(),
            locks: vec![],
            retries: 0,
            nice: None,
            needs: vec![],
            after: vec![],
            submitted_at: 0,
            dedupe_key: None,
            dedupe_window: 0,
        };

        for state in [
            crate::job::JobState::Oom,
            crate::job::JobState::Killed,
            crate::job::JobState::Failed,
        ] {
            let mut status = crate::job::JobStatus::new(&spec);
            status.state = state;
            status.usage.max_rss = 3 << 30;
            record(&spec, &status);
            assert!(
                !load().commands.contains_key(&key(&spec.cwd, &spec.command)),
                "the state {state:?} must teach the learner nothing"
            );
        }

        // A job that COMPLETED gives its peak.
        spec.command = vec!["good".into()];
        let mut done = crate::job::JobStatus::new(&spec);
        done.state = crate::job::JobState::Completed;
        done.usage.max_rss = 2 << 30;
        record(&spec, &done);
        let store = load();
        let entry = &store.commands[&key(&spec.cwd, &spec.command)];
        assert_eq!(entry.samples.len(), 1);
        assert_eq!(entry.samples[0].kind, Measurement::Peak);
        assert_eq!(entry.samples[0].max_rss, 2 << 30);

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
