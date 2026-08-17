//! This module holds the coordinator.
//!
//! The coordinator keeps the queue, starts each job when the machine has
//! capacity, and answers the CLI. It uses threads and a mutex. It does not use
//! an async runtime, because the number of jobs is small.
//!
//! The coordinator is not the owner of a job result. The supervisor of each job
//! writes `status.json`. The coordinator keeps a copy in memory only. A
//! coordinator that stops thus loses no result.

use crate::config::{Config, ConfigFile};
use crate::job::{self, JobState, JobStatus};
use crate::paths;
use crate::proto::{ErrorKind, Request, Response};
use crate::spec::JobSpec;
use crate::sys;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The time that the coordinator stays after the last job and the last command.
const IDLE_EXIT: Duration = Duration::from_secs(3600);

/// The name of the variable that changes the idle time. The tests use it.
const IDLE_EXIT_VAR: &str = "QEX_IDLE_EXIT_SECS";

/// The time that this process waits for a socket file that is already present.
///
/// The limit must exist: a connect without a limit waits for ever for a socket
/// that a stuck process holds, and this test comes before the socket of this
/// coordinator exists. Every qex command on the machine then waits, and each
/// one reports that the coordinator did not start.
///
/// The length of this limit is safe, because the limit gives the answer
/// `Unknown`, and this process stops on that answer. A refusal is the one
/// answer that lets this process delete the socket file.
const OWN_SOCKET_ANSWER_LIMIT: Duration = Duration::from_secs(1);

/// One job, as the coordinator holds it.
pub struct Job {
    pub spec: JobSpec,
    pub status: JobStatus,
    /// The process id of the supervisor, while the job operates.
    pub supervisor_pid: Option<i32>,
}

/// The data of the coordinator.
pub struct State {
    pub cfg: Config,
    pub jobs: BTreeMap<uuid::Uuid, Job>,
    /// The order of the queue. The scheduler reads this list.
    pub queue: Vec<uuid::Uuid>,
    /// The job that holds each dedupe key.
    ///
    /// The coordinator is the single writer of this map, and it reads the map
    /// and writes it in ONE lock operation. Two agents that run the same script
    /// in the same moment thus cannot both find the key free: the first one
    /// puts its id here before it gives the lock back, and the second one finds
    /// that id.
    ///
    /// A test that read the job list, gave the lock back, and then inserted
    /// would leave a gap between the two steps. Two submissions in that gap
    /// would both start a job, which is the fault that the key removes.
    pub dedupe: BTreeMap<String, uuid::Uuid>,
    /// The time of the last command from a CLI process.
    pub last_contact: Instant,
    /// The time when the queue became empty, for the oversized job rule.
    pub idle_since: Option<Instant>,
    /// The number for the next job, to keep the order of submission.
    pub next_sequence: u64,
    /// The time when this coordinator started.
    pub started_at: u64,
    /// A number made from the BYTES of the configuration file that this
    /// coordinator holds.
    ///
    /// The coordinator reads the file again when this value changes, so an
    /// edit reaches a coordinator that already operates.
    pub config_seen: u64,
    /// A number that the file gave, and the time when it FIRST gave it.
    ///
    /// The coordinator takes a change only after every look at the file gave
    /// this same number for `CONFIG_SETTLE`. See `reload_config` for the fault
    /// that this stops, and for the limit of a guard that looks.
    pub config_settling: Option<(u64, Instant)>,
    /// The fault in the configuration file, if the last read gave one.
    ///
    /// The coordinator keeps the values that it had. A file that qex cannot
    /// read must not become the DEFAULT values in silence: that would turn a
    /// budget of 2 cores into a budget of 12 with no word to anybody.
    pub config_error: Option<String>,
    /// The events that the coordinator holds for the readers of the stream.
    ///
    /// The log is behind the same lock as the jobs. The stream and `qex list`
    /// thus read one map, and the two can never disagree.
    pub events: crate::events::EventLog,
    /// What a person paused. The file `paused.json` holds the same values.
    pub paused: crate::pause::Paused,
    /// The time when a job last started, in seconds after the Unix epoch.
    pub last_start_at: Option<u64>,
    /// What the last scheduler pass found at the front of the queue.
    pub head: Option<HeadInfo>,
    /// The claims of the other users, from the last scheduler pass.
    ///
    /// `qex info` reads this copy and does not read the files of the other
    /// users again. The answer then names the same numbers that the scheduler
    /// used for its decision.
    pub peer_claims: crate::peers::Claims,
    pub stop: bool,
}

/// The job at the front of the queue that cannot start, for `qex info`.
#[derive(Debug, Clone)]
pub struct HeadInfo {
    pub id: uuid::Uuid,
    /// The SAFE name. See `job::safe_name`.
    pub name: String,
    /// The word for the class of the cause. See `sched::Blocker`.
    pub blocker: String,
    /// True when qex keeps the capacity for this job and starts nothing else.
    pub reserved: bool,
    /// The number of jobs that started after this job reached the front.
    pub passed_by: u32,
}

/// How long every look at the configuration file must give the same content
/// before qex takes it.
///
/// This is a TIME, and not a count of turns of the scheduler. It is also not a
/// promise that the file held that content for the whole of it. See
/// `reload_config` for the measurement that made it a time, and for the limit.
pub const CONFIG_SETTLE: Duration = Duration::from_millis(500);

/// Gives a short number for what one look at the file gave.
///
/// THE CONTENT, AND NOT THE TIME OF THE FILE.
///
/// Linux takes the time of a file from a coarse clock, with the granularity of
/// one tick: 4 milliseconds on a usual machine. Two writes inside one tick give
/// a file the SAME time, so a test of the time misses the second write — and it
/// misses it for ever, because nothing later changes that value. A number made
/// from the bytes has no such window.
///
/// The first byte separates the four answers, so that a file that goes away
/// and a file with content give different numbers.
///
/// This is FNV-1a, which qex uses in the other places that need a short name
/// for a long value.
fn config_fingerprint(read: &ConfigFile) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |byte: u8| {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    match read {
        ConfigFile::Missing => eat(0),
        ConfigFile::NotRegular => eat(1),
        ConfigFile::Unreadable(_) => eat(3),
        ConfigFile::Text(bytes, _) => {
            eat(2);
            for byte in bytes {
                eat(*byte);
            }
        }
    }
    hash
}

/// Reads the configuration file again when somebody changed it.
///
/// # The fault that this removes
///
/// The coordinator read the file one time, at its start, and it operates for
/// hours. A user who changed `[budget] cpu` then saw `qex config show` report
/// the NEW value, because that command reads the file, and `qex info` report
/// the OLD one, because that command asks the coordinator. The two commands of
/// qex disagreed about the budget of qex, and neither said that one of them was
/// old.
///
/// The values that this changes are the budget, the reserve and the rules of
/// the queue. They apply to the jobs that START after the change; a job that
/// operates keeps the claim that it made.
///
/// # A file that a writer has not finished
///
/// The caller gives the bytes. This function takes the values from THOSE
/// bytes, and it does not read the file again: a second read can give
/// different bytes, and the coordinator would then hold values that do not
/// belong to the number in `config_seen`.
///
/// THIS FUNCTION LOOKS AT THE FILE, AND IT DOES NOT WATCH IT. The scheduler
/// looks about ten times in `CONFIG_SETTLE`, and this function takes the
/// content when every look gave the same content. Read the limit of that below
/// before you write a sentence about it. A write that does not replace the file
/// in one step — a shell `>`
/// and a redirect, a program that writes one line at a time — leaves a file
/// that stops in the middle, and A FILE THAT STOPS IN THE MIDDLE IS STILL VALID
/// TOML. It parses and it validates, and it is wrong in two ways:
///
/// 1. Every key that the writer did not reach yet takes its DEFAULT value. A
///    review measured a budget of 2 cores that became 12, and 10 jobs that
///    started together in place of 2.
/// 2. A stop in the MIDDLE OF A LINE gives a wrong value that is not a default
///    value. A review measured a file that was becoming `cpu = 16` and that qex
///    read as `cpu = 1`.
///
/// Both said nothing, because qex CAN read such a file.
///
/// # The measurement that made this a time and not a count of turns
///
/// This test counted two TURNS of the scheduler before it counted time, and a
/// turn is not half a second. `sched::run` waits on a condition variable with a
/// timeout of 500ms, and every request thread calls `Coordinator::notify`, so a
/// coordinator with work in the queue turns far faster than the timeout. A
/// measurement with a mark on each turn gave a median gap of 500.7ms with
/// nothing to do and 17.0ms with a loop of `qex submit` running, with a minimum
/// of 1.2ms. A partial write with a pause of 300ms in the middle then installed
/// the DEFAULT budget in 3 trials of 3. The count of turns gave a guard about
/// thirty times shorter than the words promised.
///
/// # THE LIMIT OF A LOOK, WHICH NO NUMBER OF LOOKS REMOVES
///
/// A count of looks is not a promise about the time between them. A writer that
/// puts two whole files at the path in turn, in step with the looks, gives
/// every look the same content while the file was never that content for more
/// than one period. Measured on this branch: a writer that changed the file
/// every 25ms with a rename made the coordinator take a half-written file in 3
/// trials of 5, on a coordinator with nothing to do.
///
/// The first form of this guard looked every 500ms, and a writer with a period
/// near one second walked through it; `a_file_that_goes_back_and_forth_does_not
/// _change_the_budget` holds that case. The 50ms look moved the hole from a
/// period near a second to a period near a tenth of one. IT DID NOT CLOSE IT,
/// AND NO FIXED PERIOD CAN: a sampler always has a frequency that walks past
/// it. Only a message from the file system closes it, and this branch removed
/// that dependency on purpose.
///
/// This is a good trade, and the words must say which trade it is. A writer
/// that changes the file with that regularity, for long enough, is not a shell
/// `>` and is not an editor: those write the file one time. A shell loop with
/// its usual jitter could not do it in 5 trials of 5; only a writer with an
/// exact period could. So: SAY "ABOUT TEN LOOKS, AND EVERY LOOK GAVE THE SAME
/// CONTENT". NEVER SAY "UNCHANGED FOR HALF A SECOND".
///
/// A file that STAYS half-written is a different thing again, and this function
/// takes it. A file that gives the same content at every look for
/// `CONFIG_SETTLE` is the configuration, whatever the user meant.
/// What `config_error` says while a writer holds the configuration file.
///
/// It is a WAIT, and not a fault of the file: the values that the coordinator
/// holds are still the values of the last complete file.
pub(crate) const WAITING_FOR_A_WRITER: &str =
    "a writer changes the configuration file now, so qex waits for it. The coordinator keeps \
     the values that it had.";

