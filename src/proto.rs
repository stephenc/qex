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
        cpu_claimed: u64,
        mem_claimed: u64,
        /// The pools of countable resources, and what holds them.
        ///
        /// THIS FIELD IS AN OPTION, AND NOT A DEFAULTED LIST. `None` means
        /// "this coordinator cannot say", and `qex info` writes `unknown` for
        /// it. A defaulted empty list would read as "this machine has no
        /// pool", which is a different statement and can be a lie.
        #[serde(default)]
        pools: Option<Vec<PoolReport>>,
    },
    /// The things that the coordinator can do.
    Capabilities { names: Vec<String> },
    /// The command failed.
    Error { message: String, kind: ErrorKind },
}

/// One pool of a countable resource, as `qex info` reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolReport {
    pub name: String,
    /// The number of units, or of devices.
    pub total: u64,
    /// The units that the jobs of this queue hold.
    pub used: u64,
    /// The units that the other users hold.
    pub peer_used: u64,
    /// The name of the quantity that each device holds, such as `vram`.
    #[serde(default)]
    pub size_name: Option<String>,
    /// The devices of an indexed pool. The list is empty for a plain pool.
    #[serde(default)]
    pub devices: Vec<DeviceReport>,
}

/// One device of an indexed pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceReport {
    pub index: u32,
    pub capacity: u64,
    /// The quantity that the jobs of this queue hold on this device.
    pub used: u64,
    /// True when another user holds this device.
    pub peer: bool,
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
