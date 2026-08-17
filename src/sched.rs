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
}

/// The claims of one job on the pools, with each lock as a pool of one unit.
///
/// The conversion happens HERE, in the coordinator, and never on the wire. A
/// lock stays in `JobSpec::locks`, and a claim stays in `JobSpec::claims`, so a
/// coordinator of an earlier version reads a lock in the field that it knows.
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

/// Tests if a claim is a LOCK, and not a count.
///
/// A lock takes all of its pool or nothing. There is no part of the pool to
/// keep for a job that waits, so such a job never becomes the head and never
/// parks the queue behind it. One long build with a lock must not stop the
/// whole machine.
///
/// # The rule is "the configuration does not declare this name", and nothing
/// else
///
/// An earlier version of this function read the pool SIZE: a pool of one unit
/// was a lock. That rule has a hole, and the hole gives two jobs one card.
/// `lock_conflict` reads the jobs of THIS queue only, because a lock has always
/// been a name inside one queue. A DECLARED pool is a real piece of hardware
/// that the other users of the machine share, and `pool_wait` is the one place
/// that reads what those users hold. A declared pool of one device would thus
/// go through the lock path, meet no peer test, and start on the card that
/// another user's job already holds.
///
/// So: a declared pool is ALWAYS counted, whatever its size, and it is
/// peer-aware. A name that the configuration does not declare is a lock of one
/// unit, and it keeps every behaviour that `--lock` had. `pool_check` refuses
/// more than one unit of an undeclared name, so such a claim is always the
/// whole of its pool.
fn is_a_lock(pools: &[Pool], name: &str) -> bool {
    !pools.iter().any(|p| p.name == name)
}

