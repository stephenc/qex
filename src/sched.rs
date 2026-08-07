//! This module decides when each job starts.
//!
//! The rule is simple: a job starts when the machine has capacity for its
//! claim. The claims stop two agents from starting too much work together.
//!
//! One job type does not follow the rule. A job with a claim that is larger
//! than the full budget can never meet the test. qex starts such a job alone
//! when no other job operates. The job can then swap or stop with an
//! out-of-memory error. That result is data for the agent. A job that waits for
//! ever gives no data.

use crate::config::{Config, OversizedPolicy};
use crate::daemon::{log, Coordinator};
use crate::job::{self, JobState};
use crate::paths;
use crate::spec::JobSpec;
use crate::sys;
use crate::units::format_size;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The result of the test of a job size against the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Size {
    /// The job fits the budget. It waits for free capacity only.
    Fits,
    /// The job is larger than the full budget. It can never fit.
    TooBig(String),
}

/// Tests one job against the full budget.
///
/// This test uses the budget, not the free capacity. A job that fails this test
/// can never start by the normal rule.
pub fn size_check(cfg: &Config, spec: &JobSpec) -> Size {
    let cpu_budget = cfg.budget_cpu().unwrap_or(1);
    let mem_budget = cfg.budget_mem().unwrap_or(0);

    let mut reasons = Vec::new();
    if spec.cpu > cpu_budget {
        reasons.push(format!(
            "the job claims {} cores and the budget is {} cores",
            spec.cpu, cpu_budget
        ));
    }
    if spec.mem > mem_budget {
        reasons.push(format!(
            "the job claims {} of memory and the budget is {}",
            format_size(spec.mem),
            format_size(mem_budget)
        ));
    }

    if reasons.is_empty() {
        Size::Fits
    } else {
        Size::TooBig(reasons.join("; "))
    }
}

/// The result of the test of a job against the machine now.
enum Admit {
    Yes,
    /// The job waits. The text gives the reason.
    No(String),
}

/// Tests if a lock of this job is held by a job that operates.
///
/// A resource claim cannot express this need. Two builds in one directory need
/// the same quantity of memory as one build, and they still destroy each
/// other's files. Two servers need one port, whatever their size.
fn lock_conflict(state: &crate::daemon::State, spec: &JobSpec) -> Option<String> {
    if spec.locks.is_empty() {
        return None;
    }
    for job in state.jobs.values() {
        if !job.status.state.is_active() {
            continue;
        }
        for name in &spec.locks {
            if job.spec.locks.contains(name) {
                return Some(format!(
                    "waits for the lock `{name}`, which the job {} ({}) holds",
                    &job.status.id.to_string()[..8],
                    // The SAFE name. This sentence goes to a reader, through
                    // `blocked_reason`. See `job::safe_name`.
                    job.status.display_name()
                ));
            }
        }
    }
    None
}

/// Tests if a job can start now.
fn admit(cfg: &Config, spec: &JobSpec, cpu_used: u64, mem_used: u64) -> Admit {
    let cpu_budget = cfg.budget_cpu().unwrap_or(1);
    let mem_budget = cfg.budget_mem().unwrap_or(0);

    // Test 1: the budget of this user.
    if cpu_used + spec.cpu > cpu_budget {
        return Admit::No(format!(
            "waits for cores: {} of {} are in use and the job needs {}",
            cpu_used, cpu_budget, spec.cpu
        ));
    }
    if mem_used + spec.mem > mem_budget {
        return Admit::No(format!(
            "waits for memory: {} of {} is in use and the job needs {}",
            format_size(mem_used),
            format_size(mem_budget),
            format_size(spec.mem)
        ));
    }

    // Test 2: the other users. This test reads the files of the other
    // coordinators. It finds a load that this coordinator did not start.
    if cfg.peers.enabled {
        let peers = crate::peers::claims(cfg);
        if peers.cpu > 0 || peers.mem > 0 {
            if cpu_used + peers.cpu + spec.cpu > cpu_budget {
                return Admit::No(format!(
                    "waits for cores: {} user(s) claim {} cores",
                    peers.count, peers.cpu
                ));
            }
            if mem_used + peers.mem + spec.mem > mem_budget {
                return Admit::No(format!(
                    "waits for memory: {} user(s) claim {}",
                    peers.count,
                    format_size(peers.mem)
                ));
            }
        }
    }

    // Test 3: the machine. This test finds every load, and not the load of qex
    // only. It is the test that a program outside qex cannot avoid.
    let reserve = cfg.reserve_mem().unwrap_or(0);
    let available = sys::available_memory();
    if available < reserve + spec.mem {
        // Say what this number is, and what it is not.
        //
        // A machine can be healthy and still report a small number here: the
        // kernel writes the memory of an idle program to swap and keeps it
        // there, and that memory is NOT in this number. A user then sees a job
        // that waits while the machine has capacity, and the cause is not
        // visible in the words "waits for memory".
        //
        // The pressure, where the system supplies it, is the measurement that
        // separates the two cases. A machine with a small number here and no
        // pressure is a machine that parked memory that nobody wants.
        let mut reason = format!(
            "waits for memory: the machine reports {} that a new program can use, and the job \
             needs {} with {} in reserve",
            format_size(available),
            format_size(spec.mem),
            format_size(reserve)
        );
        match sys::memory_pressure() {
            Some(p) if p < 1.0 => reason.push_str(&format!(
                ". The memory pressure is {p:.1}, so the machine is NOT short of memory now: \
                 this number counts the memory that a program can use with no operation to the \
                 disk, and it does not count the memory that the kernel parked in swap. Give a \
                 smaller claim, or lower `reserve_mem` in the configuration, if this job waits \
                 and the machine is healthy"
            )),
            _ => reason.push_str(
                ". Use `qex info` to see the load of the machine, and `qex list` to see what \
                 holds the memory",
            ),
        }
        return Admit::No(reason);
    }

    if let Some(pressure) = sys::memory_pressure() {
        if pressure > cfg.system.max_pressure {
            return Admit::No(format!(
                "waits for the machine: the memory pressure is {:.1} and the limit is {:.1}",
                pressure, cfg.system.max_pressure
            ));
        }
    }

    Admit::Yes
}