pub fn reload_config(state: &mut State, read: ConfigFile) {
    let now = config_fingerprint(&read);
    if now == state.config_seen {
        state.config_settling = None;
        // The file gives what the coordinator holds, so no writer is in the
        // middle of it now. A message about a wait must not stay after the
        // wait ends: a writer that stops with the content that qex already has
        // would otherwise leave that sentence in `qex info` for ever.
        if state.config_error.as_deref() == Some(WAITING_FOR_A_WRITER) {
            state.config_error = None;
        }
        return;
    }

    // THE AGE OF THE FILE DECIDES, AND NOT THE RATE OF THE LOOKS.
    //
    // The test below asks for the same content at two looks that are far
    // enough apart. That test rests on how often this function runs, and a
    // coordinator with 300 queued jobs can take more than a second between two
    // turns. Two looks then land in two different windows of a writer, the
    // whole file between them is never seen, and qex takes a file that never
    // existed for `CONFIG_SETTLE` at all. Measured: the budget of a person
    // became the budget of the machine, which is the one value that stops
    // several agents from filling it.
    //
    // The age of the file has no such window. A file that a writer changed a
    // moment ago is young, whatever this coordinator is doing, and qex waits
    // for it to become old. The time of a file is coarse — a few milliseconds
    // on the file systems of Linux and macOS, and as much as two seconds on
    // some others — and it rounds DOWN. A file system with a granularity above
    // `CONFIG_SETTLE` can thus give a young file an age that passes this test,
    // which leaves qex on the older guard below and no worse than it was.
    //
    // A system that gives no time for the file leaves the test below alone.
    let the_file_says_it_settled = match &read {
        ConfigFile::Text(_, Some(age)) => {
            if *age < CONFIG_SETTLE {
                // A writer touched this file a moment ago. Wait, and hold no
                // candidate: the next look decides.
                //
                // SAY SO. A file that a writer changes for ever — a slow
                // program, a backup tool that touches it, a clock that moves
                // — is refused at every look, and a user who reads `qex info`
                // would otherwise see the old values with nothing to say why
                // the new ones did not arrive.
                state.config_settling = None;
                // Say it one time for each wait, and not at every look.
                if state.config_error.as_deref() != Some(WAITING_FOR_A_WRITER) {
                    log(WAITING_FOR_A_WRITER);
                }
                state.config_error = Some(WAITING_FOR_A_WRITER.to_string());
                return;
            }
            // The file itself proves that it held this content for long
            // enough, so the coordinator needs no second look to learn what
            // the file already says.
            true
        }
        _ => false,
    };

    if !the_file_says_it_settled {
        match state.config_settling {
            // The same number as before: take it when it is old enough.
            Some((seen, since)) if seen == now => {
                if since.elapsed() < CONFIG_SETTLE {
                    return;
                }
            }
            // A number that this function did not see last time. Start the
            // wait.
            _ => {
                state.config_settling = Some((now, Instant::now()));
                return;
            }
        }
    }
    state.config_settling = None;
    state.config_seen = now;

    let bytes = match read {
        ConfigFile::Text(bytes, _) if !bytes.is_empty() => bytes,
        // A FILE THAT IS GONE OR EMPTY IS NOT A NEW CONFIGURATION.
        //
        // `Config::load` gives the default values for a file that does not
        // exist, which is correct at the start of a coordinator and wrong
        // here: an editor that empties a file before it writes it, and a shell
        // `>` that does the same, would turn a budget of 2 cores into the
        // default budget for as long as that window lasts. Keep the values.
        ConfigFile::Text(..) | ConfigFile::Missing | ConfigFile::Unreadable(_) => {
            // Say WHICH fault this is. The earlier text of `config_error`
            // names a line of a file that no longer holds that line, and a
            // reader who looks for it does not find it.
            let message = "the file is empty, or qex cannot read it".to_string();
            log(&format!(
                "{message}. The coordinator keeps the values that it had."
            ));
            state.config_error = Some(message);
            return;
        }
        ConfigFile::NotRegular => {
            let message = "the path of the configuration file is not a regular file".to_string();
            log(&format!(
                "{message}. The coordinator keeps the values that it had."
            ));
            state.config_error = Some(message);
            return;
        }
    };

    let path = paths::config_file().unwrap_or_default();
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            let message = "the configuration file is not text".to_string();
            log(&format!(
                "{message}. The coordinator keeps the values that it had."
            ));
            state.config_error = Some(message);
            return;
        }
    };

    // `validate` as well as parse. The start of a coordinator refuses a file
    // that does not validate, and this path installed one: a budget of `two`
    // gave every job a budget of 0 with no word to anybody.
    match Config::parse_short(&path, &text).and_then(|c| c.validate().map(|_| c)) {
        Ok(cfg) => {
            state.config_error = None;
            log("the configuration file changed; the coordinator read it again");
            state.cfg = cfg;
        }
        Err(e) => {
            // Keep the values that this coordinator has. The default values
            // would be a budget that nobody asked for.
            let message = format!("{e:#}");
            log(&format!(
                "the configuration file changed and qex cannot read it: {message}. \
                 The coordinator keeps the values that it had."
            ));
            state.config_error = Some(message);
        }
    }
}

/// Gives the position of a state in the life of a job.
///
/// A job moves forward only. This function lets the code refuse a record that
/// moves a job back to an earlier state.
fn rank(state: JobState) -> u8 {
    match state {
        JobState::Queued => 0,
        JobState::Starting => 1,
        JobState::Running => 2,
        // Each final state has the same position.
        _ => 3,
    }
}

impl State {
    /// Makes a state for a test of `reload_config`.
    ///
    /// The fields that this function fills are the fields that the reload
    /// reads. A test of the reload must not need a coordinator, a socket or a
    /// directory.
    #[cfg(test)]
    fn for_a_test() -> Self {
        Self {
            cfg: Config::default(),
            jobs: BTreeMap::new(),
            queue: Vec::new(),
            dedupe: BTreeMap::new(),
            last_contact: Instant::now(),
            idle_since: None,
            next_sequence: 1,
            started_at: 0,
            config_seen: 0,
            config_settling: None,
            config_error: None,
            events: crate::events::EventLog::new(),
            paused: crate::pause::Paused::default(),
            last_start_at: None,
            head: None,
            peer_claims: crate::peers::Claims::default(),
            stop: false,
        }
    }

    /// Reads the status file of each job that operates.
    ///
    /// The supervisor owns the result of a job and writes `status.json`. The
    /// coordinator holds a copy in memory. This function makes the copy current.
    ///
    /// Without this function, the coordinator reports `starting` until the
    /// supervisor stops. The command `qex kill` then has no process id, and it
    /// refuses to stop a job that operates.
    ///
    /// Gives `true` if a job changed.
    pub fn refresh_active(&mut self) -> bool {
        let ids: Vec<uuid::Uuid> = self
            .jobs
            .iter()
            .filter(|(_, j)| !j.status.state.is_terminal())
            .map(|(id, _)| *id)
            .collect();

        let mut changed = false;
        for id in ids {
            let Ok(dir) = paths::job_dir(&id) else {
                continue;
            };
            let Ok(disk) = job::read_status(&dir) else {
                continue;
            };
            let Some(job) = self.jobs.get_mut(&id) else {
                continue;
            };

            // The queue owns the reason that a job waits. The supervisor does
            // not write that field, so keep the value from this process.
            if job.status.state == JobState::Queued && disk.state == JobState::Queued {
                continue;
            }

            // Never move a job back to an earlier state.
            //
            // The scheduler changes the memory copy to `starting` and then
            // writes the file. A request that arrives between those two steps
            // reads the older file. Without this test, the job returns to the
            // state `queued` while the supervisor already starts it.
            if rank(disk.state) < rank(job.status.state) {
                continue;
            }

            if job.status.state != disk.state
                || job.status.pid != disk.pid
                || job.status.exit_code != disk.exit_code
            {
                changed = true;
            }
            job.status = disk;
        }
        changed
    }

    /// Gives the resources that the jobs which operate now have claimed.
    ///
    /// The result holds the cores, the memory, the units of each pool and the
    /// quantity in use on each device. A coordinator that starts again rebuilds
    /// every one of these values from the records on the disk.
    pub fn claimed(&self) -> crate::sched::Held {
        let pools = self.cfg.pools().unwrap_or_default();
        let mut held = crate::sched::Held::default();
        for job in self.jobs.values() {
            if job.status.state.is_active() {
                held.add(&job.status, &pools);
            }
        }
        held
    }

    /// Gives the job that holds this dedupe key now.
    ///
    /// `window` comes from the submission that asks, and not from the job that
    /// holds the key. The caller thus says how old an answer it accepts, and
    /// the coordinator keeps no policy of its own.
    ///
    /// The rules, in order:
    ///
    ///   * No entry: the key is free.
    ///   * An entry with no job record: a submission with this key is in
    ///     progress in a different thread, and it holds the key. `clean`
    ///     deletes the entry with the record, so this case cannot mean "the
    ///     record went away".
    ///   * A job that has not stopped: it holds the key. This is the case that
    ///     the option exists for.
    ///   * A job that SUCCEEDED inside the window: it holds the key.
    ///   * Every other job: the key is free.
    pub fn dedupe_holder(&self, key: &str, window: u64) -> Option<uuid::Uuid> {
        let id = *self.dedupe.get(key)?;
        let Some(job) = self.jobs.get(&id) else {
            return Some(id);
        };

        if !job.status.state.is_terminal() {
            return Some(id);
        }
        if window > 0 && job.status.state == JobState::Completed {
            let finished = job.status.finished_at.unwrap_or(0);
            if sys::now_secs().saturating_sub(finished) < window {
                return Some(id);
            }
        }
        None
    }

    pub fn count_state(&self, f: impl Fn(JobState) -> bool) -> usize {
        self.jobs.values().filter(|j| f(j.status.state)).count()
    }

    /// Gives the job that holds one lock now, as `a1b2c3d4 (train)`.
    ///
    /// A person who asks for a lock that a job holds must learn which job, and
    /// must learn that the wait has a known end.
    pub fn lock_holder(&self, name: &str) -> Option<String> {
        self.jobs
            .values()
            .find(|j| j.status.state.is_active() && j.spec.locks.iter().any(|l| l == name))
            .map(|j| {
                format!(
                    "{} ({})",
                    &j.status.id.to_string()[..8],
                    j.status.display_name()
                )
            })
    }

    /// Gives the locks that a person holds, with the job that still holds each.
    pub fn paused_locks(&self) -> Vec<crate::proto::LockPause> {
        self.paused
            .locks
            .iter()
            .map(|(name, record)| crate::proto::LockPause {
                name: name.clone(),
                record: record.clone(),
                held_by: self.lock_holder(name),
            })
            .collect()
    }

    /// Writes the pause file after a change.
    ///
    /// A pause that stays in this process only would go away with the
    /// coordinator, and the queue would start work behind the person who asked
    /// for a quiet machine. See the `pause` module.
    pub fn save_pause(&self) {
        if let Err(e) = self.paused.write() {
            log(&format!("qex could not write the pause record: {e:#}"));
        }
    }

    /// Puts a job in the queue, after each job of the same priority or a higher
    /// priority. The queue is thus stable.
    ///
    /// A job can enter the queue more than one time: at the submission, and
    /// again if an older supervisor left a `queued` record after a failure.
    /// A current retry stays `running` and does not come back here. Each entry
    /// must use the same rule as the first, or a job that starts again would
    /// go in front of the jobs that waited for it.
    pub fn enqueue(&mut self, id: uuid::Uuid) {
        if self.queue.contains(&id) {
            return;
        }
        let priority = self.jobs.get(&id).map(|j| j.spec.priority).unwrap_or(0);
        let pos = self
            .queue
            .iter()
            .position(|other| {
                self.jobs
                    .get(other)
                    .map(|j| j.spec.priority < priority)
                    .unwrap_or(false)
            })
            .unwrap_or(self.queue.len());
        self.queue.insert(pos, id);
    }
}

