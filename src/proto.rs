//! This module defines the messages between the CLI and the coordinator.
//!
//! Each message is one JSON object on one line. This format is simple to read
//! in a log file, and it needs no length field.

use crate::job::JobStatus;
use crate::spec::JobSpec;
use serde::{Deserialize, Serialize};

/// A message from the CLI to the coordinator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Tests that the coordinator operates.
    Ping,
    /// Puts a job in the queue.
    Submit { spec: Box<JobSpec> },
    /// Gives the state of every job.
    List,
    /// Gives the state of one job.
    Status { id: uuid::Uuid },
    /// Waits until a job reaches a final state.
    ///
    /// The coordinator does not answer until the job stops. The CLI thus does
    /// not poll, and it uses no CPU time while it waits.
    Wait { id: uuid::Uuid },
    /// Stops a job that operates.
    Kill {
        id: uuid::Uuid,
        signal: i32,
        grace_secs: u64,
    },
    /// Removes a job from the queue.
    Cancel { id: uuid::Uuid },
    /// Deletes the record of a job that stopped.
    Clean { id: uuid::Uuid },
    /// Gives the state of the coordinator.
    Info,
    /// Gives the list of the things that the coordinator can do.
    ///
    /// A CLI sends this request only to a coordinator that is new enough to
    /// answer it. See the `capabilities` module for the reason.
    Capabilities,
    /// Opens the event stream.
    ///
    /// This request is the one request that gives MANY answers. The coordinator
    /// writes one `Event` response for each change, until the reader closes the
    /// connection or the coordinator stops. The connection carries no other
    /// request after this one.
    ///
    /// The CLI sends this request only to a coordinator that says `events` in
    /// its capabilities. An earlier coordinator cannot read the name `events`,
    /// so it answers with an error, and it does not accept the request in
    /// silence.
    Events { since: crate::events::Cursor },
    /// Stops the queue from starting work, or takes a lock for the person.
    ///
    /// A CLI sends this request only to a coordinator that gives the capability
    /// `pause`. An earlier coordinator would start the jobs of the queue while
    /// the person believes that the machine is quiet.
    Pause {
        target: PauseTarget,
        /// The text of `--reason`, for the person who reads the queue later.
        reason: Option<String>,
        /// The moment when the pause ends by itself, in seconds since the epoch.
        until: Option<u64>,
        /// The process that asked. The CLI writes its own process id here.
        ///
        /// The coordinator cannot learn this value from the socket, and a
        /// record that named the coordinator would name the same process for
        /// every pause and would explain nothing.
        #[serde(default)]
        by_pid: i32,
    },
    /// Starts the queue again, or gives a lock back.
    Resume { target: PauseTarget },
    /// Gives what is paused now.
    PauseState,
}

/// What a `Pause` or a `Resume` request acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PauseTarget {
    /// The whole queue. A paused queue starts no job.
    Queue,
    /// One named lock. The person holds it, in place of a job.
    Lock { name: String },
}

/// One lock that a person holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockPause {
    pub name: String,
    pub record: crate::pause::PauseRecord,
    /// The job that still holds the lock, as `a1b2c3d4 (train)`.
    ///
    /// The person receives the lock when that job stops. No other job takes it
    /// in the time between.
    pub held_by: Option<String>,
}

/// What the coordinator measured about the health of the queue.
///
/// The scheduler makes these values in one pass, so they name the same moment
/// and the same numbers that the scheduler used for its decision. A reader that
/// asked the machine again would get a different moment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueHealth {
    /// The time when a job last started. `None` means that no job started
    /// since this coordinator started.
    pub last_start_at: Option<u64>,
    /// The number of other coordinators that hold capacity.
    pub peer_count: usize,
    /// The cores that the other users hold.
    pub peer_cpu: u64,
    /// The memory that the other users hold.
    pub peer_mem: u64,
    /// The job at the front of the queue that cannot start, as
    /// `a1b2c3d4 (train)`. `None` means that no job waits.
    pub head_job: Option<String>,
    /// Who holds the capacity that the job at the front needs. See
    /// `sched::Blocker`.
    pub head_blocker: Option<String>,
    /// The number of jobs that started after the job at the front reached the
    /// front. `None` means that no job is at the front.
    pub head_passed_by: Option<u32>,
}

