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

use crate::job::Ancestor;
use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One pause request: when it was made, who made it, and when it ends.
///
/// # Why a queue holds a SET of these, and not one
///
/// One record for each queue let a second command change what the first one
/// asked for. Every rule for that change had a sequence of events in which one
/// session shortened the pause of a different session. With a set, no request
/// changes or removes a different request: the queue is paused while at least
/// one request stands, and each request ends only by its own end or by a
/// resume that names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRecord {
    /// The name of this request. The coordinator makes it, and the answer of
    /// `qex pause` gives it to the caller as a receipt.
    ///
    /// A record of an earlier version of qex has none. `Paused::normalize`
    /// gives it one.
    #[serde(default)]
    pub id: String,
    /// The moment of the request, in seconds since the epoch.
    pub paused_at: u64,
    /// The process id that the CLI reported for itself.
    ///
    /// NOBODY VERIFIED THIS NUMBER, and no default output shows it. It is a
    /// number in the pid namespace of the caller: a `qex abort` that was the
    /// first process of a container reported 1, a reader ran `ps -p 1`, found
    /// the first process of the machine alive, and believed for six hours that
    /// the pauser still operated. The forensic output gives it as
    /// `caller_reported_pid`. `issuer_chain` is what qex reads.
    #[serde(default)]
    pub by_pid: i32,
    /// The text that the person gave with `--reason`.
    #[serde(default)]
    pub reason: Option<String>,
    /// The moment when the request ends by itself, in seconds since the epoch.
    ///
    /// `None` means that the request has no end. Such a request needs a loud
    /// report, because a user who forgets it comes back to an empty queue.
    #[serde(default)]
    pub until: Option<u64>,
    /// True when qex made this request because it could not read the file.
    ///
    /// No person asked for such a pause, so each message about it must say
    /// what happened and must not say that a person paused the queue.
    #[serde(default)]
    pub fault: bool,
    /// The processes above the command that asked, as the COORDINATOR read
    /// them from the credential of the socket, from the parent of the command
    /// upward. The numbers are thus numbers of the machine of the coordinator.
    ///
    /// `None` says that qex could not learn who asked. qex never stores a
    /// guess here. See `daemon::issuer_chain`.
    #[serde(default)]
    pub issuer_chain: Option<Vec<Ancestor>>,
    /// The pid namespace in which `issuer_chain` was read. A reader in a
    /// different namespace cannot test those numbers, and it must say
    /// `unknown`. See `sys::pid_namespace`.
    #[serde(default)]
    pub issuer_ns: Option<String>,
}

/// The id of the request that qex makes when it cannot read the file.
///
/// It is a fixed word, because that request is made again at each read, and
/// the id that one command printed must name the request that the next
/// command finds.
pub const FAULT_ID: &str = "fault";

/// Makes the id of a new request: short, opaque, and unique in `taken`.
pub fn new_id<'a>(taken: impl IntoIterator<Item = &'a str> + Clone) -> String {
    loop {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if !taken.clone().into_iter().any(|t| t == id) {
            return id;
        }
    }
}

impl PauseRecord {
    pub fn new(by_pid: i32, reason: Option<String>, until: Option<u64>) -> Self {
        Self {
            id: new_id(std::iter::empty()),
            paused_at: crate::sys::now_secs(),
            by_pid,
            reason,
            until,
            fault: false,
            issuer_chain: None,
            issuer_ns: None,
        }
    }

    /// Tests if this request reached its end.
    pub fn expired(&self, now: u64) -> bool {
        matches!(self.until, Some(end) if now >= end)
    }
}

/// The end of a pause of the queue: the last request of the set went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueEnd {
    /// The moment when the queue stopped running.
    pub since: u64,
    /// The moment when the last request ended.
    pub ended_at: u64,
}

/// Reads one record, a list of records, or nothing.
///
/// An earlier version of qex wrote ONE record for the queue and one for each
/// lock. A coordinator that replaces that version must keep the pause that the
/// file holds, so it reads both forms.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    Many(Vec<PauseRecord>),
    One(Box<PauseRecord>),
}

impl OneOrMany {
    fn list(self) -> Vec<PauseRecord> {
        match self {
            Self::Many(list) => list,
            Self::One(record) => vec![*record],
        }
    }
}

fn read_requests<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<PauseRecord>, D::Error> {
    Ok(Option::<OneOrMany>::deserialize(d)?
        .map(OneOrMany::list)
        .unwrap_or_default())
}

fn read_lock_requests<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, Vec<PauseRecord>>, D::Error> {
    Ok(BTreeMap::<String, OneOrMany>::deserialize(d)?
        .into_iter()
        .map(|(name, records)| (name, records.list()))
        .collect())
}

/// Everything that a person paused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    /// The requests that pause the whole queue. A paused queue starts no job.
    #[serde(default, deserialize_with = "read_requests")]
    pub queue: Vec<PauseRecord>,
    /// The moment when the queue stopped running: the time of the FIRST
    /// request of this unbroken pause. A request that ended since then does
    /// not move it, and the time that `--max-queue-time` gives back counts
    /// from it.
    #[serde(default)]
    pub queue_since: Option<u64>,
    /// The locks that a person holds. The key is the name of the lock. No
    /// list is empty: a lock with no request is not in the map.
    #[serde(default, deserialize_with = "read_lock_requests")]
    pub locks: BTreeMap<String, Vec<PauseRecord>>,
    /// Says that this set holds an id that the file does not hold yet.
    ///
    /// A record of the earlier shape has no id, and `read` gives it one. An id
    /// that lives in memory only is a NEW id after each restart, and a receipt
    /// that names it then names nothing. `expire` reports this as a change, so
    /// the caller writes the file before a reader sees the id.
    #[serde(skip)]
    pub id_not_in_the_file: bool,
}

