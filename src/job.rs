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

/// Gives the SAFE FORM of a job name: the ONLY form that qex shows.
///
/// A safe name holds the letters `A` to `Z` and `a` to `z`, the numbers `0` to
/// `9`, and the three characters `-`, `_` and `.`. Every other character
/// becomes `_`, and a run of them becomes ONE `_`. A name that starts with `-`
/// loses that first character to `_`, because a word that starts with `-` has
/// the form of an option. The result stops at 128 characters.
///
///     deploy prod$(id)   ->  deploy_prod_id_
///     -version           ->  _version
///
/// # Where this is used
///
/// **Every place that qex puts a name in front of a reader**: `qex list`, `qex
/// status`, `qex top`, `qex du`, `qex gc`, the sentence that says why a job
/// waits, the log of the coordinator, the completions, and the JSON of each of
/// them. The record on the disk keeps the name that the user gave.
///
/// A machine that reads the JSON renders what qex gives it, and it knows no
/// more than qex does, so the JSON holds the safe form as well.
///
/// # Why
///
/// A name is text that another agent chose. A name that holds an ESC byte,
/// written to a terminal by `qex list`, moves the cursor and writes over the
/// text around it. No shell and no TAB are needed for that. A name that holds a
/// space or a `;` teaches a word that the reader cannot paste back.
///
/// A record on the disk is not a promise about its content. qex wrote records
/// before this rule, and one of them can hold such a name. NOTHING is carried
/// over: the safe form comes from the name that the record holds, so the rule
/// reaches every record at once and `qex gc` is not the thing that applies it.
///
/// The job stays reachable, because `resolve_id` finds a job by its safe name
/// as well as by the name that the record holds. A safe name that `qex list`
/// shows thus goes back into `qex status` as it stands.
///
/// **This is not a reason to write a word to a command line as it stands.**
/// Each shell still makes the word safe at the point of use. The answer of `qex
/// __complete` is text that came off a disk, and it is not a guarantee. The two
/// work together, and one does not replace the other.
pub fn safe_name(name: &str) -> String {
    let mut out = String::new();
    let mut replaced = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
            replaced = false;
        } else if !replaced {
            out.push('_');
            replaced = true;
        }
    }
    if out.starts_with('-') {
        out.replace_range(0..1, "_");
    }
    // Every character above is ASCII, so 128 characters are 128 bytes and this
    // cuts on a character boundary.
    out.truncate(128);
    out
}

/// What qex removed from the output files of a job.
///
/// The output of a job has a limit, so `qex status` and `qex logs` must be able
/// to say that a file is not the whole output. A reader who believes that a
/// truncated file is complete reads a wrong conclusion from it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsDropped {
    /// The bytes that qex did not keep from `stdout.log`.
    #[serde(default)]
    pub stdout_bytes: u64,
    /// The lines that qex did not keep from `stdout.log`.
    #[serde(default)]
    pub stdout_lines: u64,
    /// The bytes that qex did not keep from `stderr.log`.
    #[serde(default)]
    pub stderr_bytes: u64,
    /// The lines that qex did not keep from `stderr.log`.
    #[serde(default)]
    pub stderr_lines: u64,
    /// The limit that operated, from `[logs] max_bytes`.
    #[serde(default)]
    pub limit: u64,
    /// True when qex could not complete a log file, and could not count what
    /// went.
    ///
    /// A process of the job can hold the output open after the job stops. qex
    /// then writes the record and leaves the copy. The counts above are the
    /// counts that arrived, and they are not the full quantity. Without this
    /// field, the record of that job says that the files are complete, and a
    /// reader that reads only the record believes it.
    #[serde(default)]
    pub incomplete: bool,
}

