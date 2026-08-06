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

        // Signal the waiters when a job started, when a supervisor wrote a new
        // status, AND when this turn moved a job to a final state on its own.
        //
        // The third case is the one that is easy to lose. An expired job and a
        // skipped job have no supervisor and no request thread to announce
        // them, so a turn that forgets them leaves `qex wait` asleep until the
        // 30 second fallback in `handle_wait`. Measured with the `finished`
        // term removed: a 3s queue limit gave `qex wait` at 30.0s, 2 of 2.
        //
        // THE TERMS AND THE CONDITION ARE NOT THE SAME KIND OF THING, AND A
        // READER MUST NOT CONFUSE THEM.
        //
        // Each TERM is necessary. Remove `finished > 0` and a waiter on an
        // expired or skipped job is 30 seconds late, which the end-to-end tests
        // catch.
        //
        // The CONDITION itself is only an economy. A notify that this turn does
        // not need wakes a parked request thread that finds no change and sleeps
        // again, so `if true` here is CORRECT and merely wasteful. Measured with
        // one job running and one waiter parked for 20 seconds: the condition
        // gave 59 voluntary context switches in the coordinator and `if true`
        // gave 98, and the processor time of both was below the 10ms that
        // `/proc` can report. The latency was the same to the millisecond.
        //
        // A mutation of this condition that makes it MORE often true therefore
        // survives every test, and must: no test may fail because qex told the
        // truth too often. Only a mutation that makes it LESS often true has a
        // signature. Do not read a surviving mutant here as a hole in the tests,
        // and do not add a test that counts wakeups to close it: that test would
        // pin an economy, not a promise, and it would fail on a busy machine.
        match step(&coord) {
            Ok((started, finished)) if started > 0 || finished > 0 || changed => coord.notify(),
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

/// Starts each job that can start now.
///
/// Gives the number of jobs that started, and the number that this turn moved
/// to a final state on its own. A waiter must learn of BOTH.
fn step(coord: &Arc<Coordinator>) -> anyhow::Result<(usize, usize)> {
    let mut started = 0usize;
    let mut finished = 0usize;

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

        finished += choice.finished;

        match choice.start {
            Some(id) => {
                start_job(coord, id)?;
                started += 1;
            }
            None => break,
        }
    }

    Ok((started, finished))
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
    let cfg = state.cfg.clone();
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
        // This code holds the lock of the queue, so the hook runs in a thread
        // of its own. A hook that hangs must never hold the queue.
        crate::hook::fire_detached(&cfg, &dir, &status);
    }
    log(&format!(
        "job {id} did not run, because a job that it needed did not succeed"
    ));
}

/// What a job waited for when its queue limit ended.
///
/// Each value takes a different remedy, so the caller must give the cause and
/// not let `expire` guess it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Waited {
    /// The machine had no free capacity, or the claim was larger than the
    /// budget.
    Capacity,
    /// A job in `needs` or `after` had not stopped.
    AJob,
    /// A different job held a lock that this job asks for.
    ALock,
}

