//! This module holds the job state that qex writes to the disk.
//!
//! The file `status.json` is the primary record of a job result. The supervisor
//! writes this file in one operation. The coordinator keeps a copy in memory
//! only. A coordinator that stops, restarts or fails thus loses no result.

use crate::spec::JobSpec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// qex accepted the job. The job waits for free capacity in the budget.
    Queued,
    /// qex started the supervisor. The supervisor starts the job process.
    Starting,
    Running,
    /// The job stopped with the exit code 0.
    Completed,
    /// The job stopped with an exit code that is not 0, or a signal stopped it.
    Failed,
    /// The command `qex kill` stopped the job.
    Killed,
    /// The job used more time than the `--timeout` value.
    Timeout,
    /// The out-of-memory killer or the cgroup limit stopped the job.
    ///
    /// This state is different from `failed`. The correction is also different:
    /// the memory claim is too small, or the machine is too small.
    Oom,
    /// qex removed the job from the queue before the job started.
    Cancelled,
    /// A job that this job needed did not succeed, so this job did not start.
    ///
    /// This state is different from `failed`. In a pipeline of six stages, one
    /// stage fails and the stages after it are `skipped`. A reader thus finds
    /// one failure only, and that failure is the cause. With the state `failed`
    /// for each stage, the reader must find the first failure without help.
    Skipped,
}

impl JobState {
    /// Tests if the job is in its final state.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Starting | Self::Running)
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Timeout => "timeout",
            Self::Oom => "oom",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for JobState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "queued" => Ok(Self::Queued),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "killed" => Ok(Self::Killed),
            "timeout" => Ok(Self::Timeout),
            "oom" => Ok(Self::Oom),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            "skipped" => Ok(Self::Skipped),
            other => Err(format!(
                "unknown job state `{other}`. Use one of these states: queued, starting, \
                 running, completed, failed, killed, timeout, oom, cancelled"
            )),
        }
    }
}

/// The resources that the job used. qex measures these values.
///
/// An agent uses these values to correct its estimates. `qex status` shows a
/// job that claimed 40GB and used 6GB. The agent needs no other tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// The maximum memory in bytes, from `getrusage(RUSAGE_CHILDREN)`.
    pub max_rss: u64,
    /// The CPU time in seconds for all the processes of the job.
    pub cpu_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: uuid::Uuid,
    pub name: String,
    /// The program and its arguments.
    ///
    /// A reader of the status can then see what the job ran. Without this
    /// field, a status shows the program name only, and a reader must open
    /// `spec.json` to learn the arguments.
    #[serde(default)]
    pub command: Vec<String>,
    /// The directory of the job.
    #[serde(default)]
    pub cwd: String,
    pub state: JobState,
    /// The pid of the job process. The value is `None` before the job starts.
    pub pid: Option<i32>,
    /// The pid of the supervisor of the job.
    ///
    /// A new coordinator reads this value to learn if a job continues. Without
    /// it, a coordinator that starts again cannot separate a live job from a
    /// job that stopped.
    #[serde(default)]
    pub supervisor_pid: Option<i32>,
    /// The exit code, if the job stopped without a signal.
    pub exit_code: Option<i32>,
    /// The signal that stopped the job, if a signal stopped it.
    pub signal: Option<i32>,
    pub submitted_at: u64,
    /// The position of this job in the order of submission.
    ///
    /// The time has a resolution of one second, and an agent submits the stages
    /// of a pipeline in the same second. Without this value, `qex list` shows
    /// those stages in an order that has no meaning to the reader.
    #[serde(default)]
    pub sequence: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub cpu: u64,
    pub mem: u64,
    pub usage: Usage,
    /// Shows that qex started the job although it is larger than the budget.
    ///
    /// qex starts such a job when no other job operates. A job that stays in
    /// the queue for ever gives no data to the agent.
    #[serde(default)]
    pub forced: bool,
    /// The reason for a forced start, in text for a person to read.
    #[serde(default)]
    pub forced_reason: Option<String>,
    /// The reason that the job stays in the queue.
    ///
    /// This text tells the reader what the job waits for. The reader does not
    /// calculate the budget. The value is `None` for a job that started.
    #[serde(default)]
    pub blocked_reason: Option<String>,
    /// The reason that a job failed, when qex itself gives the reason.
    ///
    /// A command that does not exist is the usual cause. This field is separate
    /// from `blocked_reason`, because a job that failed does not wait for
    /// anything, and a reader of `blocked_reason` expects a queue reason.
    #[serde(default)]
    pub error: Option<String>,
    /// The jobs that must succeed before this job starts.
    #[serde(default)]
    pub needs: Vec<uuid::Uuid>,
    /// The jobs that must stop before this job starts. Their result is not
    /// important.
    #[serde(default)]
    pub after: Vec<uuid::Uuid>,
    /// The job that caused this job to stop, for a job in the state `skipped`.
    ///
    /// This value names the first job that failed, and not the job before this
    /// one. In a pipeline `a -> b -> c -> d` where `a` fails, this field of `d`
    /// names `a`. A reader of the last job thus learns the true cause with one
    /// command, and does not follow the chain.
    #[serde(default)]
    pub caused_by: Option<uuid::Uuid>,
    pub tags: Vec<String>,
}