/// Runs the scheduler. This function does not give control back.
pub fn run(coord: Arc<Coordinator>) {
    loop {
        if coord.state.lock().unwrap().stop {
            return;
        }

        // Read the configuration again when somebody changed it, so an edit
        // reaches a coordinator that already operates.
        //
        // THE READ IS OUTSIDE THE LOCK. A read of a file can take any length of
        // time. A review put a FIFO at the path of the configuration file and
        // measured `qex info` with no answer in 15 seconds: this thread waited
        // in the open, and the three other threads waited for the mutex that
        // this thread held. `read_config_file` now refuses a file that is not a
        // regular file, and this line keeps the mutex free while it reads.
        //
        // THIS LOOP IS NOT A CLOCK. The wait at the end of the loop has a
        // timeout of 500ms, but every request thread calls `notify`, so a turn
        // is as short as the work makes it. Measured with a mark on each turn:
        // the median gap was 500.7ms with nothing to do, and 17.0ms with a loop
        // of `qex submit` running, with a minimum of 1.2ms. `reload_config`
        // therefore measures TIME, and it must never count turns.
        let config = crate::config::read_config_file();
        crate::daemon::reload_config(&mut coord.state.lock().unwrap(), config);

        // Read the status file of each job that operates. The supervisors write
        // those files, so this is how the coordinator learns that a job started.
        let changed = coord.state.lock().unwrap().refresh_active();

        match step(&coord) {
            Ok(started) if started > 0 || changed => coord.notify(),
            Ok(_) => {}
            Err(e) => log(&format!("the scheduler failed: {e:#}")),
        }

        // Publish the claims of this coordinator, so the coordinators of the
        // other users see them.
        {
            let state = coord.state.lock().unwrap();
            let (cpu, mem) = state.claimed();
            let cfg = state.cfg.clone();
            drop(state);
            crate::peers::publish(&cfg, cpu, mem);
        }

        // Wait for a change, or test the machine again after a short time. The
        // free memory changes without a message, so a timer is necessary.
        //
        // WHILE THE CONFIGURATION FILE SETTLES, LOOK OFTEN. `reload_config`
        // takes the content when every look in `CONFIG_SETTLE` gave that
        // content. With one look every 500ms there are TWO looks in the window,
        // and a writer with a period near one second gives them both the same
        // half-written file. A test on macOS met that, and the coordinator took
        // the file. Ten looks make that writer far less likely to pass.
        //
        // TEN LOOKS DO NOT CLOSE THE HOLE. They move it to a writer with a
        // period near a tenth of a second, and no fixed period closes it: a
        // sampler always has a frequency that walks past it. Measured on this
        // branch: a writer that changed the file every 25ms with a rename made
        // the coordinator take a half-written file in 3 trials of 5. Do not
        // write that this loop makes the content certain. See `reload_config`.
        //
        // The cost is nothing when the file is not changing, because
        // `config_settling` is `None` and the wait is 500ms as before.
        let state = coord.state.lock().unwrap();
        let wait = if state.config_settling.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(500)
        };
        let _ = coord.changed.wait_timeout(state, wait).unwrap();
    }
}