/// A message from the coordinator to the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// The command succeeded and gives no data.
    Ok,
    /// The coordinator accepted the job.
    Submitted {
        id: uuid::Uuid,
        /// A message for the user, if the job needs one.
        ///
        /// The CLI writes this text to stderr. The job id stays alone on
        /// stdout, so `ID=$(qex submit ...)` continues to operate.
        warning: Option<String>,
        /// True when a dedupe key gave a job that already existed.
        ///
        /// The id above is then the id of that job, and this submission
        /// started nothing. A caller that must know if IT started the work
        /// reads this field with `qex submit --json`.
        ///
        /// An earlier coordinator does not write this field. `default` gives
        /// `false` there, which is the truth for a coordinator that has no
        /// dedupe keys.
        #[serde(default)]
        deduplicated: bool,
    },
    /// The state of many jobs.
    Jobs { jobs: Vec<JobStatus> },
    /// The state of one job.
    Status { status: Box<JobStatus> },
    /// The state of the coordinator.
    Info {
        pid: i32,
        version: String,
        /// The time when this coordinator started.
        #[serde(default)]
        started_at: u64,
        /// True when something replaced the program file of the coordinator.
        ///
        /// The coordinator then holds code that is not the code of the program
        /// on the disk. It stops when no job operates, and the next command
        /// starts a coordinator with the new program.
        #[serde(default)]
        program_replaced: bool,
        jobs_running: usize,
        jobs_queued: usize,
        cpu_budget: u64,
        mem_budget: u64,
        /// The fault in the configuration file, if the coordinator met one.
        ///
        /// The coordinator keeps the values that it had, so a reader must be
        /// told that the file and the coordinator no longer agree.
        #[serde(default)]
        config_error: Option<String>,
        cpu_claimed: u64,
        mem_claimed: u64,
        /// The health of the queue, in one word for a program to read.
        ///
        /// The words are `running`, `paused`, `paused-by-fault`, `held`,
        /// `waits-for-peer`, `waits-for-machine`, `waits-for-capacity`,
        /// `waits-for-idle` and `parked`. A pause comes before every word about
        /// the front of the queue, because a paused queue starts no job at all.
        ///
        /// # Why this field is an Option, and why it holds a string
        ///
        /// An Option, because a defaulted `false` from an earlier coordinator
        /// would read as "the queue operates" — a lie, in the one place where
        /// the honest answer matters most. `None` means "this coordinator does
        /// not say", and every command prints `unknown` for it.
        ///
        /// A string, because a later version adds more states to this field. A
        /// CLI that met an unknown name of a Rust enum would refuse the whole
        /// answer, and `qex info` would then give nothing at all.
        #[serde(default)]
        queue_state: Option<String>,
        /// The moment when a person paused the queue.
        #[serde(default)]
        paused_at: Option<u64>,
        /// The pid of the process that asked for the pause.
        ///
        /// A queue is shared, so a report of a pause must say WHO. Without this
        /// field the two readers of this answer had to invent a pid: `qex top`
        /// gave 0, and `qex info` gave the pid of the COORDINATOR. The second
        /// one is the dangerous invention — a person who reads it and runs
        /// `kill <pid>` stops the coordinator, which is the one process that
        /// must not be stopped to end a pause.
        ///
        /// `None` means that this coordinator does not say. Every command
        /// prints `unknown` for it, and never a number.
        #[serde(default)]
        paused_by_pid: Option<i32>,
        /// The text that the person gave with `--reason`.
        #[serde(default)]
        paused_reason: Option<String>,
        /// The moment when the pause ends by itself. `None` means no end.
        #[serde(default)]
        paused_until: Option<u64>,
        /// The locks that a person holds.
        #[serde(default)]
        paused_locks: Option<Vec<LockPause>>,
        /// What the coordinator measured about the health of the queue.
        ///
        /// THIS FIELD IS AN OPTION, AND THAT IS DELIBERATE.
        ///
        /// A newer CLI can talk to an older coordinator, which does not measure
        /// any of it. A defaulted `0` in `peer_cpu` would then say "no other
        /// user holds anything", which is a lie in the place where the true
        /// answer is the most valuable. `None` says "this coordinator does not
        /// measure the health", and every command prints `unknown`.
        ///
        /// One option holds them all, so a reader tests ONE field. Seven
        /// parallel options would let a later change fill three of them and
        /// leave four, and no reader could say what that state means.
        #[serde(default)]
        health: Option<Box<QueueHealth>>,
    },
    /// The things that the coordinator can do.
    Capabilities { names: Vec<String> },
    /// One line of the event stream.
    ///
    /// The coordinator sends many of these for one `Events` request. Each one
    /// holds one event, and the CLI writes that event alone on one line.
    Event { event: Box<crate::events::Event> },
    /// What is paused now.
    PauseState {
        queue: Option<crate::pause::PauseRecord>,
        locks: Vec<LockPause>,
    },
    /// The command failed.
    Error { message: String, kind: ErrorKind },
}

/// The type of a failure. The CLI maps this value to an exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// There is no job with that id.
    NoSuchJob,
    /// The job is in a state that does not accept this command.
    WrongState,
    /// The coordinator could not do the work.
    Internal,
}

impl Response {
    pub fn error(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_message_survives_one_line_of_json() {
        let id = uuid::Uuid::new_v4();
        let requests = [
            Request::Ping,
            Request::List,
            Request::Status { id },
            Request::Wait { id },
            Request::Kill {
                id,
                signal: 15,
                grace_secs: 10,
            },
            Request::Cancel { id },
            Request::Clean { id },
            Request::Info,
            Request::Capabilities,
            Request::Events {
                since: crate::events::Cursor::After {
                    seq: 7,
                    stream: Some(id),
                },
            },
            Request::Pause {
                target: PauseTarget::Queue,
                reason: Some("recording a demo".into()),
                until: Some(1_700_000_000),
                by_pid: 4321,
            },
            Request::Pause {
                target: PauseTarget::Lock {
                    name: "gpu0".into(),
                },
                reason: None,
                until: None,
                by_pid: 4321,
            },
            Request::Resume {
                target: PauseTarget::Queue,
            },
            Request::PauseState,
        ];
        for r in requests {
            let line = serde_json::to_string(&r).unwrap();
            assert!(!line.contains('\n'), "a message must fit one line: {line}");
            let back: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(
                serde_json::to_string(&back).unwrap(),
                line,
                "the message changed after a round trip"
            );
        }
    }

    #[test]
    fn an_error_response_keeps_its_kind() {
        let r = Response::error(ErrorKind::NoSuchJob, "there is no job with that id");
        let line = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&line).unwrap();
        match back {
            Response::Error { kind, message } => {
                assert_eq!(kind, ErrorKind::NoSuchJob);
                assert!(message.contains("no job"));
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    /// A newer CLI can talk to an older coordinator. An unknown message must
    /// give a clear error and must not stop the coordinator.
    #[test]
    fn an_unknown_message_is_refused_and_not_accepted() {
        let err = serde_json::from_str::<Request>(r#"{"op":"explode"}"#);
        assert!(err.is_err(), "the parser must refuse an unknown operation");
    }
}