impl LogsDropped {
    /// Gives the bytes and the lines of one stream.
    ///
    /// The name is `stdout` or `stderr`, as the commands use it.
    pub fn of(&self, stream: &str) -> Option<(u64, u64)> {
        let (bytes, lines) = match stream {
            "stdout" => (self.stdout_bytes, self.stdout_lines),
            "stderr" => (self.stderr_bytes, self.stderr_lines),
            _ => return None,
        };
        (bytes > 0).then_some((bytes, lines))
    }

    /// Tests if qex removed output, or could not complete a file.
    pub fn any(&self) -> bool {
        self.stdout_bytes > 0 || self.stderr_bytes > 0 || self.incomplete
    }
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
    /// The pid of the job process WHILE THE JOB OPERATES.
    ///
    /// The value is `None` before the job starts, and `None` again after the job
    /// stops. See `last_pid` for the historical value.
    ///
    /// A pid is not a name. The machine gives the same number to a new process
    /// soon after the earlier process stops. A pid in the record of a job that
    /// stopped would thus point at a process that has no connection with the
    /// job, and a reader that sends a signal to that number stops the work of
    /// somebody else. `pid` therefore answers one question only: which process
    /// is this job, now. A reader can act on it, because a value that exists
    /// means that the job operates.
    pub pid: Option<i32>,
    /// The pid that the job HAD, kept after the job stops.
    ///
    /// This value is for a person who reads a log of the machine. NEVER SEND A
    /// SIGNAL TO IT, and never look for it in the process list: the machine
    /// gives that number to another process later.
    #[serde(default)]
    pub last_pid: Option<i32>,
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
    /// Where the claim came from: `explicit`, `learned` or `default`.
    ///
    /// A reader can then see that a claim came from a measurement, and it does
    /// not look like a value that the agent chose.
    #[serde(default)]
    pub claim_source: String,
    /// The pipeline that this job belongs to.
    #[serde(default)]
    pub group: Option<uuid::Uuid>,
    /// The name of that pipeline, for a person to read.
    #[serde(default)]
    pub group_name: Option<String>,
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
    /// The locks that this job holds while it operates.
    #[serde(default)]
    pub locks: Vec<String>,
    /// The key that made this submission idempotent, if the user gave one.
    ///
    /// THIS FIELD IS FOR A READER, AND NOT FOR THE COORDINATOR. `qex list` and
    /// `qex status` show it, and the JSON schema names it.
    ///
    /// A coordinator that starts again reads the key from `spec.json`, and not
    /// from here. See `recover` in the `daemon` module, which reads
    /// `spec.dedupe_key`. DO NOT DELETE `dedupe_key` FROM `JobSpec`: a restart
    /// would then free every key, and the next submission would start a second
    /// copy of work that operates.
    #[serde(default)]
    pub dedupe_key: Option<String>,
    /// The number of times that qex started this job.
    ///
    /// The value is more than 1 when the job failed and `--retries` let qex
    /// start it again.
    #[serde(default)]
    pub attempts: u32,
    /// The number of times that qex may still start this job again.
    #[serde(default)]
    pub retries_left: u32,
    /// The job that caused this job to stop, for a job in the state `skipped`.
    ///
    /// This value names the first job that failed, and not the job before this
    /// one. In a pipeline `a -> b -> c -> d` where `a` fails, this field of `d`
    /// names `a`. A reader of the last job thus learns the true cause with one
    /// command, and does not follow the chain.
    #[serde(default)]
    pub caused_by: Option<uuid::Uuid>,
    /// What qex removed from the output files, when the job wrote more than
    /// `[logs] max_bytes`. The value is `None` when qex kept everything.
    #[serde(default)]
    pub logs_dropped: Option<LogsDropped>,
    pub tags: Vec<String>,
}

impl JobStatus {
    /// The name that qex SHOWS. See `safe_name`.
    ///
    /// Read the field `name` only to write the record, or to find a job by the
    /// name that the user gave.
    pub fn display_name(&self) -> String {
        safe_name(&self.name)
    }