/// Looks for a newer release of qex, away from the work.
///
/// The interval of the config file is the time between two ASKS. This thread
/// wakes more often than that, because a coordinator stops when the queue is
/// empty and the answer must not need a coordinator that lives for a week.
/// `update::check_if_due` holds the interval, and it reads the time of the
/// last ask from the state directory, so the count survives a coordinator that
/// stops.
#[cfg(unix)]
fn update_watch(coord: Arc<Coordinator>) {
    // The FIRST look comes soon, and the ones after it follow the interval.
    //
    // The first look of a fresh install asks nothing: it writes the time, and
    // that time starts the interval. A long wait here would start the interval
    // a minute after the install for no gain. A coordinator that carries one
    // short job stops before this, and it then writes nothing at all.
    let mut step = Duration::from_secs(2);
    loop {
        std::thread::sleep(step);
        // TAKE THE CONFIGURATION, AND GIVE THE LOCK BACK BEFORE THE ASK.
        // `ask` talks to a web service, and no thread may hold the state of
        // the queue while it waits for one.
        let cfg = {
            let state = match coord.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            if state.stop {
                return;
            }
            state.cfg.clone()
        };
        crate::update::check_if_due(&cfg);

        // THE STEP FOLLOWS THE INTERVAL. A user who asks for `10s` gets a look
        // every 10 seconds, and the default of `7d` costs one look each
        // minute: the limit above holds the cost of the default, and the limit
        // below holds the cost of a very small interval.
        //
        // The step comes from the configuration at EVERY turn, so a change to
        // the file reaches this thread in the same way as it reaches the rest
        // of the coordinator.
        step = crate::update::interval(&cfg)
            .ok()
            .flatten()
            .unwrap_or(Duration::from_secs(60))
            .clamp(Duration::from_secs(1), Duration::from_secs(60));
    }
}

/// The coordinator. The threads share this value.
pub struct Coordinator {
    pub state: Mutex<State>,
    /// The coordinator signals this variable when a job changes state.
    ///
    /// A `Wait` request sleeps on this variable. The CLI thus does not poll,
    /// and it learns of the result immediately.
    pub changed: Condvar,
}

impl Coordinator {
    fn new(cfg: Config, config_seen: u64) -> Self {
        Self {
            state: Mutex::new(State {
                cfg,
                jobs: BTreeMap::new(),
                queue: Vec::new(),
                dedupe: BTreeMap::new(),
                last_contact: Instant::now(),
                idle_since: Some(Instant::now()),
                next_sequence: 1,
                started_at: crate::sys::now_secs(),
                // The start of a coordinator already read the file, and it
                // stopped if it could not. Take that value as the one this
                // coordinator holds, so the first turn of the scheduler makes
                // no change.
                config_seen,
                config_settling: None,
                config_error: None,
                events: crate::events::EventLog::new(),
                paused: crate::pause::Paused::default(),
                last_start_at: None,
                head: None,
                peer_claims: crate::peers::Claims::default(),
                stop: false,
            }),
            changed: Condvar::new(),
        }
    }

    /// Tells each thread that a job changed state.
    pub fn notify(&self) {
        self.changed.notify_all();
    }
}

/// Parses and fingerprints one read of the configuration at coordinator start.
fn start_config(read: ConfigFile) -> Result<(Config, u64)> {
    let seen = config_fingerprint(&read);
    let cfg = Config::from_file(read)?;
    Ok((cfg, seen))
}

/// Runs the coordinator. This function gives control back when the coordinator
/// stops.
pub fn run() -> Result<()> {
    let (cfg, config_seen) = start_config(crate::config::read_config_file())?;
    cfg.validate()?;

    let runtime = paths::runtime_dir()?;
    paths::ensure_dir(&runtime, 0o700)?;
    paths::ensure_dir(&paths::jobs_dir()?, 0o700)?;

    let socket_path = paths::socket_path()?;

    // TAKE THE LOCK ON THE PID FILE FIRST, AND BEFORE ANY TEST OF THE SOCKET
    // FILE.
    //
    // The lock says that a different coordinator operates, and the socket file
    // does not. A socket file can go while its coordinator operates: a cleaner
    // of the temporary directory deletes it, or a person deletes it, and the
    // coordinator deletes its own socket file one moment before it stops. A
    // test that starts at the socket file thus tests nothing in each of those
    // states, and this process becomes a second coordinator.
    //
    // Two coordinators on one state directory each hold the full budget, and
    // together they start twice the permitted work. That result is the fault
    // that qex prevents, and it is worse than a start that does not happen. A
    // lock that a different process holds is therefore an END of this process,
    // and never a warning.
    //
    // The spawn lock does not give this promise. A CLI process holds that lock,
    // and `qex daemon` is a command that a person or a systemd unit starts
    // directly, with no lock at all.
    let _pid_lock = match paths::hold_pid_file(&socket_path, paths::PID_LOCK_PATIENCE) {
        paths::PidHold::Held(lock) => lock,
        paths::PidHold::Busy => {
            log("a different coordinator operates; this process stops");
            return Ok(());
        }
        paths::PidHold::Unusable(why) => {
            log(&format!("{why} This process stops."));
            return Ok(());
        }
    };

    // A socket file can stay after a failure. Delete such a file, and then bind
    // a new socket at the same path.
    //
    // THIS PROCESS MUST DELETE THAT FILE ONLY WITH THE PROOF THAT NO
    // COORDINATOR USES IT. The lock above is the strong proof, and this test
    // covers a coordinator that started before qex made a pid file.
    // A test of the LINK, and not of the target. A symbolic link with no target
    // occupies the path, and `exists` follows the link and says that the path is
    // free. `bind` then fails, and qex cannot start at all.
    if std::fs::symlink_metadata(&socket_path).is_ok() {
        match paths::ask_socket(&socket_path, OWN_SOCKET_ANSWER_LIMIT) {
            paths::SocketAnswer::NobodyListens => {
                std::fs::remove_file(&socket_path).ok();
            }
            paths::SocketAnswer::Answers => {
                log("a different coordinator operates; this process stops");
                return Ok(());
            }
            paths::SocketAnswer::Unknown => {
                log(&format!(
                    "the socket {} gives an answer that does not identify the process \
                     that holds it. A different coordinator can operate, so this process \
                     stops. Delete that file if no coordinator operates.",
                    socket_path.display()
                ));
                return Ok(());
            }
        }
    }

    // Set the umask before the socket exists.
    //
    // `bind` makes the socket with the mode of the umask. A change of the mode
    // after `bind` leaves a short time in which a different user can connect
    // and send commands, and a command starts a program as this user.
    let listener = {
        let previous = unsafe { libc::umask(0o177) };
        let result = UnixListener::bind(&socket_path);
        unsafe {
            libc::umask(previous);
        }
        result.map_err(|e| {
            // Name the usual cause. This message goes to the log file of the
            // coordinator, which is where a person looks after the CLI says
            // that the coordinator did not start.
            let note = if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) {
                "\nA sandbox that refuses a Unix socket gives this fault. See \
                 https://github.com/stephenc/qex/blob/main/docs/sandbox.md"
                    .to_string()
            } else if e.kind() == std::io::ErrorKind::AddrInUse {
                // Something that is not a socket of this coordinator holds
                // this path: a directory, or an entry that arrived after the
                // test above, or an entry that qex could not test. Name the
                // path, and do not say what holds it: without a remedy, qex
                // cannot start on this machine again, and a reader must look.
                format!(
                    "\nSomething that is not a socket of qex holds that path. Look at it, \
                     and remove it if no coordinator uses it: {}",
                    socket_path.display()
                )
            } else {
                String::new()
            };
            anyhow::anyhow!("opening the socket {}: {e}{note}", socket_path.display())
        })?
    };
    restrict_socket(&socket_path)?;

    // Delete the short socket directories of the coordinators that stopped.
    // Without this step, each unusual state directory leaves one in /tmp.
    //
    // THIS STEP COMES AFTER `bind`, AND IT RUNS ON ITS OWN THREAD. The sweep
    // asks each socket in the temporary directory to answer, and a socket that
    // a stuck process holds is slow. Before the socket of this coordinator
    // exists, every qex command on the machine waits for that sweep, and each
    // command then reports that the coordinator did not start. The sweep
    // touches the directories of other coordinators only, so the start
    // sequence needs no result from it.
    std::thread::spawn(paths::reap_stale_socket_dirs);

    // Delete the old lines of the job history. See `[history] keep`.
    crate::history::prune(&cfg);

    let coord = Arc::new(Coordinator::new(cfg, config_seen));
    recover(&coord)?;

    log(&format!(
        "the coordinator started; pid {}; socket {}",
        std::process::id(),
        socket_path.display()
    ));

    // The scheduler thread starts the jobs.
    {
        let coord = Arc::clone(&coord);
        std::thread::spawn(move || crate::sched::run(coord));
    }

    // The idle thread stops the coordinator after a quiet period.
    {
        let coord = Arc::clone(&coord);
        let path = socket_path.clone();
        std::thread::spawn(move || idle_watch(coord, path));
    }

    // The thread that looks for a newer release of qex.
    //
    // IT IS ITS OWN THREAD, AND IT HOLDS NO LOCK WHILE IT ASKS. A web service
    // that answers slowly, or never, must not hold the state of the queue: a
    // job that a user submits in that moment waits for the queue and for
    // nothing else. See `update::check_if_due`.
    {
        let coord = Arc::clone(&coord);
        std::thread::spawn(move || update_watch(coord));
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let coord = Arc::clone(&coord);
                std::thread::spawn(move || {
                    if let Err(e) = serve(coord, stream) {
                        log(&format!("a connection failed: {e:#}"));
                    }
                });
            }
            Err(e) => {
                // A failure to accept one connection must not stop the
                // coordinator. The jobs continue.
                log(&format!(
                    "the coordinator could not accept a connection: {e}"
                ));
            }
        }

        if coord.state.lock().unwrap().stop {
            break;
        }
    }

    // Delete the socket file FIRST, and then do the other work of the stop.
    //
    // This process no longer accepts a connection. While the file stays, a new
    // command connects to it, sends a request, and receives nothing: the
    // command then reports a coordinator with no version and no pid. Without
    // the file, that command starts a coordinator, which is correct.
    std::fs::remove_file(&socket_path).ok();

    // KEEP THE PID FILE, AND KEEP THE LOCK ON IT, UNTIL THIS PROCESS STOPS.
    //
    // This process lives for a moment after it deletes the socket file. A new
    // coordinator that starts in that moment must find the lock and stop, and a
    // file that this process deleted would let that coordinator make a new file,
    // take the lock on it, and operate beside this one.
    //
    // The file stays after this process stops, and that is safe: the kernel
    // gives the lock back, so a later sweep finds the lock free and asks the
    // socket.

    // Let each reader of the event stream receive the goodbye.
    //
    // Without this wait, the process ends while the `bye` line is still in a
    // buffer. Each reader then meets a socket that closed, and it cannot tell
    // an orderly stop from a failure of the machine.
    crate::events::wait_for_readers(&coord, Duration::from_secs(1));

    // Delete the record of this coordinator. Without this step, its claims stop
    // the jobs of a different user until the record becomes stale.
    {
        let cfg = coord.state.lock().unwrap().cfg.clone();
        crate::peers::withdraw(&cfg);
    }

    log("the coordinator stopped");
    Ok(())
}

/// Gives the socket mode 0600, so other users cannot send commands.
fn restrict_socket(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting the mode of {}", path.display()))
}

