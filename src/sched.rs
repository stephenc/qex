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
        return Admit::No(format!(
            "waits for memory: the machine has {} free, and the job needs {} with {} in reserve",
            format_size(available),
            format_size(spec.mem),
            format_size(reserve)
        ));
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
    log(&format!("job {id} did not run, because a job that it needed did not succeed"));
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
                let quiet = active == 0
                    && idle_since.map(|t| t.elapsed() >= settle).unwrap_or(false);

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
        let name = job.spec.name.clone();
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
        log(&format!("job {id} ({name}) starts although it is too large: {reason}"));
    }

    match crate::supervisor::spawn(id) {
        Ok(pid) => {
            let status = {
                let mut state = coord.state.lock().unwrap();
                match state.jobs.get_mut(&id) {
                    Some(job) => {
                        job.supervisor_pid = Some(pid);
                        // Record the supervisor in the file. A coordinator that
                        // starts again then knows that this job continues.
                        job.status.supervisor_pid = Some(pid);
                        Some(job.status.clone())
                    }
                    None => None,
                }
            };
            if let Some(status) = status {
                write_started(&id, &status).ok();
            }

            log(&format!("job {id} ({name}) started; the supervisor pid is {pid}"));

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
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
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
        assert!(reason.contains("cores"), "the reason must name the cores: {reason}");

        let Size::TooBig(reason) = size_check(&cfg, &spec_with(1, 64 << 30)) else {
            panic!("a job of 64GB must not fit a budget of 8GB");
        };
        assert!(reason.contains("memory"), "the reason must name the memory: {reason}");

        // A job that is too large in both values must give both reasons. The
        // agent then corrects the claim one time only.
        let Size::TooBig(reason) = size_check(&cfg, &spec_with(64, 64 << 30)) else {
            panic!("this job must not fit");
        };
        assert!(reason.contains("cores") && reason.contains("memory"), "got: {reason}");
    }

    #[test]
    fn the_budget_limits_the_jobs_that_operate_together() {
        let cfg = cfg_with("4", "8GB");
        let job = spec_with(2, 2 << 30);

        // Two cores are in use. A job of two cores fits.
        assert!(matches!(admit(&cfg, &job, 2, 2 << 30), Admit::Yes));

        // Four cores are in use. The same job must wait.
        let Admit::No(reason) = admit(&cfg, &job, 4, 2 << 30) else {
            panic!("a job must not start when the cores are in use");
        };
        assert!(reason.contains("cores"), "got: {reason}");

        // The memory is in use. The job must wait.
        let Admit::No(reason) = admit(&cfg, &job, 0, 7 << 30) else {
            panic!("a job must not start when the memory is in use");
        };
        assert!(reason.contains("memory"), "got: {reason}");
    }

    /// A job that fills the budget exactly must start. An error in the compare
    /// operator here would keep such a job in the queue for ever.
    #[test]
    fn a_job_that_fills_the_budget_exactly_starts() {
        let cfg = cfg_with("4", "8GB");
        assert!(matches!(admit(&cfg, &spec_with(4, 8 << 30), 0, 0), Admit::Yes));
        assert_eq!(size_check(&cfg, &spec_with(4, 8 << 30)), Size::Fits);
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
