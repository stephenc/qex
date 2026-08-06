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

/// Who holds the capacity that a job waits for.
///
/// The class decides one thing: does the queue keep the capacity for the job,
/// or does it start the jobs behind it?
///
/// qex controls the release of the capacity that its own jobs hold, and it
/// controls nothing else. A queue that keeps capacity for a job which waits for
/// another user therefore holds the machine empty for a time that qex cannot
/// measure. That was the measured fault: one job that a peer blocked kept two
/// small jobs in the queue for ever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// The jobs of this queue hold the capacity. qex schedules the release.
    Sibling,
    /// The coordinator of another user holds the capacity. qex does not
    /// schedule that release, and it can be hours.
    Peer { count: usize },
    /// A program outside qex holds the memory, or the machine has pressure.
    Machine,
    /// The job is larger than the budget, and it waits for a quiet machine.
    OversizedWaitsForIdle,
    /// The job is larger than the budget, and the config keeps it in the queue.
    /// Such a job never starts, so capacity that qex keeps for it buys nothing.
    OversizedParked,
}

impl Blocker {
    /// Tells if the queue may keep capacity for a job with this class.
    ///
    /// A `Peer` or a `Machine` holds capacity that qex does not schedule. An
    /// empty machine gives such a job nothing, and it stops every other job.
    fn may_reserve(&self) -> bool {
        matches!(self, Blocker::Sibling | Blocker::OversizedWaitsForIdle)
    }

    /// The word that `qex info` gives for a program to read.
    pub fn word(&self) -> &'static str {
        match self {
            Blocker::Sibling => "waits-for-capacity",
            Blocker::Peer { .. } => "waits-for-peer",
            Blocker::Machine => "waits-for-machine",
            Blocker::OversizedWaitsForIdle => "waits-for-idle",
            Blocker::OversizedParked => "parked",
        }
    }
}

/// The result of the test of a job against the machine now.
enum Admit {
    Yes,
    No {
        blocker: Blocker,
        /// The text for a job that other jobs may pass.
        reason: String,
        /// The text for a job at the front of the queue that no job may pass.
        ///
        /// `None` means that the class never keeps capacity, so a reader never
        /// sees this text.
        held_reason: Option<String>,
    },
}

/// The measurements of the machine for one pass of the scheduler.
///
/// The scheduler tests EVERY job that is ready in each pass, so each job gets a
/// reason of its own. Without this record, each test reads `/proc` and the
/// files of the other users again: that is one storm of system calls for each
/// cycle of 500ms, and two jobs in one pass can also get answers from two
/// different moments. One measurement for each pass removes both faults.
struct Machine {
    available: u64,
    pressure: Option<f64>,
    peers: crate::peers::Claims,
}

impl Machine {
    fn read(cfg: &Config) -> Self {
        Self {
            available: sys::available_memory(),
            pressure: sys::memory_pressure(),
            // This function gives an empty total when the config turns the
            // peers off, so there is no test here.
            peers: crate::peers::claims(cfg),
        }
    }
}

/// Writes a number of cores with the correct word: `1 core`, `4 cores`.
fn cores(n: u64) -> String {
    if n == 1 {
        "1 core".to_string()
    } else {
        format!("{n} cores")
    }
}

/// Writes the other users with the correct verb: `1 other user holds`.
fn other_users(n: usize) -> String {
    if n == 1 {
        "1 other user holds".to_string()
    } else {
        format!("{n} other users hold")
    }
}

/// Builds the two texts for a job that the jobs of this queue hold back.
fn sibling_wait(fact: String, resource: &str) -> Admit {
    Admit::No {
        blocker: Blocker::Sibling,
        reason: format!(
            "{fact} Those jobs release the {resource} when they stop. qex can start a smaller job \
             before this one."
        ),
        held_reason: Some(format!(
            "{fact} qex starts no other job before this one. Read `qex list` to see the jobs that \
             hold the {resource}."
        )),
    }
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
                    job.status.name
                ));
            }
        }
    }
    None
}