impl Paused {
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.locks.is_empty()
    }

    /// True while at least one request for the queue stands.
    pub fn queue_paused(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Gives the moment when the queue stopped running.
    pub fn since(&self) -> Option<u64> {
        if self.queue.is_empty() {
            return None;
        }
        self.queue_since
            .or_else(|| self.queue.iter().map(|r| r.paused_at).min())
    }

    /// Gives every id of the set, so a new id differs from each.
    fn ids(&self) -> Vec<String> {
        self.queue
            .iter()
            .chain(self.locks.values().flatten())
            .map(|r| r.id.clone())
            .collect()
    }

    /// Gives the target that the request `id` stands for: `Some(None)` is
    /// the queue, `Some(Some(name))` is a lock, and `None` says that no
    /// request with this id stands.
    pub fn home_of(&self, id: &str) -> Option<Option<String>> {
        if self.queue.iter().any(|r| r.id == id) {
            return Some(None);
        }
        self.locks
            .iter()
            .find(|(_, list)| list.iter().any(|r| r.id == id))
            .map(|(name, _)| Some(name.clone()))
    }

    /// Puts the set in its correct form after a read or a change: each request
    /// has an id, no lock has an empty list, and `queue_since` is set exactly
    /// while the queue is paused.
    pub fn normalize(&mut self) -> bool {
        let mut taken = self.ids();
        let mut gave_an_id = false;
        for record in self
            .queue
            .iter_mut()
            .chain(self.locks.values_mut().flatten())
        {
            if record.id.is_empty() {
                record.id = new_id(taken.iter().map(String::as_str));
                taken.push(record.id.clone());
                gave_an_id = true;
            }
        }
        self.locks.retain(|_, list| !list.is_empty());
        self.queue_since = self.since();
        gave_an_id
    }

    /// Adds a request for the queue. Gives `true` when this request moved the
    /// queue from running to paused.
    ///
    /// A request that qex made because it could not read the file is not a
    /// request of a person. A real request replaces it: the new file that
    /// this change writes is a file that qex can read.
    ///
    /// The fault request HELD the queue, so the request that replaces it did
    /// not stop the queue: the answer is `false`, and `queue_since` stays at
    /// the moment of the fault. The time that `--max-queue-time` gives back
    /// then covers the whole hold, and not only the part after this request.
    pub fn add_queue(&mut self, mut record: PauseRecord) -> bool {
        let created = self.queue.is_empty();
        self.queue.retain(|r| !r.fault);
        if created {
            self.queue_since = Some(record.paused_at);
        }
        record.id = new_id(self.ids().iter().map(String::as_str));
        self.queue.push(record);
        created
    }

    /// Adds a request for one lock. Gives `true` when nobody held the lock
    /// for a person before.
    pub fn add_lock(&mut self, name: &str, mut record: PauseRecord) -> bool {
        record.id = new_id(self.ids().iter().map(String::as_str));
        let list = self.locks.entry(name.to_string()).or_default();
        let created = list.is_empty();
        list.push(record);
        created
    }

    /// Removes the requests of the queue that `which` names. Gives them, and
    /// the end of the pause when none stands after that.
    pub fn remove_queue(
        &mut self,
        which: impl Fn(&PauseRecord) -> bool,
        now: u64,
    ) -> (Vec<PauseRecord>, Option<QueueEnd>) {
        let since = self.since();
        let (removed, kept): (Vec<_>, Vec<_>) = self.queue.drain(..).partition(|r| which(r));
        self.queue = kept;
        let end = match since {
            Some(since) if self.queue.is_empty() && !removed.is_empty() => Some(QueueEnd {
                since,
                ended_at: now,
            }),
            _ => None,
        };
        self.normalize();
        (removed, end)
    }

    /// Removes the requests of one lock that `which` names, and gives them.
    pub fn remove_lock(
        &mut self,
        name: &str,
        which: impl Fn(&PauseRecord) -> bool,
    ) -> Vec<PauseRecord> {
        let Some(list) = self.locks.get_mut(name) else {
            return Vec::new();
        };
        let (removed, kept): (Vec<_>, Vec<_>) = list.drain(..).partition(|r| which(r));
        *list = kept;
        self.normalize();
        removed
    }

    /// Removes each request that reached its end.
    ///
    /// `changed` says that a request went away. `queue_end` says that the
    /// LAST request of the queue went away, so the queue runs again; its
    /// `ended_at` is the latest end of the requests that went away, and never
    /// a moment after `now`.
    pub fn expire(&mut self, now: u64) -> Expired {
        let before = self.queue.len() + self.locks.values().map(Vec::len).sum::<usize>();
        let since = self.since();
        let last_end = self
            .queue
            .iter()
            .filter(|r| r.expired(now))
            .filter_map(|r| r.until)
            .max();
        self.queue.retain(|r| !r.expired(now));
        for list in self.locks.values_mut() {
            list.retain(|r| !r.expired(now));
        }
        let queue_end = match (since, last_end) {
            (Some(since), Some(end)) if self.queue.is_empty() => Some(QueueEnd {
                since,
                ended_at: end.min(now),
            }),
            _ => None,
        };
        // An id that this pass gave is a change also. The file must hold it
        // before a reader sees it: an id that lives in memory only is a new id
        // after each restart, and the receipt that names it then names nothing.
        let gave_an_id = self.normalize() || std::mem::take(&mut self.id_not_in_the_file);
        let after = self.queue.len() + self.locks.values().map(Vec::len).sum::<usize>();
        Expired {
            changed: after != before || gave_an_id,
            queue_end,
        }
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
    /// mistake costs latency, and one command corrects it: a resume of that
    /// request writes a new file. A queue that operates by mistake gives the
    /// person the opposite of the one thing that person asked for, and no
    /// command corrects that after the work started. So qex holds the queue,
    /// and it says why.
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
        match serde_json::from_str::<Self>(&text) {
            Ok(mut paused) => {
                paused.id_not_in_the_file = paused.normalize();
                paused
            }
            Err(e) => Self::held_by_fault(&path, &e.to_string()),
        }
    }

    /// Gives the pause that qex holds when it cannot read the file.
    fn held_by_fault(path: &std::path::Path, fault: &str) -> Self {
        let now = crate::sys::now_secs();
        Self {
            queue: vec![PauseRecord {
                id: FAULT_ID.to_string(),
                paused_at: now,
                by_pid: 0,
                reason: Some(format!("{}: {fault}", path.display())),
                until: None,
                fault: true,
                issuer_chain: None,
                issuer_ns: None,
            }],
            queue_since: Some(now),
            locks: BTreeMap::new(),
            id_not_in_the_file: false,
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

/// What `Paused::expire` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expired {
    pub changed: bool,
    pub queue_end: Option<QueueEnd>,
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
/// The caller has ALREADY removed the last request from `state.paused`, and it
/// gives the end here: the moment when the queue stopped running, and the
/// moment when the pause ended.
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
pub fn end_queue_pause(state: &mut crate::daemon::State, end: QueueEnd) {
    credit_paused_wait(state, end.since, end.ended_at);
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

/// What qex knows about the session that made a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssuerState {
    /// A recorded process of the session still exists. This says that a
    /// process exists, and not that anybody remembers the pause.
    Running,
    /// No recorded process of the session exists.
    Gone,
    /// qex could not learn who asked, or cannot test the processes. Every
    /// reader treats this state as `Running`, and never as less.
    #[serde(other)]
    Unknown,
}

/// If the reader of a request is in the session that made it.
///
/// This value is a hint for a reader that lost its receipt. Sibling agents
/// share one session, so the id is the proof and this value is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Yes,
    No,
    #[serde(other)]
    Unknown,
}

/// One process of `issuer_chain`, in the forensic output.
///
/// The field is `host_pid`, and never `pid`: the number has a meaning on the
/// machine of the coordinator only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainEntry {
    pub host_pid: i32,
    #[serde(default)]
    pub start: Option<u64>,
    #[serde(default)]
    pub name: String,
}

/// One standing request, as the coordinator shows it to ONE reader.
///
/// The coordinator makes this value, because it alone can test the processes
/// of the issuer and walk the chain of the reader on its own machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseView {
    pub pause_id: String,
    pub made_at: u64,
    #[serde(default)]
    pub until: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub fault: bool,
    pub issuer_session_state: IssuerState,
    /// `None` exactly when the state is `Unknown`.
    #[serde(default)]
    pub issuer_program: Option<String>,
    pub issuer_is_this_session: Relation,
    /// For the forensic output only.
    #[serde(default)]
    pub issuer_chain: Option<Vec<ChainEntry>>,
    /// For the forensic output only. See `PauseRecord::by_pid`.
    #[serde(default)]
    pub caller_reported_pid: Option<i32>,
}

/// The programs that never count as a process of a session: the first
/// process of a machine, and a container runtime or its shim. Each one lives
/// as long as the machine or the container, so it says nothing about the
/// session that made a request. The names have the safe form of
/// `job::safe_name`.
const NEVER_A_SESSION: &[&str] = &[
    "init",
    "systemd",
    "launchd",
    "tini",
    "dumb-init",
    "docker-init",
    "conmon",
    "containerd-shim",
];

/// The shells. A shell is a process of a session, but its name tells a reader
/// nothing, so the output prefers the name of a different process.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "csh", "tcsh", "nu", "pwsh",
];

/// Gives the processes of a chain that count for the state of a session:
/// those at or below the point where the session ends, without the first
/// process of the machine and without a container runtime.
///
/// The boundary process counts here, although `context::shared` leaves it
/// out. An agent that a service started is itself the boundary of its chain,
/// and without it the only process below is the `qex` command, which stops in
/// milliseconds: every such pause would read `gone`.
pub fn session_processes(chain: &[Ancestor]) -> Vec<&Ancestor> {
    let Some(index) = crate::context::boundary_index(chain) else {
        return Vec::new();
    };
    chain
        .iter()
        .take(index + 1)
        .filter(|p| p.pid != 1 && !NEVER_A_SESSION.contains(&p.name.as_str()))
        .collect()
}