/// Tests the claims of one job against the configuration.
///
/// # Why this is not part of [`size_check`]
///
/// `size_check` answers "does this job fit the budget", and the answer for a
/// job that does not fit is `TooBig`: such a job can still run alone on a quiet
/// machine, and that result is data for the agent. A fault that THIS function
/// finds can never become correct on this machine. An empty machine does not
/// make a fifth device, so the job waits for a change that no scheduler can
/// make. qex therefore refuses it at the submission, whatever `[queue]
/// oversized` says, and the message gives the correction.
pub fn pool_check(cfg: &Config, spec: &JobSpec) -> Result<(), String> {
    let pools = cfg.pools().map_err(|e| e.to_string())?;
    let claims = effective_claims(spec);

    for (name, claim) in &claims {
        let declared = pools.iter().find(|p| p.name == *name);

        if claim.count == 0 {
            // The only path to this state is `--vram` with no `--gpu`.
            let quantity = declared
                .and_then(|p| p.size_name.clone())
                .unwrap_or_else(|| "VRAM".to_string());
            return Err(format!(
                "this job claims {} of {quantity} and claims no device. {quantity} is a quantity \
                 on each device, so a job must also claim a device. Add `--gpu 1`.",
                format_size(claim.size.unwrap_or(0)),
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
                     that pool, so qex treats it as a lock of size 1. This job can never \
                     start. Add the pool to ~/.config/qex.toml:\n\n\
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
                "the pool `{name}` has no devices, so it holds no size. This job can never \
                 start. Give `--claim {name}=N` only."
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
///
/// # Which claim the queue uses
///
/// The caller gives the claim of the RECORD, which is the claim in force. qex
/// changes no claim, so it is the claim of the specification as well.
pub fn size_check(cfg: &Config, cpu: u64, mem: u64) -> Size {
    let cpu_budget = cfg.budget_cpu().unwrap_or(1);
    let mem_budget = cfg.budget_mem().unwrap_or(0);

    let mut reasons = Vec::new();
    if cpu > cpu_budget {
        reasons.push(format!(
            "the job claims {cpu} cores and the budget is {cpu_budget} cores"
        ));
    }
    if mem > mem_budget {
        reasons.push(format!(
            "the job claims {} of memory and the budget is {}",
            format_size(mem),
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
    /// The job waits. [`Blocker`] says who holds the capacity.
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

/// The resources that the jobs which operate now hold.
///
/// The cores and the memory are two numbers. A pool needs two more: the units
/// of the pool, and — for a pool whose devices qex names — the quantity in use
/// on EACH device. The capacity of the devices is never added together, so the
/// second map cannot be derived from the first.
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
        // The claim IN FORCE lives in the record, and not in the
        // specification. See `size_check`.
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

/// Tests if a claim of this job takes ALL of a pool that a job already holds.
///
/// A resource claim cannot express this need. Two builds in one directory need
/// the same quantity of memory as one build, and they still destroy each
/// other's files. Two servers need one port, whatever their size.
///
/// # Why a pool comes through here at all
///
/// `--lock NAME` IS a claim on a pool of one unit, and an undeclared name is a
/// pool of one unit. A claim that takes the whole of such a pool is thus
/// exactly a lock, and it must keep the behaviour of a lock: the job that waits
/// for it never becomes the head, so it never keeps capacity and the queue
/// continues behind it. One long build with a lock would otherwise stop the
/// whole machine.
///
/// A claim that a pool can divide is NOT here. `pool_wait` tests those, in
/// `admit`, where the job can become the head and collect capacity.
fn lock_conflict(state: &crate::daemon::State, pools: &[Pool], spec: &JobSpec) -> Option<String> {
    let claims = effective_claims(spec);
    if claims.is_empty() {
        return None;
    }

    // A person can hold a lock, in the same way as a job holds it.
    //
    // This test comes first. When a person asks for a lock that a job holds,
    // the person is next: the job keeps the lock, and no other job takes it in
    // the time between. The reason must therefore name the person, whatever job
    // holds the lock at this moment.
    for name in &spec.locks {
        if state
            .paused
            .locks
            .keys()
            .any(|k| crate::pause::lock_same(k, name))
        {
            return Some(crate::pause::lock_reason(name));
        }
    }

    for name in claims.keys() {
        if !is_a_lock(pools, name) {
            continue;
        }
        for job in state.jobs.values() {
            if !job.status.state.is_active() {
                continue;
            }
            // Read the REQUEST, and not the assignment.
            //
            // `spec.locks` and `spec.claims` are what the job asked for, and
            // they exist from the submission on. `status.assigned` holds no
            // name that is not in one of those two — `assign` receives
            // `effective_claims` of this same specification — so a test of it
            // here would be a branch that no state can reach.
            //
            // `Held::add` is the other place, and it reads the OPPOSITE way:
            // it needs the units and the devices that the coordinator GAVE, so
            // it reads `status.assigned` and falls back to `status.locks` for a
            // record that an earlier version wrote.
            if job.spec.locks.contains(name) || job.spec.claims.contains_key(name) {
                return Some(format!(
                    "waits for the lock `{}`, which the job {} ({}) holds",
                    crate::job::safe_name(name),
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
            // publishes the index only, so qex keeps the WHOLE device: qex
            // cannot see how much of that card the other user's job takes, and
            // a guess that leaves room would put two jobs on one card.
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

/// What a pool shortage is, and WHO holds the units.
///
/// The class decides whether the queue may keep capacity for the job. See
/// [`Blocker`]. A shortage that the jobs of THIS queue cause has a known end,
/// so the head may reserve. A shortage that another user causes has no known
/// end that qex can measure, so the head must never reserve: qex cannot make
/// the other user give the device back, and a reservation would hold the whole
/// machine empty for a time that qex cannot name.
struct PoolWait {
    blocker: Blocker,
    reason: String,
    held_reason: Option<String>,
}

/// Tests the counted claims of one job against the pools.
///
/// A LOCK IS NOT HERE. `lock_conflict` tests every undeclared name in pass 1,
/// with the behaviour that a lock has. See [`is_a_lock`].
fn pool_wait(
    pools: &[Pool],
    claims: &BTreeMap<String, PoolClaim>,
    held: &Held,
    peers: &crate::peers::Claims,
) -> Option<PoolWait> {
    for (name, claim) in claims {
        if is_a_lock(pools, name) {
            continue;
        }
        let pool = pool_of(pools, name);

        // Another user is the blocker only when the claim would still be short
        // after every job of this queue released the pool. A peer that holds a
        // small part of a pool must not turn a shortage caused by our siblings
        // into `Peer`: that class never reserves, so a stream of our smaller
        // jobs could then starve this claim for ever.
        let peer_units = peers.pools.get(name).copied().unwrap_or(0);

        // `arithmetic` is the count and nothing else. Each class below gives it
        // a lead of its own, because the two leads are different statements:
        // the jobs of this queue release the pool, and another user may not.
        let (short, short_without_siblings, arithmetic) = if pool.is_indexed() {
            let free = free_devices(&pool, claim, held, peers).len() as u64;
            let free_without_siblings =
                free_devices(&pool, claim, &Held::default(), peers).len() as u64;
            let each = match claim.size {
                Some(size) => format!(" with {} free each", format_size(size)),
                None => " that is free in full".to_string(),
            };
            (
                free < claim.count,
                free_without_siblings < claim.count,
                format!(
                    "this job needs {} device(s){each}, the pool has {}, and {free} can hold this \
                     job now.",
                    claim.count, pool.total
                ),
            )
        } else {
            let ours = held.pools.get(name).copied().unwrap_or(0);
            let free = pool.total.saturating_sub(ours).saturating_sub(peer_units);
            let free_without_siblings = pool.total.saturating_sub(peer_units);
            (
                claim.count > free,
                claim.count > free_without_siblings,
                format!(
                    "this job needs {}, the pool has {}, and the jobs of this queue hold {ours}.",
                    claim.count, pool.total
                ),
            )
        };

        if !short {
            continue;
        }

        return Some(if short_without_siblings {
            PoolWait {
                blocker: Blocker::Peer { count: peers.count },
                reason: format!(
                    "this job cannot fit while another user holds the pool `{name}`: \
                     {arithmetic} {} part of it. qex does not control that user, so this wait has \
                     no known end. qex starts the jobs behind this one while the pool is not \
                     free. Read `qex info` for the pools of this machine.",
                    other_users(peers.count)
                ),
                // A `Peer` never keeps capacity, so no reader sees this text.
                held_reason: None,
            }
        } else {
            PoolWait {
                blocker: Blocker::Sibling,
                reason: format!(
                    "waits for the pool `{name}`: {arithmetic} Those jobs release the pool when \
                     they stop. qex can start a job that does not need this pool before this one."
                ),
                held_reason: Some(format!(
                    "waits for the pool `{name}`: {arithmetic} qex starts no other job before \
                     this one. Read `qex list` to see the jobs that hold the pool."
                )),
            }
        });
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
///
/// `cpu` and `mem` are the claim IN FORCE. See [`size_check`].
///
/// `machine` holds one measurement of the machine and of the other users for
/// the whole pass. See [`Machine`].
fn admit(
    cfg: &Config,
    pools: &[Pool],
    claims: &BTreeMap<String, PoolClaim>,
    cpu: u64,
    mem: u64,
    held: &Held,
    machine: &Machine,
) -> Admit {
    let cpu_budget = cfg.budget_cpu().unwrap_or(1);
    let mem_budget = cfg.budget_mem().unwrap_or(0);
    let (cpu_used, mem_used) = (held.cpu, held.mem);

    // Test 1: the budget of this user.
    if cpu_used + cpu > cpu_budget {
        return sibling_wait(
            format!(
                "waits for cores: this job needs {}, and the jobs of this queue hold {} of the {} \
                 in the budget.",
                cores(cpu),
                cpu_used,
                cores(cpu_budget)
            ),
            "cores",
        );
    }
    if mem_used + mem > mem_budget {
        return sibling_wait(
            format!(
                "waits for memory: this job needs {}, and the jobs of this queue hold {} of the {} \
                 budget.",
                format_size(mem),
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
        if cpu_used + peers.cpu + cpu > cpu_budget {
            return Admit::No {
                blocker: Blocker::Peer { count: peers.count },
                reason: format!(
                    "this job cannot fit while another user holds capacity: the job needs {}, this \
                     queue holds {} of the {} in the budget, and {} {}. qex does not control that \
                     user, so this wait has no known end. qex starts the jobs behind this one \
                     while the capacity is not free. Read `qex info` for the load of the machine.",
                    cores(cpu),
                    cpu_used,
                    cores(cpu_budget),
                    other_users(peers.count),
                    cores(peers.cpu)
                ),
                held_reason: None,
            };
        }
        if mem_used + peers.mem + mem > mem_budget {
            return Admit::No {
                blocker: Blocker::Peer { count: peers.count },
                reason: format!(
                    "this job cannot fit while another user holds capacity: the job needs {}, this \
                     queue holds {} of the {} budget, and {} {}. qex does not control that user, \
                     so this wait has no known end. qex starts the jobs behind this one while the \
                     capacity is not free. Read `qex info` for the load of the machine.",
                    format_size(mem),
                    format_size(mem_used),
                    format_size(mem_budget),
                    other_users(peers.count),
                    format_size(peers.mem)
                ),
                held_reason: None,
            };
        }
    }

    // Test 3: the pools. One arithmetic serves the devices and the counts, and
    // it reads no driver: the pools come from the configuration.
    //
    // This test comes AFTER the peers and BEFORE the machine, because it can
    // name either holder. `pool_wait` chooses the class from who holds the
    // units, so a job that another user holds back never keeps capacity.
    if let Some(wait) = pool_wait(pools, claims, held, &machine.peers) {
        return Admit::No {
            blocker: wait.blocker,
            reason: wait.reason,
            held_reason: wait.held_reason,
        };
    }

    // Test 4: the machine. This test finds every load, and not the load of qex
    // only. It is the test that a program outside qex cannot avoid.
    let reserve = cfg.reserve_mem().unwrap_or(0);
    let available = machine.available;
    if available < reserve + mem {
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
            format_size(mem),
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

/// The decision for one job that has no dependency and no lock left.
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
/// `cpu` and `mem` are the claim IN FORCE. See [`size_check`].
///
/// `quiet` says that no job operates and the settle time passed.
#[allow(clippy::too_many_arguments)]
fn verdict(
    cfg: &Config,
    pools: &[Pool],
    spec: &JobSpec,
    cpu: u64,
    mem: u64,
    held: &Held,
    machine: &Machine,
    quiet: bool,
) -> Verdict {
    // A claim that the configuration can never satisfy comes first, and the
    // class is `OversizedParked`.
    //
    // The coordinator refuses such a job at the submission, so it reaches this
    // point only when the config file changed after the submission, or when the
    // record comes from an earlier version. It can never start, so it must
    // never keep capacity: `OversizedParked` says exactly that, and the queue
    // continues behind it. See `Blocker::may_reserve`.
    if let Err(reason) = pool_check(cfg, spec) {
        return Verdict::Wait {
            blocker: Blocker::OversizedParked,
            reason: format!(
                "{reason}\nThe configuration changed after the submission of this job. This job \
                 never starts, so qex starts the jobs behind it."
            ),
            held_reason: None,
        };
    }

    let claims = effective_claims(spec);
    match size_check(cfg, cpu, mem) {
        Size::Fits => match admit(cfg, pools, &claims, cpu, mem, held, machine) {
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
                let peers_idle = machine.peers.cpu == 0
                    && machine.peers.mem == 0
                    && machine.peers.pools.values().all(|n| *n == 0)
                    && machine.peers.devices.values().all(BTreeSet::is_empty);
                let pressure_ok = machine
                    .pressure
                    .map(|p| p <= cfg.system.max_pressure)
                    .unwrap_or(true);
                if quiet && peers_idle && pressure_ok {
                    return Verdict::Start;
                }
                return Verdict::Wait {
                    blocker: Blocker::OversizedWaitsForIdle,
                    reason: format!(
                        "{reason}; qex starts this job when no other qex job operates on the \
                         machine and memory pressure is within the configured limit. qex starts \
                         the jobs behind this one until then."
                    ),
                    held_reason: Some(format!(
                        "{reason}; qex starts this job when no other qex job operates on the \
                         machine and memory pressure is within the configured limit. qex starts \
                         no other job before this one, so this queue becomes empty."
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
        let changed = {
            let mut state = coord.state.lock().unwrap();
            let changed = state.refresh_active();
            // The supervisors write the records, so this read is how the
            // coordinator learns that a job started or stopped. Report each of
            // those changes to the readers of the event stream.
            state.publish_changes();
            changed
        };

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

        // The step above can start a job, skip a job, or change the reason that
        // a job waits. Report those changes as well.
        coord.state.lock().unwrap().publish_changes();

        // Publish the claims of this coordinator, so the coordinators of the
        // other users see them.
        {
            let state = coord.state.lock().unwrap();
            let held = state.claimed();
            let cfg = state.cfg.clone();
            drop(state);
            // Publish the pools as well as the cores and the memory. Another
            // user must see WHICH device this coordinator gave away, or two
            // users put two jobs on the device 0.
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
        //
        // WHILE THE CONFIGURATION FILE SETTLES, LOOK OFTEN.
        //
        // THIS IS THE OLD GUARD, AND IT IS NOW THE SECOND ONE. `reload_config`
        // takes a file by its AGE where the system gives one, and the age says
        // that no writer touched the file for `CONFIG_SETTLE`, whatever this
        // loop does. A sampler cannot miss a window that it does not use.
        //
        // The looks below still matter on a system that gives no time for a
        // file. There, `reload_config` takes the content when every look in
        // `CONFIG_SETTLE` gave that content: with one look every 500ms there
        // are TWO looks in the window, and a writer with a period near one
        // second gives them both the same half-written file. Ten looks make
        // that writer far less likely to pass, and they DO NOT CLOSE THE HOLE:
        // a sampler always has a frequency that walks past some period. Do not
        // write that this loop makes the content certain. See `reload_config`.
        //
        // With an age, `config_settling` stays `None` while a writer works, so
        // this loop keeps its 500ms wait and the fast look never starts. That
        // is correct: one look at an old file is the whole of the answer.
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

            // End a pause that reached the time of `--for`.
            //
            // The scheduler is the correct place: it is the code that reads the
            // pause, so a pause can never end in the record and continue in the
            // decision.
            let now = sys::now_secs();
            let queue_pause = state.paused.queue.clone();
            if state.paused.expire(now) {
                // A pause of the QUEUE that reached its `--for` ends in exactly
                // the same way as `qex resume queue`. This call is before
                // `choose`, so no job can expire on time that the pause took.
                if let Some(record) = queue_pause {
                    if state.paused.queue.is_none() {
                        crate::pause::end_queue_pause(&mut state, &record, now);
                        log("a pause reached its end; qex starts the queue again");
                    }
                }
                state.save_pause();
            }

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
            // `needs` promises success, and a missing record cannot prove it.
            // A current submission cannot create this state, and `clean`
            // cannot create it either; it remains possible in an old or
            // manually damaged store. Refuse to run the dependent as though
            // the missing job had succeeded. `after` stays lenient below,
            // because it promises only that the earlier job is no longer
            // operating, which a missing record does prove.
            return Depends::Broken {
                reason: format!(
                    "the record of the job {} that this job needed is gone, so qex cannot prove that it succeeded",
                    &dep.to_string()[..8]
                ),
                root: *dep,
            };
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
        // This code holds the lock of the queue, so the hook runs in a thread
        // of its own. A hook that hangs must never hold the queue.
        crate::hook::fire_detached(&dir, &status);
    }
    state.publish_changes();
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
    // refuses. A current job between two attempts of `--retries` stays
    // `running` and cannot expire. An older record in that gap can be `queued`
    // and still hold the `started_at` of the attempt that failed. A coordinator
    // that starts again puts every `queued` record back in the queue. A job in
    // that gap would otherwise be able to expire, and its record would then
    // say `expired` and hold a start time and an exit code at the same time.
    // The state alone cannot separate the two cases; the start time can.
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
        // A job that gave up waiting is the case that a person MOST wants to
        // hear about: nothing ran, and nothing else will say so. This job never
        // had a supervisor, so the coordinator runs the hook, in a thread of
        // its own because this code holds the lock of the queue.
        crate::hook::fire_detached(&dir, &status);
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
    // A paused queue expires NOTHING.
    //
    // qex started no job in this pass because a person holds the machine, so a
    // job that waits does not wait for the queue. To expire it here would make
    // a pause of 30 minutes delete every job with a smaller limit, and the
    // person who paused is away by construction and sees none of it. The wait
    // that the pause added comes back at the end of the pause, in
    // `pause::credit_paused_wait`.
    if state.paused.queue.is_some() {
        return Vec::new();
    }
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
            // The time that the queue was paused is not time in the queue.
            let waited = now
                .saturating_sub(job.status.submitted_at)
                .saturating_sub(job.status.queue_pause_secs);
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
/// The rule is a RESERVATION WITH A BOUNDED BYPASS, and it is not strict FIFO.
///
/// The scheduler walks the jobs that have no dependency and no lock left, in
/// queue order. The FIRST job that cannot start is the head. Its class
/// ([`Blocker`]) says who holds the capacity, and the class decides the rule:
///
/// * The jobs of THIS QUEUE hold it (`Sibling`), or the job waits for a quiet
///   machine (`OversizedWaitsForIdle`). qex schedules that release, so the wait
///   has a known end and it is correct to keep the capacity. qex lets
///   `[queue] max_bypass` jobs pass the head, and then it keeps the capacity:
///   no job starts at all until the head starts.
/// * ANOTHER USER (`Peer`), a program outside qex (`Machine`), or the config
///   file (`OversizedParked`) holds it. qex does not schedule that release, so
///   the head never keeps capacity and the queue continues behind it. An empty
///   machine gives such a job nothing, and a reservation would only stop every
///   other job as well.
///
/// # A large job cannot starve
///
/// `max_bypass` is the whole answer. A stream of small jobs can pass a large
/// job `max_bypass` times, and no more: at the limit the head is unpassable and
/// the queue collects capacity for it as the jobs that operate stop. The
/// default is 2. `max_bypass = 0` gives the strict order of the earlier
/// releases FOR THE CLASSES THAT KEEP CAPACITY. It changes nothing for a
/// `Peer`, a `Machine` or an `OversizedParked` head, which keep no capacity at
/// any value.
///
/// The count of the jobs that passed the head is NOT reset when the class
/// changes. A job that another user held for an hour collects bypasses freely,
/// and in the cycle in which that user releases the capacity the count is
/// already at the limit. The job is then unpassable at once, and it collects
/// capacity as the jobs of this queue stop. A wait behind another user thus
/// costs one scheduler cycle, and not the life of the other user's job.
///
/// # What the rule does NOT change
///
/// A job that waits for a DEPENDENCY, for a LOCK, or for a PAUSE never becomes
/// the head and never keeps capacity. Pass 1 removes all three from the list of
/// ready jobs before this rule runs at all:
///
/// * A dependency or a lock takes no capacity, so there is nothing to keep. One
///   long build with a lock would otherwise stop the whole machine.
/// * A pause starts NOTHING, so there is nothing to bypass.
///
/// The queue order stays the order of `--priority` and then of the submission.
/// A bypass does not move a job in the queue: it lets a job that is behind the
/// head start while the head cannot. The head keeps its place, and it starts
/// before every job behind it as soon as it can start.
fn choose(state: &mut crate::daemon::State) -> Choice {
    let held = state.claimed();
    let cfg = state.cfg.clone();
    // A config file that qex cannot read gives no pool. Every claim then falls
    // back to a pool of one unit, which is a lock, and no job gets a device it
    // must not have.
    let pools = cfg.pools().unwrap_or_default();
    let active = state.count_state(|s| s.is_active());
    let idle_since = state.idle_since;
    let paused = state.paused.queue.clone();
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
            Depends::Ready => match lock_conflict(state, &pools, &job.spec) {
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
    // A paused queue starts NOTHING, so this pass does not run at all.
    //
    // The test is here, and not before pass 1, for two reasons that a later
    // change must not lose:
    //
    //   * Pass 1 must still run. A job whose dependency failed must become
    //     `skipped` while the queue is paused. Skipping starts no process, and
    //     a job that stays in the queue makes `qex wait` block for ever.
    //   * The test comes before the oversized branch below. A paused queue is
    //     idle by construction, so a pause would otherwise start every job that
    //     is larger than the budget — the opposite of a quiet machine.
    if let Some(record) = &paused {
        let reason = crate::pause::queue_reason(record);
        for id in ready.iter().copied() {
            reasons.push((id, Some(reason.clone())));
        }
        // Pass 2 now has no job to test, so it chooses nothing and it starts
        // nothing. The reasons above are already the whole reason.
        ready.clear();
    }

    // The scheduler starts one job for each call, so a lock that this pass gives
    // away cannot be given again: the next call sees the job as active.
    //
    // This loop tests EVERY ready job while the head does not keep capacity, so
    // each job gets a reason of its own. The earlier code stopped at the head,
    // and each job behind it read a sentence about queue position that named no
    // cause. A job that another user also held back was told the wrong thing.
    let mut head: Option<Head> = None;
    let mut started_now: Option<uuid::Uuid> = None;

    for id in ready.iter().copied() {
        let Some(job) = state.jobs.get(&id) else {
            continue;
        };

        // The claim in force lives in the record, and not in the
        // specification.
        let (claim_cpu, claim_mem) = (job.status.cpu, job.status.mem);
        // The SAFE name. This sentence goes to a reader, through
        // `blocked_reason` and through `qex info`. See `job::safe_name`.
        let name = job.status.display_name();
        let passed_by = job.status.passed_by;

        match verdict(
            &cfg, &pools, &job.spec, claim_cpu, claim_mem, &held, &machine, quiet,
        ) {
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
                    name,
                    mem: claim_mem,
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
    //
    // EVERY OTHER READY JOB NEEDS THIS TEXT, and no test for an earlier reason
    // is needed here. The loop above breaks at a head that keeps the capacity,
    // so the head is the only ready job that already has a reason, and the line
    // below skips it. A job that waits for a dependency or a lock is not in
    // `ready` at all.
    if let Some(h) = &head {
        if h.reserved {
            for later in ready.iter().copied() {
                if later == h.id {
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
    //
    // COUNT THESE JOBS. A skipped job is a final state that this pass wrote on
    // its own, so nothing else tells the waiters. Measured before this count
    // existed: `qex wait` on a job whose dependency expired returned at 33.0s
    // with a 3s limit, and on a job whose dependency was cancelled at 31.0s.
    let mut finished = to_skip.len();
    for (id, reason, root) in to_skip {
        skip(state, id, reason, root);
    }

    // Collect the jobs whose record changed, and write each record ONE time.
    //
    // The head can change its reason, its `blocked_since` and its `passed_by`
    // in the same pass. A write for each field would give three operations to
    // the disk for one job in one pass.
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

    // The job that this pass chose waited for nothing, so its count starts
    // again. `start_job` does that, and it is the ONE writer of those two
    // fields for a job that starts. A second writer here would give the same
    // record two authors, and the two would disagree in the case that
    // `start_job` refuses the job after this pass chose it.

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

    // Report every change that this pass made: the skipped jobs, the expired
    // jobs, and the reason of a job that waits. A job that waits keeps the
    // state `queued`, so the reason is its only change, and a reader that sees
    // `queued` and no reason cannot learn what the job waits for. This call is
    // AFTER the two loops above, so the terminal states `skipped` and
    // `expired` each give a line as well.
    state.publish_changes();

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
    // `qex cancel` can change the job, `qex clean` can delete it, and
    // `qex pause` can stop the queue. This code must thus test the job again.
    // Without the test, qex starts a job that the user cancelled, and the user
    // receives an answer that says the opposite.
    //
    // The pause is in this list for the same reason: `qex pause queue` answers
    // "paused" and the job starts, and `qex pause lock gpu0` tells the person
    // that the lock is theirs while a job takes it in the same instant. That
    // order is the whole claim of the lock half of this feature.
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

        if state.paused.queue.is_some() {
            log(&format!("job {id} does not start: the queue is paused"));
            return Ok(());
        }
        if let Some(name) = job
            .spec
            .locks
            .iter()
            .find(|n| state.paused.locks.contains_key(*n))
        {
            log(&format!(
                "job {id} does not start: a person holds the lock `{}`",
                crate::job::safe_name(name)
            ));
            return Ok(());
        }

        // The configuration changed after the submission. Say so, and leave
        // the job in the queue. A claim that no configuration can satisfy is
        // never started alone, because an empty machine does not make a fifth
        // device.
        if let Err(reason) = pool_check(&state.cfg, &job.spec) {
            if let Some(job) = state.jobs.get_mut(&id) {
                job.status.blocked_reason = Some(reason);
            }
            state.publish_changes();
            return Ok(());
        }

        let forced = match size_check(&state.cfg, job.status.cpu, job.status.mem) {
            Size::TooBig(reason) => Some(format!(
                "{reason}. qex started this job alone because no other job operated."
            )),
            Size::Fits => None,
        };

        // Give the devices INSIDE this lock hold, before the record is written
        // and before the fork.
        //
        // The assignment is a result and not a request, so it goes to
        // `status.json` and never to `spec.json`. The coordinator is the writer
        // of the record until the supervisor starts, so this write keeps the
        // one-writer rule.
        let pools = state.cfg.pools().unwrap_or_default();
        let held = state.claimed();
        let peers = if state.cfg.peers_enabled() {
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
                state.publish_changes();
                return Ok(());
            }
        };

        let Some(job) = state.jobs.get_mut(&id) else {
            return Ok(());
        };
        job.status.assigned = assigned;
        job.status.state = JobState::Starting;
        job.status.started_at = Some(sys::now_secs());
        job.status.blocked_reason = None;
        // The job starts, so it waits for nothing and no job passed it. A
        // record that kept these two values would tell a reader that a job
        // which operates is at the front of the queue.
        job.status.blocked_since = None;
        job.status.passed_by = 0;
        job.status.forced = forced.is_some();
        job.status.forced_reason = forced.clone();
        let status = job.status.clone();
        // The SAFE name: this value goes into the log of the coordinator.
        let name = crate::job::safe_name(&job.spec.name);
        state.queue.retain(|q| *q != id);
        // `qex info` reads this time to say if the queue moves. A queue that
        // started nothing for a long time is the fault that a reader looks for.
        state.last_start_at = Some(sys::now_secs());
        state.publish_changes();
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
        state.publish_changes();
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
                state.publish_changes();
                drop(state);
                if let Ok(dir) = paths::job_dir(&id) {
                    job::write_status(&dir, &status).ok();
                    // This job has no supervisor, so the coordinator tells the
                    // person that it stopped.
                    crate::hook::fire_detached(&dir, &status);
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

    /// One measurement of the machine for a test that calls `admit` directly.
    ///
    /// The scheduler makes this record one time for each pass. A test must use
    /// the same record, or it tests a path that the scheduler does not take.
    fn machine_for(cfg: &Config) -> Machine {
        Machine::read(cfg)
    }

    /// Gives the load of the jobs that operate now: the cores and the memory
    /// only, with no pool in use.
    fn used(cpu: u64, mem: u64) -> Held {
        Held {
            cpu,
            mem,
            ..Default::default()
        }
    }

    /// Calls `admit` with no pool and no claim, for the tests of the cores and
    /// the memory.
    fn admit_plain(
        cfg: &Config,
        cpu: u64,
        mem: u64,
        cpu_used: u64,
        mem_used: u64,
        m: &Machine,
    ) -> Admit {
        admit(
            cfg,
            &[],
            &BTreeMap::new(),
            cpu,
            mem,
            &used(cpu_used, mem_used),
            m,
        )
    }

    /// Calls `verdict` with no pool and no claim, for the tests of the cores,
    /// the memory and the oversized policy.
    fn verdict_plain(
        cfg: &Config,
        cpu: u64,
        mem: u64,
        cpu_used: u64,
        mem_used: u64,
        m: &Machine,
        quiet: bool,
    ) -> Verdict {
        verdict(
            cfg,
            &[],
            &spec_with(cpu, mem),
            cpu,
            mem,
            &used(cpu_used, mem_used),
            m,
            quiet,
        )
    }

    /// Gives the reason of an `Admit::No`, and fails on an `Admit::Yes`.
    fn wait_reason(a: Admit, what: &str) -> String {
        match a {
            Admit::Yes => panic!("{what}"),
            Admit::No { reason, .. } => reason,
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
            max_queue_time: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            learn_key: None,
            group: None,
            group_name: None,
            locks: vec![],
            claims: Default::default(),
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
        assert_eq!(size_check(&cfg, 4, 8 << 30), Size::Fits);
        assert_eq!(size_check(&cfg, 1, 1 << 30), Size::Fits);
    }

    #[test]
    fn a_job_larger_than_the_budget_is_too_big() {
        let cfg = cfg_with("4", "8GB");

        let Size::TooBig(reason) = size_check(&cfg, 64, 1 << 30) else {
            panic!("a job of 64 cores must not fit a budget of 4 cores");
        };
        assert!(
            reason.contains("cores"),
            "the reason must name the cores: {reason}"
        );

        let Size::TooBig(reason) = size_check(&cfg, 1, 64 << 30) else {
            panic!("a job of 64GB must not fit a budget of 8GB");
        };
        assert!(
            reason.contains("memory"),
            "the reason must name the memory: {reason}"
        );

        // A job that is too large in both values must give both reasons. The
        // agent then corrects the claim one time only.
        let Size::TooBig(reason) = size_check(&cfg, 64, 64 << 30) else {
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
        let m = machine_for(&cfg);
        let (cpu, mem) = (2, 64 << 20);

        // Two cores are in use. A job of two cores fits.
        assert!(matches!(
            admit_plain(&cfg, cpu, mem, 2, 64 << 20, &m),
            Admit::Yes
        ));

        // Four cores are in use. The same job must wait.
        let reason = wait_reason(
            admit_plain(&cfg, cpu, mem, 4, 64 << 20, &m),
            "a job must not start when the cores are in use",
        );
        assert!(reason.contains("cores"), "got: {reason}");

        // The memory is in use. The job must wait.
        let reason = wait_reason(
            admit_plain(&cfg, cpu, mem, 0, 224 << 20, &m),
            "a job must not start when the memory is in use",
        );
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
        let m = machine_for(&cfg);
        assert!(matches!(
            admit_plain(&cfg, 4, 256 << 20, 0, 0, &m),
            Admit::Yes
        ));
        assert_eq!(size_check(&cfg, 4, 256 << 20), Size::Fits);
    }

    /// The reserve keeps memory for the programs that qex does not control.
    #[test]
    fn the_reserve_stops_a_job_when_the_machine_is_full() {
        let mut cfg = cfg_with("4", "8GB");
        // Ask for a reserve that is larger than the machine. Each job must wait.
        cfg.system.reserve_mem = "1000GB".into();
        let m = machine_for(&cfg);
        let reason = wait_reason(
            admit_plain(&cfg, 1, 1 << 20, 0, 0, &m),
            "the reserve must stop this job",
        );
        assert!(reason.contains("reserve"), "got: {reason}");
    }

    /// A paused queue must expire NO job.
    ///
    /// # The fault that this test prevents
    ///
    /// `--max-queue-time` and a pause are two features that landed apart. Their
    /// meeting is the sharp edge: a person pauses the queue for a call of 30
    /// minutes, and every job with a limit below 30 minutes dies while nobody
    /// can start it. The person comes back to an empty queue, a set of
    /// `expired` records and a stop hook for each one, and the pause — which
    /// exists to protect the machine — deleted the work instead.
    #[test]
    fn a_paused_queue_expires_no_job() {
        let mut state = state_with(JobState::Queued, Some(60), 61);
        // Without the pause this job expires. That is the control: a test that
        // did not measure it would pass with the limit removed.
        assert_eq!(
            overdue(&state, None).len(),
            1,
            "the job must be over its limit before the pause, or this test \
             measures nothing"
        );

        state.paused.queue = Some(crate::pause::PauseRecord::new(1, None, None));
        assert!(
            overdue(&state, None).is_empty(),
            "a paused queue must expire no job"
        );
    }

    /// The time that the queue was paused must not count against the limit.
    ///
    /// # The fault that this test prevents
    ///
    /// A pause that only DELAYED the expiry would kill the same jobs one moment
    /// after the resume, and the person would see the same empty queue. The
    /// clock of the limit must stop, and it must run again at the resume.
    #[test]
    fn the_time_of_a_pause_does_not_count_against_the_limit() {
        // The job waited 61 seconds against a limit of 60, and 40 of those
        // seconds were a pause. 21 seconds count, so the job stays.
        let mut state = state_with(JobState::Queued, Some(60), 61);
        let id = state.queue[0];
        state.jobs.get_mut(&id).unwrap().status.queue_pause_secs = 40;
        assert!(
            overdue(&state, None).is_empty(),
            "21 seconds of a limit of 60 must not expire the job"
        );

        // The same job with a shorter pause is over the limit, so the credit
        // does not simply switch the limit off.
        state.jobs.get_mut(&id).unwrap().status.queue_pause_secs = 1;
        assert_eq!(
            overdue(&state, None).len(),
            1,
            "60 seconds of a limit of 60 must still expire the job"
        );
    }

    /// The end of a pause must give the time back, once, to the jobs that
    /// waited through it.
    #[test]
    fn the_end_of_a_pause_gives_the_time_back_to_each_job_that_waited() {
        let now = sys::now_secs();
        let mut state = state_with(JobState::Queued, Some(60), 100);
        let waiter = state.queue[0];

        // A second job, submitted 10 seconds AFTER the pause began. It takes
        // the part of the pause that it lived through, and no more.
        let late = add_job(&mut state, JobState::Queued, 1, Some(60), 20);

        crate::pause::credit_paused_wait(&mut state, now.saturating_sub(30), now);
        assert_eq!(
            state.jobs[&waiter].status.queue_pause_secs, 30,
            "a job that waited through the whole pause takes the whole pause"
        );
        assert_eq!(
            state.jobs[&late].status.queue_pause_secs, 20,
            "a job submitted during the pause takes the part after its \
             submission only"
        );
    }

    /// A pause must not give time back to a job that is not in the queue.
    #[test]
    fn a_pause_gives_no_time_back_to_a_job_that_operates() {
        let now = sys::now_secs();
        let mut state = state_with(JobState::Running, Some(60), 100);
        let running = state.queue[0];
        crate::pause::credit_paused_wait(&mut state, now.saturating_sub(30), now);
        assert_eq!(
            state.jobs[&running].status.queue_pause_secs, 0,
            "a job that operates does not wait in the queue"
        );
    }

    /// The end of a pause of the queue must start the settle timer again.
    ///
    /// # The fault that this test prevents
    ///
    /// `idle_since` says how long no job has operated. A paused queue is idle
    /// by construction, so at the end of the pause that timer is ALREADY
    /// satisfied, and the first job that qex starts would be an OVERSIZED job,
    /// alone, in front of everything that waited for hours. The deletion of
    /// this one line left the whole suite green before this test existed.
    #[test]
    fn the_end_of_a_pause_starts_the_settle_timer_again() {
        let now = sys::now_secs();
        let mut state = state_with(JobState::Queued, Some(60), 100);
        let record = crate::pause::PauseRecord::new(1, None, None);

        // A queue that has been idle since long ago. That is the state at the
        // end of a pause, and it is the state that must not survive.
        state.idle_since = Some(Instant::now() - Duration::from_secs(3600));
        crate::pause::end_queue_pause(&mut state, &record, now);

        let waited = state.idle_since.expect("the timer must exist").elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "the end of a pause must start the settle timer again; it says {waited:?}"
        );
    }

    /// A pause that reaches its `--for` must give the time back, in the same
    /// way as `qex resume queue`.
    ///
    /// # The fault that this test prevents
    ///
    /// The two ends of a pause were two blocks of code, and the deletion of the
    /// credit in the `--for` block left the suite green: a pause of 30 minutes
    /// that ended BY ITSELF still killed every job with a smaller limit, and
    /// only a pause that a person ended was safe. `pause::end_queue_pause` is
    /// now the one place, and this test holds both paths to it.
    #[test]
    fn a_pause_that_reaches_its_time_gives_the_time_back_too() {
        let now = sys::now_secs();
        let mut state = state_with(JobState::Queued, Some(60), 100);
        let id = state.queue[0];

        // A pause that began 30 seconds ago and ends now.
        let mut record = crate::pause::PauseRecord::new(1, None, Some(now));
        record.paused_at = now.saturating_sub(30);
        state.paused.queue = Some(record.clone());

        assert!(
            state.paused.expire(now),
            "a pause with a time in the past must end"
        );
        assert!(state.paused.queue.is_none());
        crate::pause::end_queue_pause(&mut state, &record, now);

        assert_eq!(
            state.jobs[&id].status.queue_pause_secs, 30,
            "a pause that ended by itself must give its time back"
        );
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
            events: crate::events::EventLog::new(),
            paused: crate::pause::Paused::default(),
            last_start_at: None,
            head: None,
            peer_claims: Default::default(),
            stop: false,
        }
    }

    #[test]
    fn a_missing_needed_record_breaks_the_job_but_a_missing_after_record_does_not() {
        let missing = uuid::Uuid::new_v4();

        let mut needs = state_with(JobState::Queued, None, 0);
        let needs_id = needs.queue[0];
        needs.jobs.get_mut(&needs_id).unwrap().spec.needs = vec![missing];
        let Depends::Broken { reason, root } = depends(&needs, needs_id) else {
            panic!("a missing success record must not count as success");
        };
        assert_eq!(root, missing);
        assert!(reason.contains("record") && reason.contains("gone"));

        let mut after = state_with(JobState::Queued, None, 0);
        let after_id = after.queue[0];
        after.jobs.get_mut(&after_id).unwrap().spec.after = vec![missing];
        assert!(matches!(depends(&after, after_id), Depends::Ready));
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

    /// THE BYPASS, AND ITS BOUND, THROUGH `choose`.
    ///
    /// This test is the whole rule in one place, and it must ASSERT A CHANGE.
    ///
    /// A scheduler test can pass with the mechanism and without it, because the
    /// job starts in the same pass either way. This test therefore forces the
    /// wait: a job of 4 cores sits at the front of a budget of 4 with one core
    /// already in use, so it can never start in this test. The pre-condition is
    /// captured first — the front job is queued and no job passed it — and then
    /// each pass asserts that the count MOVED.
    ///
    /// `choose` does not start the job that it chooses; `start_job` does. Each
    /// pass here therefore marks the chosen job as running by hand, in the same
    /// way as the real scheduler, or the next pass would choose it again.
    #[test]
    fn a_small_job_passes_a_job_that_this_queue_holds_back_but_only_twice() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();
        assert_eq!(
            state.cfg.queue.max_bypass, 2,
            "this test reads the default, so a change to the default must reach it"
        );

        // One core of the four is in use, so three are free.
        add_job(&mut state, JobState::Running, 1, None, 0);
        // The front of the queue needs all four cores. It can never start here.
        let front = add_job(&mut state, JobState::Queued, 4, None, 0);
        // Three jobs of one core wait behind it. Each of them fits now.
        let behind: Vec<uuid::Uuid> = (0..3)
            .map(|_| add_job(&mut state, JobState::Queued, 1, None, 0))
            .collect();

        // THE PRE-CONDITION. Without this, a later change that starts the front
        // job at once would make every assertion below vacuous.
        assert_eq!(state.jobs[&front].status.state, JobState::Queued);
        assert_eq!(state.jobs[&front].status.passed_by, 0);
        assert!(state.jobs[&front].status.blocked_since.is_none());

        // Pass 1 and pass 2: a small job passes the front, and the count MOVES.
        //
        // A time that no clock of this test can give, so a rewrite is visible.
        const MARKER: u64 = 1_000_000;
        let mut first_since: Option<u64> = None;
        for (pass, id) in behind.iter().take(2).enumerate() {
            let choice = choose(&mut state);
            assert_eq!(
                choice.start,
                Some(*id),
                "pass {}: the next small job must pass the front",
                pass + 1
            );
            assert_eq!(
                state.jobs[&front].status.passed_by,
                pass as u32 + 1,
                "pass {}: the count of the front job must move",
                pass + 1
            );
            // `blocked_since` IS THE MOMENT THE JOB REACHED THE FRONT, and not
            // the moment of the last pass.
            //
            // The first pass writes it. Each later pass must LEAVE IT. A write
            // on every pass would make the field mean "now", which contradicts
            // the description in `schema.rs`, and it would also mark the record
            // dirty at every turn of the scheduler — one write to the disk
            // every 500ms for a queue that does nothing at all.
            //
            // The marker below is what makes this measurable. `now_secs` counts
            // whole seconds, and the three passes of this test run inside one
            // second, so a pass that rewrote the field with the time of the day
            // would write the same number and no test could see it.
            let since = state.jobs[&front].status.blocked_since;
            match first_since {
                None => {
                    assert!(
                        since.is_some(),
                        "the front job must record when it reached the front"
                    );
                    first_since = Some(MARKER);
                    state.jobs.get_mut(&front).unwrap().status.blocked_since = first_since;
                }
                Some(first) => assert_eq!(
                    since,
                    Some(first),
                    "blocked_since must not move while the job stays at the front"
                ),
            }
            // The real scheduler starts this job. Do the same here.
            state.jobs.get_mut(id).unwrap().status.state = JobState::Running;
            state.queue.retain(|q| q != id);
        }

        // Pass 3: the front job has reached `max_bypass`. It now keeps the
        // capacity, and qex starts NOTHING — even though one core is free and
        // the third small job needs one core.
        let choice = choose(&mut state);
        assert_eq!(
            choice.start, None,
            "the front job must be unpassable at max_bypass, or a stream of \
             small jobs starves it"
        );
        assert_eq!(
            state.jobs[&front].status.passed_by, 2,
            "the count must stop"
        );
        assert_eq!(
            state.jobs[&front].status.blocked_since, first_since,
            "blocked_since must still name the moment that the job reached the front"
        );

        // The head says so, and the job behind reads the true cause and not its
        // position only.
        let head = state.head.as_ref().expect("the pass must record a head");
        assert_eq!(head.id, front);
        assert!(head.reserved);
        assert_eq!(head.blocker, "waits-for-capacity");

        let front_reason = state.jobs[&front].status.blocked_reason.clone().unwrap();
        assert!(
            front_reason.contains("qex starts no other job before this one"),
            "the front job must read the text for a job that keeps capacity: {front_reason}"
        );
        let last = state.jobs[&behind[2]]
            .status
            .blocked_reason
            .clone()
            .unwrap();
        assert!(
            last.contains("qex keeps the capacity for that job"),
            "the job behind must read WHY it waits: {last}"
        );
    }

    /// THE HEAD IS THE FIRST JOB THAT CANNOT START, AND NOT THE LAST.
    ///
    /// The pass keeps testing after a head that does not keep capacity, so that
    /// every job gets a reason of its own. A pass that forgot which job was
    /// already the head would replace it with each later job that waits, and
    /// `qex info` would then name the wrong job at the front. The count of the
    /// bypasses would also move to that wrong job, and the real front job would
    /// collect no count and never become unpassable.
    ///
    /// The measured fault of #10 is the second half of this test: a job behind
    /// a job that keeps no capacity read a sentence about queue position, which
    /// names no cause. It must read ITS OWN cause.
    #[test]
    fn the_head_stays_the_first_job_that_cannot_start() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();
        // A job that the config parks never starts, so it keeps no capacity.
        state.cfg.queue.oversized = OversizedPolicy::Queue;

        // One core of the four is in use, so three are free.
        add_job(&mut state, JobState::Running, 1, None, 0);
        // The front of the queue is larger than the whole budget.
        let parked = add_job(&mut state, JobState::Queued, 64, None, 0);
        // Behind it, a job of four cores cannot fit beside the running job.
        let squeezed = add_job(&mut state, JobState::Queued, 4, None, 0);
        // Behind that, a job of one core fits.
        let small = add_job(&mut state, JobState::Queued, 1, None, 0);

        // THE PRE-CONDITION: none of the three has started.
        for id in [parked, squeezed, small] {
            assert_eq!(state.jobs[&id].status.state, JobState::Queued);
        }

        let choice = choose(&mut state);
        assert_eq!(
            choice.start,
            Some(small),
            "the queue must continue behind a job that keeps no capacity"
        );

        let head = state.head.as_ref().expect("the pass must record a head");
        assert_eq!(
            head.id, parked,
            "the head is the FIRST job that cannot start, and not the last one"
        );
        assert_eq!(head.blocker, "parked");
        assert!(!head.reserved);

        // The measured fault. The job in the middle must read its own cause.
        let reason = state.jobs[&squeezed].status.blocked_reason.clone().unwrap();
        assert!(
            reason.contains("waits for cores"),
            "the job behind must read its OWN cause: {reason}"
        );
        assert!(
            !reason.contains("at the front of the queue"),
            "a sentence about queue position names no cause: {reason}"
        );
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
    ///
    /// This test sets `max_bypass = 0`, so the job at the front keeps the
    /// capacity and the job behind it cannot start. That is the case in which
    /// the job behind reaches its limit and needs the remedy. With the default
    /// of 2 the job behind would simply start, and the test would cover
    /// nothing. `a_small_job_passes_a_job_that_this_queue_holds_back_but_only_twice`
    /// covers the bypass itself.
    #[test]
    fn a_job_behind_a_large_job_gets_the_remedy_for_capacity() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();
        state.cfg.queue.max_bypass = 0;

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
    /// This guard is defensive. A current job between two attempts of
    /// `--retries` stays `running`. An older record in that gap can be `queued`
    /// and still hold the `started_at` of the attempt that failed. A coordinator
    /// that starts again puts every `queued` record back in the queue. The state
    /// alone cannot separate that job from one that never ran.
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

    /// The queue tests the claim that the RECORD holds.
    ///
    /// The record is the one authority for a claim: the scheduler adds the
    /// claim of each job that operates from the record, and it must test a new
    /// job against that same number. A test against a different number would
    /// admit a job of 1GB into the space that qex keeps for 600MB, and the sum
    /// of the claims would go above the budget. Stopping that is the work of
    /// this module.
    #[test]
    fn the_queue_tests_the_claim_that_the_job_holds_now() {
        let cfg = cfg_with("4", "1GB");
        let m = machine_for(&cfg);

        // A job of 400MB operates. The first claim of 600MB fits beside it.
        assert!(matches!(
            admit_plain(&cfg, 1, 600 << 20, 1, 400 << 20, &m),
            Admit::Yes
        ));

        // A claim of 1GB does not fit beside it, and it must wait.
        let reason = wait_reason(
            admit_plain(&cfg, 1, 1 << 30, 1, 400 << 20, &m),
            "a claim that does not fit must wait for capacity",
        );
        assert!(reason.contains("memory"), "got: {reason}");

        // The claim alone still fits the budget, so the job is not an
        // oversized job and it starts when the other job stops.
        assert_eq!(size_check(&cfg, 1, 1 << 30), Size::Fits);
        assert!(matches!(
            admit_plain(&cfg, 1, 1 << 30, 0, 0, &m),
            Admit::Yes
        ));
    }

    /// The pressure limit stops a job while the machine reclaims memory.
    #[test]
    fn the_pressure_limit_stops_a_job() {
        let mut cfg = cfg_with("4", "8GB");
        cfg.system.max_pressure = -1.0;
        if sys::memory_pressure().is_some() {
            let m = machine_for(&cfg);
            let reason = wait_reason(
                admit_plain(&cfg, 1, 1 << 20, 0, 0, &m),
                "the pressure limit must stop this job",
            );
            assert!(reason.contains("pressure"), "got: {reason}");
        }
    }

    // -----------------------------------------------------------------------
    // The order of the queue, and who holds the capacity.
    //
    // The fault that these tests prevent: one job that another user held back
    // sat at the front of the queue and kept every job behind it in the queue.
    // -----------------------------------------------------------------------

    /// Makes a config with a peer that holds capacity.
    ///
    /// The record goes in the directory of THIS user with a pid that is not
    /// this process. `peers::claims` skips the record of this coordinator only,
    /// because one user can have more than one coordinator. A record with a
    /// different user id is not possible here: the reader tests the owner of
    /// the file, and this process owns each file that it writes.
    fn cfg_with_peer(
        cpu: &str,
        mem: &str,
        peer_cpu: u64,
        peer_mem: u64,
    ) -> (Config, std::path::PathBuf, std::process::Child) {
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
        // The record must name a process that is alive and is not this test.
        // `claims` skips (this user, this pid), and `pid_alive(1)` is false on
        // some runners: `kill(1, 0)` gives ESRCH, the peer is dropped, and the
        // test then measures an empty machine.
        let live = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the test needs a live process for the peer record");
        let peer = serde_json::json!({
            "uid": uid,
            "pid": live.id() as i32,
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
        (cfg, dir, live)
    }

    /// The measured fault: a job that another user holds back read a sentence
    /// about queue position, and the user had no way to learn the cause.
    ///
    /// The words must name the other user, and the class must be `Peer`, so the
    /// queue starts the jobs behind this one.
    #[test]
    fn a_job_that_another_user_holds_back_says_so_and_never_keeps_capacity() {
        let (cfg, dir, mut live) = cfg_with_peer("4", "256MB", 3, 0);
        let machine = Machine::read(&cfg);
        assert_eq!(machine.peers.count, 1, "the test peer must count");

        let Admit::No {
            blocker,
            reason,
            held_reason,
        } = admit_plain(&cfg, 4, 64 << 20, 0, 0, &machine)
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
            admit_plain(&cfg, 1, 64 << 20, 0, 0, &machine),
            Admit::Yes
        ));

        live.kill().ok();
        let _ = live.wait();
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
        } = verdict_plain(&cfg, 64, 64 << 20, 0, 0, &machine, false)
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
        } = verdict_plain(&cfg, 64, 64 << 20, 0, 0, &machine, false)
        else {
            panic!("a job of 64 cores must not start on a busy machine");
        };
        assert_eq!(blocker, Blocker::OversizedWaitsForIdle);
        assert!(held_reason.is_some());

        // The same job starts alone on a quiet machine.
        assert!(matches!(
            verdict_plain(&cfg, 64, 64 << 20, 0, 0, &machine, true),
            Verdict::Start
        ));
    }

    /// "Idle" covers the machine shared by every coordinator, and not this
    /// queue only. Otherwise two users can each force an oversized job into
    /// the same budget at once.
    #[test]
    fn an_oversized_job_does_not_bypass_peers_or_memory_pressure() {
        let mut cfg = cfg_with("2", "256MB");
        cfg.system.max_pressure = 50.0;

        let mut peer_machine = Machine {
            available: u64::MAX / 2,
            pressure: None,
            peers: crate::peers::Claims {
                cpu: 1,
                count: 1,
                ..Default::default()
            },
        };
        assert!(matches!(
            verdict_plain(&cfg, 64, 64 << 20, 0, 0, &peer_machine, true),
            Verdict::Wait { .. }
        ));

        peer_machine.peers = crate::peers::Claims::default();
        peer_machine.pressure = Some(51.0);
        assert!(matches!(
            verdict_plain(&cfg, 64, 64 << 20, 0, 0, &peer_machine, true),
            Verdict::Wait { .. }
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
        } = admit_plain(&cfg, 4, 64 << 20, 2, 0, &machine)
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

    /// The SIZE of the claim decides the class of a wait.
    ///
    /// A job whose claim fits the budget waits for its siblings. A job whose
    /// claim is above the budget waits for a quiet machine, because no set of
    /// siblings can release sufficient capacity for it. A scheduler that gave
    /// the second job the class of the first would let it hold capacity as a
    /// `Sibling` and wait for a moment that does not come.
    #[test]
    fn the_class_of_a_wait_uses_the_claim_in_force() {
        let cfg = cfg_with("4", "256MB");
        let machine = Machine::read(&cfg);

        // The first claim fits the budget, so a wait is a sibling wait.
        let Verdict::Wait { blocker, .. } = verdict_plain(&cfg, 4, 64 << 20, 2, 0, &machine, false)
        else {
            panic!("a job of 4 cores must not start while 2 are in use");
        };
        assert_eq!(blocker, Blocker::Sibling);

        // The claim is larger than the budget, so the same job now waits
        // for a quiet machine.
        let Verdict::Wait { blocker, .. } =
            verdict_plain(&cfg, 4, 512 << 20, 2, 0, &machine, false)
        else {
            panic!("a claim above the budget must not start");
        };
        assert_eq!(blocker, Blocker::OversizedWaitsForIdle);
    }

    // -----------------------------------------------------------------------
    // The pools
    // -----------------------------------------------------------------------

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

    /// Calls `admit` for one job that claims pools.
    fn admit_claim(cfg: &Config, pools: &[Pool], spec: &JobSpec, held: &Held) -> Admit {
        admit(
            cfg,
            pools,
            &effective_claims(spec),
            spec.cpu,
            spec.mem,
            held,
            &Machine::read(cfg),
        )
    }

    /// Puts one job that received `given` into the load.
    fn hold(held: &mut Held, spec: &JobSpec, given: BTreeMap<String, Assignment>, pools: &[Pool]) {
        let mut status = crate::job::JobStatus::new(spec);
        status.assigned = given;
        held.add(&status, pools);
    }

    /// A machine with no GPU must still admit a GPU claim from the
    /// configuration. This is what "a promise, and not a probe" means.
    #[test]
    fn a_machine_with_no_gpu_admits_a_gpu_claim_from_the_configuration() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let spec = spec_claiming(&[("gpu", 2, None)]);
        assert!(pool_check(&cfg, &spec).is_ok());
        assert_eq!(size_check(&cfg, spec.cpu, spec.mem), Size::Fits);
        assert!(matches!(
            admit_claim(&cfg, &pools, &spec, &Held::default()),
            Admit::Yes
        ));
    }

    /// qex must never add the memory of the devices together. Four devices of
    /// 24GB are not 96GB for one job, and the largest device here is 24GB.
    #[test]
    fn vram_is_never_added_together_over_the_devices() {
        let cfg = cfg_with_pools();
        let reason = pool_check(&cfg, &spec_claiming(&[("gpu", 2, Some(40 << 30))]))
            .expect_err("a claim of 40GB on each device must be impossible");
        assert!(
            reason.contains("never start"),
            "the message must say that the job can never start: {reason}"
        );
        assert!(
            reason.contains("24GB"),
            "the message must name the largest device: {reason}"
        );

        // The same quantity on ONE device is correct, so the test measures the
        // size of each device and not the sum.
        assert!(pool_check(&cfg, &spec_claiming(&[("gpu", 2, Some(20 << 30))])).is_ok());
    }

    /// A claim above the pool total is a refusal, and not an oversized job.
    /// An empty machine does not make a fifth device.
    #[test]
    fn a_claim_above_the_pool_total_can_never_start() {
        let cfg = cfg_with_pools();
        let reason = pool_check(&cfg, &spec_claiming(&[("gpu", 8, None)]))
            .expect_err("a claim of 8 devices from a pool of 4 must be impossible");
        assert!(reason.contains("never start"), "got: {reason}");

        let reason = pool_check(&cfg, &spec_claiming(&[("net", 5, None)]))
            .expect_err("a claim of 5 units from a pool of 4 must be impossible");
        assert!(reason.contains("never start"), "got: {reason}");
    }

    /// A job whose claim the configuration can never satisfy must be
    /// `OversizedParked`, and it must NEVER keep capacity.
    ///
    /// The precedent is `[queue] oversized = "queue"`. Such a job never starts,
    /// so capacity that qex keeps for it buys the job nothing and stops every
    /// other job. This is the case that parks a whole queue behind one job.
    #[test]
    fn a_claim_that_can_never_start_parks_and_never_reserves() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let spec = spec_claiming(&[("gpu", 8, None)]);
        let Verdict::Wait {
            blocker,
            held_reason,
            ..
        } = verdict(
            &cfg,
            &pools,
            &spec,
            spec.cpu,
            spec.mem,
            &Held::default(),
            &Machine::read(&cfg),
            false,
        )
        else {
            panic!("a claim of 8 devices from a pool of 4 must not start");
        };
        assert_eq!(blocker, Blocker::OversizedParked);
        assert!(
            !blocker.may_reserve(),
            "a job that can never start must never keep capacity"
        );
        assert!(
            held_reason.is_none(),
            "a class that never reserves must have no held text"
        );
    }

    /// A pool that the jobs of THIS queue hold is a `Sibling` wait, and the
    /// head may keep capacity for it: qex schedules that release.
    #[test]
    fn a_pool_that_this_queue_holds_is_a_sibling_wait_and_may_reserve() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("net", 3, None)]);

        // The pre-condition: with nothing held, the job starts.
        assert!(matches!(
            admit_claim(&cfg, &pools, &claim, &Held::default()),
            Admit::Yes
        ));

        // Two of the four units go to a job of this queue. The state CHANGED.
        let mut held = Held::default();
        held.pools.insert("net".into(), 2);
        let Admit::No {
            blocker,
            reason,
            held_reason,
        } = admit_claim(&cfg, &pools, &claim, &held)
        else {
            panic!("3 of `net` must not fit while 2 of the 4 are in use");
        };
        assert_eq!(blocker, Blocker::Sibling);
        assert!(blocker.may_reserve());
        assert!(reason.contains("net"), "got: {reason}");
        assert!(
            held_reason.is_some(),
            "a class that may reserve must give the text for a reserved head"
        );
    }

    /// A pool that ANOTHER USER holds is a `Peer` wait, and the head must
    /// never keep capacity for it.
    ///
    /// qex cannot make the other user give the device back, so a reservation
    /// would hold this machine empty for a time that qex cannot measure. That
    /// is the fault that parks a queue for hours.
    #[test]
    fn a_pool_that_another_user_holds_never_reserves() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("net", 3, None)]);

        let mut peers = crate::peers::Claims {
            count: 1,
            ..Default::default()
        };
        peers.pools.insert("net".into(), 2);
        let machine = Machine {
            available: u64::MAX / 2,
            pressure: None,
            peers,
        };

        let Admit::No {
            blocker,
            held_reason,
            ..
        } = admit(
            &cfg,
            &pools,
            &effective_claims(&claim),
            claim.cpu,
            claim.mem,
            &Held::default(),
            &machine,
        )
        else {
            panic!("3 of `net` must not fit while another user holds 2 of the 4");
        };
        assert_eq!(blocker, Blocker::Peer { count: 1 });
        assert!(
            !blocker.may_reserve(),
            "a wait on another user must never keep capacity"
        );
        assert!(held_reason.is_none());
    }

    /// A peer can hold part of a pool without causing this shortage. When the
    /// claim fits after our siblings release their units, the scheduler owns
    /// the end of the wait and must reserve after the bypass limit.
    #[test]
    fn a_peer_does_not_hide_a_pool_shortage_caused_by_siblings() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("net", 2, None)]);
        let mut held = Held::default();
        held.pools.insert("net".into(), 2);
        let mut peers = crate::peers::Claims {
            count: 1,
            ..Default::default()
        };
        peers.pools.insert("net".into(), 1);
        let machine = Machine {
            available: u64::MAX / 2,
            pressure: None,
            peers,
        };

        let Admit::No {
            blocker,
            held_reason,
            ..
        } = admit(
            &cfg,
            &pools,
            &effective_claims(&claim),
            claim.cpu,
            claim.mem,
            &held,
            &machine,
        )
        else {
            panic!("only one of the two requested units is free now");
        };
        assert_eq!(blocker, Blocker::Sibling);
        assert!(blocker.may_reserve());
        assert!(held_reason.is_some());
    }

    /// A name that the configuration does not declare is a lock of one unit.
    /// `--lock NAME` needs no configuration, and it must keep that.
    #[test]
    fn an_undeclared_pool_name_is_a_lock_and_not_an_error() {
        let cfg = cfg_with_pools();
        assert!(pool_check(&cfg, &spec_claiming(&[("build-dir", 1, None)])).is_ok());

        // More than one unit of a pool that nobody declared is a fault. qex
        // cannot invent a second unit.
        let reason = pool_check(&cfg, &spec_claiming(&[("build-dir", 2, None)]))
            .expect_err("2 of an undeclared pool must be impossible");
        assert!(reason.contains("lock of size 1"), "got: {reason}");
    }

    /// `--gpu` promises a device index and an environment variable. A machine
    /// with no `gpu` pool can give neither, so qex says so and does not make a
    /// silent lock.
    #[test]
    fn a_gpu_claim_with_no_gpu_pool_names_the_configuration() {
        let cfg = cfg_with("4", "1GB");
        let reason = pool_check(&cfg, &spec_claiming(&[("gpu", 1, None)]))
            .expect_err("a GPU claim with no pool must be impossible");
        assert!(reason.contains("[[pool]]"), "got: {reason}");
        assert!(reason.contains("qex.toml"), "got: {reason}");
    }

    /// VRAM is a quantity on a device. A job that asks for it must also ask
    /// for a device.
    #[test]
    fn vram_with_no_device_claim_is_refused() {
        let cfg = cfg_with_pools();
        let reason = pool_check(&cfg, &spec_claiming(&[("gpu", 0, Some(4 << 30))]))
            .expect_err("VRAM with no device must be impossible");
        assert!(reason.contains("--gpu 1"), "got: {reason}");
    }

    /// A pool with no devices holds no size.
    #[test]
    fn a_size_on_a_pool_with_no_devices_is_refused() {
        let cfg = cfg_with_pools();
        let reason = pool_check(&cfg, &spec_claiming(&[("net", 1, Some(1 << 30))]))
            .expect_err("a size on a plain pool must be impossible");
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
        hold(&mut held, &claim, first, &pools);

        let second = assign(&pools, &effective_claims(&claim), &held, &no_peers).unwrap();
        assert_eq!(
            second["gpu"].devices,
            vec![1],
            "the second job must get a device that the first job does not hold"
        );
        hold(&mut held, &claim, second, &pools);

        // Both devices are in use. The third job must wait.
        let Admit::No { reason, .. } = admit_claim(&cfg, &pools, &claim, &held) else {
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

        // The pre-condition: a small claim fits while nothing holds the device.
        let small = spec_claiming(&[("gpu", 1, Some(1 << 30))]);
        assert!(assign(&pools, &effective_claims(&small), &held, &no_peers).is_ok());

        hold(&mut held, &whole, given, &pools);
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
        for n in 0..3 {
            let given = assign(&pools, &effective_claims(&claim), &held, &no_peers)
                .unwrap_or_else(|e| panic!("the job {n} of 8GB must fit a device of 24GB: {e}"));
            assert_eq!(given["gpu"].devices, vec![0]);
            hold(&mut held, &claim, given, &pools);
        }
        // 24GB holds three jobs of 8GB, and no fourth.
        assert_eq!(held.devices["gpu"][&0], 24 << 30);
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
        let no_peers = crate::peers::Claims::default();

        // The pre-condition: with no other user, all four devices are free.
        assert!(assign(
            &pools,
            &effective_claims(&claim),
            &Held::default(),
            &no_peers
        )
        .is_ok());

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
        assert!(is_a_lock(&[], "target"));
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

    /// A LOCK must never go through `pool_wait`.
    ///
    /// `lock_conflict` answers a lock in pass 1, so the job never becomes the
    /// head and never keeps capacity. A `pool_wait` that also answered it would
    /// make the job the head and park the whole queue behind one lock.
    ///
    /// A DECLARED pool is the other case, and it does go through `pool_wait`
    /// whatever its size. See `is_a_lock`.
    #[test]
    fn a_lock_never_becomes_a_head_that_keeps_capacity() {
        let mut held = Held::default();
        held.pools.insert("target".into(), 1);
        let mut spec = spec_with(1, 1 << 20);
        spec.locks = vec!["target".into()];
        assert!(
            pool_wait(
                &[],
                &effective_claims(&spec),
                &held,
                &crate::peers::Claims::default()
            )
            .is_none(),
            "a lock must not reach the capacity pass"
        );
    }

    /// A DECLARED POOL IS ALWAYS COUNTED, EVEN WHEN IT HAS ONE DEVICE.
    ///
    /// This is the rule that closes the hole. `lock_conflict` reads the jobs of
    /// THIS queue only, and `pool_wait` is the one place that reads what the
    /// other users of the machine hold. A rule that sent a pool of one device
    /// through the lock path would meet no peer test at all, and two users
    /// would then each start a job on the device 0.
    #[test]
    fn a_declared_pool_of_one_device_is_counted_and_sees_the_other_users() {
        let cfg: Config = toml::from_str(
            "[budget]\ncpu = \"8\"\nmem = \"8GB\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n\
             [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [\"24GB\"]\n",
        )
        .unwrap();
        let pools = cfg.pools().unwrap();
        let whole = spec_claiming(&[("gpu", 1, None)]);

        assert!(
            !is_a_lock(&pools, "gpu"),
            "a declared pool is counted, whatever its size"
        );
        assert!(
            is_a_lock(&pools, "build-dir"),
            "a name that the configuration does not declare is a lock"
        );

        // The pre-condition: with no other user, the one device is free.
        let machine_free = Machine {
            available: u64::MAX / 2,
            pressure: None,
            peers: crate::peers::Claims::default(),
        };
        assert!(matches!(
            admit(
                &cfg,
                &pools,
                &effective_claims(&whole),
                whole.cpu,
                whole.mem,
                &Held::default(),
                &machine_free,
            ),
            Admit::Yes
        ));

        // Another user holds the device 0. The state CHANGED: this job must
        // now wait, and it must never keep capacity.
        let mut peers = crate::peers::Claims {
            count: 1,
            ..Default::default()
        };
        peers
            .devices
            .insert("gpu".into(), [0u32].into_iter().collect());
        let machine_busy = Machine {
            available: u64::MAX / 2,
            pressure: None,
            peers,
        };
        let Admit::No { blocker, .. } = admit(
            &cfg,
            &pools,
            &effective_claims(&whole),
            whole.cpu,
            whole.mem,
            &Held::default(),
            &machine_busy,
        ) else {
            panic!("a device that another user holds must not go to this job as well");
        };
        assert_eq!(blocker, Blocker::Peer { count: 1 });
        assert!(!blocker.may_reserve());
    }

    /// A partial claim on a pool of one device must still divide that device.
    #[test]
    fn a_partial_claim_on_one_device_still_shares_the_device() {
        let cfg: Config = toml::from_str(
            "[budget]\ncpu = \"8\"\nmem = \"8GB\"\n\
             [system]\nreserve_mem = \"0\"\nmax_pressure = 100\n\
             [peers]\nenabled = false\n\
             [[pool]]\nname = \"gpu\"\nsize = \"vram\"\ndevices = [\"24GB\"]\n",
        )
        .unwrap();
        let pools = cfg.pools().unwrap();
        let part = spec_claiming(&[("gpu", 1, Some(4 << 30))]);

        let mut held = Held::default();
        let given = assign(
            &pools,
            &effective_claims(&part),
            &held,
            &crate::peers::Claims::default(),
        )
        .unwrap();
        hold(&mut held, &part, given, &pools);
        assert!(matches!(
            admit_claim(&cfg, &pools, &part, &held),
            Admit::Yes
        ));
    }

    /// A claim of one unit made with `--claim NAME=1` must exclude in the same
    /// way as `--lock NAME`.
    ///
    /// `lock_conflict` reads the REQUEST and the RESULT. A version that read
    /// `spec.locks` only would let two jobs take one unit: the second job would
    /// find no holder and start beside the first.
    #[test]
    fn a_claim_of_one_unit_excludes_in_the_same_way_as_a_lock() {
        let mut state = state_with(JobState::Running, None, 0);
        state.queue.clear();
        state.jobs.clear();

        let holder = add_job(&mut state, JobState::Running, 1, None, 0);
        state.jobs.get_mut(&holder).unwrap().spec.claims.insert(
            "port".into(),
            PoolClaim {
                count: 1,
                size: None,
            },
        );

        let mut spec = spec_with(1, 1 << 20);
        spec.claims.insert(
            "port".into(),
            PoolClaim {
                count: 1,
                size: None,
            },
        );

        // The pre-condition: a job that claims a DIFFERENT name is free.
        let mut other = spec_with(1, 1 << 20);
        other.claims.insert(
            "other-port".into(),
            PoolClaim {
                count: 1,
                size: None,
            },
        );
        assert!(lock_conflict(&state, &[], &other).is_none());

        let reason = lock_conflict(&state, &[], &spec)
            .expect("a second claim of the one unit must wait for the first");
        assert!(reason.contains("port"), "got: {reason}");
    }

    /// A pool with more than one unit counts, and it does not behave as a lock.
    #[test]
    fn a_counted_pool_admits_jobs_until_it_is_full() {
        let cfg = cfg_with_pools();
        let pools = cfg.pools().unwrap();
        let claim = spec_claiming(&[("net", 3, None)]);

        let mut held = Held::default();
        assert!(matches!(
            admit_claim(&cfg, &pools, &claim, &held),
            Admit::Yes
        ));

        held.pools.insert("net".into(), 2);
        let Admit::No { reason, .. } = admit_claim(&cfg, &pools, &claim, &held) else {
            panic!("3 of `net` must not fit while 2 of 4 are in use");
        };
        assert!(reason.contains("net"), "got: {reason}");
    }
}