/// Reads the job directories at the start.
///
/// A coordinator can stop while jobs operate. The supervisors continue. This
/// function reads their records, so the new coordinator knows about them.
fn recover(coord: &Arc<Coordinator>) -> Result<()> {
    let dir = paths::jobs_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    let mut state = coord.state.lock().unwrap();
    let mut recovered = 0usize;
    let mut queued = Vec::new();

    // Read the pause before the jobs.
    //
    // A coordinator stops when the program file changes, and qex itself tells a
    // user to run `kill <pid>` on it. Without this step, a person who follows
    // that instruction loses the pause in silence, and the machine starts work
    // again while that person believes it is quiet.
    state.paused = crate::pause::Paused::read();

    // A pause of the queue can reach its `--for` WHILE NO COORDINATOR OPERATES.
    //
    // That is the third place where a pause of the queue ends, beside
    // `qex resume queue` and `sched::step`, and it is not a corner case: qex
    // itself tells a user to run `kill <pid>` on the coordinator, and a reboot
    // or an out-of-memory kill does the same. Measured before this step
    // existed: a `--for 20s` pause, a `kill -9`, and a job with a
    // `--max-queue-time` of 5s came back `expired` with `queue_pause_secs` of
    // 0, blaming the person for a wait that the pause caused.
    //
    // The credit CANNOT go here. The job directories are read below, so
    // `state.jobs` and `state.queue` are still empty and there is nobody to
    // credit. Hold the record, and pay it after the jobs are in the state.
    let now = sys::now_secs();
    let ended_while_down = state
        .paused
        .queue
        .clone()
        .filter(|record| record.expired(now));
    if state.paused.expire(now) {
        state.save_pause();
    }
    match &state.paused.queue {
        Some(record) if record.fault => {
            // Say this in the log AND keep it in the file. The next command
            // then reads a record that qex can parse, and the words that a
            // person needs stay with it.
            log(&format!(
                "qex could not read its pause record, so it holds the queue: {}",
                record.reason.as_deref().unwrap_or("unknown")
            ));
            state.save_pause();
        }
        Some(record) => log(&format!(
            "the queue is paused; a person paused it at {}",
            sys::clock_text(record.paused_at)
        )),
        None => {}
    }
    for name in state.paused.locks.keys() {
        log(&format!(
            "a person holds the lock `{}`",
            crate::job::safe_name(name)
        ));
    }

    let current_boot = sys::boot_id();
    let boot_time = sys::boot_time_secs();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let (spec, mut status) = match (job::read_spec(&path), job::read_status(&path)) {
            (Ok(s), Ok(st)) => (s, st),
            // A directory without both files is incomplete. `qex clean` deletes
            // it. It must not stop the start of the coordinator.
            _ => continue,
        };

        // A job that says "running" can be dead. Its supervisor stopped with
        // the coordinator, or the machine restarted.
        //
        // Test the job process and the supervisor process. A job in the state
        // `starting` has no job process yet, and a test of the job process
        // alone would mark a live job as failed. That job then completes on the
        // disk while the coordinator reports a failure for ever.
        if status.state.is_active() {
            // A pid is not a name, so test the identity of each pid before
            // the life of it. The machine uses each number again; without
            // these tests a recovered record can point at strangers, and the
            // job then says `running` for ever while qex watches — or signals
            // — processes that have no connection with the job.
            //
            // A record from an EARLIER START OF THE MACHINE is dead, whatever
            // its pids say. The submission writes `boot_id`, so every record
            // of this version has one from its birth. A record WITHOUT the
            // value comes from an earlier version of qex; date it by the time
            // of its file instead — a status that was written before this
            // machine started belongs to an earlier start. Without that
            // fallback, a reboot with old records on the disk would take a
            // boot-time daemon that reuses a recorded pid for the job.
            let same_boot = match status.boot_id.as_deref() {
                Some(recorded) => recorded == current_boot,
                None => written_this_boot(&path.join("status.json"), boot_time),
            };
            // Two tests for the job pid, because neither alone is a name:
            // the pid must lead its own process group, which the supervisor
            // gave the job at its start, and the process must have the start
            // time that the supervisor recorded beside the pid. A recycled
            // number can lead a group of its own (a shell, a daemon); it
            // cannot also have the recorded start time. A process of another
            // user (which `pid_alive` counts as alive) is never the job.
            let job_alive = same_boot
                && status
                    .pid
                    .map(|pid| {
                        sys::job_pid_alive(pid)
                            && sys::same_process_start(pid, status.pid_start_token)
                    })
                    .unwrap_or(false);
            // The pid comes from the record, or from the file that the
            // coordinator wrote at the fork. The second one covers the moment
            // between the fork and the first write of the supervisor: without
            // it, a coordinator that starts again in that moment finds a job
            // with no process and marks it failed, while the job runs. The
            // file also carries the start time of the supervisor, so that
            // moment has an identity too.
            let (supervisor_pid, fork_start) = match status.supervisor_pid {
                Some(pid) => (Some(pid), None),
                None => match crate::supervisor::supervisor_record_of(&path) {
                    Some((pid, start)) => (Some(pid), start),
                    None => (None, None),
                },
            };
            // Any supervisor identity proves that the coordinator reached the
            // spawn path. A dead supervisor with no job pid is not proof that
            // exec never happened: the supervisor writes the job pid only
            // after spawn and after its log-copy threads start.
            let supervisor_was_recorded = status.supervisor_pid.is_some()
                || !matches!(
                    std::fs::symlink_metadata(path.join("supervisor.pid")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound
                );
            let supervisor_start = status.supervisor_start_token.or(fork_start);
            // The supervisor runs as this user, so `own_pid_alive`: a pid
            // that the kernel refuses to signal belongs to somebody else, and
            // is not the supervisor whatever a permission error suggests. And
            // the process must have the recorded start time: a stranger that
            // holds the number must not keep the job `running`, and the watch
            // that `reap` starts must not follow it.
            let supervisor_alive = same_boot
                && supervisor_pid
                    .map(|pid| {
                        sys::own_pid_alive(pid) && sys::same_process_start(pid, supervisor_start)
                    })
                    .unwrap_or(false);

            if supervisor_alive {
                // The job continues. Keep its state, and let the supervisor
                // write the result.
                if let Some(pid) = supervisor_pid {
                    // Watch the supervisor again, so the coordinator learns
                    // when the job stops.
                    let coord2 = Arc::clone(coord);
                    let id = status.id;
                    std::thread::spawn(move || crate::supervisor::reap(coord2, id, pid));
                }
            } else if job_alive {
                // The supervisor is gone, and the job process continues. No
                // process will ever write the result: the supervisor was the
                // parent of the job, and the exit code of the job goes to that
                // parent alone.
                //
                // Stop the job, exactly as `supervisor::reap` does when a
                // supervisor stops under a live coordinator. Before this branch
                // existed, the coordinator kept such a job as `running` and
                // watched nothing: the record never changed, the job held its
                // claim and its locks for ever, `qex wait` on it never
                // returned, and the coordinator never became idle.
                let mut note = String::from(
                    "the coordinator stopped, and the supervisor of this job did not continue",
                );
                if let Some(text) = crate::supervisor::supervisor_log_tail(&path) {
                    note.push_str(&format!(". The supervisor said: {text}"));
                }
                if let Some(pid) = status.pid {
                    log(&format!(
                        "job {} lost its supervisor, and the job process {pid} \
                         continues; qex stops the job now",
                        status.id
                    ));
                    crate::supervisor::stop_process_group(pid, status.pid_start_token);
                    note.push_str("; qex stopped the job process");
                }
                status.state = JobState::Failed;
                status.finished_at = Some(sys::now_secs());
                status.blocked_reason = None;
                status.error = Some(note);
                retire_the_pids(&mut status);
                job::write_status(&path, &status).ok();
                // This coordinator made the job terminal, so this coordinator
                // tells the person. The supervisor is gone and cannot.
                crate::hook::fire_detached(&path, &status);
            } else if requeue_unstarted(&mut status, supervisor_was_recorded) {
                // `starting` is written before the supervisor creates the job
                // process. With neither process alive and no job pid ever
                // recorded, no command ran and there is no result to lose.
                // Put the work back where it was instead of inventing a
                // failure for an attempt that never began.
                job::write_status(&path, &status).ok();
                log(&format!(
                    "job {} lost its supervisor before its process started; it is in the queue again",
                    status.id
                ));
            } else {
                status.state = JobState::Failed;
                status.finished_at = Some(sys::now_secs());
                status.blocked_reason = None;
                status.error = Some(if same_boot {
                    "the coordinator stopped, and neither the job nor its supervisor continued"
                        .to_string()
                } else {
                    // Say the true cause. The pids of this record can point at
                    // live processes of the new start of the machine, and a
                    // reader who tests them would call the record a lie.
                    "the machine restarted while the job was active; no process of the job \
                     continued"
                        .to_string()
                });
                retire_the_pids(&mut status);
                job::write_status(&path, &status).ok();
                log(&format!(
                    "job {} was active but its processes are gone; the state is now failed",
                    status.id
                ));
                // This coordinator made the job terminal, so this coordinator
                // tells the person. The supervisor is gone and cannot.
                crate::hook::fire_detached(&path, &status);
            }
        }

        if status.state == JobState::Queued {
            queued.push((status.id, status.submitted_at, spec.priority));
        }

        state.jobs.insert(
            status.id,
            Job {
                spec,
                status,
                supervisor_pid: None,
            },
        );
        recovered += 1;
    }

    // Continue the counter after the highest number that qex read. The order
    // of the jobs of a pipeline thus stays correct after a restart.
    state.next_sequence = state
        .jobs
        .values()
        .map(|j| j.status.sequence)
        .max()
        .unwrap_or(0)
        + 1;

    // Put the queue back in its order: the priority first, then the time.
    queued.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
    state.queue = queued.into_iter().map(|(id, _, _)| id).collect();

    // Pay the pause that ended while no coordinator operated.
    //
    // The queue exists only now, so this is the first moment that has somebody
    // to credit. `end_queue_pause` is the same function that the other two ends
    // of a pause call, so the three cannot drift.
    //
    // The end of the pause is the moment that the record gave, and NOT this
    // moment: the pause stopped at its `--for`, and the time between that end
    // and this restart is time that the queue was free to run. To credit it
    // would give a job more time than the pause took.
    if let Some(record) = ended_while_down {
        let ended_at = record.until.unwrap_or(now).min(now);
        crate::pause::end_queue_pause(&mut state, &record, ended_at);
        log("a pause reached its end while no coordinator operated");
    }

    // Give each dedupe key back to a job.
    //
    // Without this step, a coordinator that starts again frees every key, and
    // the next submission starts a second copy of work that already operates.
    // The key comes from `spec.json`, which the CLI wrote at the submission.
    // (`JobStatus` holds the key also, but that copy is for a reader of
    // `qex status`.)
    //
    // Two jobs can hold one key: the job of yesterday stopped, and the job of
    // today operates. The job that has not stopped wins, and after that the
    // latest job wins. A record of an earlier run must never hide a job that
    // operates now.
    let mut holders: Vec<(String, uuid::Uuid, bool, u64, u64)> = state
        .jobs
        .values()
        .filter_map(|j| {
            let key = j.spec.dedupe_key.clone()?;
            Some((
                key,
                j.status.id,
                j.status.state.is_terminal(),
                j.status.submitted_at,
                j.status.sequence,
            ))
        })
        .collect();
    holders.sort_by(|a, b| {
        // The best holder comes last, so the insert of it replaces the others.
        b.2.cmp(&a.2).then(a.3.cmp(&b.3)).then(a.4.cmp(&b.4))
    });
    for (key, id, ..) in holders {
        state.dedupe.insert(key, id);
    }
    // Give the stream the state of each job that this coordinator read. A
    // reader that connects to a new coordinator thus learns what it holds, and
    // it does not wait for a change that already happened.
    state.publish_changes();

    if recovered > 0 {
        log(&format!("the coordinator read {recovered} job record(s)"));
    }
    Ok(())
}

/// Tests if a file was written in the current start of the machine.
///
/// Recovery uses this to date a record that has no `boot_id`, which an earlier
/// version of qex wrote. Without a date, that record would take the process
/// tests after a reboot, and a boot-time process that reuses a recorded pid
/// could pass them.
///
/// When the machine gives no boot time, or the file gives no time, the answer
/// is yes: qex then loses the restart test for that record only, which is the
/// behavior it had before the test existed.
fn written_this_boot(file: &std::path::Path, boot_time: Option<u64>) -> bool {
    let Some(boot_time) = boot_time else {
        return true;
    };
    let Some(written) = std::fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
    else {
        return true;
    };
    written.as_secs() >= boot_time
}

