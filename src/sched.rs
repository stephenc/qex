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

use crate::config::{Config, OversizedPolicy, Pool};
use crate::daemon::{log, Coordinator};
use crate::job::{self, Assignment, JobState};
use crate::paths;
use crate::spec::{JobSpec, PoolClaim};
use crate::sys;
use crate::units::format_size;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The result of the test of a job size against the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Size {
    /// The job fits the budget. It waits for free capacity only.
    Fits,
    /// The job is larger than the full budget. It can never fit.
    TooBig(String),
    /// The job asks for something that this machine can never give.
    ///
    /// This result is different from `TooBig`, and the difference is the
    /// reason for it. An oversized memory job can run alone and swap, and that
    /// result is data. A job that claims 8 devices from a pool of 4 gets no
    /// data from running alone: an empty machine does not make a fifth device.
    /// qex thus refuses it at submission, whatever `[queue] oversized` says.
    Impossible(String),
}

/// The claims of one job on the pools, with each lock as a pool of one unit.
///
/// The conversion happens HERE, in the coordinator, and never on the wire. See
/// the note on `JobSpec::claims`.
pub fn effective_claims(spec: &JobSpec) -> BTreeMap<String, PoolClaim> {
    let mut all = spec.claims.clone();
    for name in &spec.locks {
        all.entry(name.clone()).or_insert(PoolClaim {
            count: 1,
            size: None,
        });
    }
    all
}

/// Gives the pool with one name, or the pool of one unit that a lock uses.
fn pool_of(pools: &[Pool], name: &str) -> Pool {
    pools
        .iter()
        .find(|p| p.name == name)
        .cloned()
        .unwrap_or_else(|| Pool::implicit(name))
}

/// Tests if a claim takes all of a pool or nothing.
///
/// Such a claim behaves as a lock: there is no part of the pool to keep for it,
/// so a job that waits for it does not park the queue behind it.
fn is_all_or_nothing(pool: &Pool, claim: &PoolClaim) -> bool {
    pool.total <= 1 && (!pool.is_indexed() || claim.size.is_none())
}

/// Tests the claims of one job against the configuration.
///
/// A fault that this function finds can never become correct on this machine,
/// so the message says so and gives the correction.
fn pool_check(cfg: &Config, spec: &JobSpec) -> Result<(), String> {
    let pools = cfg.pools().map_err(|e| e.to_string())?;
    let claims = effective_claims(spec);

    for (name, claim) in &claims {
        let declared = pools.iter().find(|p| p.name == *name);

        if claim.count == 0 {
            // The only path to this state is `--vram` with no `--gpu`.
            return Err(format!(
                "this job claims {} of {} and claims no device. {} is a quantity on each \
                 device, so a job must also claim a device. Add `--gpu 1`.",
                format_size(claim.size.unwrap_or(0)),
                declared
                    .and_then(|p| p.size_name.clone())
                    .unwrap_or_else(|| "VRAM".to_string()),
                declared
                    .and_then(|p| p.size_name.clone())
                    .unwrap_or_else(|| "VRAM".to_string()),
            ));
        }

        let Some(pool) = declared else {
            // A name that the configuration does not declare is a lock, and a
            // lock needs no configuration. That rule holds for one unit only:
            // qex cannot invent a second unit of something that nobody
            // declared.
            //
            // The name `gpu` is the one exception, because `--gpu` promises a
            // device index and an environment variable. A silent lock would
            // give the job neither, and the job would then use a card that qex
            // is not accounting for.
            if name == crate::config::GPU_POOL {
                return Err(format!(
                    "there is no pool `{name}` in the configuration, so qex cannot give this \
                     job a GPU. Add a pool to ~/.config/qex.toml:\n\n\
                     \x20   [[pool]]\n\
                     \x20   name    = \"gpu\"\n\
                     \x20   size    = \"vram\"\n\
                     \x20   devices = [\"24GB\", \"24GB\"]\n\n\
                     Then start the job again."
                ));
            }
            if claim.count > 1 || claim.size.is_some() {
                return Err(format!(
                    "the job claims {} of `{name}`, and the configuration does not declare \
                     that pool, so qex treats it as a lock of size 1. Add the pool to \
                     ~/.config/qex.toml:\n\n\
                     \x20   [[pool]]\n\
                     \x20   name  = \"{name}\"\n\
                     \x20   count = 4\n\n\
                     Then start the job again.",
                    claim.count
                ));
            }
            continue;
        };

        if claim.size.is_some() && !pool.is_indexed() {
            return Err(format!(
                "the pool `{name}` has no devices, so it holds no size. Give `--claim \
                 {name}=N` only."
            ));
        }
        if claim.count > pool.total {
            return Err(format!(
                "the job claims {} of the pool `{name}` and the pool has {}. This job can \
                 never start. Claim {} or fewer, or add the devices to `[[pool]]` in \
                 ~/.config/qex.toml.",
                claim.count, pool.total, pool.total
            ));
        }
        if let Some(size) = claim.size {
            // NEVER add the capacity of the devices together. Four devices of
            // 24GB are not 96GB for one job, and an arithmetic that says they
            // are admits a job that cannot run.
            if size > pool.largest_device() {
                return Err(format!(
                    "the job claims {} of {} for each device, and the largest device of the \
                     pool `{name}` has {}. qex does not add the memory of the devices \
                     together, so this job can never start. Claim {} or less.",
                    format_size(size),
                    pool.size_name.clone().unwrap_or_else(|| "size".to_string()),
                    format_size(pool.largest_device()),
                    format_size(pool.largest_device())
                ));
            }
        }
    }
    Ok(())
}