/// Starts each job that can start now. Gives the number of jobs that started.
fn step(coord: &Arc<Coordinator>) -> anyhow::Result<usize> {
    let mut started = 0usize;

    loop {
        // Choose one job, then release the lock. The start of a job forks a
        // process, and this code must not hold the lock during that work.
        let choice = {
            let mut state = coord.state.lock().unwrap();

            let active = state.count_state(|s| s.is_active());
            if active == 0 && state.idle_since.is_none() {
                state.idle_since = Some(Instant::now());
            } else if active > 0 {
                state.idle_since = None;
            }

            choose(&mut state)
        };

        match choice {
            Some(id) => {
                start_job(coord, id)?;
                started += 1;
            }
            None => break,
        }
    }

    Ok(started)
}

/// The result of the test of the dependencies of one job.
enum Depends {
    /// Each job that this job needs succeeded. This job can start.
    Ready,
    /// A job that this job needs still operates.
    Waiting(String),
    /// A job that this job needs did not succeed. This job must not start.
    ///
    /// The id is the first job that failed, and not the job before this one.
    Broken { reason: String, root: uuid::Uuid },
}

/// Tests the dependencies of one job.
fn depends(state: &crate::daemon::State, id: uuid::Uuid) -> Depends {
    let Some(job) = state.jobs.get(&id) else {
        return Depends::Ready;
    };

    // `needs`: the job must succeed.
    for dep in &job.spec.needs {
        let Some(other) = state.jobs.get(dep) else {
            // `qex clean` does not delete a job that a queued job needs, so
            // this case is not usual. Continue, because a job that waits for a
            // record that does not exist would wait with no end.
            continue;
        };

        if !other.status.state.is_terminal() {
            return Depends::Waiting(format!(
                "waits for the job {} ({}), which is {}",
                &dep.to_string()[..8],
                other.status.display_name(),
                other.status.state
            ));
        }

        if other.status.state != JobState::Completed {
            // Give the first job that failed, and not the job before this one.
            //
            // In a pipeline `a -> b -> c -> d` where `a` fails, the reader of
            // `d` must learn that `a` failed. Without this step, the reader of
            // `d` learns that `c` was skipped, and must follow the chain to
            // find the cause.
            let root = other.status.caused_by.unwrap_or(*dep);
            let root_name = state
                .jobs
                .get(&root)
                .map(|j| j.status.display_name())
                .unwrap_or_else(|| "unknown".to_string());
            let root_state = state
                .jobs
                .get(&root)
                .map(|j| j.status.state.to_string())
                .unwrap_or_else(|| other.status.state.to_string());

            // Name the log file only when the job wrote one. A cancelled job
            // and an expired job never started, so a reader who follows that
            // instruction finds an empty file and learns nothing.
            //
            // Add every state that a job can reach without a start to this
            // list. The list is short, and a state that is missing from it
            // sends the reader to an empty file.
            let never_ran = matches!(root_state.as_str(), "cancelled" | "expired");
            let advice = if never_ran {
                String::new()
            } else {
                format!(" Read `qex logs {}` for the cause.", &root.to_string()[..8])
            };

            return Depends::Broken {
                reason: format!(
                    "the job {} ({}) is {}, so this job did not run.{advice}",
                    &root.to_string()[..8],
                    root_name,
                    root_state
                ),
                root,
            };
        }
    }

    // `after`: the job must stop. Its result is not important.
    for dep in &job.spec.after {
        let Some(other) = state.jobs.get(dep) else {
            continue;
        };
        if !other.status.state.is_terminal() {
            return Depends::Waiting(format!(
                "waits for the job {} ({}) to stop, whatever its result",
                &dep.to_string()[..8],
                other.status.display_name()
            ));
        }
    }

    Depends::Ready
}

/// Marks a job as skipped, because a job that it needed did not succeed.
fn skip(state: &mut crate::daemon::State, id: uuid::Uuid, reason: String, root: uuid::Uuid) {
    let Some(job) = state.jobs.get_mut(&id) else {
        return;
    };
    job.status.state = JobState::Skipped;
    job.status.finished_at = Some(sys::now_secs());
    job.status.blocked_reason = None;
    job.status.error = Some(reason);
    job.status.caused_by = Some(root);
    let status = job.status.clone();
    state.queue.retain(|q| *q != id);

    if let Ok(dir) = paths::job_dir(&id) {
        job::write_status(&dir, &status).ok();
    }
    log(&format!(
        "job {id} did not run, because a job that it needed did not succeed"
    ));
}