/// Moves the pids of a job out of the active fields, for a terminal write.
///
/// The published rule is that a pid in a record exists only while the job
/// operates, and that a reader can act on it. The recovery paths that make a
/// job terminal know that these numbers now point at nothing — or at
/// strangers — so they must not stay where a reader is told to trust them.
/// This mirrors the stop write of the supervisor.
fn retire_the_pids(status: &mut crate::job::JobStatus) {
    if let Some(pid) = status.pid.take() {
        status.last_pid = Some(pid);
    }
    status.pid_start_token = None;
    status.supervisor_pid = None;
}

/// Returns an active record to the queue only when it proves that no job
/// process was ever created.
fn requeue_unstarted(status: &mut crate::job::JobStatus, supervisor_was_recorded: bool) -> bool {
    if status.state != JobState::Starting || status.pid.is_some() || supervisor_was_recorded {
        return false;
    }
    status.state = JobState::Queued;
    status.started_at = None;
    status.finished_at = None;
    status.error = None;
    status.blocked_reason = None;
    status.supervisor_start_token = None;
    status.assigned.clear();
    status.forced = false;
    status.forced_reason = None;
    retire_the_pids(status);
    true
}

/// Holds the jobs that one connection owns, and gives them back when it ends.
///
/// The work happens in `Drop`, so every end of a connection reaches it: a
/// request that qex cannot read, a write that fails, a socket that closes, and
/// a process that a signal stopped. Cleanup that sits after the fallible part
/// of a function runs only when nothing failed, and that is the moment when it
/// is least needed.
struct OwnedJobs {
    coord: Arc<Coordinator>,
    ids: Vec<uuid::Uuid>,
}

impl Drop for OwnedJobs {
    fn drop(&mut self) {
        for id in std::mem::take(&mut self.ids) {
            if cancel_queued(
                &self.coord,
                id,
                Some(String::from(
                    "the command that started this job stopped, so the job did not start. \
                     Submit it again with `qex submit` for work that must live longer than \
                     the command that starts it.",
                )),
            ) {
                log(&format!(
                    "the job {id} is cancelled, because the command that owns it stopped"
                ));
            }
        }
    }
}

/// Answers the requests of one CLI process.
fn serve(coord: Arc<Coordinator>, stream: UnixStream) -> Result<()> {
    let mut writer = stream.try_clone().context("copying the socket handle")?;
    let reader = BufReader::new(stream);

    // This guard must live for the whole function. It cancels the jobs of this
    // connection that never started, when the connection ends.
    let mut owned = OwnedJobs {
        coord: Arc::clone(&coord),
        ids: Vec::new(),
    };

    for line in reader.lines() {
        let line = line.context("reading a request")?;
        if line.trim().is_empty() {
            continue;
        }

        coord.state.lock().unwrap().last_contact = Instant::now();

        let parsed = serde_json::from_str::<Request>(&line);

        // The event stream gives many answers to one request, so it takes this
        // connection for the rest of its life. Every other request gives one
        // answer, and the loop continues.
        //
        // A reader that goes away gives a write error here. That is normal, and
        // it must not appear in the log of the coordinator as a failure.
        if let Ok(Request::Events { since }) = parsed {
            crate::events::stream(&coord, &mut writer, since).ok();
            break;
        }

        let response = match parsed {
            // The ownership belongs to THIS connection, so this request cannot
            // go to `handle`, which knows nothing about the connection.
            Ok(Request::OwnJob { id }) => {
                if coord.state.lock().unwrap().jobs.contains_key(&id) {
                    owned.ids.push(id);
                    Response::Ok
                } else {
                    no_such_job(id)
                }
            }
            Ok(request) => handle(&coord, request),
            Err(e) => Response::error(
                ErrorKind::Internal,
                format!("qex could not read this request: {e}"),
            ),
        };

        let mut text = serde_json::to_string(&response).context("writing the answer")?;
        text.push('\n');
        if writer.write_all(text.as_bytes()).is_err() {
            // The CLI stopped. This is normal.
            break;
        }
        writer.flush().ok();
    }
    Ok(())
}

fn handle(coord: &Arc<Coordinator>, request: Request) -> Response {
    match request {
        Request::Ping => Response::Ok,
        // `serve` answers this one, because the ownership belongs to a
        // connection and this function has none. A request that arrives here
        // comes from a caller that qex does not have, so it gets a refusal and
        // never a silent success.
        Request::OwnJob { .. } => Response::error(
            ErrorKind::Internal,
            String::from("qex handles the ownership of a job on the connection that asks"),
        ),
        Request::Info => handle_info(coord),
        Request::Capabilities => Response::Capabilities {
            names: crate::capabilities::ALL
                .iter()
                .map(|s| s.to_string())
                .collect(),
        },
        Request::Submit { spec } => handle_submit(coord, *spec),
        Request::List => {
            let mut state = coord.state.lock().unwrap();
            state.refresh_active();
            // Report each change that this read found. A change that a command
            // sees and the stream does not is a change that an agent that reads
            // the stream never learns.
            state.publish_changes();
            Response::Jobs {
                jobs: state.jobs.values().map(|j| j.status.clone()).collect(),
            }
        }
        Request::Status { id } => {
            let mut state = coord.state.lock().unwrap();
            state.refresh_active();
            state.publish_changes();
            match state.jobs.get(&id) {
                Some(j) => Response::Status {
                    status: Box::new(j.status.clone()),
                },
                None => no_such_job(id),
            }
        }
        // `serve` takes the connection for the stream before it calls this
        // function, so this arm never runs. It exists because a request that
        // has no answer must never leave a reader with an open socket and
        // nothing on it.
        Request::Events { .. } => Response::error(
            ErrorKind::Internal,
            "qex could not open the event stream on this connection. Run `qex events` again.",
        ),
        Request::Wait { id } => handle_wait(coord, id),
        Request::Cancel { id } => handle_cancel(coord, id),
        Request::Kill {
            id,
            signal,
            grace_secs,
        } => crate::lifecycle::kill(coord, id, signal, grace_secs),
        Request::Clean { id } => crate::lifecycle::clean(coord, id),
        Request::Pause {
            target,
            reason,
            until,
            by_pid,
        } => handle_pause(coord, target, reason, until, by_pid),
        Request::Resume { target } => handle_resume(coord, target),
        Request::PauseState => {
            let state = coord.state.lock().unwrap();
            Response::PauseState {
                queue: state.paused.queue.clone(),
                locks: state.paused_locks(),
            }
        }
    }
}

/// Gives the stored lock name that a pause or a resume should use.
fn resolve_paused_lock(state: &State, word: &str) -> Result<String, String> {
    let known: Vec<&str> = state
        .jobs
        .values()
        .flat_map(|j| j.spec.locks.iter().map(String::as_str))
        .chain(state.paused.locks.keys().map(String::as_str))
        .collect();
    crate::pause::resolve_lock_name(word, known)
}

/// Records a pause of the queue or of one lock.
///
/// A pause of a lock is never refused, whatever job holds that lock now. The
/// request is safe to type at any moment: the job that holds the lock keeps it,
/// no other job takes it, and the person receives it when that job stops.
fn handle_pause(
    coord: &Arc<Coordinator>,
    target: crate::proto::PauseTarget,
    reason: Option<String>,
    until: Option<u64>,
    by_pid: i32,
) -> Response {
    use crate::proto::PauseTarget;

    let mut state = coord.state.lock().unwrap();

    match target {
        PauseTarget::Queue => {
            let record = keep_the_end(state.paused.queue.take(), by_pid, reason, until);
            state.paused.queue = Some(record);
            log("a person paused the queue; qex starts no job");
        }
        PauseTarget::Lock { name } => {
            let name = match resolve_paused_lock(&state, &name) {
                Ok(n) => n,
                Err(msg) => return Response::error(ErrorKind::WrongState, msg),
            };
            let record = keep_the_end(state.paused.locks.remove(&name), by_pid, reason, until);
            log(&format!(
                "a person asked for the lock `{}`",
                crate::job::safe_name(&name)
            ));
            state.paused.locks.insert(name, record);
        }
    }
    state.save_pause();

    let answer = Response::PauseState {
        queue: state.paused.queue.clone(),
        locks: state.paused_locks(),
    };
    drop(state);
    // Wake the scheduler, so the jobs of the queue get the new reason at once.
    coord.notify();
    answer
}

/// Makes the record of a pause, and keeps what a second command did not give.
///
/// # The fault that this function prevents
///
/// `qex pause queue` looks like a command that a person can run twice. A second
/// command that replaced the whole record would change a pause of 30 minutes
/// into a pause with no end, because the second command gave no `--for`. An
/// agent that pauses to protect itself would thus take the machine from a
/// person for the rest of the day.
///
/// A second command therefore ADDS: it keeps the start, and it keeps the end
/// and the reason that it does not give. To replace an end, run `qex resume`
/// first.
fn keep_the_end(
    old: Option<crate::pause::PauseRecord>,
    by_pid: i32,
    reason: Option<String>,
    until: Option<u64>,
) -> crate::pause::PauseRecord {
    let mut record = crate::pause::PauseRecord::new(by_pid, reason, until);
    // A record that qex made because it could not read the file is not a pause
    // that a person asked for, so a real pause replaces it in full.
    if let Some(old) = old.filter(|o| !o.fault) {
        record.paused_at = old.paused_at;
        record.by_pid = old.by_pid;
        record.reason = record.reason.or(old.reason);
        record.until = record.until.or(old.until);
    }
    record
}

/// Removes a pause.
fn handle_resume(coord: &Arc<Coordinator>, target: crate::proto::PauseTarget) -> Response {
    use crate::proto::PauseTarget;

    let mut state = coord.state.lock().unwrap();
    match target {
        PauseTarget::Queue => {
            // A `--for` that reaches its time and this command are the same
            // event. `pause::end_queue_pause` is the one place that says what
            // that event does. See it for both of the things that it does.
            if let Some(record) = state.paused.queue.take() {
                crate::pause::end_queue_pause(&mut state, &record, crate::sys::now_secs());
            }
            log("a person started the queue again");
        }
        PauseTarget::Lock { name } => {
            let name = match resolve_paused_lock(&state, &name) {
                Ok(n) => n,
                Err(msg) => return Response::error(ErrorKind::WrongState, msg),
            };
            log(&format!(
                "a person gave the lock `{}` back",
                crate::job::safe_name(&name)
            ));
            state.paused.locks.remove(&name);
        }
    }
    state.save_pause();

    let answer = Response::PauseState {
        queue: state.paused.queue.clone(),
        locks: state.paused_locks(),
    };
    drop(state);
    coord.notify();
    answer
}

fn no_such_job(id: uuid::Uuid) -> Response {
    Response::error(
        ErrorKind::NoSuchJob,
        format!("there is no job with the id {id}"),
    )
}