/// Tests if a job can start now.
fn admit(cfg: &Config, spec: &JobSpec, cpu_used: u64, mem_used: u64, machine: &Machine) -> Admit {
    let cpu_budget = cfg.budget_cpu().unwrap_or(1);
    let mem_budget = cfg.budget_mem().unwrap_or(0);

    // Test 1: the budget of this user.
    if cpu_used + spec.cpu > cpu_budget {
        return sibling_wait(
            format!(
                "waits for cores: this job needs {}, and the jobs of this queue hold {} of the {} \
                 in the budget.",
                cores(spec.cpu),
                cpu_used,
                cores(cpu_budget)
            ),
            "cores",
        );
    }
    if mem_used + spec.mem > mem_budget {
        return sibling_wait(
            format!(
                "waits for memory: this job needs {}, and the jobs of this queue hold {} of the {} \
                 budget.",
                format_size(spec.mem),
                format_size(mem_used),
                format_size(mem_budget)
            ),
            "memory",
        );
    }

    // Test 2: the other users. This test reads the files of the other
    // coordinators. It finds a load that this coordinator did not start.
    //
    // The words must name the cause. The user of the fault saw "waits for the
    // job at the front of the queue" and had no way to learn that a colleague
    // held the machine.
    let peers = &machine.peers;
    if peers.cpu > 0 || peers.mem > 0 {
        if cpu_used + peers.cpu + spec.cpu > cpu_budget {
            return Admit::No {
                blocker: Blocker::Peer { count: peers.count },
                reason: format!(
                    "this job cannot fit while another user holds capacity: the job needs {}, \
                     this queue holds {} of the {} in the budget, and {} {}. \
                     qex does not control that user, so this wait has no known end. qex starts \
                     the jobs behind this one while the capacity is not free. Read `qex info` for \
                     the load of the machine.",
                    cores(spec.cpu),
                    cpu_used,
                    cores(cpu_budget),
                    other_users(peers.count),
                    cores(peers.cpu)
                ),
                held_reason: None,
            };
        }
        if mem_used + peers.mem + spec.mem > mem_budget {
            return Admit::No {
                blocker: Blocker::Peer { count: peers.count },
                reason: format!(
                    "this job cannot fit while another user holds capacity: the job needs {}, this \
                     queue holds {} of the {} budget, and {} {}. qex does not \
                     control that user, so this wait has no known end. qex starts the jobs behind \
                     this one while the capacity is not free. Read `qex info` for the load of the \
                     machine.",
                    format_size(spec.mem),
                    format_size(mem_used),
                    format_size(mem_budget),
                    other_users(peers.count),
                    format_size(peers.mem)
                ),
                held_reason: None,
            };
        }
    }

    // Test 3: the machine. This test finds every load, and not the load of qex
    // only. It is the test that a program outside qex cannot avoid.
    let reserve = cfg.reserve_mem().unwrap_or(0);
    let available = machine.available;
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
        match machine.pressure {
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
        // Say that this wait has no known end, and that the queue continues.
        // The memory belongs to a program that qex never saw, so an empty
        // queue does not give it back.
        reason.push_str(
            ". qex does not control the programs outside this queue, so this wait has no known \
             end. qex starts the jobs behind this one while the memory is not free.",
        );
        return Admit::No {
            blocker: Blocker::Machine,
            reason,
            held_reason: None,
        };
    }

    if let Some(pressure) = machine.pressure {
        if pressure > cfg.system.max_pressure {
            return Admit::No {
                blocker: Blocker::Machine,
                reason: format!(
                    "waits for the machine: the memory pressure is {:.1} and the limit is {:.1}. \
                     qex does not control the programs outside this queue, so this wait has no \
                     known end. qex starts the jobs behind this one while the pressure is high.",
                    pressure, cfg.system.max_pressure
                ),
                held_reason: None,
            };
        }
    }

    Admit::Yes
}

/// The decision for one job that has no dependency left.
enum Verdict {
    /// The job can start now.
    Start,
    /// The job waits. See [`Blocker`] for the meaning of the class.
    Wait {
        blocker: Blocker,
        reason: String,
        held_reason: Option<String>,
    },
}

