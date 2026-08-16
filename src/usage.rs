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
//!
//! # A file that qex cannot read
//!
//! The store loads as one value, so a file that this version cannot read in
//! full would give an empty store — and the next write would then DELETE every
//! learned peak from the disk, in silence. So the reader salvages: it keeps
//! every entry and every sample that it can read. The writer goes one step
//! further, because the writer is the one that can destroy the file: it moves
//! a damaged file aside, to `usage.json.corrupt-<time>`, before it writes, and
//! it writes nothing at all over a file whose bytes it could not read.

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
    /// A kind that this version does not know, with the word that the file
    /// held.
    ///
    /// A LATER qex can write a kind that this one has never seen, and the store
    /// loads as ONE value: without this arm, that one word would give an empty
    /// store and the reader would lose every peak of every command with no
    /// message. `suggest` passes over these samples, because qex cannot say
    /// what they measure.
    ///
    /// The arm HOLDS THE WORD, and the writer gives it back unchanged. An arm
    /// with no word would write `unknown` over a kind such as `ceiling`, and
    /// the first completed job after an upgrade-and-return would destroy the
    /// label of every sample of the later qex, permanently and with no message.
    Unknown(String),
}

// The kind reads and writes as one word, by hand and not by derive, so that a
// word of a later qex comes back out of the file exactly as it went in.
impl Serialize for Measurement {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Measurement::Peak => "peak",
            Measurement::LowerBound => "lower-bound",
            Measurement::Unknown(word) => word,
        })
    }
}

impl<'de> Deserialize<'de> for Measurement {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        Ok(match word.as_str() {
            "peak" => Measurement::Peak,
            "lower-bound" => Measurement::LowerBound,
            _ => Measurement::Unknown(word),
        })
    }
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
/// A missing number reads as ZERO, and a peak of zero bytes is not a
/// measurement: no job uses no memory. `suggest` thus passes over a peak of
/// zero, in the same way that it passes over a kind that it cannot use. The
/// sample stays in the file, and it gives no claim.
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

/// The measurements of one command.
///
/// EVERY FIELD TAKES A DEFAULT, for the reason that `Sample` gives: the store
/// loads as ONE value, so an entry that is missing a field empties the store of
/// EVERY command. The guard on `Sample` alone is not enough, because a later
/// qex can change this level and not that one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
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
    /// The number of measurements behind this claim: the peaks, and no other
    /// kind of sample. A sample that gives no claim does not count as evidence
    /// for one.
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

/// What the reader found on the disk.
///
/// `Missing` and `Unreadable` are two different answers, and `add` acts on the
/// difference: a store that is MISSING starts empty, and the first write makes
/// the file; a store that qex could not READ must take no write at all, because
/// a write over a file that qex has not seen destroys measurements that qex
/// cannot count.
enum OnDisk {
    /// No file exists. The store starts empty, and that is correct.
    Missing,
    /// A file exists and qex could not read its bytes.
    Unreadable,
    /// The file loaded in full.
    Whole(Store),
    /// The file held text that this version could not read as a store. The
    /// value holds every entry and every sample that qex COULD read.
    Damaged(Store),
}

fn read_store(path: &std::path::Path) -> OnDisk {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OnDisk::Missing,
        Err(_) => return OnDisk::Unreadable,
    };
    match serde_json::from_str(&text) {
        Ok(store) => OnDisk::Whole(store),
        Err(_) => OnDisk::Damaged(salvage(&text)),
    }
}