fn handle_info(coord: &Arc<Coordinator>) -> Response {
    let state = coord.state.lock().unwrap();
    // One word that answers "is the queue healthy".
    //
    // A PAUSE COMES FIRST. A paused queue starts no job at all, whatever the
    // front of the queue holds, so a word about the front would name a cause
    // that is not the cause. `paused-by-fault` is a state of its own, and not a
    // flag beside `paused`. Two parallel fields drift, and a reader that learns
    // the pause but not its cause cannot correct the cause.
    //
    // `held` then says that qex keeps the capacity for the job at the front and
    // starts nothing. Each other word names the holder of the capacity, so a
    // reader learns at once whether the cause is inside this queue or outside.
    let queue_state = match &state.paused.queue {
        Some(record) if record.fault => "paused-by-fault".to_string(),
        Some(_) => "paused".to_string(),
        None => match &state.head {
            None => "running".to_string(),
            Some(h) if h.reserved => "held".to_string(),
            Some(h) => h.blocker.clone(),
        },
    };
    let held = state.claimed();
    let (cpu_claimed, mem_claimed) = (held.cpu, held.mem);

    // Report the pools, and report what the other users hold.
    //
    // This field is an Option, and it is NOT a defaulted number. An older
    // coordinator gives no value, and the reader must see `unknown`. A
    // defaulted `0` would read as "no other user holds anything", which is a
    // lie in the place where the honest answer matters most.
    let peers = if state.cfg.peers_enabled() {
        crate::peers::claims(&state.cfg)
    } else {
        crate::peers::Claims::default()
    };
    let pools: Vec<crate::proto::PoolReport> = state
        .cfg
        .pools()
        .unwrap_or_default()
        .into_iter()
        .map(|p| crate::proto::PoolReport {
            devices: p
                .devices
                .iter()
                .enumerate()
                .map(|(i, capacity)| crate::proto::DeviceReport {
                    index: i as u32,
                    capacity: *capacity,
                    used: held
                        .devices
                        .get(&p.name)
                        .and_then(|m| m.get(&(i as u32)))
                        .copied()
                        .unwrap_or(0),
                    peer: peers
                        .devices
                        .get(&p.name)
                        .map(|s| s.contains(&(i as u32)))
                        .unwrap_or(false),
                })
                .collect(),
            used: held.pools.get(&p.name).copied().unwrap_or(0),
            peer_used: peers.pools.get(&p.name).copied().unwrap_or(0),
            total: p.total,
            size_name: p.size_name,
            name: p.name,
        })
        .collect();

    Response::Info {
        pools: Some(pools),
        pid: std::process::id() as i32,
        version: crate::version::VERSION.to_string(),
        started_at: state.started_at,
        program_replaced: paths::program_file_changed(),
        jobs_running: state.count_state(|s| s.is_active()),
        jobs_queued: state.count_state(|s| s == JobState::Queued),
        cpu_budget: state.cfg.budget_cpu().unwrap_or(0),
        mem_budget: state.cfg.budget_mem().unwrap_or(0),
        config_error: state.config_error.clone(),
        cpu_claimed,
        mem_claimed,
        queue_state: Some(queue_state),
        paused_at: state.paused.queue.as_ref().map(|p| p.paused_at),
        paused_by_pid: state.paused.queue.as_ref().map(|p| p.by_pid),
        paused_reason: state.paused.queue.as_ref().and_then(|p| p.reason.clone()),
        paused_until: state.paused.queue.as_ref().and_then(|p| p.until),
        paused_locks: Some(state.paused_locks()),
        health: Some(Box::new(crate::proto::QueueHealth {
            last_start_at: state.last_start_at,
            peer_count: state.peer_claims.count,
            peer_cpu: state.peer_claims.cpu,
            peer_mem: state.peer_claims.mem,
            head_job: state
                .head
                .as_ref()
                .map(|h| format!("{} ({})", &h.id.to_string()[..8], h.name)),
            head_blocker: state.head.as_ref().map(|h| h.blocker.clone()),
            head_passed_by: state.head.as_ref().map(|h| h.passed_by),
        })),
    }
}

fn handle_submit(coord: &Arc<Coordinator>, spec: JobSpec) -> Response {
    let id = spec.id;
    let mut status = JobStatus::new(&spec);

    // Test each dependency here as well as in the CLI.
    //
    // The coordinator owns the job list, so this is the only test that cannot
    // be wrong. A dependency that names no job would make the queue start a
    // job in the wrong order, and the user would receive no warning.
    //
    // This one lock operation also holds the dedupe key, the size test and the
    // reservation of the key. See the comment on `State::dedupe`: the test of
    // the key and the reservation of the key must not be two steps.
    // A pause is the true cause of a wait, and it hides the capacity cause.
    // `handle_submit` reads it under the same lock as the dedupe key and the
    // size test, and it uses it after that lock goes.
    let mut pause_warning: Option<String> = None;
    let mut pause_reason: Option<String> = None;

    let warning = {
        let mut state = coord.state.lock().unwrap();
        for dep in spec.needs.iter().chain(spec.after.iter()) {
            if !state.jobs.contains_key(dep) {
                return Response::error(
                    ErrorKind::NoSuchJob,
                    format!(
                        "the job {dep} does not exist, so this job cannot wait for it.\n\
                         Start that job first, and give the id that `qex submit` wrote."
                    ),
                );
            }
        }

        // The coordinator receives ids only, so the test above is the test that
        // it can make. An id names one job for ever, so its existence is
        // sufficient.
        //
        // A dependency given by name has one more rule, because a name can give
        // a job of an earlier run. The CLI is the only part that sees a name,
        // so that rule is in `resolve_dependencies`.

        // The dedupe key. Test it and reserve it here, with the lock held.
        if let Some(key) = spec.dedupe_key.clone() {
            // Read the record of each job that operates first. A job that
            // stopped one moment ago must free its key now, and not at the next
            // request.
            state.refresh_active();

            if let Some(other) = state.dedupe_holder(&key, spec.dedupe_window) {
                let doing = match state.jobs.get(&other) {
                    Some(j) => format!("is in the state `{}`", j.status.state),
                    // The record is not written yet, so the state is the state
                    // of every new job.
                    None => String::from("starts now"),
                };
                // Show the SAFE form of the key. The key is text that another
                // agent chose, and this sentence goes to the log of the
                // coordinator and to the terminal of the caller. See
                // `job::safe_name`. The map keeps the key that the user gave.
                let shown = crate::job::safe_name(&key);
                log(&format!(
                    "a submission with the dedupe key `{shown}` gave the job {other}"
                ));
                return Response::Submitted {
                    id: other,
                    warning: Some(format!(
                        "this submission started no job. The dedupe key `{shown}` gives the job \
                         {other}, and that job {doing}.\n\
                         qex gives you the id of that job, so `qex wait` and `qex status` \
                         operate on the work that already exists.\n\
                         A key names the work, and qex does not compare the command. Run \
                         `qex status {other}` to see the work that this id names.\n\
                         To run the work a second time, wait for that job to stop, or use a \
                         different key."
                    )),
                    deduplicated: true,
                };
            }

            // Hold the key for this job now, before the lock goes. A second
            // submission with the same key finds this id, whatever moment it
            // arrives in.
            state.dedupe.insert(key, id);
        }

        // Say now that this job waits for a pause, and not for capacity.
        //
        // An agent that submits a job into a paused queue must learn the true
        // cause at the moment of the submission. Without this text the agent
        // reads `queued`, waits, and looks at the budget for a cause that is
        // not there.
        let mut lines = Vec::new();
        if let Some(record) = &state.paused.queue {
            pause_reason = Some(crate::pause::queue_reason(record));
            lines.push(
                "the queue is paused, so this job waits. Run `qex resume queue` to start the \
                 queue again."
                    .to_string(),
            );
        }
        for name in &spec.locks {
            if state
                .paused
                .locks
                .keys()
                .any(|k| crate::pause::lock_same(k, name))
            {
                // The lock name is text that a person typed. Show the safe
                // form of it in a sentence that a terminal prints.
                let shown = crate::job::safe_name(name);
                lines.push(format!(
                    "a person holds the lock `{shown}`, so this job waits. Run \
                     `qex resume lock {shown}` to give it back."
                ));
            }
        }
        if !lines.is_empty() {
            pause_warning = Some(lines.join("\n"));
        }

        // Test the size of the job against the budget, and warn now. The agent
        // then learns immediately. It does not wait for the job to start.
        // A claim above the pool total is ALWAYS a refusal, whatever `[queue]
        // oversized` says. This is a change in kind from the cores and the
        // memory: an oversized memory job can run alone and swap, and that
        // result is data. Running alone does not make a fifth device.
        //
        // qex refuses this job, so it must not hold the dedupe key. A key that
        // a refused job holds would stop each later submission, and no job
        // would ever free it.
        if let Err(reason) = crate::sched::pool_check(&state.cfg, &spec) {
            release_dedupe(&mut state, id);
            return Response::error(ErrorKind::WrongState, reason);
        }

        match crate::sched::size_check(&state.cfg, spec.cpu, spec.mem) {
            crate::sched::Size::Fits => None,
            crate::sched::Size::TooBig(reason) => {
                use crate::config::OversizedPolicy;
                match state.cfg.queue.oversized {
                    OversizedPolicy::Reject => {
                        // qex refuses this job, so it must not hold the key.
                        // A key that a refused job holds would stop each later
                        // submission, and no job would ever free it.
                        release_dedupe(&mut state, id);
                        return Response::error(
                            ErrorKind::WrongState,
                            format!(
                                "{reason}\nThe config file sets [queue] oversized = \"reject\". \
                                 Decrease the claim, or increase [budget]."
                            ),
                        );
                    }
                    OversizedPolicy::Queue => Some(format!(
                        "{reason}\nThe config file sets [queue] oversized = \"queue\". \
                         This job waits until you change the budget."
                    )),
                    OversizedPolicy::RunWhenIdle => {
                        status.blocked_reason = Some(reason.clone());
                        Some(format!(
                            "{reason}\nqex starts this job alone when no other job operates. \
                             The job can swap, use every core, or stop with an out-of-memory \
                             error. Read `qex status {id}` for the result."
                        ))
                    }
                }
            }
        }
    };

    // The pause REPLACES the capacity reason in the record; it never stands
    // beside one.
    //
    // A record that said "waits for memory" would send the reader to change the
    // budget, and no budget starts a job while the queue is paused. This is one
    // assignment, and it is AFTER the size test above: the oversized branch of
    // that test writes its own reason, and the order of the two decides which
    // reason the reader gets.
    //
    // `sched::choose` writes the same reason on its next pass, so this line
    // covers the window before that pass — the record is correct from the first
    // byte that qex writes for this job, and not 500ms later.
    if let Some(reason) = &pause_reason {
        status.blocked_reason = Some(reason.clone());
    }

    // The WARNING gives both. The person who typed the command must learn about
    // the claim as well, because that claim is a fault of the command and the
    // pause is not.
    let warning = match (pause_warning, warning) {
        (Some(pause), Some(size)) => Some(format!("{pause}\n{size}")),
        (Some(pause), None) => Some(pause),
        (None, size) => size,
    };

    let dir = match paths::job_dir(&id) {
        Ok(d) => d,
        Err(e) => {
            release_dedupe(&mut coord.state.lock().unwrap(), id);
            return Response::error(ErrorKind::Internal, e.to_string());
        }
    };

    // Write the record before the answer. If the coordinator stops now, the
    // job is still in the queue after the restart.
    if let Err(e) = (|| -> Result<()> {
        paths::ensure_dir(&dir, 0o700)?;
        job::write_spec(&dir, &spec)?;
        job::write_status(&dir, &status)?;
        Ok(())
    })() {
        // This job does not exist, so it must not hold its key. Without this
        // step, the key would name a job with no record for ever, and each
        // later submission with that key would give an id that answers nothing.
        release_dedupe(&mut coord.state.lock().unwrap(), id);
        return Response::error(
            ErrorKind::Internal,
            format!("qex could not write the job record: {e:#}"),
        );
    }

    let name_for_history = spec.name.clone();
    let submitted_at = spec.submitted_at;

    {
        let mut state = coord.state.lock().unwrap();
        status.sequence = state.next_sequence;
        state.next_sequence += 1;
        state.jobs.insert(
            id,
            Job {
                spec,
                status,
                supervisor_pid: None,
            },
        );

        state.enqueue(id);
        // The admission of a job is the first event of that job.
        state.publish_changes();
    }

    // Keep a short record of this job, so a reader can tell "the record was
    // deleted" from "this job never existed" if the record disappears.
    crate::history::record_submit_for(&id, &name_for_history, submitted_at);

    coord.notify();
    Response::Submitted {
        id,
        warning,
        deduplicated: false,
    }
}