impl JobStatus {
    pub fn new(spec: &JobSpec) -> Self {
        Self {
            id: spec.id,
            name: spec.name.clone(),
            command: spec.command.clone(),
            cwd: spec.cwd.to_string_lossy().into_owned(),
            state: JobState::Queued,
            pid: None,
            supervisor_pid: None,
            exit_code: None,
            signal: None,
            submitted_at: spec.submitted_at,
            sequence: 0,
            started_at: None,
            finished_at: None,
            cpu: spec.cpu,
            mem: spec.mem,
            usage: Usage::default(),
            forced: false,
            forced_reason: None,
            blocked_reason: None,
            error: None,
            needs: spec.needs.clone(),
            after: spec.after.clone(),
            caused_by: None,
            tags: spec.tags.clone(),
        }
    }

    /// Gives the time that the job operated, or the time that it operates now.
    pub fn elapsed(&self) -> Option<std::time::Duration> {
        let start = self.started_at?;
        let end = self.finished_at.unwrap_or_else(crate::sys::now_secs);
        Some(std::time::Duration::from_secs(end.saturating_sub(start)))
    }
}

/// Writes a file in one operation.
///
/// This function writes a temporary file in the same directory. It then renames
/// the temporary file to the target name. A reader thus sees the old contents
/// or the new contents. A reader never sees a part of the new contents.
///
/// This behaviour is necessary because `qex wait` reads `status.json` directly
/// when the coordinator does not operate.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|f| f.to_str()).unwrap_or("f"),
        std::process::id()
    ));

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        // Write the data to the disk now. If the machine loses power during a
        // job, the status file must not be incomplete after the restart.
        f.sync_all().ok();
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} into place", tmp.display()))?;
    Ok(())
}

pub fn read_status(dir: &Path) -> Result<JobStatus> {
    let path = dir.join("status.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn write_status(dir: &Path, status: &JobStatus) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(status)?;
    write_atomic(&dir.join("status.json"), &bytes, 0o600)
}

pub fn read_spec(dir: &Path) -> Result<JobSpec> {
    let path = dir.join("spec.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Writes the job specification with mode `0600`.
///
/// A captured environment frequently contains secrets.
pub fn write_spec(dir: &Path, spec: &JobSpec) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(spec)?;
    write_atomic(&dir.join("spec.json"), &bytes, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn terminal_states_are_classified_correctly() {
        for s in [
            JobState::Completed,
            JobState::Failed,
            JobState::Killed,
            JobState::Timeout,
            JobState::Oom,
            JobState::Cancelled,
        ] {
            assert!(s.is_terminal(), "{s} should be terminal");
            assert!(!s.is_active(), "{s} should not be active");
        }
        for s in [JobState::Queued, JobState::Starting, JobState::Running] {
            assert!(!s.is_terminal(), "{s} should not be terminal");
        }
        assert!(JobState::Running.is_active());
        assert!(!JobState::Queued.is_active());
    }

    #[test]
    fn states_round_trip_through_strings() {
        use std::str::FromStr;
        for s in [
            JobState::Queued,
            JobState::Running,
            JobState::Completed,
            JobState::Oom,
            JobState::Cancelled,
        ] {
            assert_eq!(JobState::from_str(s.as_str()).unwrap(), s);
        }
        assert_eq!(JobState::from_str("canceled").unwrap(), JobState::Cancelled);
        assert!(JobState::from_str("wat").is_err());
    }

    #[test]
    fn atomic_write_applies_the_requested_mode() {
        let dir = std::env::temp_dir().join(format!("qex-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.json");
        write_atomic(&path, b"{}", 0o600).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "other users must not read a captured environment");

        // A second write must keep the same mode. It must not open the file.
        write_atomic(&path, b"{\"a\":1}", 0o600).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");

        // The function must delete each temporary file. A reader must not find
        // one of these files.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "atomic write left temp files behind");

        std::fs::remove_dir_all(&dir).ok();
    }
}
