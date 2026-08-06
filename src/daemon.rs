//! This module holds the coordinator.
//!
//! The coordinator keeps the queue, starts each job when the machine has
//! capacity, and answers the CLI. It uses threads and a mutex. It does not use
//! an async runtime, because the number of jobs is small.
//!
//! The coordinator is not the owner of a job result. The supervisor of each job
//! writes `status.json`. The coordinator keeps a copy in memory only. A
//! coordinator that stops thus loses no result.

use crate::config::Config;
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
    pub stop: bool,
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

    /// Puts a job in the queue, after each job of the same priority or a higher
    /// priority. The queue is thus stable.
    ///
    /// A job enters the queue two times in its life: at the submission, and
    /// again after the kernel stopped it for memory and qex raised its claim.
    /// The second case must use the same rule as the first, or a job that qex
    /// corrects would go in front of the jobs that waited for it.
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
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: state.started_at,
        program_replaced: paths::program_file_changed(),
        jobs_running: state.count_state(|s| s.is_active()),
        jobs_queued: state.count_state(|s| s == JobState::Queued),
        cpu_budget: state.cfg.budget_cpu().unwrap_or(0),
        mem_budget: state.cfg.budget_mem().unwrap_or(0),
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
        match crate::sched::size_check(&state.cfg, spec.cpu, spec.mem) {
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
