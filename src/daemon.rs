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
    pub stop: bool,
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
        ConfigFile::Text(bytes) => {
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
pub fn reload_config(state: &mut State, read: ConfigFile) {
    let now = config_fingerprint(&read);
    if now == state.config_seen {
        state.config_settling = None;
        return;
    }
    match state.config_settling {
        // The same number as before: take it when it is old enough.
        Some((seen, since)) if seen == now => {
            if since.elapsed() < CONFIG_SETTLE {
                return;
            }
        }
        // A number that this function did not see last time. Start the wait.
        _ => {
            state.config_settling = Some((now, Instant::now()));
            return;
        }
    }
    state.config_settling = None;
    state.config_seen = now;

    let bytes = match read {
        ConfigFile::Text(bytes) if !bytes.is_empty() => bytes,
        // A FILE THAT IS GONE OR EMPTY IS NOT A NEW CONFIGURATION.
        //
        // `Config::load` gives the default values for a file that does not
        // exist, which is correct at the start of a coordinator and wrong
        // here: an editor that empties a file before it writes it, and a shell
        // `>` that does the same, would turn a budget of 2 cores into the
        // default budget for as long as that window lasts. Keep the values.
        ConfigFile::Text(_) | ConfigFile::Missing | ConfigFile::Unreadable(_) => {
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
    pub fn claimed(&self) -> (u64, u64) {
        self.jobs
            .values()
            .filter(|j| j.status.state.is_active())
            .fold((0, 0), |(c, m), j| (c + j.status.cpu, m + j.status.mem))
    }

    pub fn count_state(&self, f: impl Fn(JobState) -> bool) -> usize {
        self.jobs.values().filter(|j| f(j.status.state)).count()
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
    fn new(cfg: Config) -> Self {
        Self {
            state: Mutex::new(State {
                cfg,
                jobs: BTreeMap::new(),
                queue: Vec::new(),
                last_contact: Instant::now(),
                idle_since: Some(Instant::now()),
                next_sequence: 1,
                started_at: crate::sys::now_secs(),
                // The start of a coordinator already read the file, and it
                // stopped if it could not. Take that value as the one this
                // coordinator holds, so the first turn of the scheduler makes
                // no change.
                config_seen: config_fingerprint(&crate::config::read_config_file()),
                config_settling: None,
                config_error: None,
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

/// Runs the coordinator. This function gives control back when the coordinator
/// stops.
pub fn run() -> Result<()> {
    let cfg = Config::load()?;
    cfg.validate()?;

    // If the config asks for a memory limit, this process can need a cgroup
    // that it owns, and systemd gives one.
    //
    // Do this step before the socket exists. The new process opens the socket,
    // and two processes must never try to open it together.
    if crate::enforce::restart_with_systemd(&cfg) {
        log("the coordinator starts again in a systemd unit, to get a cgroup that it owns");
        return Ok(());
    }

    // Delete the short socket directories of the coordinators that stopped.
    // Without this step, each unusual state directory leaves one in /tmp.
    paths::reap_stale_socket_dirs();

    let runtime = paths::runtime_dir()?;
    paths::ensure_dir(&runtime, 0o700)?;
    paths::ensure_dir(&paths::jobs_dir()?, 0o700)?;

    let socket_path = paths::socket_path()?;
    // A socket file can stay after a failure. Test it, then delete it. The CLI
    // holds the spawn lock now, so no other coordinator can start here.
    if socket_path.exists() {
        if UnixStream::connect(&socket_path).is_ok() {
            log("a different coordinator operates; this process stops");
            return Ok(());
        }
        std::fs::remove_file(&socket_path).ok();
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
        result.with_context(|| format!("opening the socket {}", socket_path.display()))?
    };
    restrict_socket(&socket_path)?;

    // Warn now if the config asks for a limit that this system cannot apply. A
    // silent failure is dangerous: the user reads the config file and believes
    // that a limit is active.
    if let Some(warning) = crate::enforce::startup_warning(&cfg) {
        log(&format!("warning: {warning}"));
    }

    // Delete the old lines of the job history. See `[history] keep`.
    crate::history::prune(&cfg);

    let coord = Arc::new(Coordinator::new(cfg));
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

    // Delete the record of this coordinator. Without this step, its claims stop
    // the jobs of a different user until the record becomes stale.
    {
        let cfg = coord.state.lock().unwrap().cfg.clone();
        crate::peers::withdraw(&cfg);
    }

    std::fs::remove_file(&socket_path).ok();
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
            let job_alive = status.pid.map(sys::pid_alive).unwrap_or(false);
            // The pid comes from the record, or from the file that the
            // coordinator wrote at the fork. The second one covers the moment
            // between the fork and the first write of the supervisor: without
            // it, a coordinator that starts again in that moment finds a job
            // with no process and marks it failed, while the job runs.
            let supervisor_pid = status
                .supervisor_pid
                .or_else(|| crate::supervisor::supervisor_pid_of(&path));
            let supervisor_alive = supervisor_pid.map(sys::pid_alive).unwrap_or(false);

            if job_alive || supervisor_alive {
                // The job continues. Keep its state, and let the supervisor
                // write the result.
                if supervisor_alive {
                    if let Some(pid) = supervisor_pid {
                        // Watch the supervisor again, so the coordinator learns
                        // when the job stops.
                        let coord2 = Arc::clone(coord);
                        let id = status.id;
                        std::thread::spawn(move || crate::supervisor::reap(coord2, id, pid));
                    }
                }
            } else {
                status.state = JobState::Failed;
                status.finished_at = Some(sys::now_secs());
                status.blocked_reason = None;
                status.error = Some(
                    "the coordinator stopped, and neither the job nor its supervisor continued"
                        .to_string(),
                );
                job::write_status(&path, &status).ok();
                log(&format!(
                    "job {} was active but its processes are gone; the state is now failed",
                    status.id
                ));
                // This coordinator made the job terminal, so this coordinator
                // tells the person. The supervisor is gone and cannot.
                crate::hook::fire_detached(&state.cfg, &path, &status);
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

    if recovered > 0 {
        log(&format!("the coordinator read {recovered} job record(s)"));
    }
    Ok(())
}

/// Answers the requests of one CLI process.
fn serve(coord: Arc<Coordinator>, stream: UnixStream) -> Result<()> {
    let mut writer = stream.try_clone().context("copying the socket handle")?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line.context("reading a request")?;
        if line.trim().is_empty() {
            continue;
        }

        coord.state.lock().unwrap().last_contact = Instant::now();

        let response = match serde_json::from_str::<Request>(&line) {
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
            Response::Jobs {
                jobs: state.jobs.values().map(|j| j.status.clone()).collect(),
            }
        }
        Request::Status { id } => {
            let mut state = coord.state.lock().unwrap();
            state.refresh_active();
            match state.jobs.get(&id) {
                Some(j) => Response::Status {
                    status: Box::new(j.status.clone()),
                },
                None => no_such_job(id),
            }
        }
        Request::Wait { id } => handle_wait(coord, id),
        Request::Cancel { id } => handle_cancel(coord, id),
        Request::Kill {
            id,
            signal,
            grace_secs,
        } => crate::lifecycle::kill(coord, id, signal, grace_secs),
        Request::Clean { id } => crate::lifecycle::clean(coord, id),
    }
}

fn no_such_job(id: uuid::Uuid) -> Response {
    Response::error(
        ErrorKind::NoSuchJob,
        format!("there is no job with the id {id}"),
    )
}

fn handle_info(coord: &Arc<Coordinator>) -> Response {
    let state = coord.state.lock().unwrap();
    let (cpu_claimed, mem_claimed) = state.claimed();
    Response::Info {
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
    }
}

fn handle_submit(coord: &Arc<Coordinator>, spec: JobSpec) -> Response {
    let id = spec.id;

    // Test each dependency here as well as in the CLI.
    //
    // The coordinator owns the job list, so this is the only test that cannot
    // be wrong. A dependency that names no job would make the queue start a
    // job in the wrong order, and the user would receive no warning.
    {
        let state = coord.state.lock().unwrap();
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
    }
    let dir = match paths::job_dir(&id) {
        Ok(d) => d,
        Err(e) => return Response::error(ErrorKind::Internal, e.to_string()),
    };

    let mut status = JobStatus::new(&spec);

    // Test the size of the job against the budget, and warn now. The agent then
    // learns immediately. It does not wait for the job to start.
    let warning = {
        let state = coord.state.lock().unwrap();
        match crate::sched::size_check(&state.cfg, &spec) {
            crate::sched::Size::Fits => None,
            crate::sched::Size::TooBig(reason) => {
                use crate::config::OversizedPolicy;
                match state.cfg.queue.oversized {
                    OversizedPolicy::Reject => {
                        return Response::error(
                            ErrorKind::WrongState,
                            format!(
                                "{reason}\nThe config file sets [queue] oversized = \"reject\". \
                                 Decrease the claim, or increase [budget]."
                            ),
                        )
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

    // Write the record before the answer. If the coordinator stops now, the
    // job is still in the queue after the restart.
    if let Err(e) = (|| -> Result<()> {
        paths::ensure_dir(&dir, 0o700)?;
        job::write_spec(&dir, &spec)?;
        job::write_status(&dir, &status)?;
        Ok(())
    })() {
        return Response::error(
            ErrorKind::Internal,
            format!("qex could not write the job record: {e:#}"),
        );
    }

    let name_for_history = spec.name.clone();
    let submitted_at = spec.submitted_at;

    {
        let mut state = coord.state.lock().unwrap();
        let priority = spec.priority;
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

        // Put the job in the queue after each job of the same priority or a
        // higher priority. The queue is thus stable.
        let pos = state
            .queue
            .iter()
            .position(|other| {
                state
                    .jobs
                    .get(other)
                    .map(|j| j.spec.priority < priority)
                    .unwrap_or(false)
            })
            .unwrap_or(state.queue.len());
        state.queue.insert(pos, id);
    }

    // Keep a short record of this job, so a reader can tell "the record was
    // deleted" from "this job never existed" if the record disappears.
    crate::history::record_submit_for(&id, &name_for_history, submitted_at);

    coord.notify();
    Response::Submitted { id, warning }
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

fn handle_cancel(coord: &Arc<Coordinator>, id: uuid::Uuid) -> Response {
    let mut state = coord.state.lock().unwrap();

    let cfg = state.cfg.clone();
    let Some(job) = state.jobs.get_mut(&id) else {
        return no_such_job(id);
    };

    match job.status.state {
        JobState::Queued => {
            job.status.state = JobState::Cancelled;
            job.status.finished_at = Some(sys::now_secs());
            job.status.blocked_reason = None;
            let status = job.status.clone();
            state.queue.retain(|q| *q != id);
            drop(state);

            if let Ok(dir) = paths::job_dir(&id) {
                job::write_status(&dir, &status).ok();
                // A cancelled job is not in the default filter. A user who asks
                // for `cancelled` gets it here.
                crate::hook::fire_detached(&cfg, &dir, &status);
            }
            coord.notify();
            Response::Ok
        }
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