    pub fn new(spec: &JobSpec) -> Self {
        Self {
            id: spec.id,
            name: spec.name.clone(),
            command: spec.command.clone(),
            cwd: spec.cwd.to_string_lossy().into_owned(),
            state: JobState::Queued,
            pid: None,
            last_pid: None,
            supervisor_pid: None,
            exit_code: None,
            signal: None,
            submitted_at: spec.submitted_at,
            sequence: 0,
            started_at: None,
            finished_at: None,
            cpu: spec.cpu,
            mem: spec.mem,
            claim_source: spec.claim_source.clone(),
            group: spec.group,
            group_name: spec.group_name.clone(),
            usage: Usage::default(),
            forced: false,
            forced_reason: None,
            blocked_reason: None,
            error: None,
            needs: spec.needs.clone(),
            after: spec.after.clone(),
            locks: spec.locks.clone(),
            dedupe_key: spec.dedupe_key.clone(),
            attempts: 0,
            retries_left: spec.retries,
            caused_by: None,
            logs_dropped: None,
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
///
/// # The three steps that make the record durable
///
/// qex says that a job keeps its result when the coordinator stops and when the
/// machine loses power. Three steps together give that, and two of the three are
/// easy to forget:
///
/// 1. `sync_all` on the file, so that the contents reach the disk.
/// 2. `rename`, which is one operation. A reader sees the old file or the new
///    file, and never a part of either.
/// 3. `sync_all` on the DIRECTORY, so that the name reaches the disk as well.
///    Without this step the contents can be durable while the name is not, and
///    a machine that loses power can start with the earlier file, or with no
///    file at all.
///
/// An error in any of these steps is an error of this function. A record that
/// qex could not write is not a record, and a caller must hear that.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    // The name of the temporary file must belong to this writer alone. The pid
    // separates the processes, and the counter separates the threads of one
    // process. `create_new` then proves it: an open that finds an existing file
    // gives an error, and no two writers can share one temporary file.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().and_then(|f| f.to_str()).unwrap_or("f"),
        std::process::id(),
        unique
    ));

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing {}", tmp.display()))?;
        // Step 1. Write the data to the disk now. If the machine loses power
        // during a job, the status file must not be incomplete after the
        // restart.
        f.sync_all()
            .with_context(|| format!("writing {} to the disk", tmp.display()))?;
    }

    // Step 2.
    if let Err(e) = std::fs::rename(&tmp, path) {
        // The temporary file must not stay behind. A directory that fills with
        // these files holds space that `qex du` cannot explain.
        std::fs::remove_file(&tmp).ok();
        return Err(e).with_context(|| format!("renaming {} into place", tmp.display()));
    }

    // Step 3. Some systems give an error for a sync of a directory. That is not
    // a failure of the write, and the file is in place, so this step gives no
    // error to the caller.
    if let Ok(handle) = std::fs::File::open(dir) {
        handle.sync_all().ok();
    }
    Ok(())
}

pub fn read_status(dir: &Path) -> Result<JobStatus> {
    let path = dir.join("status.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn write_status(dir: &Path, status: &JobStatus) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(status)?;
    write_atomic(&dir.join("status.json"), &bytes, 0o600)
}

/// Reads the record of every job from the state directory.
///
/// A command that watches the queue uses this function when no coordinator
/// operates. The supervisor of each job writes its own record, so these files
/// hold the truth whether a coordinator operates or not.
pub fn read_all_from_disk() -> Vec<JobStatus> {
    let Ok(dir) = crate::paths::jobs_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut jobs: Vec<JobStatus> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| read_status(&e.path()).ok())
        .collect();

    jobs.sort_by_key(|j| (j.submitted_at, j.sequence));
    jobs
}

pub fn read_spec(dir: &Path) -> Result<JobSpec> {
    let path = dir.join("spec.json");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
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
        assert_eq!(
            mode, 0o600,
            "other users must not read a captured environment"
        );

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