/// Removes a job that waited more time than its `--max-queue-time` value.
///
/// The reason names what the job waited for and how long it waited. A reader
/// that gets `expired` and no other text cannot act: the machine was busy, the
/// claim was too large, or a lock was held, and each of those needs a different
/// correction.
fn expire(state: &mut crate::daemon::State, id: uuid::Uuid, waited: u64, last_reason: &str) {
    let Some(job) = state.jobs.get_mut(&id) else {
        return;
    };

    // A job that is not in the queue must never get this state.
    //
    // This is the race that the record must not show. The scheduler chooses a
    // job, releases the lock, and `start_job` takes the lock again and writes
    // `starting`. A test that trusted the queue list alone could then write
    // `expired` over a job that already operates, and `qex wait` would report a
    // failure for a job that succeeded.
    if job.status.state != JobState::Queued {
        return;
    }

    let limit = job.spec.max_queue_time.unwrap_or(0);
    job.status.state = JobState::Expired;
    job.status.finished_at = Some(sys::now_secs());
    // The queue reason goes into the text below, so this field becomes empty: a
    // job that stopped waits for nothing.
    job.status.blocked_reason = None;
    // Give the remedy that fits the cause.
    //
    // A job that waited for a job that it needs did not wait for capacity, and
    // a smaller claim changes nothing for it. A remedy that does not fit sends
    // the reader to make a change that cannot help.
    let waited_for_a_job = last_reason.contains("waits for the job");
    let remedy = if waited_for_a_job {
        "The job waited for a job that it needs, and not for capacity. Give a \
         --max-queue-time that covers the whole pipeline, or give no value on a stage \
         that waits for an earlier stage."
    } else {
        "Give the job a smaller claim, wait until the machine is quiet, or give a longer \
         --max-queue-time, then submit the job again."
    };
    job.status.error = Some(format!(
        "the job did not start. It waited {} in the queue, and its --max-queue-time is {}. \
         The last reason was: {last_reason}. {remedy}",
        crate::units::format_duration(Duration::from_secs(waited)),
        crate::units::format_duration(Duration::from_secs(limit)),
    ));
    let status = job.status.clone();
    state.queue.retain(|q| *q != id);

    if let Ok(dir) = paths::job_dir(&id) {
        job::write_status(&dir, &status).ok();
    }
    log(&format!(
        "job {id} did not start; it waited {waited}s and its queue limit is {limit}s"
    ));
}

/// Gives the jobs that waited more time than their limit.
///
/// The clock starts at the submission, and not at the last scheduling pass. A
/// coordinator that starts again thus continues the same count. Without that
/// rule, a restart would give each queued job a new full wait, and the limit
/// would give no promise at all.
///
/// The time of a job that waits for a different job COUNTS. `--max-queue-time`
/// answers one question for the reader: "does this id give me an answer inside
/// this time?" A clock that stops while a job waits for a dependency cannot
/// answer it, because a chain of slow stages would hold the clock for hours. A
/// stage that must wait for the stages before it therefore takes a limit that
/// covers the whole pipeline, or no limit.
fn overdue(state: &crate::daemon::State, chosen: Option<uuid::Uuid>) -> Vec<(uuid::Uuid, u64)> {
    let now = sys::now_secs();
    state
        .queue
        .iter()
        .copied()
        // The job that this pass chose starts now. A job that can start must
        // start, and it must not expire in the same moment.
        .filter(|id| Some(*id) != chosen)
        .filter_map(|id| {
            let job = state.jobs.get(&id)?;
            if job.status.state != JobState::Queued {
                return None;
            }
            let limit = job.spec.max_queue_time?;
            let waited = now.saturating_sub(job.status.submitted_at);
            (waited >= limit).then_some((id, waited))
        })
        .collect()
}