/// Tests one job against the budget, the other users, the machine and its size.
///
/// `quiet` says that no job operates and the settle time passed.
fn verdict(
    cfg: &Config,
    spec: &JobSpec,
    cpu_used: u64,
    mem_used: u64,
    machine: &Machine,
    quiet: bool,
) -> Verdict {
    match size_check(cfg, spec) {
        Size::Fits => match admit(cfg, spec, cpu_used, mem_used, machine) {
            Admit::Yes => Verdict::Start,
            Admit::No {
                blocker,
                reason,
                held_reason,
            } => Verdict::Wait {
                blocker,
                reason,
                held_reason,
            },
        },
        Size::TooBig(reason) => {
            // This job can never fit. Start it alone when the machine is quiet,
            // so the agent gets a result and not an endless wait.
            if cfg.queue.oversized == OversizedPolicy::RunWhenIdle {
                if quiet {
                    return Verdict::Start;
                }
                return Verdict::Wait {
                    blocker: Blocker::OversizedWaitsForIdle,
                    reason: format!(
                        "{reason}; qex starts this job when no other job operates. qex starts the \
                         jobs behind this one until then."
                    ),
                    held_reason: Some(format!(
                        "{reason}; qex starts this job when no other job operates. qex starts no \
                         other job before this one, so the queue becomes empty."
                    )),
                };
            }

            let text = match cfg.queue.oversized {
                // The config keeps this job in the queue, so it never starts.
                // Capacity that qex keeps for it thus gives it nothing and
                // stops every other job. Say that the queue continues.
                OversizedPolicy::Queue => format!(
                    "{reason}; the config file keeps this job in the queue. This job never starts, \
                     so qex starts the jobs behind it."
                ),
                _ => reason.clone(),
            };
            Verdict::Wait {
                blocker: Blocker::OversizedParked,
                reason: text,
                held_reason: None,
            }
        }
    }
}