/// Tests one job against the full budget.
///
/// This test uses the budget, not the free capacity. A job that fails this test
/// can never start by the normal rule.
pub fn size_check(cfg: &Config, spec: &JobSpec) -> Size {
    // Test the pools first. A claim that no configuration can satisfy is a
    // refusal, and the reader must hear that and not a message about memory.
    if let Err(reason) = pool_check(cfg, spec) {
        return Size::Impossible(reason);
    }

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

/// The resources that the jobs which operate now hold.
#[derive(Debug, Clone, Default)]
pub struct Held {
    pub cpu: u64,
    pub mem: u64,
    /// The units of each pool. The key is the pool name.
    pub pools: BTreeMap<String, u64>,
    /// The quantity in use on each device. The keys are the pool name and the
    /// device index.
    pub devices: BTreeMap<String, BTreeMap<u32, u64>>,
}

impl Held {
    /// Adds the claims of one job that operates.
    pub fn add(&mut self, status: &crate::job::JobStatus, pools: &[Pool]) {
        self.cpu += status.cpu;
        self.mem += status.mem;

        for (name, given) in &status.assigned {
            *self.pools.entry(name.clone()).or_insert(0) += given.units;
            if given.devices.is_empty() {
                continue;
            }
            let pool = pool_of(pools, name);
            let per_device = self.devices.entry(name.clone()).or_default();
            for index in &given.devices {
                // A job with no size takes the whole device, so it holds the
                // capacity of that device.
                let capacity = pool.devices.get(*index as usize).copied().unwrap_or(0);
                *per_device.entry(*index).or_insert(0) += given.size.unwrap_or(capacity);
            }
        }

        // A record that an earlier version wrote holds `locks` and no
        // `assigned`. Count those locks, or a coordinator that starts after an
        // upgrade gives a lock that a live job already holds.
        for name in &status.locks {
            if !status.assigned.contains_key(name) {
                *self.pools.entry(name.clone()).or_insert(0) += 1;
            }
        }
    }

    /// Gives the units of each pool, for `peers::publish`.
    pub fn pool_units(&self) -> BTreeMap<String, u64> {
        self.pools.clone()
    }

    /// Gives the device indices of each pool, for `peers::publish`.
    ///
    /// Another user must see WHICH device this coordinator gave away, or two
    /// users put two jobs on the device 0.
    pub fn device_indices(&self) -> BTreeMap<String, Vec<u32>> {
        self.devices
            .iter()
            .map(|(name, used)| (name.clone(), used.keys().copied().collect()))
            .collect()
    }
}

/// Tests if a claim of this job takes all of a pool that a job already holds.
///
/// A claim of this shape is a lock: two jobs with one lock name never operate
/// together, whatever their size. A resource claim cannot express that need.
/// Two builds in one directory need the same quantity of memory as one build,
/// and they still destroy each other's files.
///
/// A job that waits here does not park the queue. There is no part of the pool
/// to keep for it, so the caller continues to the next job.
fn pool_conflict(state: &crate::daemon::State, pools: &[Pool], spec: &JobSpec) -> Option<String> {
    let claims = effective_claims(spec);
    if claims.is_empty() {
        return None;
    }

    for (name, claim) in &claims {
        let pool = pool_of(pools, name);
        if !is_all_or_nothing(&pool, claim) {
            continue;
        }
        for job in state.jobs.values() {
            if !job.status.state.is_active() {
                continue;
            }
            let holds = job.status.assigned.contains_key(name) || job.status.locks.contains(name);
            if holds {
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

/// Gives the devices of one pool that can hold this claim now, best first.
///
/// The order is the most free capacity first, and then the lowest index. That
/// order is deterministic, it repeats, and it spreads the work over the devices
/// in place of filling one device.
fn free_devices(
    pool: &Pool,
    claim: &PoolClaim,
    held: &Held,
    peers: &crate::peers::Claims,
) -> Vec<(u32, u64)> {
    let ours = held.devices.get(&pool.name);
    let theirs: Option<&BTreeSet<u32>> = peers.devices.get(&pool.name);

    let mut free: Vec<(u32, u64)> = pool
        .devices
        .iter()
        .enumerate()
        .filter_map(|(i, capacity)| {
            let index = i as u32;
            // A device that another user holds is not available. A peer
            // publishes the index only, so qex keeps the whole device.
            if theirs.map(|t| t.contains(&index)).unwrap_or(false) {
                return None;
            }
            let used = ours.and_then(|m| m.get(&index)).copied().unwrap_or(0);
            let left = capacity.saturating_sub(used);
            let needed = claim.size.unwrap_or(*capacity);
            if left >= needed && needed > 0 {
                Some((index, left))
            } else {
                None
            }
        })
        .collect();

    free.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    free
}

/// Tests the counted claims of one job against the pools.
///
/// The claims that take all of a pool are not here. `pool_conflict` tests them,
/// with the behaviour that a lock has.
fn pool_wait(
    pools: &[Pool],
    claims: &BTreeMap<String, PoolClaim>,
    held: &Held,
    peers: &crate::peers::Claims,
) -> Option<String> {
    for (name, claim) in claims {
        let pool = pool_of(pools, name);
        if is_all_or_nothing(&pool, claim) {
            continue;
        }

        if pool.is_indexed() {
            let free = free_devices(&pool, claim, held, peers);
            if (free.len() as u64) < claim.count {
                let each = match claim.size {
                    Some(size) => format!(" with {} each", format_size(size)),
                    None => " that are free".to_string(),
                };
                return Some(format!(
                    "waits for the pool `{name}`: this job needs {} device(s){each}, and {} \
                     of the {} device(s) can hold it now",
                    claim.count,
                    free.len(),
                    pool.total
                ));
            }
        } else {
            let ours = held.pools.get(name).copied().unwrap_or(0);
            let theirs = peers.pools.get(name).copied().unwrap_or(0);
            let free = pool.total.saturating_sub(ours).saturating_sub(theirs);
            if claim.count > free {
                let others = if theirs > 0 {
                    format!(", and {} other user(s) hold {theirs}", peers.count)
                } else {
                    String::new()
                };
                return Some(format!(
                    "waits for the pool `{name}`: this job needs {}, the pool has {}, the \
                     jobs of this queue hold {ours}{others}",
                    claim.count, pool.total
                ));
            }
        }
    }
    None
}

/// Gives the units and the devices of each pool to one job.
///
/// The choice is the device with the most free capacity first, and the lowest
/// index for a tie. The result goes into `status.json`, so a job learns which
/// device it received and a coordinator that starts again can count the pools.
fn assign(
    pools: &[Pool],
    claims: &BTreeMap<String, PoolClaim>,
    held: &Held,
    peers: &crate::peers::Claims,
) -> Result<BTreeMap<String, Assignment>, String> {
    let mut out = BTreeMap::new();
    for (name, claim) in claims {
        let pool = pool_of(pools, name);
        if !pool.is_indexed() {
            out.insert(
                name.clone(),
                Assignment {
                    units: claim.count,
                    devices: Vec::new(),
                    size: None,
                },
            );
            continue;
        }

        let free = free_devices(&pool, claim, held, peers);
        if (free.len() as u64) < claim.count {
            return Err(format!(
                "waits for the pool `{name}`: this job needs {} device(s), and {} can hold \
                 it now",
                claim.count,
                free.len()
            ));
        }
        let mut devices: Vec<u32> = free
            .iter()
            .take(claim.count as usize)
            .map(|(index, _)| *index)
            .collect();
        // The choice used the free capacity. The RECORD uses the index order,
        // so `CUDA_VISIBLE_DEVICES=2,3` reads in the way that a person expects
        // and two equal assignments give one text.
        devices.sort_unstable();
        // Keep `None` for a claim that takes the whole of each device.
        //
        // A number here would be wrong. The devices of a pool can have
        // different capacities, and any single number would leave a part of
        // the largest device free — which would let qex put a second job on a
        // card that the first job already owns in full.
        out.insert(
            name.clone(),
            Assignment {
                units: claim.count,
                devices,
                size: claim.size,
            },
        );
    }
    Ok(out)
}

/// Tests if a job can start now.
fn admit(cfg: &Config, pools: &[Pool], spec: &JobSpec, held: &Held) -> Admit {
    let cpu_budget = cfg.budget_cpu().unwrap_or(1);
    let mem_budget = cfg.budget_mem().unwrap_or(0);
    let (cpu_used, mem_used) = (held.cpu, held.mem);

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
    let peers = if cfg.peers.enabled {
        crate::peers::claims(cfg)
    } else {
        crate::peers::Claims::default()
    };
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

    // Test 3: the pools. One arithmetic serves the devices and the counts, and
    // it needs no driver: the pools come from the configuration.
    if let Some(reason) = pool_wait(pools, &effective_claims(spec), held, &peers) {
        return Admit::No(reason);
    }

    // Test 4: the machine. This test finds every load, and not the load of qex
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
            let held = state.claimed();
            let cfg = state.cfg.clone();
            drop(state);
            crate::peers::publish(
                &cfg,
                held.cpu,
                held.mem,
                held.pool_units(),
                held.device_indices(),
            );
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

/// Chooses the next job to start.
fn choose(state: &mut crate::daemon::State) -> Option<uuid::Uuid> {
    let held = state.claimed();
    let cfg = state.cfg.clone();
    let pools = cfg.pools().unwrap_or_default();
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
        if let Some(reason) = pool_conflict(state, &pools, &job.spec) {
            reasons.push((id, Some(reason)));
            continue;
        }

        match size_check(&cfg, &job.spec) {
            Size::Fits => match admit(&cfg, &pools, &job.spec, &held) {
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
            Size::Impossible(reason) => {
                // A job of this kind must never reach the queue: the
                // coordinator refuses it at submission. A record from an
                // earlier version, or a config file that changed since the
                // submission, can still put one here. It never starts, so it
                // keeps no capacity and the loop continues.
                reasons.push((id, Some(reason)));
                continue;
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
            Size::Impossible(reason) => {
                // The configuration changed after the submission. Say so, and
                // leave the job in the queue.
                if let Some(job) = state.jobs.get_mut(&id) {
                    job.status.blocked_reason = Some(reason);
                }
                return Ok(());
            }
        };

        // Give the devices INSIDE this lock hold, before the record is written
        // and before the fork.
        //
        // The assignment is a result and not a request, so it goes to
        // `status.json` and never to `spec.json`. The coordinator is the writer
        // of the record until the supervisor starts, so this write respects the
        // one-writer rule.
        let pools = state.cfg.pools().unwrap_or_default();
        let held = state.claimed();
        let peers = if state.cfg.peers.enabled {
            crate::peers::claims(&state.cfg)
        } else {
            crate::peers::Claims::default()
        };
        let Some(job) = state.jobs.get(&id) else {
            return Ok(());
        };
        let assigned = match assign(&pools, &effective_claims(&job.spec), &held, &peers) {
            Ok(a) => a,
            Err(reason) => {
                // The capacity changed between the choice and this moment: a
                // job of another user took the last device. Leave the job in
                // the queue with the reason, and try again on the next tick.
                if let Some(job) = state.jobs.get_mut(&id) {
                    job.status.blocked_reason = Some(reason);
                }
                return Ok(());
            }
        };

        let Some(job) = state.jobs.get_mut(&id) else {
            return Ok(());
        };
        job.status.state = JobState::Starting;
        job.status.started_at = Some(sys::now_secs());
        job.status.blocked_reason = None;
        job.status.forced = forced.is_some();
        job.status.forced_reason = forced.clone();
        job.status.assigned = assigned;
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

    /// Gives the load of the jobs that operate now.
    fn used(cpu: u64, mem: u64) -> Held {
        Held {
            cpu,
            mem,
            ..Default::default()
        }
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
            claims: Default::default(),
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

        // Two cores are in use. A job of two cores fits.
        assert!(matches!(
            admit(&cfg, &[], &job, &used(2, 64 << 20)),
            Admit::Yes
        ));

        // Four cores are in use. The same job must wait.
        let Admit::No(reason) = admit(&cfg, &[], &job, &used(4, 64 << 20)) else {
            panic!("a job must not start when the cores are in use");
        };
        assert!(reason.contains("cores"), "got: {reason}");

        // The memory is in use. The job must wait.
        let Admit::No(reason) = admit(&cfg, &[], &job, &used(0, 224 << 20)) else {
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
            admit(&cfg, &[], &spec_with(4, 256 << 20), &used(0, 0)),
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
        let Admit::No(reason) = admit(&cfg, &[], &spec_with(1, 1 << 20), &used(0, 0)) else {
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
            let Admit::No(reason) = admit(&cfg, &[], &spec_with(1, 1 << 20), &used(0, 0)) else {
                panic!("the pressure limit must stop this job");
            };
            assert!(reason.contains("pressure"), "got: {reason}");
        }
    }

    // ---- the pools ----

    /// Gives a configuration with a pool of four devices and a pool of four
    /// units. THE MACHINE THAT RUNS THIS TEST HAS NO GPU, and that is the
    /// point: a pool is a PROMISE from the configuration, and not a probe of a
    /// driver.
    fn cfg_with_pools() -> Config {
        toml::from_str(
            "[budget]\ncpu = \"8\"\nmem = \"8GB\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n\
             [[pool]]\nname = \"gpu\"\nsize = \"vram\"\n\
             devices = [\"24GB\", \"24GB\", \"16GB\", \"24GB\"]\n\
             env = \"CUDA_VISIBLE_DEVICES\"\n\
             [[pool]]\nname = \"net\"\ncount = 4\n",
        )
        .unwrap()
    }

    fn spec_claiming(claims: &[(&str, u64, Option<u64>)]) -> JobSpec {
        let mut spec = spec_with(1, 1 << 20);
        for (name, count, size) in claims {
            spec.claims.insert(
                (*name).to_string(),
                PoolClaim {
                    count: *count,
                    size: *size,
                },
            );
        }
        spec
    }

    /// A machine with no GPU must still admit a GPU claim from the
    /// configuration. This is what "a promise, and not a probe" means.
    #[test]
    fn a_machine_with_no_gpu_admits_a_gpu_claim_from_the_configuration() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let spec = spec_claiming(&[("gpu", 2, None)]);
        assert_eq!(size_check(&cfg, &spec), Size::Fits);
        assert!(matches!(
            admit(&cfg, &pools, &spec, &used(0, 0)),
            Admit::Yes
        ));
    }

    /// qex must never add the memory of the devices together. Four devices of
    /// 24GB are not 96GB for one job, and the largest device here is 24GB.
    #[test]
    fn vram_is_never_added_together_over_the_devices() {
        let cfg = cfg_with_pools();
        let Size::Impossible(reason) =
            size_check(&cfg, &spec_claiming(&[("gpu", 2, Some(40 << 30))]))
        else {
            panic!("a claim of 40GB on each device must be impossible");
        };
        assert!(
            reason.contains("never start"),
            "the message must say that the job can never start: {reason}"
        );
        assert!(
            reason.contains("24GB"),
            "the message must name the largest device: {reason}"
        );

        // The same quantity on ONE device is correct, so the test measures the
        // sum and not the size.
        assert_eq!(
            size_check(&cfg, &spec_claiming(&[("gpu", 2, Some(20 << 30))])),
            Size::Fits
        );
    }

    /// A claim above the pool total is a refusal, and not an oversized job.
    /// An empty machine does not make a fifth device.
    #[test]
    fn a_claim_above_the_pool_total_can_never_start() {
        let cfg = cfg_with_pools();
        let Size::Impossible(reason) = size_check(&cfg, &spec_claiming(&[("gpu", 8, None)])) else {
            panic!("a claim of 8 devices from a pool of 4 must be impossible");
        };
        assert!(reason.contains("never start"), "got: {reason}");

        let Size::Impossible(reason) = size_check(&cfg, &spec_claiming(&[("net", 5, None)])) else {
            panic!("a claim of 5 units from a pool of 4 must be impossible");
        };
        assert!(reason.contains("never start"), "got: {reason}");
    }

    /// A name that the configuration does not declare is a lock of one unit.
    /// `--lock NAME` needs no configuration, and it must keep that.
    #[test]
    fn an_undeclared_pool_name_is_a_lock_and_not_an_error() {
        let cfg = cfg_with_pools();
        assert_eq!(
            size_check(&cfg, &spec_claiming(&[("build-dir", 1, None)])),
            Size::Fits
        );

        // More than one unit of a pool that nobody declared is a fault. qex
        // cannot invent a second unit.
        let Size::Impossible(reason) = size_check(&cfg, &spec_claiming(&[("build-dir", 2, None)]))
        else {
            panic!("2 of an undeclared pool must be impossible");
        };
        assert!(reason.contains("lock of size 1"), "got: {reason}");
    }

    /// `--gpu` promises a device index and an environment variable. A machine
    /// with no `gpu` pool can give neither, so qex says so and does not make a
    /// silent lock.
    #[test]
    fn a_gpu_claim_with_no_gpu_pool_names_the_configuration() {
        let cfg = cfg_with("4", "1GB");
        let Size::Impossible(reason) = size_check(&cfg, &spec_claiming(&[("gpu", 1, None)])) else {
            panic!("a GPU claim with no pool must be impossible");
        };
        assert!(reason.contains("[[pool]]"), "got: {reason}");
        assert!(reason.contains("qex.toml"), "got: {reason}");
    }

    /// VRAM is a quantity on a device. A job that asks for it must also ask
    /// for a device.
    #[test]
    fn vram_with_no_device_claim_is_refused() {
        let cfg = cfg_with_pools();
        let Size::Impossible(reason) =
            size_check(&cfg, &spec_claiming(&[("gpu", 0, Some(4 << 30))]))
        else {
            panic!("VRAM with no device must be impossible");
        };
        assert!(reason.contains("--gpu 1"), "got: {reason}");
    }

    /// A pool with no devices holds no size.
    #[test]
    fn a_size_on_a_pool_with_no_devices_is_refused() {
        let cfg = cfg_with_pools();
        let Size::Impossible(reason) =
            size_check(&cfg, &spec_claiming(&[("net", 1, Some(1 << 30))]))
        else {
            panic!("a size on a plain pool must be impossible");
        };
        assert!(reason.contains("no devices"), "got: {reason}");
    }

    /// Two jobs that claim one device each must get DIFFERENT indices, and a
    /// job that asks for more than the pool has left must wait.
    #[test]
    fn two_jobs_get_different_devices_and_a_third_waits() {
        let cfg: Config = toml::from_str(
            "[budget]\ncpu = \"8\"\nmem = \"8GB\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n\
             [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [\"24GB\", \"24GB\"]\n",
        )
        .unwrap();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("gpu", 1, None)]);
        let no_peers = crate::peers::Claims::default();

        let mut held = Held::default();
        let first = assign(&pools, &effective_claims(&claim), &held, &no_peers).unwrap();
        assert_eq!(first["gpu"].devices, vec![0]);

        // Put the first job in the load, then give a device to the second.
        let mut status = crate::job::JobStatus::new(&claim);
        status.assigned = first.clone();
        held.add(&status, &pools);

        let second = assign(&pools, &effective_claims(&claim), &held, &no_peers).unwrap();
        assert_eq!(
            second["gpu"].devices,
            vec![1],
            "the second job must get a device that the first job does not hold"
        );

        let mut status2 = crate::job::JobStatus::new(&claim);
        status2.assigned = second;
        held.add(&status2, &pools);

        // Both devices are in use. The third job must wait.
        let Admit::No(reason) = admit(&cfg, &pools, &claim, &held) else {
            panic!("a third job must wait when both devices are in use");
        };
        assert!(reason.contains("gpu"), "got: {reason}");
    }

    /// A job with no `--vram` takes the WHOLE of each device that it gets. A
    /// part of that device must not go to a second job.
    #[test]
    fn a_claim_with_no_vram_takes_the_whole_device() {
        let cfg: Config = toml::from_str(
            "[budget]\ncpu = \"8\"\nmem = \"8GB\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n\
             [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [\"24GB\"]\n",
        )
        .unwrap();
        let pools = cfg.pools().unwrap();
        let whole = spec_claiming(&[("gpu", 1, None)]);
        let no_peers = crate::peers::Claims::default();

        let mut held = Held::default();
        let given = assign(&pools, &effective_claims(&whole), &held, &no_peers).unwrap();
        assert_eq!(given["gpu"].size, None, "a whole device records no size");
        let mut status = crate::job::JobStatus::new(&whole);
        status.assigned = given;
        held.add(&status, &pools);

        // A small claim must not fit beside a job that owns the whole device.
        let small = spec_claiming(&[("gpu", 1, Some(1 << 30))]);
        assert!(
            assign(&pools, &effective_claims(&small), &held, &no_peers).is_err(),
            "a device that a job owns in full must hold no second job"
        );
    }

    /// A device that a job holds in part must still take a second job while
    /// its capacity permits.
    #[test]
    fn a_device_holds_two_jobs_while_its_capacity_permits() {
        let cfg: Config = toml::from_str(
            "[budget]\ncpu = \"8\"\nmem = \"8GB\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n\
             [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [\"24GB\"]\n",
        )
        .unwrap();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("gpu", 1, Some(8 << 30))]);
        let no_peers = crate::peers::Claims::default();

        let mut held = Held::default();
        for _ in 0..3 {
            let given = assign(&pools, &effective_claims(&claim), &held, &no_peers).unwrap();
            assert_eq!(given["gpu"].devices, vec![0]);
            let mut status = crate::job::JobStatus::new(&claim);
            status.assigned = given;
            held.add(&status, &pools);
        }
        // 24GB holds three jobs of 8GB, and no fourth.
        assert!(
            assign(&pools, &effective_claims(&claim), &held, &no_peers).is_err(),
            "a fourth job of 8GB must not fit a device of 24GB"
        );
    }

    /// The choice must be the most free capacity first, and the lowest index
    /// for a tie. That order spreads the work in place of filling one device.
    #[test]
    fn the_device_with_the_most_free_capacity_comes_first() {
        let cfg: Config = toml::from_str(
            "[budget]\ncpu = \"8\"\nmem = \"8GB\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n\
             [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [\"16GB\", \"24GB\", \"16GB\"]\n",
        )
        .unwrap();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("gpu", 1, Some(4 << 30))]);
        let given = assign(
            &pools,
            &effective_claims(&claim),
            &Held::default(),
            &crate::peers::Claims::default(),
        )
        .unwrap();
        assert_eq!(
            given["gpu"].devices,
            vec![1],
            "the device with 24GB must come before the two devices with 16GB"
        );
    }

    /// A device that another user holds must not go to a job of this user.
    /// Without this, two users put two jobs on the device 0.
    #[test]
    fn a_device_that_another_user_holds_is_not_given_again() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("gpu", 4, None)]);

        let mut peers = crate::peers::Claims::default();
        peers
            .devices
            .insert("gpu".into(), [0u32, 1].into_iter().collect());
        peers.count = 1;

        assert!(
            assign(&pools, &effective_claims(&claim), &Held::default(), &peers).is_err(),
            "a claim of 4 devices must fail while another user holds 2"
        );

        let two = spec_claiming(&[("gpu", 2, None)]);
        let given = assign(&pools, &effective_claims(&two), &Held::default(), &peers).unwrap();
        assert_eq!(
            given["gpu"].devices,
            vec![2, 3],
            "qex must give the devices that no other user holds"
        );
    }

    /// A lock is a pool of one unit, and a lock needs no configuration. The
    /// conversion happens in the coordinator and never on the wire.
    #[test]
    fn a_lock_becomes_a_pool_of_one_unit_inside_the_coordinator() {
        let mut spec = spec_with(1, 1 << 20);
        spec.locks = vec!["target".into()];
        let claims = effective_claims(&spec);
        assert_eq!(
            claims["target"],
            PoolClaim {
                count: 1,
                size: None
            }
        );
        assert!(
            spec.claims.is_empty(),
            "the wire field `claims` must stay empty for a job with a lock only"
        );

        // A lock takes all of its pool or nothing, so it does not keep
        // capacity for itself and the queue continues behind it.
        assert!(is_all_or_nothing(
            &Pool::implicit("target"),
            &claims["target"]
        ));
    }

    /// A record that an earlier version wrote holds `locks` and no `assigned`.
    /// Those locks must still count, or a coordinator that starts after an
    /// upgrade gives a lock that a live job already holds.
    #[test]
    fn a_lock_of_an_earlier_record_still_counts() {
        let mut spec = spec_with(1, 1 << 20);
        spec.locks = vec!["target".into()];
        let status = crate::job::JobStatus::new(&spec);
        assert!(status.assigned.is_empty());

        let mut held = Held::default();
        held.add(&status, &[]);
        assert_eq!(held.pools.get("target"), Some(&1));
    }

    /// A pool with more than one unit counts, and it does not behave as a lock.
    #[test]
    fn a_counted_pool_admits_jobs_until_it_is_full() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("net", 3, None)]);

        let mut held = Held::default();
        assert!(matches!(admit(&cfg, &pools, &claim, &held), Admit::Yes));

        held.pools.insert("net".into(), 2);
        let Admit::No(reason) = admit(&cfg, &pools, &claim, &held) else {
            panic!("3 of `net` must not fit while 2 of 4 are in use");
        };
        assert!(reason.contains("net"), "got: {reason}");
    }
}