/// Chooses the next job to start.
fn choose(state: &mut crate::daemon::State) -> Option<uuid::Uuid> {
    let (cpu_used, mem_used) = state.claimed();
    let cfg = state.cfg.clone();
    let active = state.count_state(|s| s.is_active());
    let idle_since = state.idle_since;

    // Collect the decisions first. The loop cannot change the state while it
    // reads the jobs.
    let mut chosen = None;
    let mut reasons: Vec<(uuid::Uuid, Option<String>)> = Vec::new();
    let mut to_skip: Vec<(uuid::Uuid, String, uuid::Uuid)> = Vec::new();

    // Pass 1: test the dependencies of EVERY job in the queue.
    //
    // This pass is separate from the capacity pass below, and it must stay
    // separate. The capacity pass stops at the first job that must wait, to
    // keep capacity for that job. If the dependency test were in that loop, a
    // job behind a job that waits for capacity would never be tested. A job
    // whose dependency already failed would then stay in the queue for ever,
    // and `qex wait` on it would never give an answer.
    //
    // A dependency decision does not use capacity, so qex can make it for every
    // job at once.
    let mut ready: Vec<uuid::Uuid> = Vec::new();
    for id in state.queue.iter().copied() {
        let Some(job) = state.jobs.get(&id) else {
            continue;
        };
        if job.status.state != JobState::Queued {
            continue;
        }

        match depends(state, id) {
            Depends::Ready => ready.push(id),
            Depends::Waiting(reason) => reasons.push((id, Some(reason))),
            Depends::Broken { reason, root } => to_skip.push((id, reason, root)),
        }
    }

    // Pass 2: choose one job from the jobs that have no dependency left.
    //
    // The scheduler starts one job for each call, so a lock that this pass gives
    // away cannot be given again: the next call sees the job as active.
    for id in ready.iter().copied() {
        let Some(job) = state.jobs.get(&id) else {
            continue;
        };

        // A lock comes before the capacity. A job that waits for a lock does
        // not hold capacity, in the same way as a job that waits for a
        // different job, so the loop continues to the next job.
        if let Some(reason) = lock_conflict(state, &job.spec) {
            reasons.push((id, Some(reason)));
            continue;
        }

        match size_check(&cfg, &job.spec) {
            Size::Fits => match admit(&cfg, &job.spec, cpu_used, mem_used) {
                Admit::Yes if chosen.is_none() => {
                    chosen = Some(id);
                    break;
                }
                Admit::Yes => break,
                Admit::No(reason) => {
                    reasons.push((id, Some(reason)));
                    // Keep the capacity for this job. A smaller job must not
                    // pass it again and again, or the large job never starts.
                    //
                    // Give a reason to each job behind this one. A user who
                    // asks "why does my job wait" must get an answer for every
                    // job, and not for the first job only.
                    //
                    // Use the jobs that have no dependency left. A job that
                    // waits for a different job already has its own reason, and
                    // this text would replace it with a text that is not
                    // correct.
                    for later in ready.iter().copied() {
                        if later == id {
                            continue;
                        }
                        if !reasons.iter().any(|(r, _)| *r == later) {
                            reasons.push((
                                later,
                                Some(format!(
                                    "waits for the job {} at the front of the queue",
                                    &id.to_string()[..8]
                                )),
                            ));
                        }
                    }
                    break;
                }
            },
            Size::TooBig(reason) => {
                // This job can never fit. Start it alone when the machine is
                // quiet, so the agent gets a result and not an endless wait.
                let settle = cfg.settle().unwrap_or(Duration::from_secs(3));
                let quiet =
                    active == 0 && idle_since.map(|t| t.elapsed() >= settle).unwrap_or(false);

                if cfg.queue.oversized == OversizedPolicy::RunWhenIdle && quiet {
                    chosen = Some(id);
                    break;
                }

                let text = match cfg.queue.oversized {
                    OversizedPolicy::RunWhenIdle => {
                        format!("{reason}; qex starts this job when no other job operates")
                    }
                    OversizedPolicy::Queue => {
                        format!("{reason}; the config file keeps this job in the queue")
                    }
                    OversizedPolicy::Reject => reason.clone(),
                };
                reasons.push((id, Some(text)));
                break;
            }
        }
    }

    // Mark each job whose dependency did not succeed. Do this step before the
    // reasons, so a skipped job does not also get a queue reason.
    for (id, reason, root) in to_skip {
        skip(state, id, reason, root);
    }

    for (id, reason) in reasons {
        if let Some(job) = state.jobs.get_mut(&id) {
            if job.status.state != JobState::Queued {
                continue;
            }
            if job.status.blocked_reason != reason {
                job.status.blocked_reason = reason;
                let status = job.status.clone();
                if let Ok(dir) = paths::job_dir(&id) {
                    job::write_status(&dir, &status).ok();
                }
            }
        }
    }

    // Remove each job that waited more time than its limit. This step is last,
    // so the text of the job holds the newest queue reason.
    for (id, waited) in overdue(state, chosen) {
        let reason = state
            .jobs
            .get(&id)
            .and_then(|j| j.status.blocked_reason.clone())
            .unwrap_or_else(|| "the job waited for free capacity".to_string());
        expire(state, id, waited, &reason);
    }

    chosen
}