/// Runs the scheduler. This function does not give control back.
pub fn run(coord: Arc<Coordinator>) {
    loop {
        if coord.state.lock().unwrap().stop {
            return;
        }

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
        let state = coord.state.lock().unwrap();
        let _ = coord
            .changed
            .wait_timeout(state, Duration::from_millis(500))
            .unwrap();
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
                other.status.name,
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
                .map(|j| j.status.name.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let root_state = state
                .jobs
                .get(&root)
                .map(|j| j.status.state.to_string())
                .unwrap_or_else(|| other.status.state.to_string());

            // Name the log file only when the job wrote one. A cancelled job
            // never started, so a reader who follows that instruction finds an
            // empty file and learns nothing.
            let advice = if root_state == "cancelled" {
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
                other.status.name
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

/// The job at the front of the queue that cannot start now.
struct Head {
    id: uuid::Uuid,
    name: String,
    mem: u64,
    blocker: Blocker,
    /// True when no other job may pass this one.
    reserved: bool,
    /// The number of jobs that started after this job reached the front.
    passed_by: u32,
}

/// Chooses the next job to start.
///
/// # The rule
///
/// The scheduler walks the jobs that have no dependency left, in queue order.
/// The FIRST job that cannot start is the head. Its class ([`Blocker`]) says
/// who holds the capacity:
///
/// * The jobs of this queue hold it, or the job waits for a quiet machine. qex
///   schedules that release, so it is correct to wait. qex lets `max_bypass`
///   jobs pass the head, and then it keeps the capacity: no job starts at all.
/// * Another user, or a program outside qex, holds it. qex does not schedule
///   that release, so the head never keeps capacity and the queue continues.
///
/// The count of the jobs that passed the head is NOT reset when the class
/// changes. A job that another user held for an hour collects bypasses freely,
/// and in the cycle in which that user releases the capacity the count is
/// already at the limit. The job is then unpassable at once, and it collects
/// capacity as the jobs of this queue stop. A wait behind another user thus
/// costs one scheduler cycle, and not the life of the other user's job.
fn choose(state: &mut crate::daemon::State) -> Option<uuid::Uuid> {
    let (cpu_used, mem_used) = state.claimed();
    let cfg = state.cfg.clone();
    let active = state.count_state(|s| s.is_active());
    let idle_since = state.idle_since;
    let max_bypass = cfg.queue.max_bypass;
    let settle = cfg.settle().unwrap_or(Duration::from_secs(3));
    let quiet = active == 0 && idle_since.map(|t| t.elapsed() >= settle).unwrap_or(false);

    // Measure the machine one time for this pass. See [`Machine`].
    let machine = Machine::read(&cfg);

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
    //
    // This pass tests EVERY ready job while the head does not keep capacity, so
    // each job gets a reason of its own. The earlier code stopped at the head,
    // and each job behind it read a sentence about queue position that named no
    // cause. A job that the other user also held back was told the wrong thing.
    let mut head: Option<Head> = None;
    let mut started_now: Option<uuid::Uuid> = None;

    for id in ready.iter().copied() {
        let Some(job) = state.jobs.get(&id) else {
            continue;
        };

        // A lock comes before the capacity. A job that waits for a lock does
        // not hold capacity, in the same way as a job that waits for a
        // different job, so the loop continues to the next job.
        //
        // A lock is all or nothing, so there is no part of it to protect. Such
        // a job therefore never becomes the head, and it never keeps capacity:
        // one long build with a lock would otherwise stop the whole machine.
        if let Some(reason) = lock_conflict(state, &job.spec) {
            reasons.push((id, Some(reason)));
            continue;
        }

        let Some(job) = state.jobs.get(&id) else {
            continue;
        };

        match verdict(&cfg, &job.spec, cpu_used, mem_used, &machine, quiet) {
            Verdict::Start => {
                chosen = Some(id);
                started_now = Some(id);
                break;
            }
            Verdict::Wait {
                blocker,
                reason,
                held_reason,
            } => {
                if head.is_some() {
                    // A job behind a head that does not keep capacity. It gets
                    // its own reason, because no job holds it back.
                    reasons.push((id, Some(reason)));
                    continue;
                }

                let passed_by = job.status.passed_by;
                let reserved = blocker.may_reserve() && passed_by >= max_bypass;
                reasons.push((
                    id,
                    Some(if reserved {
                        held_reason.unwrap_or(reason)
                    } else {
                        reason
                    }),
                ));
                head = Some(Head {
                    id,
                    name: job.status.name.clone(),
                    mem: job.status.mem,
                    blocker,
                    reserved,
                    passed_by,
                });
                if reserved {
                    break;
                }
            }
        }
    }

    // The head keeps the capacity. Tell each job behind it WHY, and not its
    // position only.
    //
    // The count in this sentence does not change while a reader sees it: no job
    // starts while the head keeps the capacity, so the count cannot move. A
    // number that moves would rewrite the record of every queued job at each
    // start, with two operations to the disk for each record.
    if let Some(h) = &head {
        if h.reserved {
            for later in ready.iter().copied() {
                if later == h.id {
                    continue;
                }
                if reasons.iter().any(|(r, _)| *r == later) {
                    continue;
                }
                reasons.push((
                    later,
                    Some(format!(
                        "waits for the job {} ({}), which is at the front of the queue and needs \
                         {}. qex keeps the capacity for that job, because {} job(s) already \
                         started before it.",
                        &h.id.to_string()[..8],
                        h.name,
                        format_size(h.mem),
                        h.passed_by
                    )),
                ));
            }
        }
    }

    // Mark each job whose dependency did not succeed. Do this step before the
    // reasons, so a skipped job does not also get a queue reason.
    for (id, reason, root) in to_skip {
        skip(state, id, reason, root);
    }

    // Collect the jobs whose record changed, and write each record one time.
    let mut dirty: std::collections::BTreeSet<uuid::Uuid> = Default::default();

    for (id, reason) in reasons {
        if let Some(job) = state.jobs.get_mut(&id) {
            if job.status.state != JobState::Queued {
                continue;
            }
            if job.status.blocked_reason != reason {
                job.status.blocked_reason = reason;
                dirty.insert(id);
            }
        }
    }

    // Count the jobs that pass the head.
    //
    // The count belongs to the head only. A count on every queued job would
    // rewrite every record at each start, and the rule needs the count of the
    // front-most job only.
    if let Some(h) = &head {
        if let Some(job) = state.jobs.get_mut(&h.id) {
            if job.status.blocked_since.is_none() {
                job.status.blocked_since = Some(sys::now_secs());
                dirty.insert(h.id);
            }
            if started_now.is_some() {
                job.status.passed_by = job.status.passed_by.saturating_add(1);
                dirty.insert(h.id);
            }
        }
    }

    // A job that can start waited for nothing. Its count starts again.
    if let Some(id) = started_now {
        if let Some(job) = state.jobs.get_mut(&id) {
            if job.status.blocked_since.is_some() || job.status.passed_by > 0 {
                job.status.blocked_since = None;
                job.status.passed_by = 0;
                dirty.insert(id);
            }
        }
    }

    for id in dirty {
        let Some(job) = state.jobs.get(&id) else {
            continue;
        };
        let status = job.status.clone();
        if let Ok(dir) = paths::job_dir(&id) {
            job::write_status(&dir, &status).ok();
        }
    }

    // Record what this pass found, so `qex info` and `qex top` can say in one
    // line whether the queue is healthy.
    state.head = head.map(|h| crate::daemon::HeadInfo {
        id: h.id,
        name: h.name,
        blocker: h.blocker.word().to_string(),
        reserved: h.reserved,
        passed_by: h.passed_by,
    });
    state.peer_claims = machine.peers;

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
        job.status.blocked_since = None;
        job.status.passed_by = 0;
        job.status.forced = forced.is_some();
        job.status.forced_reason = forced.clone();
        let status = job.status.clone();
        let name = job.spec.name.clone();
        state.queue.retain(|q| *q != id);
        // `qex info` reads this time to say if the queue moves. A queue that
        // started nothing for a long time is the fault that a reader looks for.
        state.last_start_at = Some(sys::now_secs());
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
    use std::path::PathBuf;

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
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            locks: vec![],
            retries: 0,
            needs: vec![],
            after: vec![],
            submitted_at: 0,
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

        let m = Machine::read(&cfg);

        // Two cores are in use. A job of two cores fits.
        assert!(matches!(admit(&cfg, &job, 2, 64 << 20, &m), Admit::Yes));

        // Four cores are in use. The same job must wait.
        let Admit::No {
            reason, blocker, ..
        } = admit(&cfg, &job, 4, 64 << 20, &m)
        else {
            panic!("a job must not start when the cores are in use");
        };
        assert!(reason.contains("cores"), "got: {reason}");
        assert_eq!(blocker, Blocker::Sibling);

        // The memory is in use. The job must wait.
        let Admit::No {
            reason, blocker, ..
        } = admit(&cfg, &job, 0, 224 << 20, &m)
        else {
            panic!("a job must not start when the memory is in use");
        };
        assert!(reason.contains("memory"), "got: {reason}");
        assert_eq!(blocker, Blocker::Sibling);
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
            admit(&cfg, &spec_with(4, 256 << 20), 0, 0, &Machine::read(&cfg)),
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
        let Admit::No {
            reason, blocker, ..
        } = admit(&cfg, &spec_with(1, 1 << 20), 0, 0, &Machine::read(&cfg))
        else {
            panic!("the reserve must stop this job");
        };
        assert!(reason.contains("reserve"), "got: {reason}");
        assert_eq!(blocker, Blocker::Machine);
    }

    /// The pressure limit stops a job while the machine reclaims memory.
    #[test]
    fn the_pressure_limit_stops_a_job() {
        let mut cfg = cfg_with("4", "8GB");
        cfg.system.max_pressure = -1.0;
        if sys::memory_pressure().is_some() {
            let Admit::No {
                reason, blocker, ..
            } = admit(&cfg, &spec_with(1, 1 << 20), 0, 0, &Machine::read(&cfg))
            else {
                panic!("the pressure limit must stop this job");
            };
            assert!(reason.contains("pressure"), "got: {reason}");
            assert_eq!(blocker, Blocker::Machine);
        }
    }

    /// Makes a config with a peer that holds capacity.
    ///
    /// The record goes in the directory of THIS user with a pid that is not
    /// this process. `peers::claims` skips the record of this coordinator only,
    /// because one user can have more than one coordinator. A record with a
    /// different user id is not possible here: the reader tests the owner of
    /// the file, and this process owns each file that it writes.
    fn cfg_with_peer(cpu: &str, mem: &str, peer_cpu: u64, peer_mem: u64) -> (Config, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "qex-sched-peer-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();

        let uid = crate::peers::current_uid();
        let mine = dir.join(format!("u{uid}"));
        std::fs::create_dir_all(&mine).unwrap();
        // Pid 1 always exists. `pid_alive` also accepts the answer "you may not
        // signal this process", so the test needs no process of its own.
        let peer = serde_json::json!({
            "uid": uid,
            "pid": 1,
            "boot_id": sys::boot_id(),
            "cpu": peer_cpu,
            "mem": peer_mem,
            "updated_at": sys::now_secs(),
        });
        std::fs::write(mine.join("peer-1.json"), serde_json::to_vec(&peer).unwrap()).unwrap();

        let cfg: Config = toml::from_str(&format!(
            "[budget]\ncpu = \"{cpu}\"\nmem = \"{mem}\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = true\ndir = \"{}\"\nstale_after = \"1h\"\n",
            dir.display()
        ))
        .unwrap();
        (cfg, dir)
    }

    /// The measured fault: a job that another user holds back read a sentence
    /// about queue position, and the user had no way to learn the cause.
    ///
    /// The words must name the other user, and the class must be `Peer`, so the
    /// queue starts the jobs behind this one.
    #[test]
    fn a_job_that_another_user_holds_back_says_so_and_never_keeps_capacity() {
        let (cfg, dir) = cfg_with_peer("4", "256MB", 3, 0);
        let machine = Machine::read(&cfg);
        assert_eq!(machine.peers.count, 1, "the test peer must count");

        let Admit::No {
            blocker,
            reason,
            held_reason,
        } = admit(&cfg, &spec_with(4, 64 << 20), 0, 0, &machine)
        else {
            panic!("a job of 4 cores must not start while another user holds 3");
        };
        assert_eq!(blocker, Blocker::Peer { count: 1 });
        assert!(
            reason.contains("another user holds capacity"),
            "the reason must name the other user: {reason}"
        );
        assert!(
            reason.contains("no known end"),
            "the reason must say that qex cannot schedule the release: {reason}"
        );
        assert!(
            held_reason.is_none() && !blocker.may_reserve(),
            "a job that another user holds back must never keep the capacity"
        );

        // A smaller job still fits, so the queue continues.
        assert!(matches!(
            admit(&cfg, &spec_with(1, 64 << 20), 0, 0, &machine),
            Admit::Yes
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// qex keeps capacity only for a holder whose release it schedules.
    ///
    /// A queue that keeps capacity against another user, or against a program
    /// outside qex, holds the machine empty for a time that qex cannot measure.
    /// That is the measured fault.
    #[test]
    fn the_queue_keeps_capacity_only_for_a_holder_that_it_schedules() {
        assert!(Blocker::Sibling.may_reserve());
        assert!(Blocker::OversizedWaitsForIdle.may_reserve());
        assert!(!Blocker::Peer { count: 1 }.may_reserve());
        assert!(!Blocker::Machine.may_reserve());
        assert!(!Blocker::OversizedParked.may_reserve());
    }

    /// A job that the config keeps in the queue never starts, so capacity that
    /// qex keeps for it gives it nothing and stops every other job.
    #[test]
    fn a_job_that_the_config_parks_does_not_stop_the_jobs_behind_it() {
        let mut cfg = cfg_with("2", "256MB");
        cfg.queue.oversized = OversizedPolicy::Queue;
        let machine = Machine::read(&cfg);

        let Verdict::Wait {
            blocker, reason, ..
        } = verdict(&cfg, &spec_with(64, 64 << 20), 0, 0, &machine, false)
        else {
            panic!("a job of 64 cores must not start with a budget of 2");
        };
        assert_eq!(blocker, Blocker::OversizedParked);
        assert!(
            reason.contains("starts the jobs behind it"),
            "the reason must say that the queue continues: {reason}"
        );
    }

    /// A job that is larger than the budget waits for a quiet machine. qex
    /// schedules that release, because the queue becomes empty, so this class
    /// keeps the capacity after the permitted bypasses.
    #[test]
    fn a_job_that_waits_for_a_quiet_machine_keeps_the_capacity() {
        let cfg = cfg_with("2", "256MB");
        let machine = Machine::read(&cfg);

        let Verdict::Wait {
            blocker,
            held_reason,
            ..
        } = verdict(&cfg, &spec_with(64, 64 << 20), 0, 0, &machine, false)
        else {
            panic!("a job of 64 cores must not start on a busy machine");
        };
        assert_eq!(blocker, Blocker::OversizedWaitsForIdle);
        assert!(held_reason.is_some());

        // The same job starts alone on a quiet machine.
        assert!(matches!(
            verdict(&cfg, &spec_with(64, 64 << 20), 0, 0, &machine, true),
            Verdict::Start
        ));
    }

    /// The two texts of a sibling wait must give different instructions. The
    /// first says that a smaller job can pass; the second says that no job can.
    #[test]
    fn a_sibling_wait_says_if_another_job_can_pass_it() {
        let cfg = cfg_with("4", "256MB");
        let machine = Machine::read(&cfg);
        let Admit::No {
            blocker,
            reason,
            held_reason,
        } = admit(&cfg, &spec_with(4, 64 << 20), 2, 0, &machine)
        else {
            panic!("a job of 4 cores must not start while 2 are in use");
        };
        assert_eq!(blocker, Blocker::Sibling);
        assert!(
            reason.contains("qex can start a smaller job before this one"),
            "got: {reason}"
        );
        let held = held_reason.expect("a sibling wait has a text for a job that keeps capacity");
        assert!(
            held.contains("qex starts no other job before this one"),
            "got: {held}"
        );
        // The count of the jobs that passed is NOT in the text. A number that
        // changes at each start would rewrite the record of the job, with two
        // operations to the disk, at each start.
        assert!(!held.contains("time(s)"), "got: {held}");
    }
}