/// Frees each dedupe key that names this job.
///
/// The map holds one id for each key, so this function reads every entry. The
/// number of keys is the number of jobs, and a submission is not frequent, so
/// the cost is not important.
pub fn release_dedupe(state: &mut State, id: uuid::Uuid) {
    state.dedupe.retain(|_, holder| *holder != id);
}

fn handle_wait(coord: &Arc<Coordinator>, id: uuid::Uuid) -> Response {
    let mut state = coord.state.lock().unwrap();

    if !state.jobs.contains_key(&id) {
        return no_such_job(id);
    }

    // Sleep until the job reaches a final state. The condition variable wakes
    // this thread. This thread uses no CPU time while it waits.
    loop {
        match state.jobs.get(&id) {
            Some(j) if j.status.state.is_terminal() => {
                return Response::Status {
                    status: Box::new(j.status.clone()),
                }
            }
            Some(_) => {}
            None => return no_such_job(id),
        }

        let (guard, _) = coord
            .changed
            .wait_timeout(state, Duration::from_secs(30))
            .unwrap();
        state = guard;
    }
}

/// Cancels a job that still waits in the queue.
///
/// Gives `true` when this call cancelled the job. A job that started, or that
/// stopped, keeps its state and gives `false`: a job that operates holds a
/// claim and does work, and its output stays for a reader that attaches again.
///
/// `reason` says WHY qex cancelled the job, for a cancel that no person asked
/// for. The state stays `cancelled`, so it never reads as a failure of the
/// work, and the reason separates it from a cancel that a person typed.
///
/// THE REASON REPLACES THE ERROR OF THE JOB, AND `None` CLEARS IT. The error
/// field of a cancelled job says why QEX cancelled it, and nothing else. A
/// queued job can carry the error of an earlier attempt: a job with `--retries`
/// goes back to the queue and keeps the text of the attempt that failed. That
/// text on a cancelled record tells a reader that the job failed and starts
/// again, and both of those are then false.
fn cancel_queued(coord: &Arc<Coordinator>, id: uuid::Uuid, reason: Option<String>) -> bool {
    let mut state = coord.state.lock().unwrap();

    let Some(job) = state.jobs.get_mut(&id) else {
        return false;
    };
    if job.status.state != JobState::Queued {
        return false;
    }

    job.status.state = JobState::Cancelled;
    job.status.finished_at = Some(sys::now_secs());
    job.status.blocked_reason = None;
    job.status.error = reason;
    let status = job.status.clone();
    state.queue.retain(|q| *q != id);
    state.publish_changes();
    drop(state);

    if let Ok(dir) = paths::job_dir(&id) {
        job::write_status(&dir, &status).ok();
        // A cancelled job is not in the default filter. A user who asks
        // for `cancelled` gets it here.
        crate::hook::fire_detached(&dir, &status);
    }
    coord.notify();
    true
}

fn handle_cancel(coord: &Arc<Coordinator>, id: uuid::Uuid) -> Response {
    // Try the cancel FIRST, and read the state only to explain a refusal. A job
    // can start between a look and an action, so an answer that comes from a
    // look can say that qex cancelled a job that now operates.
    if cancel_queued(coord, id, None) {
        return Response::Ok;
    }

    let state = coord.state.lock().unwrap();
    let Some(job) = state.jobs.get(&id) else {
        return no_such_job(id);
    };
    let state_now = job.status.state;
    drop(state);

    refusal_for_a_cancel(id, state_now)
}

/// Says why a cancel did not happen, for the state that the job holds now.
///
/// Each state gets the step that fits it. A job that went back to the queue
/// takes the SAME command again, and a message that names `qex kill` for such
/// a job sends the reader to a command that refuses it.
///
/// This function is separate because the states that reach it need a race, and
/// a race is not a test. The words are the part that a reader acts on, and
/// they are testable here.
fn refusal_for_a_cancel(id: uuid::Uuid, state: JobState) -> Response {
    match state {
        // The job went back to the queue in the moment between the two steps
        // of the cancel. The cancel did not happen, and the remedy is the same
        // command.
        JobState::Queued => Response::error(
            ErrorKind::WrongState,
            format!("the job {id} went back to the queue. Run `qex cancel {id}` again."),
        ),
        // The job started in the moment between those two steps.
        JobState::Starting | JobState::Running => Response::error(
            ErrorKind::WrongState,
            format!("the job {id} operates now. Use `qex kill {id}` to stop it."),
        ),
        other => Response::error(
            ErrorKind::WrongState,
            format!("the job {id} is in the state `{other}`, so qex cannot cancel it"),
        ),
    }
}

/// Stops the coordinator after a quiet period.
///
/// The coordinator stops when two conditions are true: no job is in the queue
/// or operates, and no CLI process has connected for the idle time. The
/// coordinator thus uses no memory between the tasks of an agent.
fn idle_watch(coord: Arc<Coordinator>, socket: std::path::PathBuf) {
    let idle_limit = std::env::var(IDLE_EXIT_VAR)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(IDLE_EXIT);

    loop {
        std::thread::sleep(Duration::from_secs(1).min(idle_limit));

        // Decide and set the flag with one lock only.
        //
        // With two lock operations, a `Submit` request can arrive between them.
        // qex would accept that job, write its record, give the id to the user,
        // and then stop. The job would never start, and `qex wait` would block
        // with no end.
        // Stop when something replaced the qex program file.
        //
        // A coordinator can operate for hours. During development, a new build
        // replaces the program file, and this process then holds the old code.
        // A stop when no job operates lets the next command start a coordinator
        // with the new program. No job is lost: the next command starts a new
        // coordinator, which reads the same job records.
        let replaced = paths::program_file_changed();

        let should_stop = {
            let mut state = coord.state.lock().unwrap();
            let active = state.count_state(|s| !s.is_terminal());
            let idle = active == 0 && (replaced || state.last_contact.elapsed() >= idle_limit);
            if idle {
                state.stop = true;
            }
            idle
        };

        if should_stop && replaced {
            log(
                "the qex program file changed; this coordinator stops so that the next \
                 command starts one with the new program",
            );
        }

        if should_stop {
            log("the coordinator is idle and stops");
            // Open one connection, so the accept loop wakes and reads the flag.
            UnixStream::connect(&socket).ok();
            return;
        }
    }
}