/// Starts the supervisor of one job.
fn start_job(coord: &Arc<Coordinator>, id: uuid::Uuid) -> anyhow::Result<()> {
    // Take the job and test it again, with one lock only.
    //
    // The scheduler chose this job and then released the lock. In that moment,
    // `qex cancel` can change the job, and `qex clean` can delete it. This code
    // must thus test the job again. Without the test, qex starts a job that the
    // user cancelled, and the user receives an answer that says the opposite.
    //
    // This code uses no `expect` on the map. A panic here occurs while this
    // thread holds the lock, which poisons the lock and stops the coordinator.
    let (forced_reason, name, status) = {
        let mut state = coord.state.lock().unwrap();

        let Some(job) = state.jobs.get(&id) else {
            // `qex clean` deleted the job. There is nothing to start.
            return Ok(());
        };
        if job.status.state != JobState::Queued {
            // `qex cancel` or a different thread changed the job.
            return Ok(());
        }

        let forced = match size_check(&state.cfg, &job.spec) {
            Size::TooBig(reason) => Some(format!(
                "{reason}. qex started this job alone because no other job operated."
            )),
            Size::Fits => None,
        };

        let Some(job) = state.jobs.get_mut(&id) else {
            return Ok(());
        };
        job.status.state = JobState::Starting;
        job.status.started_at = Some(sys::now_secs());
        job.status.blocked_reason = None;
        job.status.forced = forced.is_some();
        job.status.forced_reason = forced.clone();
        let status = job.status.clone();
        // The SAFE name: this value goes into the log of the coordinator.
        let name = crate::job::safe_name(&job.spec.name);
        state.queue.retain(|q| *q != id);
        (forced, name, status)
    };

    // Write the record after the change in memory, and before the fork.
    //
    // A fault here must not leave the job in the state `starting` for ever.
    // Such a job holds its claim in the budget, stops the idle exit, and makes
    // `qex wait` block with no end.
    if let Err(e) = write_started(&id, &status) {
        let mut state = coord.state.lock().unwrap();
        if let Some(job) = state.jobs.get_mut(&id) {
            job.status.state = JobState::Failed;
            job.status.finished_at = Some(sys::now_secs());
            job.status.error = Some(format!("qex could not write the job record: {e:#}"));
        }
        drop(state);
        coord.notify();
        log(&format!("job {id} could not start: {e:#}"));
        return Ok(());
    }

    if let Some(reason) = &forced_reason {
        log(&format!(
            "job {id} ({name}) starts although it is too large: {reason}"
        ));
    }

    match crate::supervisor::spawn(id) {
        Ok(pid) => {
            {
                let mut state = coord.state.lock().unwrap();
                if let Some(job) = state.jobs.get_mut(&id) {
                    job.supervisor_pid = Some(pid);
                    job.status.supervisor_pid = Some(pid);
                }
            }

            // THIS CODE MUST NOT WRITE THE RECORD NOW.
            //
            // The supervisor owns the record from the moment that it starts.
            // It writes its own process id, and then it writes `running` with
            // the process id of the job.
            //
            // A write here would race those two. The supervisor frequently wins
            // that race, and this code then returned the record to `starting`
            // with no pid — and the supervisor does not write again until the
            // job stops. A job of five minutes thus said `starting` for five
            // minutes, `qex top` could measure nothing, and `qex kill` refused
            // the job with "the job starts now. Try the command again."
            //
            // One writer at a time: the coordinator until the supervisor
            // starts, and the supervisor after that.
            //
            // The pid of the supervisor goes to a FILE OF ITS OWN instead. A
            // coordinator that starts again needs that pid to learn that the
            // job continues, and it needs it from the moment of the fork: the
            // supervisor cannot write its own pid before it exists. Each file
            // thus has one writer, and no write races another.
            crate::supervisor::record_supervisor_pid(&id, pid);

            log(&format!(
                "job {id} ({name}) started; the supervisor pid is {pid}"
            ));

            // One thread reads the result of each supervisor. The number of
            // jobs is small, so a thread for each job is not expensive.
            let coord = Arc::clone(coord);
            std::thread::spawn(move || crate::supervisor::reap(coord, id, pid));
            Ok(())
        }
        Err(e) => {
            let mut state = coord.state.lock().unwrap();
            if let Some(job) = state.jobs.get_mut(&id) {
                job.status.state = JobState::Failed;
                job.status.finished_at = Some(sys::now_secs());
                job.status.error = Some(format!("qex could not start the job: {e:#}"));
                let status = job.status.clone();
                drop(state);
                if let Ok(dir) = paths::job_dir(&id) {
                    job::write_status(&dir, &status).ok();
                }
            }
            coord.notify();
            log(&format!("job {id} could not start: {e:#}"));
            Ok(())
        }
    }
}