/// Chooses the name that the output gives for a session: the lowest process
/// that is not a shell, or the highest process when each one is a shell.
fn program_of(processes: &[&Ancestor]) -> Option<String> {
    processes
        .iter()
        .find(|p| !SHELLS.contains(&p.name.as_str()))
        .or(processes.last())
        .map(|p| p.name.clone())
}

/// Tests if the process that a record names still exists: the number AND the
/// start time. The machine gives the number of a stopped process to a later
/// one, so the number alone proves nothing, and a record with no start time
/// proves nothing either.
pub fn still_exists(process: &Ancestor) -> bool {
    process.start.is_some() && crate::sys::process_start_token(process.pid) == process.start
}

/// Gives the state of the session that made a request, and the name to show.
///
/// `exists` tests one recorded process. `same_namespace` is false when the
/// record was made in a pid namespace that this process is not in.
pub fn issuer_state(
    record: &PauseRecord,
    same_namespace: bool,
    exists: &dyn Fn(&Ancestor) -> bool,
) -> (IssuerState, Option<String>) {
    let unknown = (IssuerState::Unknown, None);
    if record.fault || !same_namespace {
        return unknown;
    }
    let Some(chain) = &record.issuer_chain else {
        return unknown;
    };
    let session = session_processes(chain);
    if session.is_empty() {
        return unknown;
    }
    let alive: Vec<&Ancestor> = session.iter().copied().filter(|p| exists(p)).collect();
    if !alive.is_empty() {
        return (IssuerState::Running, program_of(&alive));
    }
    // A process with no start time can never be shown to exist, so qex
    // cannot say that a session with such a process is gone.
    if session.iter().any(|p| p.start.is_none()) {
        return unknown;
    }
    (IssuerState::Gone, program_of(&session))
}

/// Says if the reader is in the session that made a request.
///
/// `Yes` needs a shared process BELOW the boundary, which is the rule of
/// `context::shared`. Two agents in two panes of one multiplexer share the
/// multiplexer only; that is not one session, and qex cannot say that it is
/// two, because an agent that IS its boundary shares nothing else with its
/// own commands. The answer is then `Unknown`.
pub fn relation(issuer: Option<&[Ancestor]>, reader: Option<&[Ancestor]>) -> Relation {
    let (Some(issuer), Some(reader)) = (issuer, reader) else {
        return Relation::Unknown;
    };
    if crate::context::shared(issuer, reader) {
        return Relation::Yes;
    }
    let (ours, theirs) = (session_processes(issuer), session_processes(reader));
    if ours.is_empty() || theirs.is_empty() {
        return Relation::Unknown;
    }
    let boundary_only = ours
        .iter()
        .any(|a| a.start.is_some() && theirs.iter().any(|b| b.pid == a.pid && b.start == a.start));
    if boundary_only {
        Relation::Unknown
    } else {
        Relation::No
    }
}

/// Makes the view of one request for one reader.
///
/// `reader` is the chain above the command that reads, as the coordinator
/// walked it, or `None` when qex could not learn it.
pub fn view(
    record: &PauseRecord,
    reader: Option<&[Ancestor]>,
    same_namespace: bool,
    exists: &dyn Fn(&Ancestor) -> bool,
) -> PauseView {
    let (state, program) = issuer_state(record, same_namespace, exists);
    // `Unknown` on either side gives `Unknown`.
    let relation = match state {
        IssuerState::Unknown => Relation::Unknown,
        _ => relation(record.issuer_chain.as_deref(), reader),
    };
    PauseView {
        pause_id: record.id.clone(),
        made_at: record.paused_at,
        until: record.until,
        reason: record.reason.clone(),
        fault: record.fault,
        issuer_session_state: state,
        issuer_program: program,
        issuer_is_this_session: relation,
        issuer_chain: record.issuer_chain.as_ref().map(|chain| {
            chain
                .iter()
                .map(|p| ChainEntry {
                    host_pid: p.pid,
                    start: p.start,
                    name: p.name.clone(),
                })
                .collect()
        }),
        caller_reported_pid: (record.by_pid > 0).then_some(record.by_pid),
    }
}

/// Makes the views of a list of requests, for a reader on THIS machine.
pub fn views(records: &[PauseRecord], reader: Option<&[Ancestor]>) -> Vec<PauseView> {
    let here = crate::sys::pid_namespace();
    records
        .iter()
        .map(|r| view(r, reader, r.issuer_ns == here, &still_exists))
        .collect()
}

/// Gives ONE record that stands for a set, for a CLI of an earlier version,
/// which reads one record and prints its pid. The record holds NO process id,
/// so that CLI prints "an unknown process". It starts at the oldest request,
/// and it has an end only when every request has one.
pub fn for_an_earlier_cli(records: &[PauseRecord]) -> Option<PauseRecord> {
    let oldest = records.iter().min_by_key(|r| r.paused_at)?;
    Some(PauseRecord {
        by_pid: 0,
        until: records
            .iter()
            .map(|r| r.until)
            .collect::<Option<Vec<u64>>>()
            .and_then(|ends| ends.into_iter().max()),
        issuer_chain: None,
        issuer_ns: None,
        ..oldest.clone()
    })
}

/// Gives the view of a record that came from a coordinator of an earlier
/// version. That coordinator read no issuer, so the state is `Unknown`.
pub fn unknown_view(record: &PauseRecord) -> PauseView {
    PauseView {
        caller_reported_pid: None,
        ..view(record, None, false, &|_| false)
    }
}

/// The thing that a request pauses, for the words of a command.
#[derive(Debug, Clone, Copy)]
pub enum Target<'a> {
    Queue,
    Lock(&'a str),
}

/// Gives the guarded command that ends ONE request.
pub fn guarded_command(target: Target, id: &str) -> String {
    match target {
        Target::Queue => format!("qex resume queue --pause {id}"),
        Target::Lock(name) => format!("qex resume lock {} --pause {id}", shown_lock(name)),
    }
}

/// Gives the refusal for a resume that names an id of a DIFFERENT target.
/// `home` is the lock that the request stands for, or `None` for the queue.
///
/// The command is there for every reader: the person who typed the id holds
/// it, and the guarded command is the only one that qex ever prints.
pub fn stands_elsewhere(id: &str, home: Option<&str>) -> String {
    let id = crate::job::safe_name(id);
    let (place, target) = match home {
        None => ("the queue".to_string(), Target::Queue),
        Some(name) => (
            format!("the lock `{}`", shown_lock(name)),
            Target::Lock(name),
        ),
    };
    format!(
        "pause {id} stands for {place}, and not for the target of this command. qex resumed \
         nothing, and the request still stands. To end it: `{}`",
        guarded_command(target, &id)
    )
}

/// Gives the command that resumes ONE request, or `None` when the rules for
/// the advice allow no command.
///
/// THE RULES. qex prints a command that is ready to run ONLY for a request of
/// the session of the reader, and for a request of a session that is gone. It
/// never prints one for a different session that still runs, and never for
/// `Unknown`, which must not read more permissively than `Running`. The
/// command is always the guarded one. The text and the JSON both take their
/// answer from this function, so they cannot disagree.
///
/// A request that qex made because it could not read the file is the request
/// of nobody. The command is the remedy, so qex prints it.
pub fn resume_command(view: &PauseView, target: Target) -> Option<String> {
    // A coordinator of an earlier version gives no id, so no guarded command
    // exists.
    if view.pause_id.is_empty() {
        return None;
    }
    let allowed = view.fault
        || match (view.issuer_session_state, view.issuer_is_this_session) {
            (IssuerState::Unknown, _) => false,
            (IssuerState::Gone, _) => true,
            (IssuerState::Running, Relation::Yes) => true,
            (IssuerState::Running, _) => false,
        };
    allowed.then(|| guarded_command(target, &view.pause_id))
}