/// Removes a job that waited more time than its `--max-queue-time` value.
///
/// The reason names what the job waited for and how long it waited. A reader
/// that gets `expired` and no other text cannot act: the machine was busy, the
/// claim was too large, or a lock was held, and each of those needs a different
/// correction. [`Waited`] carries which one it was.
fn expire(
    state: &mut crate::daemon::State,
    id: uuid::Uuid,
    waited: u64,
    last_reason: &str,
    cause: Waited,
) {
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

    // A job that ALREADY RAN must never get this state either.
    //
    // This guard is defensive, and no test here reproduces the state that it
    // refuses. The reading behind it: a job between two attempts of `--retries`
    // is `queued` and holds the `started_at` of the attempt that failed, and a
    // coordinator that starts again puts every `queued` record back in the
    // queue. A job in that gap would otherwise be able to expire, and its
    // record would then say `expired` and hold a start time and an exit code at
    // the same time. The state alone cannot separate the two cases; the start
    // time can.
    if job.status.started_at.is_some() {
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
    // a smaller claim changes nothing for it. A job that waited for a lock did
    // not wait for capacity either: qex gives the lock to one job at a time,
    // whatever the machine has free. A remedy that does not fit sends the
    // reader to make a change that cannot help.
    //
    // The caller decides this, from `depends` and from `lock_conflict`. DO NOT
    // read it out of the prose of `last_reason`: the pure-capacity text "waits
    // for the job <id> at the front of the queue" holds the same words as a
    // dependency wait, and a job with no `needs` then got the remedy for a
    // pipeline that it does not have.
    let remedy = match cause {
        Waited::AJob => {
            "The job waited for a job that it needs, and not for capacity. Give a \
             --max-queue-time that covers the whole pipeline, or give no value on a stage \
             that waits for an earlier stage."
        }
        Waited::ALock => {
            "The job waited for a lock, and not for capacity. qex gives a lock to one job \
             at a time, whatever the machine has free. Give a --max-queue-time that covers \
             the longest job that takes the same lock, or give the two jobs different lock \
             names if they can operate together."
        }
        Waited::Capacity => {
            "Give the job a smaller claim, wait until the machine is quiet, or give a longer \
             --max-queue-time, then submit the job again."
        }
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

/// What one pass of the scheduler decided.
struct Choice {
    /// The job to start now.
    start: Option<uuid::Uuid>,
    /// The number of jobs that this pass moved to a final state ON ITS OWN.
    ///
    /// A waiter sleeps on the condition variable, and the scheduler signals it
    /// when a job changes state. TWO final states have no messenger outside
    /// this pass: `expired`, which `expire` writes, and `skipped`, which `skip`
    /// writes for a job whose dependency did not succeed. Neither job ever had
    /// a supervisor, so `refresh_active` sees nothing, and no request thread
    /// made the change, so nothing signals the variable. Every other final
    /// state comes from a supervisor that writes the status file, or from a
    /// request thread that signals the variable itself.
    ///
    /// This count must therefore hold BOTH. A pass that reported one of them
    /// only would leave `qex wait` asleep until its 30 second fallback.
    /// Measured with the skipped jobs left out: `qex wait` on a job whose
    /// dependency expired at 3s returned at 33.0s, and on a job whose
    /// dependency was cancelled at 1s returned at 31.0s.
    finished: usize,
}

/// Chooses the next job to start.
fn choose(state: &mut crate::daemon::State) -> Choice {
    let (cpu_used, mem_used) = state.claimed();
    let cfg = state.cfg.clone();
    let active = state.count_state(|s| s.is_active());
    let idle_since = state.idle_since;

    // Collect the decisions first. The loop cannot change the state while it
    // reads the jobs.
    let mut chosen = None;
    let mut reasons: Vec<(uuid::Uuid, Option<String>)> = Vec::new();
    let mut to_skip: Vec<(uuid::Uuid, String, uuid::Uuid)> = Vec::new();
    // What each job waits for, when it is not capacity.
    //
    // `depends` and `lock_conflict` are the two places that know this, so the
    // remedy of an expired job comes from here. An earlier version of this code
    // read the prose of the queue reason instead, and the pure-capacity text
    // "waits for the job <id> at the front of the queue" holds the same words
    // as a dependency wait. A job with no `needs` at all then got the remedy
    // for a pipeline.
    let mut waits_for: std::collections::BTreeMap<uuid::Uuid, Waited> = Default::default();

    // Pass 1: test the dependencies AND THE LOCKS of EVERY job in the queue.
    //
    // This pass is separate from the capacity pass below, and it must stay
    // separate. The capacity pass stops at the first job that must wait, to
    // keep capacity for that job. If the dependency test were in that loop, a
    // job behind a job that waits for capacity would never be tested. A job
    // whose dependency already failed would then stay in the queue for ever,
    // and `qex wait` on it would never give an answer.
    //
    // THE LOCK TEST BELONGS HERE FOR THE SAME REASON. A review measured the
    // fault: with a job that held the lock, a job of two cores in front of a
    // budget with one core free, and a victim of one core that asks for the
    // same lock, the capacity pass stopped at the job in front and never tested
    // the lock of the victim. The victim then expired with the remedy for
    // capacity — "give the job a smaller claim" — which gives a lock to nobody.
    // A busy machine is exactly the machine that this option exists for, so the
    // fault hit the common case and not a corner.
    //
    // Neither decision uses capacity, so qex can make both for every job at
    // once. `lock_conflict` gives `None` at once for a job with no lock, which
    // is nearly every job, so this pass stays cheap.
    let mut ready: Vec<uuid::Uuid> = Vec::new();
    for id in state.queue.iter().copied() {
        let Some(job) = state.jobs.get(&id) else {
            continue;
        };
        if job.status.state != JobState::Queued {
            continue;
        }

        match depends(state, id) {
            // A lock comes before the capacity. A job that waits for a lock
            // does not hold capacity, in the same way as a job that waits for a
            // different job, so it never reaches the list of jobs that can
            // start.
            Depends::Ready => match lock_conflict(state, &job.spec) {
                Some(reason) => {
                    waits_for.insert(id, Waited::ALock);
                    reasons.push((id, Some(reason)));
                }
                None => ready.push(id),
            },
            Depends::Waiting(reason) => {
                waits_for.insert(id, Waited::AJob);
                reasons.push((id, Some(reason)));
            }
            Depends::Broken { reason, root } => to_skip.push((id, reason, root)),
        }
    }

    // Pass 2: choose one job from the jobs that have no dependency and no lock
    // left.
    //
    // The scheduler starts one job for each call, so a lock that this pass gives
    // away cannot be given again: the next call sees the job as active.
    for id in ready.iter().copied() {
        let Some(job) = state.jobs.get(&id) else {
            continue;
        };

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
    //
    // COUNT THESE JOBS. A skipped job is a final state that this pass wrote on
    // its own, so nothing else tells the waiters. Measured before this count
    // existed: `qex wait` on a job whose dependency expired returned at 33.0s
    // with a 3s limit, and on a job whose dependency was cancelled at 31.0s.
    let mut finished = to_skip.len();
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
        let cause = waits_for.get(&id).copied().unwrap_or(Waited::Capacity);
        expire(state, id, waited, &reason, cause);
        finished += 1;
    }

    Choice {
        start: chosen,
        finished,
    }
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
            let cfg = state.cfg.clone();
            if let Some(job) = state.jobs.get_mut(&id) {
                job.status.state = JobState::Failed;
                job.status.finished_at = Some(sys::now_secs());
                job.status.error = Some(format!("qex could not start the job: {e:#}"));
                let status = job.status.clone();
                drop(state);
                if let Ok(dir) = paths::job_dir(&id) {
                    job::write_status(&dir, &status).ok();
                    // This job has no supervisor, so the coordinator tells the
                    // person that it stopped.
                    crate::hook::fire_detached(&cfg, &dir, &status);
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
            dedupe: Default::default(),
            stop: false,
        }
    }

    /// Puts one job in a state, with a claim, a state and a queue limit.
    fn add_job(
        state: &mut crate::daemon::State,
        job_state: JobState,
        cpu: u64,
        max_queue_time: Option<u64>,
        waited: u64,
    ) -> uuid::Uuid {
        let mut spec = spec_with(cpu, 1 << 20);
        spec.max_queue_time = max_queue_time;
        let id = spec.id;

        let mut status = crate::job::JobStatus::new(&spec);
        status.state = job_state;
        status.submitted_at = sys::now_secs().saturating_sub(waited);

        state.jobs.insert(
            id,
            crate::daemon::Job {
                spec,
                status,
                supervisor_pid: None,
            },
        );
        if job_state == JobState::Queued {
            state.queue.push(id);
        }
        id
    }

    /// A JOB BEHIND A LARGE JOB WAITED FOR CAPACITY, AND `choose` MUST SAY SO.
    ///
    /// This test covers the WIRING, and `the_remedy_fits_the_reason_for_the_wait`
    /// covers the text. The fault that a review found lived here, and not in
    /// `expire`: `choose` decided the remedy by reading the words "waits for
    /// the job" out of the queue reason, and the pure-capacity text "waits for
    /// the job <id> at the front of the queue" holds those same words. A job
    /// with an empty `needs` was told to cover a pipeline that it has none of.
    ///
    /// The remedy must come from `depends`, which is the one place that knows.
    #[test]
    fn a_job_behind_a_large_job_gets_the_remedy_for_capacity() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();

        // A job of three cores operates, so one core of the four is free.
        add_job(&mut state, JobState::Running, 3, None, 0);
        // A job of two cores is at the front of the queue. It cannot fit now,
        // and it holds the capacity for itself.
        add_job(&mut state, JobState::Queued, 2, None, 0);
        // A job of one core waits behind it, and it passed its limit.
        let behind = add_job(&mut state, JobState::Queued, 1, Some(5), 600);

        let choice = choose(&mut state);
        assert_eq!(choice.start, None, "no job can start with one core free");
        assert_eq!(choice.finished, 1, "the job behind must give up");

        let job = &state.jobs[&behind];
        assert_eq!(job.status.state, JobState::Expired);
        let text = job.status.error.clone().unwrap();
        assert!(
            text.contains("smaller claim"),
            "this job waited for CAPACITY, and it has no `needs` at all: {text}"
        );
        assert!(
            !text.contains("whole pipeline"),
            "this job has no pipeline to cover: {text}"
        );
    }

    /// A JOB THAT WAITED FOR A LOCK MUST GET THE REMEDY FOR A LOCK.
    ///
    /// `expire` says that the machine, the claim and a lock each need a
    /// different correction. Measured before this branch existed: a job with
    /// `--lock shared` behind a job that held the same lock expired with the
    /// text "Give the job a smaller claim, wait until the machine is quiet".
    /// A smaller claim gives a lock to nobody, so that text sends the reader to
    /// make a change that cannot help.
    ///
    /// This test covers the WIRING. `lock_conflict` is the one place that knows
    /// that a lock stopped the job, in the same way as `depends` for a
    /// dependency.
    #[test]
    fn a_job_that_waited_for_a_lock_gets_the_remedy_for_a_lock() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();

        // One job of one core operates and holds the lock. Three cores stay
        // free, so capacity is not the cause.
        let holder = add_job(&mut state, JobState::Running, 1, None, 0);
        state.jobs.get_mut(&holder).unwrap().spec.locks = vec!["shared".to_string()];

        // A job that asks for the same lock, and that passed its limit.
        let blocked = add_job(&mut state, JobState::Queued, 1, Some(5), 600);
        state.jobs.get_mut(&blocked).unwrap().spec.locks = vec!["shared".to_string()];

        let choice = choose(&mut state);
        assert_eq!(choice.start, None, "the lock stops the only queued job");
        assert_eq!(choice.finished, 1, "the job that waited must give up");

        let text = state.jobs[&blocked].status.error.clone().unwrap();
        assert!(
            text.contains("lock"),
            "the remedy must name the lock: {text}"
        );
        assert!(
            !text.contains("smaller claim"),
            "the machine had three free cores; a smaller claim changes nothing: {text}"
        );
    }

    /// A JOB THAT WAITS FOR A LOCK BEHIND A JOB THAT WAITS FOR CAPACITY.
    ///
    /// THIS IS THE COMMON CASE, AND IT WAS WRONG. A review measured it end to
    /// end. `lock_conflict` used to live in the capacity pass, which STOPS at
    /// the first job that cannot fit, so a job behind that one was never tested
    /// for a lock. `waits_for` then held no entry for it and the remedy fell
    /// back to the one for capacity: "give the job a smaller claim", which
    /// gives a lock to nobody.
    ///
    /// A busy machine is the machine that `--max-queue-time` exists for, so the
    /// fault hit the case that matters and not a corner. The lock test now runs
    /// in the pass that covers EVERY queued job.
    #[test]
    fn a_lock_behind_a_job_that_waits_for_capacity_gets_the_lock_remedy() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();

        // A job of one core operates and holds the lock. Three cores stay free.
        let holder = add_job(&mut state, JobState::Running, 1, None, 0);
        state.jobs.get_mut(&holder).unwrap().spec.locks = vec!["shared".to_string()];

        // A job of four cores is at the front of the queue. Three cores are
        // free, so it cannot fit, and it holds the capacity for itself.
        add_job(&mut state, JobState::Queued, 4, None, 0);

        // The victim: one core, the same lock, and past its limit. It sits
        // BEHIND the job that cannot fit.
        let victim = add_job(&mut state, JobState::Queued, 1, Some(5), 600);
        state.jobs.get_mut(&victim).unwrap().spec.locks = vec!["shared".to_string()];

        let choice = choose(&mut state);
        assert_eq!(choice.start, None, "no job can start");
        assert_eq!(choice.finished, 1, "the victim must give up");

        let text = state.jobs[&victim].status.error.clone().unwrap();
        assert!(
            text.contains("lock"),
            "the victim waited for a LOCK, and the remedy must say so: {text}"
        );
        assert!(
            !text.contains("smaller claim"),
            "a smaller claim gives a lock to nobody: {text}"
        );
    }

    /// THE REMEDY FOR A DEPENDENCY MUST REACH THE JOB THROUGH `choose`.
    ///
    /// `the_remedy_fits_the_reason_for_the_wait` calls `expire` with the cause
    /// already decided, so it cannot show that `choose` gives the right cause.
    /// A review deleted the line that records the cause of a dependency wait
    /// and the whole suite stayed green. This test holds that line.
    #[test]
    fn a_job_that_waits_for_a_dependency_gets_the_pipeline_remedy_through_choose() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();

        // A job that operates, and a queued job that needs it and passed its
        // limit. The machine has free cores, so capacity is not the cause.
        let root = add_job(&mut state, JobState::Running, 1, None, 0);
        let waiter = add_job(&mut state, JobState::Queued, 1, Some(5), 600);
        state.jobs.get_mut(&waiter).unwrap().spec.needs = vec![root];

        let choice = choose(&mut state);
        assert_eq!(choice.finished, 1, "the job that waited must give up");

        let text = state.jobs[&waiter].status.error.clone().unwrap();
        assert!(
            text.contains("whole pipeline"),
            "a job that waited for a job that it needs takes the pipeline remedy: {text}"
        );
        assert!(
            !text.contains("smaller claim"),
            "the machine had free cores; a smaller claim changes nothing: {text}"
        );
    }

    /// A SKIPPED JOB MUST WAKE THE WAITERS, IN THE SAME PASS.
    ///
    /// `skip` writes a final state that no supervisor and no request thread
    /// announces, exactly as `expire` does. A pass that counted the expired
    /// jobs only left `qex wait` asleep until its 30 second fallback. Measured
    /// with the count of the skipped jobs left out: `qex wait` on a job whose
    /// dependency expired at 3s returned at 33.0s, and on a job whose
    /// dependency was cancelled at 1s returned at 31.0s. With the count, both
    /// return inside a second of the event.
    #[test]
    fn a_pass_that_skips_a_job_reports_it_to_the_waiters() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();

        // A job that failed, and a queued job that needed it.
        let root = add_job(&mut state, JobState::Failed, 1, None, 0);
        let after = add_job(&mut state, JobState::Queued, 1, None, 0);
        state.jobs.get_mut(&after).unwrap().spec.needs = vec![root];

        let choice = choose(&mut state);
        assert_eq!(
            state.jobs[&after].status.state,
            JobState::Skipped,
            "the job that needed a failed job must be skipped"
        );
        assert_eq!(
            choice.finished, 1,
            "a skipped job is a final state that this pass wrote, so the pass \
             must report it and wake the waiters"
        );
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
            Waited::Capacity,
        );

        let job = &state.jobs[&id];
        assert_eq!(job.status.state, JobState::Expired);
        assert!(job.status.finished_at.is_some());
        assert!(state.queue.is_empty(), "an expired job leaves the queue");
        // A job that stopped waits for nothing. `qex top` and `qex list` print
        // this field first, so a reason that stayed would tell a reader that a
        // job which gave up is still waiting for cores.
        assert_eq!(
            job.status.blocked_reason, None,
            "an expired job must hold no queue reason"
        );

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

    /// THE REMEDY MUST FIT THE CAUSE, AND THE PROSE MUST NOT DECIDE IT.
    ///
    /// A job that waited for a job that it needs did not wait for capacity, so
    /// "give the job a smaller claim" sends the reader to make a change that
    /// cannot help. The two causes give two texts.
    ///
    /// The third case is the one that a review found. An earlier version read
    /// the words "waits for the job" out of the queue reason, and the PURE
    /// CAPACITY text "waits for the job <id> at the front of the queue" holds
    /// those same words. A job with no `needs` at all was then told to give a
    /// value that covers a pipeline that it does not have.
    #[test]
    fn the_remedy_fits_the_reason_for_the_wait() {
        // A wait for capacity, in the plain form.
        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];
        expire(
            &mut state,
            id,
            61,
            "waits for cores: 4 of 4 are in use",
            Waited::Capacity,
        );
        let text = state.jobs[&id].status.error.clone().unwrap();
        assert!(
            text.contains("smaller claim"),
            "a job that waited for capacity needs the claim remedy: {text}"
        );

        // A wait for capacity BEHIND A LARGE JOB. This text names a job, and
        // the remedy must still be the one for capacity.
        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];
        expire(
            &mut state,
            id,
            61,
            "waits for the job 1a2b3c4d at the front of the queue",
            Waited::Capacity,
        );
        let text = state.jobs[&id].status.error.clone().unwrap();
        assert!(
            text.contains("smaller claim"),
            "a job behind a large job waited for CAPACITY: {text}"
        );
        assert!(
            !text.contains("whole pipeline"),
            "this job has no pipeline to cover: {text}"
        );

        // A wait for a job that this job needs.
        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];
        expire(
            &mut state,
            id,
            61,
            "waits for the job 1a2b3c4d (build), which is running",
            Waited::AJob,
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

        // A wait for a LOCK. qex gives a lock to one job at a time, so neither
        // a smaller claim nor a quiet machine changes anything.
        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];
        expire(
            &mut state,
            id,
            61,
            "waits for the lock `shared`, which the job 1a2b3c4d (build) holds",
            Waited::ALock,
        );
        let text = state.jobs[&id].status.error.clone().unwrap();
        assert!(
            text.contains("lock"),
            "a job that waited for a lock must be told about the lock: {text}"
        );
        assert!(
            !text.contains("smaller claim"),
            "a smaller claim gives a lock to nobody: {text}"
        );
        assert!(
            !text.contains("whole pipeline"),
            "a lock is not a pipeline: {text}"
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

    /// THE LIMIT IS REACHED AT THE LIMIT, AND NOT ONE SECOND AFTER IT.
    ///
    /// The test is `waited >= limit`. With `>`, every job would give up a full
    /// second later than each document states, and no other test would see it,
    /// because each of them uses a wait that is well past the limit.
    #[test]
    fn a_job_expires_in_the_second_that_it_reaches_its_limit() {
        let state = state_with(JobState::Queued, Some(60), 59);
        assert!(
            overdue(&state, None).is_empty(),
            "a job one second below its limit must wait"
        );

        let state = state_with(JobState::Queued, Some(60), 60);
        assert_eq!(
            overdue(&state, None).len(),
            1,
            "a job that reached its limit exactly must give up"
        );
    }

    /// A JOB THAT ALREADY RAN MUST NEVER GET THE STATE `expired`.
    ///
    /// This guard is defensive. A job between two attempts of `--retries` is
    /// `queued` and holds the `started_at` of the attempt that failed, and a
    /// coordinator that starts again puts every `queued` record back in the
    /// queue. The state alone cannot separate that job from one that never ran.
    #[test]
    fn a_job_that_holds_a_start_time_never_expires() {
        let mut state = state_with(JobState::Queued, Some(1), 3600);
        let id = state.queue[0];
        state.jobs.get_mut(&id).unwrap().status.started_at = Some(sys::now_secs() - 3000);

        expire(&mut state, id, 3600, "waits for cores", Waited::Capacity);
        assert_eq!(
            state.jobs[&id].status.state,
            JobState::Queued,
            "a job that already ran must keep its state"
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
            expire(&mut state, id, 3600, "waits for cores", Waited::Capacity);
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