/// Keeps what a damaged file still says.
///
/// The store loads as ONE value, so one entry that this version cannot read
/// would take every peak of every command with it. The defaults on `Sample` and
/// `Entry` hold the shapes that a later qex is EXPECTED to write; they do not
/// hold a truncated file, a field of the wrong type, or one damaged entry among
/// many. This function reads the file one entry and one sample at a time, and
/// keeps each one that this version can read.
fn salvage(text: &str) -> Store {
    let mut store = Store::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        // The text is not JSON at all, so no entry is readable. The caller
        // quarantines the file, so the bytes stay for a person to read.
        return store;
    };
    let Some(commands) = value.get("commands").and_then(|c| c.as_object()) else {
        return store;
    };
    for (key, entry) in commands {
        let name = entry
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or_default()
            .to_string();
        let samples: Vec<Sample> = entry
            .get("samples")
            .and_then(|s| s.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|s| serde_json::from_value(s.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        if samples.is_empty() {
            // An entry with no readable sample gives no claim, so it says
            // nothing worth a line in the file.
            continue;
        }
        store.commands.insert(key.clone(), Entry { name, samples });
    }
    store
}

/// Reads the store, for a reader that gives claims.
///
/// A file that qex cannot read IN FULL gives the entries that qex could read,
/// and a file that qex cannot read AT ALL gives an empty store: a claim is an
/// estimate, so a reader loses accuracy only. The writer, `add`, does not use
/// this function, because a writer that starts from a part of the file would
/// then write that part OVER the file.
pub fn load() -> Store {
    let Ok(path) = store_path() else {
        return Store::default();
    };
    match read_store(&path) {
        OnDisk::Missing | OnDisk::Unreadable => Store::default(),
        OnDisk::Whole(store) | OnDisk::Damaged(store) => store,
    }
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

    // Read the file under the lock, and read it with its damage visible. An
    // earlier qex read with `unwrap_or_default()` here, so ONE byte that it
    // could not read gave an empty store, and this write then DELETED every
    // learned peak of every command from the disk, in silence. The first job
    // that completed after the damage was the one that destroyed the file.
    let mut store = match read_store(&path) {
        OnDisk::Missing => Store::default(),
        OnDisk::Whole(store) => store,
        OnDisk::Unreadable => {
            // The file exists and its bytes did not come. A write here would
            // replace measurements that qex has not seen. One lost sample
            // costs less than the whole store, so the write is skipped.
            crate::daemon::log(&format!(
                "qex could not read {} and did not record the measurement of \
                 this job. The file stays as it is.",
                path.display()
            ));
            release(&lock);
            return;
        }
        OnDisk::Damaged(salvaged) => {
            // Move the damaged file aside BEFORE any write, so that the write
            // cannot destroy it and a person can read what it held. The store
            // continues from the entries that qex could read.
            let aside =
                path.with_file_name(format!("usage.json.corrupt-{}", crate::sys::now_secs()));
            if std::fs::rename(&path, &aside).is_err() {
                // The file cannot move, so it also must not be written over.
                release(&lock);
                return;
            }
            crate::daemon::log(&format!(
                "qex could not read every part of {}. The file moved to {}, so \
                 that no write destroys it, and the store keeps the entries that \
                 qex could read ({} of them). The claims of the other commands \
                 come back as their jobs complete.",
                path.display(),
                aside.display(),
                salvaged.commands.len()
            ));
            salvaged
        }
    };
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
    if let Ok(bytes) = serde_json::to_vec_pretty(store) {
        // Mode 0600: this file names the jobs of this user.
        crate::job::write_atomic(path, &bytes, 0o600).ok();
    }
    release(lock);
}

/// Releases the lock on the store.
fn release(lock: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
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
    //
    // A PEAK OF MORE THAN ZERO BYTES is the only measurement that gives a
    // claim, and an entry that holds no such sample gives NO answer. Two
    // shapes get past the reader and reach this point:
    //
    // - a `lower-bound`, which a file of an earlier qex holds for a command
    //   whose jobs never completed. It says that a job stopped, and it names no
    //   memory that the job used;
    // - a peak of ZERO bytes, which is what a sample with no number reads as.
    //   No job uses no memory, so the value measures nothing.
    //
    // Each one gives the smallest claim that qex permits, with `learned` in the
    // record: the reader is told that 64MB came from the earlier jobs of a
    // command that possibly needs 8GB. NO answer is the correct answer, because
    // the reader then gets the default, which no measurement contradicts.
    let peaks: Vec<u64> = entry
        .samples
        .iter()
        .filter(|s| s.kind == Measurement::Peak && s.max_rss > 0)
        .map(|s| s.max_rss)
        .collect();
    let peak_mem = peaks.iter().copied().max()?;
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

    // The count is the count of the PEAKS, and not of every sample. The claim
    // rests on the peaks alone, so a count that took every sample would say
    // that more evidence stands behind the claim than the claim rests on.
    Some(Suggestion {
        cpu,
        mem,
        samples: peaks.len(),
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

    /// A specification for a job whose command is one word, for the tests that
    /// drive `record` against a real file.
    fn spec_for(command: &str) -> JobSpec {
        JobSpec {
            id: uuid::Uuid::new_v4(),
            name: command.into(),
            cwd: "/project".into(),
            command: vec![command.into()],
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
        }
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
        // The claim rests on ONE peak. A count of two would say that more
        // evidence stands behind the claim than the claim rests on.
        assert_eq!(s.samples, 1, "the bound is not evidence for the claim");
    }

    /// The count of the evidence counts the PEAKS, and no other sample.
    ///
    /// A reader that sees `samples: 3` takes the claim as three measurements
    /// strong. A bound and a peak of zero give no claim, so a count that took
    /// them would overstate the evidence.
    #[test]
    fn the_count_of_the_evidence_counts_the_peaks_alone() {
        let cmd: Vec<String> = vec!["train".into()];
        let store = store_with(
            &["train"],
            vec![
                sample(1 << 30, 1.0, 10),
                sample(2 << 30, 1.0, 10),
                lower_bound(8 << 30),
                sample(0, 1.0, 10),
            ],
        );
        let s = suggest(&store, &dir(), &cmd, 1.5).unwrap();
        assert_eq!(s.samples, 2, "two peaks stand behind this claim");
    }

    /// A kind of a later qex must come out of the file as it went in.
    ///
    /// An arm without the word would write `unknown` over a kind such as
    /// `ceiling`, so the first completed job after a return to this version
    /// would destroy the label of every sample of the later qex, permanently
    /// and with no message. The peak would stay; the name of the measurement
    /// would not.
    #[test]
    fn a_kind_of_a_later_qex_keeps_its_word() {
        let text = r#"{"kind":"ceiling","max_rss":1,"cpu_secs":0.0,"elapsed_secs":0,"at":0}"#;
        let s: Sample = serde_json::from_str(text).unwrap();
        assert_eq!(s.kind, Measurement::Unknown("ceiling".into()));
        let out = serde_json::to_string(&s).unwrap();
        assert!(
            out.contains(r#""kind":"ceiling""#),
            "the word must come back unchanged: {out}"
        );
    }

    /// A file with ONE entry that this version cannot read at all must keep
    /// the other entries.
    ///
    /// The defaults on `Sample` and `Entry` hold the shapes that a later qex
    /// is EXPECTED to write. They do not hold a field of the wrong type: a
    /// `kind` that is a number refuses the WHOLE file, and an earlier qex then
    /// read the store as empty. The salvage reads one entry at a time, so the
    /// damage stops at the entry that holds it.
    #[test]
    fn one_entry_of_the_wrong_shape_leaves_the_other_entries() {
        let good = r#"{"name":"t","samples":[{"kind":"peak","max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}]}"#;
        let bad = r#"{"name":"t","samples":[{"kind":3,"max_rss":2,"cpu_secs":1.0,"elapsed_secs":10,"at":0}]}"#;
        let text = format!(r#"{{"commands":{{"good":{good},"damaged":{bad}}}}}"#);

        // The whole file refuses to load as one value; that is the fault that
        // the salvage exists for.
        assert!(serde_json::from_str::<Store>(&text).is_err());

        let store = salvage(&text);
        assert_eq!(
            store.commands["good"].samples[0].max_rss,
            1 << 30,
            "the entry beside the damage must stay"
        );
        assert!(
            !store.commands.contains_key("damaged"),
            "an entry with no readable sample says nothing"
        );
    }

    /// One damaged SAMPLE must not take the peaks of its own entry with it.
    #[test]
    fn one_sample_of_the_wrong_shape_leaves_the_peaks_beside_it() {
        let text = r#"{"commands":{"x":{"name":"t","samples":[
            {"kind":3,"max_rss":2,"cpu_secs":1.0,"elapsed_secs":10,"at":0},
            {"kind":"peak","max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}]}}}"#;
        let store = salvage(text);
        let peaks: Vec<u64> = store.commands["x"]
            .samples
            .iter()
            .filter(|s| s.kind == Measurement::Peak)
            .map(|s| s.max_rss)
            .collect();
        assert_eq!(peaks, vec![1 << 30], "the peak beside the damage must stay");
    }

    /// A file that is not JSON at all salvages as an empty store. The caller
    /// quarantines the file, so nothing is lost; qex simply cannot read it.
    #[test]
    fn a_file_that_is_not_json_salvages_nothing() {
        assert!(salvage("").commands.is_empty());
        assert!(salvage(r#"{"commands":{"x":"#).commands.is_empty());
        assert!(salvage("not json").commands.is_empty());
    }

    /// A write over a damaged file must move the file aside first, and it must
    /// keep the entries that qex could read.
    ///
    /// This is the fault of issue #101: `add` read a damaged file as an EMPTY
    /// store, and then wrote that empty store back. The first job that
    /// completed after the damage deleted every learned peak from the disk,
    /// and qex gave no message.
    #[test]
    fn a_write_over_a_damaged_file_quarantines_it_and_keeps_what_it_can() {
        use crate::testutil::{env_lock, EnvVar};
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("qex-usage-corrupt-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let _env = EnvVar::set("XDG_STATE_HOME", dir.to_str().unwrap());

        // A store with one readable entry and one that this version cannot
        // read. The readable entry belongs to `earlier`, a different command
        // from the one that completes below.
        let earlier: Vec<String> = vec!["earlier".into()];
        let good = format!(
            r#""{}":{{"name":"earlier","samples":[{{"kind":"peak","max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}}]}}"#,
            key(&std::path::PathBuf::from("/project"), &earlier)
        );
        let bad = r#""damaged":{"name":"t","samples":[{"kind":3}]}"#;
        let path = store_path().unwrap();
        crate::paths::ensure_dir(path.parent().unwrap(), 0o700).unwrap();
        std::fs::write(&path, format!(r#"{{"commands":{{{good},{bad}}}}}"#)).unwrap();

        // A job completes, and its measurement goes into the store.
        let spec = spec_for("train");
        let mut done = crate::job::JobStatus::new(&spec);
        done.state = crate::job::JobState::Completed;
        done.usage.max_rss = 2 << 30;
        record(&spec, &done);

        // The damaged file moved aside, with its bytes as they were.
        let aside: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("usage.json.corrupt-")
            })
            .collect();
        assert_eq!(aside.len(), 1, "the damaged file must move aside");
        let kept = std::fs::read_to_string(aside[0].path()).unwrap();
        assert!(
            kept.contains(r#""kind":3"#),
            "the quarantine must hold the bytes that qex could not read"
        );

        // The new store holds the salvaged peak AND the new measurement.
        let store = load();
        let old = &store.commands[&key(&std::path::PathBuf::from("/project"), &earlier)];
        assert_eq!(
            old.samples[0].max_rss,
            1 << 30,
            "the entry that qex could read must survive the write"
        );
        let new = &store.commands[&key(&spec.cwd, &spec.command)];
        assert_eq!(new.samples[0].max_rss, 2 << 30);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file whose bytes qex cannot read takes NO write.
    ///
    /// A write over a file that qex has not seen replaces measurements that
    /// qex cannot count. One lost sample costs less than the whole store.
    #[test]
    fn a_file_that_qex_cannot_read_takes_no_write() {
        use crate::testutil::{env_lock, EnvVar};
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            // root reads every file, so this test cannot make one unreadable.
            return;
        }
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("qex-usage-noread-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let _env = EnvVar::set("XDG_STATE_HOME", dir.to_str().unwrap());

        let path = store_path().unwrap();
        crate::paths::ensure_dir(path.parent().unwrap(), 0o700).unwrap();
        std::fs::write(&path, r#"{"commands":{}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let spec = spec_for("train");
        let mut done = crate::job::JobStatus::new(&spec);
        done.state = crate::job::JobState::Completed;
        done.usage.max_rss = 2 << 30;
        record(&spec, &done);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            load().commands.is_empty(),
            "no write may land on a file that qex could not read"
        );

        std::fs::remove_dir_all(&dir).ok();
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

    /// An ENTRY that this version cannot read in full must not empty the store.
    ///
    /// The guard on `Sample` holds the level of one measurement. A later qex can
    /// change the level ABOVE it, and the store still loads as one value, so an
    /// entry that is missing a field would take the peaks of every OTHER command
    /// with it.
    ///
    /// The command `good` holds the peak in each shape here, and the damage is
    /// always in the command beside it.
    #[test]
    fn an_entry_that_this_version_cannot_read_in_full_keeps_the_other_commands() {
        let good = r#"{"name":"t","samples":[{"kind":"peak","max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}]}"#;
        for (what, entry) in [
            ("an entry with no name", r#"{"samples":[]}"#),
            ("an entry where the samples went", r#"{"name":"t"}"#),
            ("an entry with no field at all", r#"{}"#),
            (
                "an entry with a field of a later qex",
                r#"{"name":"t","samples":[],"ceiling":42}"#,
            ),
        ] {
            let text = format!(r#"{{"commands":{{"good":{good},"damaged":{entry}}}}}"#);
            let store: Store = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{what} must not empty the store: {e}"));
            let peaks: Vec<u64> = store.commands["good"]
                .samples
                .iter()
                .filter(|s| s.kind == Measurement::Peak)
                .map(|s| s.max_rss)
                .collect();
            assert_eq!(
                peaks,
                vec![1 << 30],
                "{what} must leave the peak of the command beside it"
            );
        }
    }

    /// A STORE that this version cannot read in full must not lose its commands.
    ///
    /// This is the level above the entry. A later qex can add a field beside
    /// `commands`, or write a file that has no `commands` field at all.
    #[test]
    fn a_store_with_a_field_of_a_later_qex_keeps_its_commands() {
        let good = r#"{"name":"t","samples":[{"kind":"peak","max_rss":1073741824,"cpu_secs":1.0,"elapsed_secs":10,"at":0}]}"#;
        let text = format!(r#"{{"commands":{{"good":{good}}},"written_by":"a later qex"}}"#);
        let store: Store = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("a field of a later qex must not empty the store: {e}"));
        assert_eq!(store.commands["good"].samples[0].max_rss, 1 << 30);

        let empty: Store = serde_json::from_str(r#"{"written_by":"a later qex"}"#)
            .expect("a file with no commands field must read as an empty store");
        assert!(empty.commands.is_empty());
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

    /// A PEAK OF ZERO BYTES gives no claim.
    ///
    /// No job uses no memory, so a peak of zero measures nothing. A sample that
    /// is missing its number reads as a peak of zero, because every field of a
    /// sample takes a default: that is what keeps the peaks of a file that this
    /// version cannot read in full. A claim from such a sample would be the
    /// smallest that qex permits, and the record would say `learned`, so the
    /// reader would get a number that no job supports.
    #[test]
    fn a_peak_of_zero_bytes_gives_no_claim() {
        let cmd: Vec<String> = vec!["train".into()];
        for (what, text) in [
            (
                "a sample with a kind and no number",
                r#"{"commands":{"KEY":{"name":"t","samples":[{"kind":"peak"}]}}}"#,
            ),
            (
                "a sample with no field at all",
                r#"{"commands":{"KEY":{"name":"t","samples":[{}]}}}"#,
            ),
        ] {
            let text = text.replace("KEY", &key(&dir(), &cmd));
            let store: Store = serde_json::from_str(&text).unwrap();
            assert!(
                suggest(&store, &dir(), &cmd, 1.5).is_none(),
                "{what} measures nothing, so it must give no claim at all"
            );
        }
    }

    /// A peak of zero bytes takes no peak of the same command with it.
    ///
    /// THIS TEST DOES NOT GUARD THE FILTER ON A PEAK OF ZERO. It passes with
    /// that filter gone, because the largest of `0` and `1GB` is `1GB` either
    /// way. `a_peak_of_zero_bytes_gives_no_claim` is the test that fails when
    /// the filter goes.
    ///
    /// This test guards a DIFFERENT wrong answer: a version that refused the
    /// whole entry because one sample in it holds a zero. The command keeps its
    /// claim, and only the sample that measures nothing goes.
    #[test]
    fn a_peak_of_zero_bytes_leaves_the_claim_of_the_true_peaks() {
        let cmd: Vec<String> = vec!["train".into()];
        let store = store_with(
            &["train"],
            vec![sample(0, 1.0, 10), sample(1 << 30, 1.0, 10)],
        );
        let s = suggest(&store, &dir(), &cmd, 1.5).expect("the peak must give a claim");
        assert_eq!(
            s.mem,
            (1 << 30) * 3 / 2,
            "the claim must come from the peak that measures memory"
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

        let mut spec = spec_for("train");

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