/// Writes the record of a job that starts.
fn write_started(id: &uuid::Uuid, status: &crate::job::JobStatus) -> anyhow::Result<()> {
    let dir = paths::job_dir(id)?;
    job::write_status(&dir, status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(cpu: &str, mem: &str) -> Config {
        toml::from_str(&format!(
            "[budget]\ncpu = \"{cpu}\"\nmem = \"{mem}\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n"
        ))
        .unwrap()
    }

    fn spec_with(cpu: u64, mem: u64) -> JobSpec {
        JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "t".into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu,
            mem,
            timeout: None,
            max_queue_time: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
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

    #[test]
    fn a_job_inside_the_budget_fits() {
        let cfg = cfg_with("4", "8GB");
        assert_eq!(size_check(&cfg, &spec_with(4, 8 << 30)), Size::Fits);
        assert_eq!(size_check(&cfg, &spec_with(1, 1 << 30)), Size::Fits);
    }

    #[test]
    fn a_job_larger_than_the_budget_is_too_big() {
        let cfg = cfg_with("4", "8GB");

        let Size::TooBig(reason) = size_check(&cfg, &spec_with(64, 1 << 30)) else {
            panic!("a job of 64 cores must not fit a budget of 4 cores");
        };
        assert!(
            reason.contains("cores"),
            "the reason must name the cores: {reason}"
        );

        let Size::TooBig(reason) = size_check(&cfg, &spec_with(1, 64 << 30)) else {
            panic!("a job of 64GB must not fit a budget of 8GB");
        };
        assert!(
            reason.contains("memory"),
            "the reason must name the memory: {reason}"
        );

        // A job that is too large in both values must give both reasons. The
        // agent then corrects the claim one time only.
        let Size::TooBig(reason) = size_check(&cfg, &spec_with(64, 64 << 30)) else {
            panic!("this job must not fit");
        };
        assert!(
            reason.contains("cores") && reason.contains("memory"),
            "got: {reason}"
        );
    }

    /// These sizes are small, and that is deliberate.
    ///
    /// `admit` makes three tests, and the third one reads the free memory of
    /// the machine that runs the test. A test that claims 8GB thus gives
    /// `Admit::Yes` on a machine with 28GB and `Admit::No` on a machine with
    /// 7GB, and it would report a fault that the program does not have. A
    /// build machine is frequently the small one.
    ///
    /// This test is about the arithmetic of the budget, so the numbers stay
    /// small enough that each machine has the memory. They stay above 64MB as
    /// well, because `budget_mem` gives 64MB as its lowest value: a budget of
    /// zero would make each job too large for the budget.
    /// `the_reserve_stops_a_job_when_the_machine_is_full` covers the third test
    /// with a value that no machine can meet.
    #[test]
    fn the_budget_limits_the_jobs_that_operate_together() {
        let cfg = cfg_with("4", "256MB");
        let job = spec_with(2, 64 << 20);

        // Two cores are in use. A job of two cores fits.
        assert!(matches!(admit(&cfg, &job, 2, 64 << 20), Admit::Yes));

        // Four cores are in use. The same job must wait.
        let Admit::No(reason) = admit(&cfg, &job, 4, 64 << 20) else {
            panic!("a job must not start when the cores are in use");
        };
        assert!(reason.contains("cores"), "got: {reason}");

        // The memory is in use. The job must wait.
        let Admit::No(reason) = admit(&cfg, &job, 0, 224 << 20) else {
            panic!("a job must not start when the memory is in use");
        };
        assert!(reason.contains("memory"), "got: {reason}");
    }

    /// A job that fills the budget exactly must start. An error in the compare
    /// operator here would keep such a job in the queue for ever.
    ///
    /// The size is small. See the note above: `admit` also reads the free
    /// memory of the machine, and a claim of 8GB gives a different answer on a
    /// machine of 28GB and on a machine of 7GB.
    #[test]
    fn a_job_that_fills_the_budget_exactly_starts() {
        let cfg = cfg_with("4", "256MB");
        assert!(matches!(
            admit(&cfg, &spec_with(4, 256 << 20), 0, 0),
            Admit::Yes
        ));
        assert_eq!(size_check(&cfg, &spec_with(4, 256 << 20)), Size::Fits);
    }

    /// The reserve keeps memory for the programs that qex does not control.
    #[test]
    fn the_reserve_stops_a_job_when_the_machine_is_full() {
        let mut cfg = cfg_with("4", "8GB");
        // Ask for a reserve that is larger than the machine. Each job must wait.
        cfg.system.reserve_mem = "1000GB".into();
        let Admit::No(reason) = admit(&cfg, &spec_with(1, 1 << 20), 0, 0) else {
            panic!("the reserve must stop this job");
        };
        assert!(reason.contains("reserve"), "got: {reason}");
    }

    /// Makes a state with one job in the queue, for the tests of the limit.
    fn state_with(
        job_state: JobState,
        max_queue_time: Option<u64>,
        waited: u64,
    ) -> crate::daemon::State {
        let mut spec = spec_with(1, 1 << 20);
        spec.max_queue_time = max_queue_time;
        let id = spec.id;

        let mut status = crate::job::JobStatus::new(&spec);
        status.state = job_state;
        status.submitted_at = sys::now_secs().saturating_sub(waited);
        status.blocked_reason = Some("waits for cores: 4 of 4 are in use".into());

        let mut jobs = std::collections::BTreeMap::new();
        jobs.insert(
            id,
            crate::daemon::Job {
                spec,
                status,
                supervisor_pid: None,
            },
        );

        crate::daemon::State {
            cfg: cfg_with("4", "8GB"),
            jobs,
            queue: vec![id],
            last_contact: Instant::now(),
            idle_since: None,
            next_sequence: 1,
            started_at: sys::now_secs(),
            config_seen: 0,
            config_settling: None,
            config_error: None,
            stop: false,
        }
    }

    /// A job that waits more time than its limit must give up and say so.
    ///
    /// Without this rule, an agent that waits for a job which the budget can
    /// never admit waits for ever, and it learns nothing.
    #[test]
    fn a_job_that_waits_more_than_its_limit_expires() {
        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];

        let overdue = overdue(&state, None);
        assert_eq!(overdue.len(), 1, "the job passed its limit");

        expire(
            &mut state,
            id,
            overdue[0].1,
            "waits for cores: 4 of 4 are in use",
        );

        let job = &state.jobs[&id];
        assert_eq!(job.status.state, JobState::Expired);
        assert!(job.status.finished_at.is_some());
        assert!(state.queue.is_empty(), "an expired job leaves the queue");

        // The text must name the wait and the time. A reader that gets the state
        // alone cannot act.
        let reason = job.status.error.clone().unwrap();
        assert!(reason.contains("cores"), "got: {reason}");
        assert!(reason.contains("--max-queue-time"), "got: {reason}");
        assert!(
            reason.contains("did not start"),
            "the text must say that the job never ran: {reason}"
        );
    }

    /// THE REMEDY MUST FIT THE CAUSE.
    ///
    /// A job that waited for a job that it needs did not wait for capacity, so
    /// "give the job a smaller claim" sends the reader to make a change that
    /// cannot help. The two causes give two texts.
    #[test]
    fn the_remedy_fits_the_reason_for_the_wait() {
        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];
        expire(&mut state, id, 61, "waits for cores: 4 of 4 are in use");
        let text = state.jobs[&id].status.error.clone().unwrap();
        assert!(
            text.contains("smaller claim"),
            "a job that waited for capacity needs the claim remedy: {text}"
        );

        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];
        expire(
            &mut state,
            id,
            61,
            "waits for the job 1a2b3c4d (build), which is running",
        );
        let text = state.jobs[&id].status.error.clone().unwrap();
        assert!(
            text.contains("whole pipeline"),
            "a job that waited for a dependency needs the pipeline remedy: {text}"
        );
        assert!(
            !text.contains("smaller claim"),
            "a smaller claim changes nothing for a job that waits for a job: {text}"
        );
    }

    /// A job below its limit, and a job with no limit, must stay in the queue.
    #[test]
    fn a_job_inside_its_limit_stays_in_the_queue() {
        let state = state_with(JobState::Queued, Some(600), 10);
        assert!(overdue(&state, None).is_empty());

        let state = state_with(JobState::Queued, None, 100_000);
        assert!(
            overdue(&state, None).is_empty(),
            "a job with no limit must wait"
        );
    }

    /// A JOB THAT STARTED MUST NEVER GET THE STATE `expired`.
    ///
    /// The scheduler chooses a job, releases the lock, and `start_job` writes
    /// `starting`. A pass that expired a job in that moment would give a record
    /// that says `expired` for a job that ran, and `qex wait` would report a
    /// failure for a job that succeeded. Two guards stop it: the chosen job is
    /// never in the list, and `expire` refuses a job that is not queued.
    #[test]
    fn a_job_that_started_never_expires() {
        // The job that this pass chose is not in the list, although it passed
        // its limit.
        let state = state_with(JobState::Queued, Some(1), 3600);
        let chosen = state.queue[0];
        assert!(
            overdue(&state, Some(chosen)).is_empty(),
            "the job that starts now must not expire"
        );

        // A job that already left the queue keeps its state.
        for started in [
            JobState::Starting,
            JobState::Running,
            JobState::Completed,
            JobState::Failed,
        ] {
            let mut state = state_with(started, Some(1), 3600);
            let id = state.queue[0];
            assert!(
                overdue(&state, None).is_empty(),
                "a job in the state {started} must not be in the list"
            );

            // The second guard, for the moment between the two locks.
            expire(&mut state, id, 3600, "waits for cores");
            assert_eq!(
                state.jobs[&id].status.state, started,
                "a job in the state {started} must keep it"
            );
        }
    }

    /// The pressure limit stops a job while the machine reclaims memory.
    #[test]
    fn the_pressure_limit_stops_a_job() {
        let mut cfg = cfg_with("4", "8GB");
        cfg.system.max_pressure = -1.0;
        if sys::memory_pressure().is_some() {
            let Admit::No(reason) = admit(&cfg, &spec_with(1, 1 << 20), 0, 0) else {
                panic!("the pressure limit must stop this job");
            };
            assert!(reason.contains("pressure"), "got: {reason}");
        }
    }
}