/// Writes one line to the log file of the coordinator.
///
/// The coordinator writes its stdout to that file, so `println` is sufficient.
pub fn log(message: &str) {
    println!("[{}] {message}", sys::now_secs());
    use std::io::Write as _;
    std::io::stdout().flush().ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::JobSpec;

    #[test]
    fn a_starting_record_with_no_job_process_returns_to_the_queue() {
        let spec = spec_with_key("unstarted");
        let mut status = JobStatus::new(&spec);
        status.state = JobState::Starting;
        status.started_at = Some(crate::sys::now_secs());
        status.error = Some("stale".into());
        status.assigned.insert(
            "gpu".into(),
            crate::job::Assignment {
                units: 1,
                devices: vec![0],
                size: None,
            },
        );
        status.forced = true;
        status.forced_reason = Some("too large".into());

        assert!(requeue_unstarted(&mut status, false));
        assert_eq!(status.state, JobState::Queued);
        assert_eq!(status.started_at, None);
        assert_eq!(status.supervisor_pid, None);
        assert_eq!(status.error, None);
        assert!(status.assigned.is_empty());
        assert!(!status.forced);
        assert_eq!(status.forced_reason, None);

        status.state = JobState::Running;
        assert!(!requeue_unstarted(&mut status, false));
        status.state = JobState::Starting;
        status.pid = Some(12345);
        assert!(!requeue_unstarted(&mut status, false));
        status.pid = None;
        assert!(
            !requeue_unstarted(&mut status, true),
            "a supervisor identity means that the spawn path began"
        );
    }

    #[test]
    fn startup_parses_and_fingerprints_the_same_config_value() {
        let read = ConfigFile::Text(b"[budget]\ncpu = \"1\"\n".to_vec(), None);
        let expected = config_fingerprint(&read);

        let (cfg, seen) = start_config(read).unwrap();

        assert_eq!(seen, expected);
        assert_eq!(cfg.budget.cpu, "1");
    }

    /// A cancel must leave no error text from an earlier attempt on the record.
    ///
    /// WHAT THIS TEST OBSERVES: the field, in the function that writes it. A
    /// job that waits again after a failed attempt carries the text of that
    /// attempt, and a cancel that keeps the text gives a record with the state
    /// `cancelled` and a sentence that says the job failed and starts again.
    ///
    /// WHAT IT DOES NOT OBSERVE: the way that a coordinator reaches that state.
    /// The supervisor writes `queued` with the text for about one second before
    /// it starts the attempt again, and a coordinator that recovers in that
    /// second reads both from the disk. A test cannot hold that window open, so
    /// this test makes the state instead of racing for it.
    #[test]
    fn a_cancel_leaves_no_error_text_from_an_earlier_attempt() {
        let coord = Arc::new(Coordinator::new(Config::default(), 0));
        let id = uuid::Uuid::new_v4();

        {
            let mut state = coord.state.lock().unwrap();
            let mut spec = spec_with_key("retried");
            spec.id = id;
            let mut status = JobStatus::new(&spec);
            status.state = JobState::Queued;
            // The exact text that the supervisor writes for a job that waits
            // again after a failed attempt.
            status.error = Some(String::from(
                "attempt 1 failed with the exit code 3; qex starts the job again",
            ));
            state.jobs.insert(
                id,
                Job {
                    spec,
                    status,
                    supervisor_pid: None,
                },
            );
            state.queue.push(id);
        }

        assert!(
            cancel_queued(&coord, id, None),
            "a job that waits must cancel"
        );

        let state = coord.state.lock().unwrap();
        let status = &state.jobs.get(&id).unwrap().status;
        assert_eq!(status.state, JobState::Cancelled);
        assert!(
            status.error.is_none(),
            "a cancel that a person typed leaves no error on the record: {:?}",
            status.error
        );
    }

    /// A cancel that did not happen must name the step that fits the state.
    ///
    /// The job that went back to the QUEUE is the case that matters. A message
    /// that names `qex kill` for such a job sends the reader to a command that
    /// refuses it, and the reader then has no step that works.
    #[test]
    fn a_refused_cancel_names_the_step_for_the_state_of_the_job() {
        let id = uuid::Uuid::new_v4();

        let message = |state| match refusal_for_a_cancel(id, state) {
            Response::Error { message, .. } => message,
            other => panic!("a refusal must give an error, and it gave {other:?}"),
        };

        let queued = message(JobState::Queued);
        assert!(
            queued.contains("went back to the queue") && queued.contains("qex cancel"),
            "a job that waits again takes the same command: {queued}"
        );
        assert!(
            !queued.contains("qex kill"),
            "a job that waits has no process, so `qex kill` refuses it: {queued}"
        );

        for state in [JobState::Starting, JobState::Running] {
            let text = message(state);
            assert!(
                text.contains("operates now") && text.contains("qex kill"),
                "a job that operates takes `qex kill`: {text}"
            );
        }

        let done = message(JobState::Completed);
        assert!(
            done.contains("completed") && !done.contains("qex kill"),
            "a job that stopped names its state and no step: {done}"
        );
    }

    /// A file that a writer changed a moment ago is not a configuration.
    ///
    /// The rule cannot rest on the rate of the looks: a coordinator that
    /// carries 300 jobs takes more than a second between two turns, and two
    /// looks then land in two windows of a writer with the whole file between
    /// them unseen. The AGE of the file has no such window.
    #[test]
    fn a_file_that_a_writer_changed_a_moment_ago_is_not_taken() {
        let young = ConfigFile::Text(b"[budget]\ncpu = \"1\"\n".to_vec(), Some(Duration::ZERO));
        let mut state = State::for_a_test();
        let before = state.config_seen;

        // Every look gives the same young file, and no look may take it. Ten
        // turns cover any settle time that the coordinator uses.
        for _ in 0..10 {
            reload_config(&mut state, clone_of(&young));
        }
        assert_eq!(
            state.config_seen, before,
            "a file that is younger than the settle time must not become the configuration"
        );

        // The same content, once the file is old enough. ONE look is enough:
        // the file itself proves that it held this content.
        let old = ConfigFile::Text(b"[budget]\ncpu = \"1\"\n".to_vec(), Some(CONFIG_SETTLE * 2));
        reload_config(&mut state, clone_of(&old));
        assert_ne!(
            state.config_seen, before,
            "a file that settled must become the configuration"
        );
    }

    /// The fault of issue #64, in the shape that it really had.
    ///
    /// Two looks give the SAME half-written file, and `CONFIG_SETTLE` of wall
    /// time passes between them, because the coordinator was busy and the
    /// whole file between the two writes was never seen. The older test takes
    /// that file. The age refuses it.
    #[test]
    fn two_looks_at_one_young_file_far_apart_still_do_not_take_it() {
        let young = ConfigFile::Text(b"[budget]\ncpu = \"1\"\n".to_vec(), Some(Duration::ZERO));
        let mut state = State::for_a_test();
        let before = state.config_seen;

        reload_config(&mut state, clone_of(&young));
        // The coordinator did not look again for longer than the settle time.
        if let Some((_, since)) = &mut state.config_settling {
            *since = Instant::now() - CONFIG_SETTLE * 2;
        }
        reload_config(&mut state, clone_of(&young));

        assert_eq!(
            state.config_seen, before,
            "a file that a writer touched a moment ago must not become the configuration, \
             whatever time passed between two looks"
        );
        // The user must be able to see why the new values did not arrive.
        assert!(
            state.config_error.is_some(),
            "qex must say that it waits for a writer"
        );
    }

    /// A system that gives no time for a file leaves the older test alone.
    #[test]
    fn a_file_with_no_time_still_settles_by_its_content() {
        let file = ConfigFile::Text(b"[budget]\ncpu = \"1\"\n".to_vec(), None);
        let mut state = State::for_a_test();
        let before = state.config_seen;

        reload_config(&mut state, clone_of(&file));
        // The first look starts the wait, and the settle time has not passed.
        assert_eq!(state.config_seen, before);
        if let Some((_, since)) = &mut state.config_settling {
            *since = Instant::now() - CONFIG_SETTLE * 2;
        }
        reload_config(&mut state, clone_of(&file));
        assert_ne!(
            state.config_seen, before,
            "a file with no time must still reach the coordinator"
        );
    }

    fn clone_of(file: &ConfigFile) -> ConfigFile {
        match file {
            ConfigFile::Text(bytes, age) => ConfigFile::Text(bytes.clone(), *age),
            ConfigFile::Missing => ConfigFile::Missing,
            ConfigFile::NotRegular => ConfigFile::NotRegular,
            ConfigFile::Unreadable(_) => ConfigFile::NotRegular,
        }
    }

    fn spec_with_key(key: &str) -> JobSpec {
        JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "t".into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu: 1,
            mem: 1 << 20,
            timeout: None,
            max_queue_time: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            locks: vec![],
            claims: Default::default(),
            retries: 0,
            nice: None,
            needs: vec![],
            after: vec![],
            dedupe_key: Some(key.to_string()),
            dedupe_window: 0,
            learn_key: None,
            submitted_at: 0,
        }
    }

    fn empty_state() -> State {
        State {
            cfg: Config::default(),
            jobs: BTreeMap::new(),
            queue: Vec::new(),
            dedupe: BTreeMap::new(),
            last_contact: Instant::now(),
            idle_since: None,
            next_sequence: 1,
            started_at: 0,
            paused: crate::pause::Paused::default(),
            last_start_at: None,
            head: None,
            peer_claims: Default::default(),
            stop: false,
            config_seen: 0,
            config_settling: None,
            config_error: None,
            events: crate::events::EventLog::new(),
        }
    }

    /// Puts one job with a key in the state, and gives its id.
    fn add(
        state: &mut State,
        key: &str,
        job_state: JobState,
        finished_at: Option<u64>,
    ) -> uuid::Uuid {
        let spec = spec_with_key(key);
        let id = spec.id;
        let mut status = JobStatus::new(&spec);
        status.state = job_state;
        status.finished_at = finished_at;
        state.jobs.insert(
            id,
            Job {
                spec,
                status,
                supervisor_pid: None,
            },
        );
        state.dedupe.insert(key.to_string(), id);
        id
    }

    /// A key holds the job that waits or operates. This is the rule that stops
    /// a second copy of a four-hour run.
    #[test]
    fn a_key_holds_a_job_that_waits_or_operates() {
        for job_state in [JobState::Queued, JobState::Starting, JobState::Running] {
            let mut state = empty_state();
            let id = add(&mut state, "build:/x", job_state, None);
            assert_eq!(
                state.dedupe_holder("build:/x", 0),
                Some(id),
                "a job in the state `{job_state}` must hold its key"
            );
        }
    }

    /// A key that is free gives nothing. A different key gives nothing also: a
    /// key must name the work AND the place, and two places must not meet.
    #[test]
    fn a_key_with_no_job_is_free() {
        let mut state = empty_state();
        add(&mut state, "build:/x", JobState::Running, None);
        assert_eq!(state.dedupe_holder("build:/y", 0), None);
        assert_eq!(state.dedupe_holder("", 0), None);
    }

    /// A job that stopped frees its key.
    ///
    /// A key that held a job for ever would give an agent the id of a job of
    /// yesterday, and that answer would look like a success.
    #[test]
    fn a_job_that_stopped_frees_its_key() {
        for job_state in [
            JobState::Completed,
            JobState::Failed,
            JobState::Killed,
            JobState::Timeout,
            JobState::Oom,
            JobState::Cancelled,
            JobState::Skipped,
        ] {
            let mut state = empty_state();
            add(&mut state, "build:/x", job_state, Some(sys::now_secs()));
            assert_eq!(
                state.dedupe_holder("build:/x", 0),
                None,
                "a job in the state `{job_state}` must free its key"
            );
        }
    }

    /// The window keeps the key of a job that SUCCEEDED, and of no other job.
    ///
    /// A window that held the key of a job that failed would be dangerous: the
    /// one remedy for a failure is another run, and the option would stop it.
    #[test]
    fn the_window_keeps_the_key_of_a_job_that_succeeded_only() {
        let now = sys::now_secs();

        let mut state = empty_state();
        let id = add(&mut state, "k", JobState::Completed, Some(now));
        assert_eq!(state.dedupe_holder("k", 3600), Some(id));

        // A job that succeeded before the window gives the key back.
        let mut state = empty_state();
        add(&mut state, "k", JobState::Completed, Some(now - 7200));
        assert_eq!(state.dedupe_holder("k", 3600), None);

        // A job that failed inside the window gives the key back.
        for job_state in [JobState::Failed, JobState::Timeout, JobState::Oom] {
            let mut state = empty_state();
            add(&mut state, "k", job_state, Some(now));
            assert_eq!(
                state.dedupe_holder("k", 3600),
                None,
                "a job in the state `{job_state}` must free its key inside the window"
            );
        }
    }

    /// The key goes AT the end of the window, and not one second after it.
    ///
    /// This is the edge of the rule, and the only place where a window of this
    /// shape goes wrong. `<` and `<=` differ for one whole second, and a caller
    /// that met that second would be given the job of the earlier run.
    ///
    /// The test reads the clock twice and repeats if the second changed while
    /// it worked. The elapsed time is then EXACTLY the window, with no race and
    /// no wait. An end-to-end test of this edge has to wait for a whole second
    /// and can be pushed past it by the load of the machine; this one cannot.
    #[test]
    fn a_key_goes_at_the_end_of_the_window_and_not_after_it() {
        const WINDOW: u64 = 600;

        for _ in 0..100 {
            let now = sys::now_secs();
            let mut state = empty_state();
            add(&mut state, "k", JobState::Completed, Some(now - WINDOW));
            let answer = state.dedupe_holder("k", WINDOW);
            // Repeat if the clock moved on while the test worked. The elapsed
            // time was then not the window, and the answer says nothing.
            if sys::now_secs() != now {
                continue;
            }
            assert_eq!(
                answer, None,
                "a job that succeeded exactly one window ago must give the key back"
            );

            // One second inside the window, the key stays. The two together
            // pin the comparison from both sides.
            let mut state = empty_state();
            let id = add(&mut state, "k", JobState::Completed, Some(now - WINDOW + 1));
            let answer = state.dedupe_holder("k", WINDOW);
            if sys::now_secs() != now {
                continue;
            }
            assert_eq!(
                answer,
                Some(id),
                "a job that succeeded inside the window must keep the key"
            );
            return;
        }
        panic!("the clock moved on every attempt, so the edge was never tested");
    }

    /// A submission that holds the key writes its record after it takes the
    /// key. A second submission in that moment must find the key TAKEN.
    ///
    /// Without this rule, two submissions in the same moment would both start a
    /// job, which is the fault that the key removes.
    #[test]
    fn a_key_that_a_submission_reserved_is_taken_before_the_record_exists() {
        let mut state = empty_state();
        let id = uuid::Uuid::new_v4();
        state.dedupe.insert("k".into(), id);
        assert_eq!(state.dedupe_holder("k", 0), Some(id));
    }

    /// The record and the key go together. A key that named a job with no
    /// record would give an id that `qex status` cannot answer.
    #[test]
    fn the_key_goes_when_the_record_goes() {
        let mut state = empty_state();
        let id = add(&mut state, "k", JobState::Completed, Some(sys::now_secs()));
        state.jobs.remove(&id);
        release_dedupe(&mut state, id);
        assert!(state.dedupe.is_empty());
        assert_eq!(state.dedupe_holder("k", 3600), None);
    }

    /// A second `qex pause queue` must not lose the end of the first one.
    ///
    /// # The fault that this test prevents
    ///
    /// `qex pause queue` looks like a command that a person can run twice. A
    /// second command that replaced the whole record would change a pause of 30
    /// minutes into a pause with no end, because the second command gave no
    /// `--for`. An agent that pauses to protect itself would then take the
    /// machine from a person for the rest of the day.
    #[test]
    fn a_second_pause_keeps_the_end_and_the_reason_of_the_first() {
        let first = crate::pause::PauseRecord {
            paused_at: 1_000,
            by_pid: 11,
            reason: Some("recording a demo".into()),
            until: Some(2_800),
            fault: false,
        };

        let second = keep_the_end(Some(first.clone()), 22, None, None);
        assert_eq!(second.until, Some(2_800), "the end must stay");
        assert_eq!(second.reason.as_deref(), Some("recording a demo"));
        assert_eq!(second.paused_at, 1_000, "the pause began at the first one");

        // A second command that GIVES a value uses it.
        let third = keep_the_end(Some(first.clone()), 22, Some("a build".into()), Some(9_000));
        assert_eq!(third.until, Some(9_000));
        assert_eq!(third.reason.as_deref(), Some("a build"));

        // A pause that qex made because it could not read the file is not a
        // pause that a person asked for, so a real pause replaces it in full.
        let fault = crate::pause::PauseRecord {
            fault: true,
            ..first
        };
        let real = keep_the_end(Some(fault), 22, None, None);
        assert_eq!(real.until, None);
        assert_eq!(real.by_pid, 22);
        assert!(!real.fault);
    }
}