/// The one exception that each "do not resume" sentence names beside the
/// user: the holder of the id.
const DO_NOT_RESUME: &str = "Do not resume it unless you hold this id from your own `pause` \
     answer or your user tells you to.";

fn ago(then: u64, now: u64) -> String {
    format!(
        "{} ago",
        crate::units::format_duration(std::time::Duration::from_secs(now.saturating_sub(then)))
    )
}

fn end_text(until: Option<u64>, now: u64) -> String {
    match until {
        Some(end) => format!(
            "until {} (in {})",
            crate::sys::near_stamp_text(end, now),
            crate::units::format_duration(std::time::Duration::from_secs(end.saturating_sub(now)))
        ),
        None => "until somebody resumes it".to_string(),
    }
}

/// Gives the facts of one request with no advice: its id, its age, its end,
/// the session that made it, and its reason. A report of a request that a
/// resume ended uses this form, because advice about a request that no longer
/// stands has no reader.
pub fn request_head(view: &PauseView, now: u64) -> String {
    let program = view
        .issuer_program
        .as_deref()
        .map(crate::job::safe_name)
        .unwrap_or_else(|| "unknown".into());
    let from = match (view.issuer_session_state, view.issuer_is_this_session) {
        (IssuerState::Unknown, _) => None,
        (IssuerState::Running, Relation::Yes) => {
            Some(format!("from THIS session ({program}, still running)"))
        }
        (IssuerState::Running, Relation::No) => Some(format!(
            "from another session that is still running ({program})"
        )),
        (IssuerState::Running, Relation::Unknown) => {
            Some(format!("from a session that is still running ({program})"))
        }
        (IssuerState::Gone, _) => Some(format!("from a session that is gone ({program})")),
    };

    let name = if view.pause_id.is_empty() {
        // A coordinator of an earlier version holds one record with no id.
        String::from("a pause with no id (this coordinator is an earlier version)")
    } else {
        format!("pause {}", view.pause_id)
    };
    let mut text = format!(
        "{name}, made {} ({}), {}",
        crate::sys::near_stamp_text(view.made_at, now),
        ago(view.made_at, now),
        end_text(view.until, now)
    );
    if let Some(from) = from {
        text.push_str(&format!(", {from}"));
    }
    match &view.reason {
        Some(reason) => text.push_str(&format!(", reason: {}.", shown_reason(reason))),
        None => text.push_str(", no reason given."),
    }

    text
}

/// Gives the line of ONE standing request: its id, its age, its end, the
/// session that made it, its reason, and one sentence of advice.
///
/// The line holds no process id. See `PauseRecord::by_pid`.
pub fn request_line(view: &PauseView, target: Target, now: u64) -> String {
    if view.fault {
        let fault = view
            .reason
            .as_deref()
            .map(shown_reason)
            .unwrap_or_else(|| "unknown".into());
        let remedy = match resume_command(view, target) {
            Some(command) => format!("Correct that file, or write a new one: {command}"),
            None => String::from("Correct that file."),
        };
        return format!(
            "pause {}: PAUSED BY A FAULT. qex could not read its pause record, and a record \
             that qex cannot read can hold a pause, so qex holds the queue. The fault: {fault}. \
             {remedy}",
            view.pause_id
        );
    }

    let text = request_head(view, now);

    let command = resume_command(view, target);
    let advice = match (view.issuer_session_state, &command) {
        (IssuerState::Unknown, _) if view.pause_id.is_empty() => String::from(
            "qex cannot tell which session set it or whether that session is still there, so \
             treat it as still there. Do not resume it unless your user tells you to.",
        ),
        (IssuerState::Unknown, _) => format!(
            "qex cannot tell which session set it or whether that session is still there, so \
             treat it as still there. {DO_NOT_RESUME}"
        ),
        (IssuerState::Running, Some(command)) => format!(
            "If a request of yours (not of a sibling agent) set it and its work is done: {command}"
        ),
        (IssuerState::Running, None) => format!("That session can resume it. {DO_NOT_RESUME}"),
        (IssuerState::Gone, command) => {
            let first = match view.until {
                Some(end) => format!(
                    "It ends by itself at {}, so waiting is the default.",
                    crate::sys::near_stamp_text(end, now)
                ),
                None => "It still stands, and the processes of the session that set it have \
                         exited, so expect nobody to resume it."
                    .to_string(),
            };
            format!(
                "{first} Ask your user, or resume it if the reason no longer applies: {}",
                command.as_deref().unwrap_or_default()
            )
        }
    };
    format!("{text} {advice}")
}

/// Gives the latest end of a set, or `None` when one request has no end.
pub fn last_end(views: &[PauseView]) -> Option<u64> {
    views
        .iter()
        .map(|v| v.until)
        .collect::<Option<Vec<u64>>>()
        .and_then(|ends| ends.into_iter().max())
}

/// Gives the words "N requests stand; it runs again when ..." for a set.
///
/// `again` says what the target does after that: "it runs again", or "it is
/// free again".
fn standing_text(views: &[PauseView], again: &str, now: u64) -> String {
    let end = end_text(last_end(views), now);
    if views.len() == 1 {
        format!("1 request stands; {again} when that request ends (its end is: {end})")
    } else {
        format!(
            "{} requests stand; {again} when all of them end (the last end is: {end})",
            views.len()
        )
    }
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
/// # Why this text holds no elapsed time, and no session
///
/// The scheduler writes `status.json` for every job whose reason changed, and
/// each write does two `fsync` calls. The scheduler tests the queue every
/// 500ms. A reason that said "6 minutes ago", or that gave the state of a
/// session, would thus change and write the record of every job in the queue,
/// for the whole length of the pause.
///
/// This text changes only when the set of requests changes. `qex pause`
/// calculates the rest when a person reads it, and it is the command that
/// this text names: it gives each request with its id, and a bare
/// `qex resume queue` removes nothing.
pub fn queue_reason(paused: &Paused) -> String {
    if let Some(record) = paused.queue.iter().find(|r| r.fault) {
        return format!(
            "the queue is paused, so qex starts no job. qex could not read its pause record, and \
             a record that qex cannot read can hold a pause, so qex holds the queue. The fault: \
             {}. Correct that file, or run `{}` to write a new one.",
            record
                .reason
                .as_deref()
                .map(shown_reason)
                .unwrap_or_else(|| "unknown".into()),
            guarded_command(Target::Queue, &record.id)
        );
    }

    let mut text = String::from("the queue is paused, so qex starts no job.");
    match paused.queue.as_slice() {
        [one] => {
            text.push_str(" 1 pause request stands.");
            if let Some(reason) = &one.reason {
                text.push_str(&format!(" Reason: {}.", shown_reason(reason)));
            }
        }
        many => text.push_str(&format!(" {} pause requests stand.", many.len())),
    }
    if let Some(since) = paused.since() {
        text.push_str(&format!(
            " The queue stopped at {}.",
            crate::sys::stamp_text(since)
        ));
    }
    text.push_str(" Run `qex pause` to read each request, who made it and how it ends.");
    text
}

/// Gives the reason that a job waits for a lock that a person holds.
pub fn lock_reason(name: &str) -> String {
    format!(
        "waits for the lock `{}`, which a person holds",
        shown_lock(name)
    )
}

/// Gives the lines that say how the queue is paused: one summary line, and
/// then one line for each standing request.
///
/// Every command that shows the pause uses this function, so `qex info`,
/// `qex top`, `qex list` and `qex pause` never disagree.
///
/// `since` is the moment when the queue stopped running. It is NOT the time
/// of the newest request: a reader who sees "4m ago" on a queue that has been
/// quiet for six hours looks for the wrong cause.
pub fn queue_lines(views: &[PauseView], since: Option<u64>, now: u64) -> Vec<String> {
    if views.is_empty() {
        return Vec::new();
    }
    let since = since.unwrap_or_else(|| views.iter().map(|v| v.made_at).min().unwrap_or(now));
    let mut lines = vec![format!(
        "the queue is paused since {} ({}): {}",
        crate::sys::stamp_text(since),
        ago(since, now),
        standing_text(views, "it runs again", now)
    )];
    lines.extend(views.iter().map(|v| request_line(v, Target::Queue, now)));
    lines
}

/// Gives the lines for a lock that a person holds: one summary line, and then
/// one line for each standing request.
pub fn lock_lines(name: &str, views: &[PauseView], held_by: Option<&str>, now: u64) -> Vec<String> {
    let since = views.iter().map(|v| v.made_at).min().unwrap_or(now);
    let mut text = format!(
        "lock `{}`: a person holds it since {} ({}): {}",
        shown_lock(name),
        crate::sys::stamp_text(since),
        ago(since, now),
        standing_text(views, "it is free again", now)
    );
    match held_by {
        Some(job) => text.push_str(&format!(
            " · the job {job} still holds it · qex gives it to the person when that job stops"
        )),
        None => text.push_str(" · no job holds it"),
    }
    let mut lines = vec![text];
    lines.extend(
        views
            .iter()
            .map(|v| request_line(v, Target::Lock(name), now)),
    );
    lines
}

/// Gives one request as the default JSON shows it. It holds NO process id of
/// any kind. `forensic` adds the chain that the coordinator read and the
/// number that the caller reported.
pub fn view_json(view: &PauseView, target: Target, forensic: bool) -> serde_json::Value {
    let mut value = serde_json::json!({
        "pause_id": view.pause_id,
        "made_at": crate::sys::rfc3339(view.made_at),
        "until": view.until.map(crate::sys::rfc3339),
        "reason": view.reason,
        "fault": view.fault,
        "issuer_session_state": view.issuer_session_state,
        "issuer_program": view.issuer_program,
        "issuer_is_this_session": match view.issuer_is_this_session {
            Relation::Yes => "yes",
            Relation::No => "no",
            Relation::Unknown => "unknown",
        },
        "resume_command": resume_command(view, target),
    });
    if forensic {
        value["issuer_chain"] = serde_json::json!(view.issuer_chain);
        value["caller_reported_pid"] = serde_json::json!(view.caller_reported_pid);
    }
    value
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
            id: "7f3c9a1e".into(),
            paused_at: 1_000,
            by_pid: 42,
            reason: None,
            until: None,
            fault: false,
            issuer_chain: None,
            issuer_ns: None,
        }
    }

    fn process(pid: i32, ppid: i32, name: &str, terminal: bool) -> Ancestor {
        Ancestor {
            pid,
            ppid,
            start: Some(1000 + pid as u64),
            name: name.into(),
            cwd: None,
            terminal,
        }
    }

    /// The chain above a `qex` command of an agent in one pane of a
    /// multiplexer, from the shell of the agent upward.
    fn agent(shell: i32, agent: i32, pane: i32) -> Vec<Ancestor> {
        vec![
            process(shell, agent, "bash", false),
            process(agent, pane, "claude", true),
            process(pane, 6152, "bash", true),
            process(6152, 1, "tmux_server", false),
            process(1, 0, "systemd", false),
        ]
    }

    fn from(chain: Vec<Ancestor>) -> PauseRecord {
        PauseRecord {
            issuer_chain: Some(chain),
            ..record()
        }
    }

    /// Every process exists, or none does.
    fn all(_: &Ancestor) -> bool {
        true
    }
    fn none(_: &Ancestor) -> bool {
        false
    }

    /// The four views that the four advice lines come from.
    fn this_session() -> PauseView {
        view(
            &from(agent(100, 50, 40)),
            Some(&agent(101, 50, 40)),
            true,
            &all,
        )
    }
    fn another_session() -> PauseView {
        let other = vec![
            process(200, 60, "bash", false),
            process(60, 41, "claude", true),
            process(41, 7000, "zsh", true),
            process(7000, 1, "sshd", false),
        ];
        view(&from(agent(100, 50, 40)), Some(&other), true, &all)
    }
    fn gone_session() -> PauseView {
        view(
            &from(agent(100, 50, 40)),
            Some(&agent(201, 60, 41)),
            true,
            &none,
        )
    }
    fn unknown_session() -> PauseView {
        view(&record(), Some(&agent(101, 50, 40)), true, &all)
    }

    /// Two requests, in each order, with and without an end: no request
    /// changes or removes the other one, and the queue is paused while one
    /// stands.
    ///
    /// # The fault that this test prevents
    ///
    /// With ONE record for each queue, a second `qex pause queue` had to
    /// change the first. Each rule for that change had a sequence in which
    /// one session shortened or ended what a different session asked for.
    #[test]
    fn no_request_changes_or_removes_a_different_request() {
        for timed_first in [true, false] {
            let timed = PauseRecord {
                until: Some(2_800),
                reason: Some("recording a demo".into()),
                ..record()
            };
            let endless = PauseRecord {
                reason: Some("bulk abort".into()),
                ..record()
            };
            let (first, second) = if timed_first {
                (timed.clone(), endless.clone())
            } else {
                (endless.clone(), timed.clone())
            };

            let mut p = Paused::default();
            assert!(p.add_queue(first), "the first request pauses the queue");
            assert!(
                !p.add_queue(second),
                "the second request did not move the queue from running to paused"
            );
            assert_eq!(p.queue.len(), 2);
            assert_ne!(p.queue[0].id, p.queue[1].id, "each request has its own id");
            let id_of = |p: &Paused, timed: bool| {
                p.queue
                    .iter()
                    .find(|r| r.until.is_some() == timed)
                    .map(|r| r.id.clone())
                    .unwrap()
            };

            // A resume of the request with no end leaves the timed one as it
            // was, and the queue stays paused.
            let mut a = p.clone();
            let id = id_of(&a, false);
            let (removed, end) = a.remove_queue(|r| r.id == id, 1_500);
            assert_eq!(removed.len(), 1);
            assert_eq!(end, None, "one request stands, so the pause did not end");
            assert_eq!(a.queue.len(), 1);
            assert_eq!(a.queue[0].until, Some(2_800));
            assert_eq!(a.queue[0].reason.as_deref(), Some("recording a demo"));

            // A resume of the timed request leaves the request with no end.
            let mut b = p.clone();
            let id = id_of(&b, true);
            let (_, end) = b.remove_queue(|r| r.id == id, 1_500);
            assert_eq!(end, None);
            assert_eq!(b.queue[0].until, None, "no request got the end of another");
            assert_eq!(b.queue[0].reason.as_deref(), Some("bulk abort"));

            // The end of the timed request by itself does the same.
            let mut c = p.clone();
            let expired = c.expire(2_800);
            assert!(expired.changed);
            assert_eq!(expired.queue_end, None);
            assert!(c.queue_paused());

            // The resume of the last request ends the pause, and the pause
            // began at the FIRST request.
            let id = a.queue[0].id.clone();
            let (_, end) = a.remove_queue(|r| r.id == id, 1_700);
            assert_eq!(
                end,
                Some(QueueEnd {
                    since: 1_000,
                    ended_at: 1_700
                })
            );
            assert!(!a.queue_paused());
            assert!(a.is_empty());
        }
    }

    /// An id that stands no more, and an id that never existed, remove
    /// nothing and end nothing.
    #[test]
    fn an_id_that_does_not_stand_removes_nothing() {
        let mut p = Paused::default();
        p.add_queue(record());
        let id = p.queue[0].id.clone();

        let (removed, end) = p.remove_queue(|r| r.id == "00000000", 1_100);
        assert!(removed.is_empty(), "an id that never existed");
        assert_eq!(end, None);
        assert!(p.queue_paused());

        let (removed, end) = p.remove_queue(|r| r.id == id, 1_100);
        assert_eq!(removed.len(), 1);
        assert!(end.is_some());

        let (removed, end) = p.remove_queue(|r| r.id == id, 1_200);
        assert!(removed.is_empty(), "an id that stands no more");
        assert_eq!(end, None, "a pause that ended must not end a second time");
    }

    /// `since` is the moment when the queue stopped running, and not the time
    /// of the newest request or of the oldest request that still stands.
    #[test]
    fn the_pause_began_when_the_queue_stopped() {
        let mut p = Paused::default();
        p.add_queue(PauseRecord {
            until: Some(1_500),
            ..record()
        });
        p.add_queue(PauseRecord {
            paused_at: 1_200,
            ..record()
        });
        assert_eq!(p.since(), Some(1_000));

        // The first request ends. The queue did not run in between.
        assert!(p.expire(1_500).changed);
        assert_eq!(p.queue.len(), 1);
        assert_eq!(p.since(), Some(1_000), "the pause is unbroken");

        // The value survives the file, so a new coordinator gives back the
        // whole of the pause to `--max-queue-time`.
        let back: Paused = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back.since(), Some(1_000));

        // After the queue ran, a new pause begins at its own first request.
        let id = p.queue[0].id.clone();
        p.remove_queue(|r| r.id == id, 1_600);
        assert_eq!(p.since(), None);
        p.add_queue(PauseRecord {
            paused_at: 1_700,
            ..record()
        });
        assert_eq!(p.since(), Some(1_700));
    }

    /// A pause with `--for` must end by itself. Without this test a queue that
    /// a person paused for 30 minutes would stay paused for ever.
    #[test]
    fn a_pause_with_an_end_goes_away_by_itself() {
        let mut p = Paused::default();
        p.add_queue(PauseRecord {
            until: Some(1_100),
            ..record()
        });
        p.add_lock(
            "gpu0",
            PauseRecord {
                until: Some(2_000),
                ..record()
            },
        );

        assert!(
            !p.expire(1_099).changed,
            "the pause must stay before its end"
        );
        assert!(p.queue_paused());

        let expired = p.expire(9_000_000);
        assert!(expired.changed, "the pause must go away at its end");
        assert!(!p.queue_paused(), "the queue must operate again");
        assert_eq!(
            expired.queue_end,
            Some(QueueEnd {
                since: 1_000,
                ended_at: 1_100
            }),
            "the pause ended at ITS end, and not at the moment of the test"
        );
        assert!(p.is_empty(), "a lock whose last request ended is free");
    }

    /// A pause with no end never goes away by itself.
    #[test]
    fn a_pause_with_no_end_stays() {
        let mut p = Paused::default();
        p.add_queue(record());
        assert!(!p.expire(9_999_999).changed);
        assert!(p.queue_paused());
    }

    /// A lock is held for a person while at least one request for it stands.
    #[test]
    fn a_lock_is_held_while_one_request_stands() {
        let mut p = Paused::default();
        assert!(p.add_lock("gpu0", record()));
        assert!(!p.add_lock("gpu0", record()));
        let first = p.locks["gpu0"][0].id.clone();
        assert_eq!(p.remove_lock("gpu0", |r| r.id == first).len(), 1);
        assert!(p.locks.contains_key("gpu0"), "one request stands");
        assert_eq!(p.remove_lock("gpu0", |_| true).len(), 1);
        assert!(!p.locks.contains_key("gpu0"), "no empty list stays");
        assert!(p.remove_lock("gpu0", |_| true).is_empty());
    }

    /// The reason of a queued job must not hold a number that changes.
    ///
    /// The scheduler writes `status.json` with two `fsync` calls for every job
    /// whose reason changed, and it tests the queue every 500ms. A reason with
    /// an elapsed time would rewrite every record of the queue, twice a second,
    /// for the whole length of the pause.
    #[test]
    fn the_reason_of_a_paused_job_does_not_change_with_time() {
        let mut p = Paused::default();
        p.add_queue(from(agent(100, 50, 40)));
        let reason = queue_reason(&p);
        assert_eq!(reason, queue_reason(&p));
        assert!(reason.contains("the queue is paused"));
        assert!(
            reason.contains("`qex pause`"),
            "the reason must give the command that shows each request: {reason}"
        );
        assert!(
            !reason.contains("qex resume"),
            "a resume with no id removes nothing, so the reason must not name it: {reason}"
        );
        assert!(
            !reason.contains("ago") && !reason.contains("running") && !reason.contains("gone"),
            "the reason must hold no elapsed time and no state of a session: {reason}"
        );
    }

    /// A pause with no end must say so wherever a person reads it.
    #[test]
    fn a_pause_with_no_end_says_so() {
        let lines = queue_lines(&[unknown_session()], Some(1_000), 1_360);
        assert_eq!(lines.len(), 2, "one summary line and one request line");
        assert!(
            lines[0].contains("until somebody resumes it"),
            "got: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("6m"),
            "the line must give the length: {}",
            lines[0]
        );
        assert!(lines[0].contains("1 request stands"), "got: {}", lines[0]);

        let timed = PauseView {
            until: Some(2_000),
            ..unknown_session()
        };
        let lines = queue_lines(&[timed.clone(), unknown_session()], Some(1_000), 1_360);
        assert!(
            lines[0].contains("2 requests stand")
                && lines[0].contains("the last end is: until somebody resumes it"),
            "one request with no end gives the set no end: {}",
            lines[0]
        );
        assert_eq!(last_end(std::slice::from_ref(&timed)), Some(2_000));
        assert_eq!(last_end(&[timed, unknown_session()]), None);
    }

    /// NO LINE OF A PAUSE HOLDS A PROCESS ID.
    ///
    /// # The fault that this test prevents
    ///
    /// A `qex abort` that was the first process of a container reported its
    /// own pid, which was 1. The line said "by pid 1", a reader ran `ps -p 1`
    /// on the machine, found the first process alive, and believed for six
    /// hours that the pauser still operated. A reader takes a pid in a line
    /// for a pid of ITS machine, so the lines give the session and no number.
    #[test]
    fn no_line_of_a_pause_holds_a_process_id() {
        let mut p = Paused::default();
        p.add_queue(PauseRecord {
            by_pid: 31_337,
            ..from(agent(100, 50, 40))
        });
        let mut texts = vec![queue_reason(&p)];
        for v in [
            this_session(),
            another_session(),
            gone_session(),
            unknown_session(),
        ] {
            let v = PauseView {
                caller_reported_pid: Some(31_337),
                ..v
            };
            texts.extend(queue_lines(std::slice::from_ref(&v), Some(1_000), 1_360));
            texts.extend(lock_lines("gpu0", &[v], None, 1_360));
        }
        for text in texts {
            assert!(!text.contains("pid"), "a line names a pid: {text}");
            for number in ["31337", " 50", "(50", " 6152"] {
                assert!(!text.contains(number), "a line holds {number}: {text}");
            }
        }
    }

    /// The four advice lines, each against the rules.
    #[test]
    fn the_advice_obeys_its_rules() {
        let now = 1_240;
        let q = Target::Queue;
        let guarded = "qex resume queue --pause 7f3c9a1e";
        let exception = "unless you hold this id from your own `pause` answer or your user";

        // A request of this session: the guarded command, for the holder.
        let v = this_session();
        assert_eq!(v.issuer_session_state, IssuerState::Running);
        assert_eq!(v.issuer_is_this_session, Relation::Yes);
        assert_eq!(v.issuer_program.as_deref(), Some("claude"));
        let line = request_line(&v, q, now);
        assert!(
            line.contains("from THIS session (claude, still running)"),
            "{line}"
        );
        assert!(line.ends_with(guarded), "{line}");
        assert!(line.contains("not of a sibling agent"), "{line}");

        // A request of a different session that still runs: NO command.
        let v = another_session();
        assert_eq!(v.issuer_is_this_session, Relation::No);
        let line = request_line(&v, q, now);
        assert!(
            line.contains("from another session that is still running (claude)"),
            "{line}"
        );
        assert!(!line.contains("qex resume"), "{line}");
        assert!(line.contains(exception), "{line}");
        assert_eq!(resume_command(&v, q), None);

        // A request of a session that is gone: the guarded command.
        let v = gone_session();
        assert_eq!(v.issuer_session_state, IssuerState::Gone);
        assert_eq!(
            v.issuer_program.as_deref(),
            Some("claude"),
            "the name stays"
        );
        let line = request_line(&v, q, now);
        assert!(
            line.contains("from a session that is gone (claude)"),
            "{line}"
        );
        assert!(line.contains("expect nobody to resume it"), "{line}");
        assert!(line.ends_with(guarded), "{line}");
        let timed = PauseView {
            until: Some(4_000),
            ..v
        };
        let line = request_line(&timed, q, now);
        assert!(line.contains("It ends by itself at"), "{line}");
        assert!(line.contains("so waiting is the default"), "{line}");

        // Unknown: never more permissive than `running`, no command, and no
        // "this" or "another".
        let v = unknown_session();
        assert_eq!(v.issuer_session_state, IssuerState::Unknown);
        assert_eq!(v.issuer_is_this_session, Relation::Unknown);
        assert_eq!(v.issuer_program, None, "null exactly when unknown");
        let line = request_line(&v, q, now);
        assert!(line.contains("treat it as still there"), "{line}");
        assert!(!line.contains("qex resume"), "{line}");
        assert!(line.contains(exception), "{line}");
        assert!(
            !line.contains("THIS") && !line.contains("another"),
            "{line}"
        );

        // A session that runs, read by a command whose own chain is unknown:
        // no command, and no "this" or "another".
        let v = view(&from(agent(100, 50, 40)), None, true, &all);
        assert_eq!(v.issuer_is_this_session, Relation::Unknown);
        let line = request_line(&v, q, now);
        assert!(!line.contains("qex resume"), "{line}");
        assert!(
            !line.contains("THIS") && !line.contains("another"),
            "{line}"
        );
        assert!(line.contains(exception), "{line}");

        // No message names `--all`. Only the help does.
        for v in [
            this_session(),
            another_session(),
            gone_session(),
            unknown_session(),
        ] {
            assert!(!request_line(&v, q, now).contains("--all"));
        }
    }

    /// The text and the JSON agree on the command, and the default JSON holds
    /// no field whose name holds `pid`.
    #[test]
    fn the_text_and_the_json_agree_and_the_json_holds_no_pid() {
        for v in [
            this_session(),
            another_session(),
            gone_session(),
            unknown_session(),
        ] {
            let v = PauseView {
                caller_reported_pid: Some(1),
                ..v
            };
            for target in [Target::Queue, Target::Lock("gpu0")] {
                let json = view_json(&v, target, false);
                let line = request_line(&v, target, 1_240);
                match json["resume_command"].as_str() {
                    Some(command) => {
                        assert!(line.ends_with(command), "{line} / {command}");
                        assert!(command.contains("--pause 7f3c9a1e"), "{command}");
                    }
                    None => assert!(!line.contains("qex resume"), "{line}"),
                }
                assert_eq!(
                    json["issuer_program"].is_null(),
                    json["issuer_session_state"] == "unknown",
                    "the program is null exactly when the state is unknown"
                );
                let text = json.to_string();
                assert!(
                    !text.contains("pid"),
                    "the default JSON names a pid: {text}"
                );
                assert!(!text.contains("holder"), "{text}");

                // The forensic form names each number for what it is, and has
                // no field that is called `pid`.
                let forensic = view_json(&v, target, true);
                assert_eq!(forensic["caller_reported_pid"], 1);
                assert!(!forensic.to_string().contains("\"pid\""));
                if let Some(chain) = forensic["issuer_chain"].as_array() {
                    assert!(chain.iter().all(|p| p["host_pid"].is_number()));
                }
            }
        }
        assert_eq!(
            resume_command(&this_session(), Target::Lock("gpu0")).as_deref(),
            Some("qex resume lock gpu0 --pause 7f3c9a1e")
        );
    }

    /// A process of a session exists when its number AND its start time are
    /// those of the record.
    #[test]
    fn a_process_exists_by_its_number_and_its_start_time() {
        let me = std::process::id() as i32;
        let mut process = process(me, 1, "test", false);
        process.start = crate::sys::process_start_token(me);
        assert!(still_exists(&process), "this process exists");

        // The machine gave the number to a later process.
        process.start = process.start.map(|s| s + 1);
        assert!(!still_exists(&process), "the start time differs");

        // A record with no start time proves nothing.
        process.start = None;
        assert!(!still_exists(&process));
    }

    /// What counts as a process of a session.
    #[test]
    fn the_first_process_and_a_runtime_never_count() {
        // The multiplexer is the boundary, and it counts. The first process
        // of the machine is above it, and it never counts.
        let chain = agent(100, 50, 40);
        let names: Vec<&str> = session_processes(&chain)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["bash", "claude", "bash", "tmux_server"]);

        // A container: the shim is the boundary, and it does not count. The
        // agent that is the first process INSIDE the container is an ordinary
        // process of the machine, and it counts.
        let container = vec![
            process(900, 800, "bash", false),
            process(800, 700, "claude", false),
            process(700, 1, "containerd-shim", false),
            process(1, 0, "systemd", false),
        ];
        let names: Vec<&str> = session_processes(&container)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["bash", "claude"]);

        // Only the first process and a runtime: qex cannot say, and it never
        // says `running` for such a chain.
        let bare = vec![process(1, 0, "systemd", false)];
        assert!(session_processes(&bare).is_empty());
        let v = view(&from(bare), None, true, &all);
        assert_eq!(v.issuer_session_state, IssuerState::Unknown);

        // An agent that a service started is the boundary of its own chain
        // (its parent cannot be read). It counts, so its pause does not read
        // `gone` while the agent operates.
        let service = vec![
            process(100, 50, "bash", false),
            process(50, 40, "claude", false),
        ];
        let only_agent = |p: &Ancestor| p.pid == 50;
        let v = view(&from(service), None, true, &only_agent);
        assert_eq!(v.issuer_session_state, IssuerState::Running);
        assert_eq!(v.issuer_program.as_deref(), Some("claude"));
    }

    /// Two agents in two panes of one multiplexer share the multiplexer only.
    /// qex must not say `this session`, which prints a command that ends the
    /// request of the other agent, and it cannot say `another`.
    #[test]
    fn a_shared_boundary_is_not_a_shared_session() {
        let v = view(
            &from(agent(100, 50, 40)),
            Some(&agent(200, 60, 41)),
            true,
            &all,
        );
        assert_eq!(v.issuer_is_this_session, Relation::Unknown);
        assert_eq!(resume_command(&v, Target::Queue), None);
    }

    /// A record that was made in a different pid namespace holds numbers that
    /// this process cannot test.
    #[test]
    fn a_record_of_a_different_pid_namespace_is_unknown() {
        let v = view(
            &from(agent(100, 50, 40)),
            Some(&agent(101, 50, 40)),
            false,
            &all,
        );
        assert_eq!(v.issuer_session_state, IssuerState::Unknown);
        assert_eq!(v.issuer_is_this_session, Relation::Unknown);
        assert_eq!(v.issuer_program, None);
    }

    /// A process with no start time can never be shown to exist, so qex must
    /// not say that its session is gone.
    #[test]
    fn a_session_with_no_start_time_is_never_gone() {
        let mut chain = agent(100, 50, 40);
        for p in &mut chain {
            p.start = None;
        }
        let v = view(&from(chain), None, true, &none);
        assert_eq!(v.issuer_session_state, IssuerState::Unknown);
    }

    /// A file that qex cannot read must PAUSE the queue, and say why.
    ///
    /// # The fault that this test prevents
    ///
    /// A parse fault that gave "nothing is paused" would start the work while
    /// the person believes that the machine is quiet, with no line in any log
    /// and no word in any command. The two directions are not equal: a queue
    /// that qex holds by mistake costs latency and one command corrects it,
    /// and a queue that operates by mistake cannot be corrected after the
    /// work started.
    #[test]
    fn a_record_that_qex_cannot_read_holds_the_queue() {
        let paused =
            Paused::held_by_fault(std::path::Path::new("/x/paused.json"), "expected value");

        let record = paused.queue.first().expect("the queue must be paused");
        assert!(record.fault);
        assert_eq!(record.id, FAULT_ID, "the id is the same at each read");
        assert!(!record.expired(9_999_999), "such a pause has no end");

        // The words must say what happened, must not say that a person paused
        // the queue, and must give a remedy that WORKS: a resume with no id
        // removes nothing.
        let reason = queue_reason(&paused);
        assert!(reason.contains("could not read"), "got: {reason}");
        assert!(reason.contains("/x/paused.json"), "got: {reason}");
        assert!(
            reason.contains("qex resume queue --pause fault"),
            "got: {reason}"
        );

        let v = view(record, None, true, &all);
        let lines = queue_lines(&[v], paused.since(), record.paused_at);
        assert!(lines[1].contains("PAUSED BY A FAULT"), "got: {}", lines[1]);
        assert!(
            lines[1].ends_with("qex resume queue --pause fault"),
            "got: {}",
            lines[1]
        );

        // A real request replaces it: the file that this change writes is a
        // file that qex can read.
        //
        // The fault held the queue already, so the new request did not stop
        // it, and the queue stopped at the moment of the fault. A later
        // `since` would take the hours of the fault away from the time that
        // `--max-queue-time` gives back.
        let fault_at = record.paused_at;
        let mut paused = paused;
        let fault_since = paused.since();
        let mut later = super::PauseRecord::new(7, None, None);
        later.paused_at = fault_at + 3600;
        assert!(
            !paused.add_queue(later),
            "the queue was held already, so this request did not stop it"
        );
        assert_eq!(paused.queue.len(), 1);
        assert!(!paused.queue[0].fault);
        assert_eq!(paused.since(), fault_since);
        let (_, end) = paused.remove_queue(|_| true, fault_at + 7200);
        assert_eq!(end.map(|e| e.since), fault_since);
    }

    /// An id is unique across the queue and all the locks, so qex can say
    /// where a request stands. A resume that names the wrong target must get
    /// that place and the command, and never "does not stand".
    #[test]
    fn qex_knows_the_target_that_an_id_stands_for() {
        let mut paused = Paused::default();
        paused.add_queue(super::PauseRecord::new(7, None, None));
        paused.add_lock("gpu0", super::PauseRecord::new(7, None, None));
        let queue_id = paused.queue[0].id.clone();
        let lock_id = paused.locks["gpu0"][0].id.clone();

        assert_eq!(paused.home_of(&queue_id), Some(None));
        assert_eq!(paused.home_of(&lock_id), Some(Some("gpu0".to_string())));
        assert_eq!(paused.home_of("00000000"), None);

        let words = stands_elsewhere(&lock_id, Some("gpu0"));
        assert!(words.contains("stands for the lock `gpu0`"), "{words}");
        assert!(
            words.contains(&format!("`qex resume lock gpu0 --pause {lock_id}`")),
            "{words}"
        );
        assert!(!words.contains("does not stand"), "{words}");
        let words = stands_elsewhere(&queue_id, None);
        assert!(
            words.contains(&format!("`qex resume queue --pause {queue_id}`")),
            "{words}"
        );
    }

    /// An unknown field must not stop the file from parsing, and the file of
    /// an earlier version, with ONE record for the queue and one for each
    /// lock, must keep its pause.
    ///
    /// A parse that refused either one would turn the file into a fault, and
    /// an upgrade would change a pause of a person into a pause of nobody.
    /// An id that `read` gives to a record of the earlier shape must reach the
    /// file. `expire` is the step that tells the coordinator to write, and the
    /// number of requests does not change here, so the id itself must count.
    #[test]
    fn an_id_that_the_file_does_not_hold_is_a_change_to_write() {
        let text = r#"{"queue":{"paused_at":10,"by_pid":7,"reason":"a demo","until":null}}"#;
        let mut back: Paused = serde_json::from_str(text).expect("the file must parse");
        back.id_not_in_the_file = back.normalize();
        assert!(back.id_not_in_the_file, "the record had no id");
        let id = back.queue[0].id.clone();

        let first = back.expire(20);
        assert!(first.changed, "the caller must write the id that qex gave");
        assert_eq!(back.queue[0].id, id, "the id must not change again");

        let second = back.expire(30);
        assert!(!second.changed, "the file holds the id now");
    }

    #[test]
    fn the_file_of_an_earlier_version_keeps_its_pause() {
        let text = r#"{"queue":{"paused_at":10,"by_pid":7,"reason":"a demo","until":null,
                       "paused_by_user":"someone"},
                       "locks":{"gpu0":{"paused_at":20,"by_pid":7}},"maintenance":true}"#;
        let mut back: Paused = serde_json::from_str(text).expect("the file must parse");
        back.normalize();
        assert_eq!(back.queue.len(), 1);
        let record = &back.queue[0];
        assert_eq!(record.paused_at, 10);
        assert_eq!(record.reason.as_deref(), Some("a demo"));
        assert!(!record.fault);
        assert_eq!(
            record.id.len(),
            8,
            "an old record gets an id: {}",
            record.id
        );
        assert_eq!(back.since(), Some(10));
        assert_eq!(back.locks["gpu0"].len(), 1);
        assert_ne!(back.locks["gpu0"][0].id, record.id);

        // Nobody read the issuer of such a record.
        let v = view(record, None, true, &all);
        assert_eq!(v.issuer_session_state, IssuerState::Unknown);

        let none: Paused = serde_json::from_str(r#"{"queue":null,"locks":{}}"#).unwrap();
        assert!(none.is_empty());
    }

    /// The set must survive the JSON, with the chain of each issuer, or a
    /// restart of the coordinator loses a pause or the state of its issuer.
    #[test]
    fn the_requests_survive_the_json() {
        let mut p = Paused::default();
        p.add_queue(PauseRecord {
            reason: Some("recording a demo".into()),
            until: Some(2_000),
            issuer_ns: Some("pid:[4026531836]".into()),
            ..from(agent(100, 50, 40))
        });
        p.add_queue(record());
        p.add_lock("gpu0", record());

        let text = serde_json::to_string(&p).unwrap();
        let back: Paused = serde_json::from_str(&text).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.queue[0].issuer_chain, Some(agent(100, 50, 40)));
    }

    /// A CLI of an earlier version reads ONE record and prints its pid. The
    /// record that the coordinator gives it holds no pid.
    #[test]
    fn the_record_for_an_earlier_cli_holds_no_pid() {
        let timed = PauseRecord {
            paused_at: 900,
            until: Some(2_000),
            ..from(agent(100, 50, 40))
        };
        let one = for_an_earlier_cli(&[record(), timed.clone()]).unwrap();
        assert_eq!(one.by_pid, 0);
        assert_eq!(one.issuer_chain, None);
        assert_eq!(one.paused_at, 900, "the oldest request");
        assert_eq!(one.until, None, "one request has no end");
        assert_eq!(for_an_earlier_cli(&[timed]).unwrap().until, Some(2_000));
        assert!(for_an_earlier_cli(&[]).is_none());
    }
}
