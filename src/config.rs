//! This module reads the config file `~/.config/qex.toml`.
//!
//! Each field has a default value. The config file is thus optional. If the
//! file does not exist, qex uses the default values and does not give an error.

use crate::{paths, sys, units};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Selects the quantity of the environment that a job receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EnvCapture {
    /// Copy all the variables of the process that submits the job.
    ///
    /// This is the default. The job then operates in the same way as a command
    /// that you type in that shell.
    #[default]
    All,
    /// Copy the variables in the `minimal_env` list only.
    ///
    /// Use this mode if the shell holds secrets.
    Minimal,
    /// Start with an empty environment.
    ///
    /// The job receives the values from `[env]` and from `--env` only.
    None,
}

impl std::str::FromStr for EnvCapture {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "minimal" => Ok(Self::Minimal),
            "none" => Ok(Self::None),
            other => Err(format!(
                "unknown env capture mode `{other}`; expected all, minimal or none"
            )),
        }
    }
}

/// Selects the operation for a job that is larger than the full budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OversizedPolicy {
    /// Start the job alone when no other job operates.
    ///
    /// The job can then cause swap operations, use all the cores, or stop
    /// because of the out-of-memory killer. Each of these results is data that
    /// the agent needs. A job that stays in the queue supplies no data.
    #[default]
    RunWhenIdle,
    /// Refuse the job at submission. Use this mode on a strict shared machine.
    Reject,
    /// Keep the job in the queue. Do not start it.
    Queue,
}

/// Selects if qex applies the claimed limits to the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EnforceMode {
    /// Use the claims for the queue only. Do not set a limit on the job.
    ///
    /// This is the default mode. It is the only mode on macOS.
    #[default]
    Off,
    /// Set `memory.high` to the claim. The kernel then slows the job and
    /// reclaims memory. Set `memory.max` to `claim * mem_overcommit` as a
    /// second limit.
    Soft,
    /// Set `memory.max` to the claim. The kernel stops a job that goes above it.
    Hard,
}

impl EnforceMode {
    pub fn is_on(self) -> bool {
        self != Self::Off
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// The number of cores that qex can use.
    ///
    /// Give an integer, or a percentage of the machine.
    pub cpu: String,
    /// The quantity of memory that qex can use.
    ///
    /// Give a size, or a percentage of the machine.
    pub mem: String,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            cpu: "75%".into(),
            mem: "75%".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemConfig {
    /// The quantity of memory to keep free at all times.
    ///
    /// qex does not start a job if the job decreases the available memory below
    /// this value. This test finds the load from other users and from programs
    /// that qex does not control.
    pub reserve_mem: String,
    /// The maximum permitted memory pressure.
    ///
    /// qex does not start a job while the PSI value is above this limit.
    /// Linux supplies this measurement. macOS does not.
    pub max_pressure: f64,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            reserve_mem: "2GB".into(),
            max_pressure: 20.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnforceConfig {
    pub mode: EnforceMode,
    /// The multiplier for the second memory limit in the soft mode.
    ///
    /// qex sets `memory.max` to the claim multiplied by this value.
    pub mem_overcommit: f64,
    /// Permits qex to start the coordinator in a temporary systemd unit.
    ///
    /// qex uses the command `systemd-run --user`. The coordinator then controls
    /// its own cgroup, and it can set a memory limit on each job. systemd holds
    /// a temporary unit in memory and writes no file to the disk.
    pub use_systemd: bool,
}

impl Default for EnforceConfig {
    fn default() -> Self {
        Self {
            mode: EnforceMode::Off,
            mem_overcommit: 1.5,
            use_systemd: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeersConfig {
    pub enabled: bool,
    pub dir: String,
    pub stale_after: String,
}

impl Default for PeersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "/tmp/qex".into(),
            stale_after: "30s".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    pub oversized: OversizedPolicy,
    /// The time that the queue must stay empty before qex starts a large job.
    ///
    /// This delay prevents a start while the last jobs stop.
    pub settle: String,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            oversized: OversizedPolicy::RunWhenIdle,
            settle: "3s".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubmitConfig {
    pub env_capture: EnvCapture,
    pub minimal_env: Vec<String>,
}

impl Default for SubmitConfig {
    fn default() -> Self {
        Self {
            env_capture: EnvCapture::All,
            // These variables let a command find its interpreter and its home
            // directory. With fewer variables, `uv`, `git` and most language
            // runtimes fail.
            minimal_env: ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TZ"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// The job size that qex uses when a submission gives no size.
///
/// Each field is optional. If a field has no value, qex calculates a default.
/// See [`Config::default_cpu`] and [`Config::default_mem`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    /// The number of cores for a job. The default is 1 core.
    pub cpu: Option<u64>,
    /// The quantity of memory for a job.
    ///
    /// The default is the machine memory divided by the number of cores.
    pub mem: Option<String>,
    /// The time limit for a job. The default is `0`, which sets no limit.
    pub timeout: Option<String>,
    /// The quantity on each device for a job that claims a device.
    ///
    /// The value `0`, and no value, mean that the job takes the whole of each
    /// device that qex gives it. That is the safe value: a job that consumed
    /// nothing would let qex put four unlimited jobs on one card.
    pub vram: Option<String>,
}

/// The name of the pool that `--gpu` and `--vram` claim.
///
/// The two options are fixed names in the command line, because clap cannot
/// grow an option from a config file at run time, and because an agent that
/// writes `--gpu 1` must be correct on the first try. The scheduler holds no
/// special case for this name.
pub const GPU_POOL: &str = "gpu";

/// One pool of a countable resource, as the config file gives it.
///
/// A pool has a name and a total. A pool with `devices` also has a capacity for
/// each device, and qex then says WHICH device each job gets.
///
/// qex never probes a driver. The devices come from this file only, so a
/// machine with no CUDA and no driver library schedules GPU claims correctly.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolConfig {
    /// The name that a job claims: `--claim NAME=N`, or `--gpu` for `gpu`.
    pub name: String,
    /// The name of the quantity that each device holds, such as `vram`.
    ///
    /// This value is for the reader of the configuration and of the messages.
    pub size: Option<String>,
    /// The capacity of each device. Give one size for each device.
    pub devices: Vec<String>,
    /// The number of units, for a pool that has no devices.
    pub count: Option<u64>,
    /// The environment variable that receives the device indices.
    pub env: Option<String>,
}

/// One pool, with each value calculated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pool {
    pub name: String,
    /// The capacity of each device, in bytes. The list is empty for a pool
    /// that has no devices.
    pub devices: Vec<u64>,
    /// The number of units in the pool. For an indexed pool this is the number
    /// of devices.
    pub total: u64,
    /// The name of the quantity that each device holds, such as `vram`.
    pub size_name: Option<String>,
    /// The environment variable that receives the device indices.
    pub env: Option<String>,
}

impl Pool {
    /// Tests if qex says WHICH device each job of this pool gets.
    pub fn is_indexed(&self) -> bool {
        !self.devices.is_empty()
    }

    /// Gives the capacity of the largest device.
    pub fn largest_device(&self) -> u64 {
        self.devices.iter().copied().max().unwrap_or(0)
    }

    /// Gives the pool that qex uses for a name that the configuration does not
    /// declare.
    ///
    /// `--lock NAME` needs no configuration, and it must keep that. A name that
    /// the configuration does not declare is thus a pool of one unit, which is
    /// exactly a lock.
    pub fn implicit(name: &str) -> Self {
        Self {
            name: name.to_string(),
            devices: Vec::new(),
            total: 1,
            size_name: None,
            env: None,
        }
    }
}

/// Controls the command that collects the old records.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GcConfig {
    /// The age of a record that `qex gc` deletes.
    ///
    /// `qex gc` works on every directory, so this value is larger than the one
    /// hour of `qex clean --auto`, which works on one directory tree.
    pub keep: String,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self { keep: "1d".into() }
    }
}

/// Controls the short record of the jobs that qex accepted.
///
/// The record lets `qex status` tell "the record was deleted" from "this job
/// never existed". An agent then knows whether to submit the job again.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    /// The time to keep the id of a job after its record is gone.
    ///
    /// An agent asks about a job of the last minutes or hours. An id of last
    /// month answers no question, so qex does not keep it.
    pub keep: String,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { keep: "1d".into() }
    }
}

/// Controls the claim that qex calculates from the earlier jobs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LearnConfig {
    /// Permits qex to use the measurements of the earlier jobs as the claim.
    pub enabled: bool,
    /// The multiplier for a measurement.
    ///
    /// A measurement is the peak that qex saw. A job can use more with a larger
    /// input, so the claim is larger than the measurement.
    pub margin: f64,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            margin: 1.5,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub budget: BudgetConfig,
    pub system: SystemConfig,
    pub enforce: EnforceConfig,
    pub peers: PeersConfig,
    pub queue: QueueConfig,
    pub submit: SubmitConfig,
    pub defaults: DefaultsConfig,
    pub learn: LearnConfig,
    pub history: HistoryConfig,
    pub gc: GcConfig,
    /// The pools of countable resources. The key in the file is `[[pool]]`.
    #[serde(rename = "pool")]
    pub pools: Vec<PoolConfig>,
}

impl Config {
    /// Reads `~/.config/qex.toml`.
    ///
    /// If the file does not exist, this function gives the default values.
    pub fn load() -> Result<Self> {
        let path = paths::config_file()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parsing config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading config file {}", path.display())),
        }
    }

    /// Gives the budget in cores for this machine.
    pub fn budget_cpu(&self) -> Result<u64> {
        let total = sys::cpu_count();
        let n = units::parse_budget(&self.budget.cpu, total, false)
            .map_err(|e| anyhow::anyhow!("config [budget] cpu: {e}"))?;
        // A budget of zero keeps all the jobs in the queue for ever. A small
        // percentage that rounds down to zero does not have that intention.
        Ok(n.max(1))
    }

    /// Gives the budget in bytes for this machine.
    pub fn budget_mem(&self) -> Result<u64> {
        let total = sys::total_memory();
        let n = units::parse_budget(&self.budget.mem, total, true)
            .map_err(|e| anyhow::anyhow!("config [budget] mem: {e}"))?;
        // A budget of zero makes every job too large for the budget. Each job
        // then runs alone with a warning. A memory probe that gives zero, in an
        // unusual container, would cause that result.
        Ok(n.max(64 << 20))
    }

    pub fn reserve_mem(&self) -> Result<u64> {
        units::parse_size(&self.system.reserve_mem)
            .map_err(|e| anyhow::anyhow!("config [system] reserve_mem: {e}"))
    }

    pub fn settle(&self) -> Result<std::time::Duration> {
        units::parse_duration(&self.queue.settle)
            .map_err(|e| anyhow::anyhow!("config [queue] settle: {e}"))
            .map(|d| d.unwrap_or(std::time::Duration::ZERO))
    }

    /// Gives the age of a record that `qex gc` deletes.
    pub fn gc_keep(&self) -> Result<std::time::Duration> {
        units::parse_duration(&self.gc.keep)
            .map_err(|e| anyhow::anyhow!("config [gc] keep: {e}"))
            .map(|d| d.unwrap_or(std::time::Duration::from_secs(86400)))
    }

    /// Gives the time to keep the id of a job after its record is gone.
    pub fn history_keep(&self) -> Result<std::time::Duration> {
        units::parse_duration(&self.history.keep)
            .map_err(|e| anyhow::anyhow!("config [history] keep: {e}"))
            .map(|d| d.unwrap_or(std::time::Duration::from_secs(86400)))
    }

    pub fn peer_stale_after(&self) -> Result<std::time::Duration> {
        units::parse_duration(&self.peers.stale_after)
            .map_err(|e| anyhow::anyhow!("config [peers] stale_after: {e}"))
            .map(|d| d.unwrap_or(std::time::Duration::from_secs(30)))
    }

    /// Gives the default number of cores for a job.
    ///
    /// If the config file gives no value, the result is 1 core.
    pub fn default_cpu(&self) -> u64 {
        self.defaults.cpu.unwrap_or(1).max(1)
    }

    /// Gives the default quantity of memory for a job.
    ///
    /// If the config file gives no value, qex divides the machine memory by the
    /// number of cores. A job of 1 core thus receives an equal part of the
    /// memory. The default job size then scales with the machine.
    pub fn default_mem(&self) -> Result<u64> {
        match &self.defaults.mem {
            Some(s) => {
                units::parse_size(s).map_err(|e| anyhow::anyhow!("config [defaults] mem: {e}"))
            }
            None => {
                let cores = sys::cpu_count().max(1);
                let total = sys::total_memory();
                // If the memory probe fails, use a small claim. A claim of zero
                // lets qex start an unlimited number of jobs together.
                Ok((total / cores).max(1 << 28))
            }
        }
    }

    /// Gives the quantity on each device for a job that gives no value.
    ///
    /// The result `None` means that a job takes the whole of each device.
    pub fn default_vram(&self) -> Result<Option<u64>> {
        match &self.defaults.vram {
            Some(s) => {
                let n = units::parse_size(s)
                    .map_err(|e| anyhow::anyhow!("config [defaults] vram: {e}"))?;
                // `0` says "the whole device". A claim of zero bytes on a
                // device would let qex put an unlimited number of jobs on one
                // card, which is the fault that a claim prevents.
                Ok(if n == 0 { None } else { Some(n) })
            }
            None => Ok(None),
        }
    }

    /// Gives each pool with its values calculated.
    pub fn pools(&self) -> Result<Vec<Pool>> {
        let mut out = Vec::with_capacity(self.pools.len());
        for p in &self.pools {
            let name = p.name.trim().to_string();
            let mut devices = Vec::with_capacity(p.devices.len());
            for (i, d) in p.devices.iter().enumerate() {
                let bytes = units::parse_size(d)
                    .map_err(|e| anyhow::anyhow!("config [[pool]] `{name}` device {i}: {e}"))?;
                devices.push(bytes);
            }
            let total = if devices.is_empty() {
                p.count.unwrap_or(0)
            } else {
                devices.len() as u64
            };
            out.push(Pool {
                name,
                devices,
                total,
                size_name: p.size.clone(),
                env: p.env.clone(),
            });
        }
        Ok(out)
    }

    /// Gives the pool with one name, or `None`.
    pub fn pool(&self, name: &str) -> Option<Pool> {
        self.pools()
            .ok()
            .and_then(|all| all.into_iter().find(|p| p.name == name))
    }

    /// Tests each `[[pool]]` entry.
    fn validate_pools(&self) -> Result<()> {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for p in &self.pools {
            let name = p.name.trim();
            if name.is_empty() {
                anyhow::bail!(
                    "config [[pool]] has no name. Give a name, such as `gpu`. A job claims \
                     a pool by its name."
                );
            }
            if name == "cpu" || name == "mem" {
                anyhow::bail!(
                    "config [[pool]] cannot use the name `cpu` or `mem`. Those two are in \
                     [budget]."
                );
            }
            if !p.devices.is_empty() && p.count.is_some() {
                anyhow::bail!(
                    "config [[pool]] `{name}` gives `count` and `devices`. Give one only. \
                     Use `devices` when qex must say WHICH one a job gets. Use `count` when \
                     the number is sufficient."
                );
            }
            if p.devices.is_empty() && p.count.is_none() {
                anyhow::bail!(
                    "config [[pool]] `{name}` gives no `count` and no `devices`, so qex \
                     does not know its size. Give `count = 4`, or `devices = [\"24GB\", \
                     \"24GB\"]`."
                );
            }
            if let Some(0) = p.count {
                anyhow::bail!(
                    "config [[pool]] `{name}` has `count = 0`, so no job that claims it can \
                     ever start. Give 1 or more, or delete the pool."
                );
            }
            if !seen.insert(name.to_string()) {
                anyhow::bail!(
                    "config [[pool]] `{name}` has two entries with the same name. Give one \
                     entry for each pool."
                );
            }
        }
        self.pools()?;
        Ok(())
    }

    /// Gives the default time limit for a job.
    ///
    /// If the config file gives no value, the result is `None`. A job then has
    /// no time limit.
    pub fn default_timeout(&self) -> Result<Option<std::time::Duration>> {
        match &self.defaults.timeout {
            Some(s) => units::parse_duration(s)
                .map_err(|e| anyhow::anyhow!("config [defaults] timeout: {e}")),
            None => Ok(None),
        }
    }

    /// Reads each field that the config parser does not read immediately.
    ///
    /// Call this function at start. qex then reports an incorrect config file
    /// one time, and not at the moment that it first uses the field.
    pub fn validate(&self) -> Result<()> {
        self.budget_cpu()?;
        self.budget_mem()?;
        self.reserve_mem()?;
        self.settle()?;
        self.peer_stale_after()?;
        self.history_keep()?;
        self.gc_keep()?;
        self.default_mem()?;
        self.default_timeout()?;
        self.default_vram()?;
        self.validate_pools()?;
        if self.learn.margin < 1.0 {
            anyhow::bail!(
                "config [learn] margin is {}. Use a value of 1.0 or more. A smaller value \
                 gives a claim below the measurement, and the job would then stop.",
                self.learn.margin
            );
        }
        if self.enforce.mem_overcommit < 1.0 {
            anyhow::bail!(
                "config [enforce] mem_overcommit is {}. Use a value of 1.0 or more. \
                 A smaller value sets memory.max below memory.high. The kernel then \
                 stops every job that reaches its claim.",
                self.enforce.mem_overcommit
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_valid_and_gives_working_defaults() {
        let c: Config = toml::from_str("").unwrap();
        c.validate().unwrap();
        assert_eq!(c.submit.env_capture, EnvCapture::All);
        assert_eq!(c.queue.oversized, OversizedPolicy::RunWhenIdle);
        assert_eq!(c.enforce.mode, EnforceMode::Off);
        assert!(c.budget_cpu().unwrap() >= 1);
        assert_eq!(c.default_cpu(), 1);
        assert_eq!(c.default_timeout().unwrap(), None);
    }

    /// With no `[defaults]` section, a job gets 1 core and an equal part of the
    /// machine memory. The default job size thus scales with the machine.
    #[test]
    fn default_job_size_scales_with_the_machine() {
        let c = Config::default();
        let cores = sys::cpu_count().max(1);
        let expected = (sys::total_memory() / cores).max(1 << 28);
        assert_eq!(c.default_mem().unwrap(), expected);

        // The budget must hold at least one job of the default size. If it does
        // not, every job waits for ever.
        let budget_mem = c.budget_mem().unwrap();
        assert!(
            c.default_mem().unwrap() <= budget_mem,
            "a job of the default size ({}) does not fit the default budget ({})",
            units::format_size(c.default_mem().unwrap()),
            units::format_size(budget_mem)
        );
    }

    /// A value in the config file replaces the calculated default.
    #[test]
    fn config_defaults_replace_the_calculated_values() {
        let c: Config =
            toml::from_str("[defaults]\ncpu = 4\nmem = \"6GB\"\ntimeout = \"30m\"\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.default_cpu(), 4);
        assert_eq!(c.default_mem().unwrap(), 6 << 30);
        assert_eq!(
            c.default_timeout().unwrap(),
            Some(std::time::Duration::from_secs(1800))
        );
    }

    #[test]
    fn documented_config_parses() {
        // The command `qex help config` writes this example. If the parser
        // refuses it, the documentation is incorrect.
        let text = r#"
[budget]
cpu = "75%"
mem = "20GB"

[system]
reserve_mem = "2GB"
max_pressure = 20

[enforce]
mode = "soft"
mem_overcommit = 1.5
use_systemd = true

[peers]
enabled = true
dir = "/tmp/qex"
stale_after = "30s"

[queue]
oversized = "run-when-idle"
settle = "3s"

[submit]
env_capture = "minimal"
minimal_env = ["PATH", "HOME"]

[defaults]
cpu = 1
mem = "1GB"
timeout = "0"
vram = "0"

[[pool]]
name    = "gpu"
size    = "vram"
devices = ["24GB", "24GB", "24GB", "24GB"]
env     = "CUDA_VISIBLE_DEVICES"

[[pool]]
name  = "net"
count = 4
"#;
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.enforce.mode, EnforceMode::Soft);
        assert_eq!(c.submit.env_capture, EnvCapture::Minimal);
        assert_eq!(c.budget_mem().unwrap(), 20 << 30);
        assert_eq!(c.default_timeout().unwrap(), None);

        let gpu = c.pool("gpu").expect("the pool `gpu` must parse");
        assert!(gpu.is_indexed());
        assert_eq!(gpu.total, 4);
        assert_eq!(gpu.largest_device(), 24 << 30);
        assert_eq!(gpu.env.as_deref(), Some("CUDA_VISIBLE_DEVICES"));

        let net = c.pool("net").expect("the pool `net` must parse");
        assert!(!net.is_indexed());
        assert_eq!(net.total, 4);

        // `vram = "0"` means "the whole device", and not "no memory at all".
        assert_eq!(c.default_vram().unwrap(), None);
    }

    /// A pool that gives `count` and `devices` says two different things, and
    /// qex must not choose one of them in silence.
    #[test]
    fn a_pool_with_a_count_and_devices_is_refused() {
        let c: Config =
            toml::from_str("[[pool]]\nname = \"gpu\"\ncount = 2\ndevices = [\"24GB\"]\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("Give one only"), "got: {err}");
    }

    /// The cores and the memory live in `[budget]`. A pool with one of those
    /// names would give a second, silent budget.
    #[test]
    fn a_pool_cannot_take_the_name_of_the_budget() {
        for name in ["cpu", "mem"] {
            let c: Config =
                toml::from_str(&format!("[[pool]]\nname = \"{name}\"\ncount = 2\n")).unwrap();
            assert!(
                c.validate().is_err(),
                "the name `{name}` must not be a pool"
            );
        }
    }

    /// Two entries with one name give two answers for one question.
    #[test]
    fn two_pools_with_one_name_are_refused() {
        let c: Config = toml::from_str(
            "[[pool]]\nname = \"gpu\"\ncount = 2\n\n[[pool]]\nname = \"gpu\"\ncount = 4\n",
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("two entries"), "got: {err}");
    }

    /// A pool with no size declares nothing. qex must say so, and must not
    /// give the pool a size that no person chose.
    #[test]
    fn a_pool_with_no_count_and_no_devices_is_refused() {
        let c: Config = toml::from_str("[[pool]]\nname = \"gpu\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("does not know its size"), "got: {err}");
    }

    #[test]
    fn typos_in_config_keys_are_rejected_not_ignored() {
        // If qex ignores a key with a spelling error, the user believes that a
        // limit is active. The limit is not active.
        let err = toml::from_str::<Config>("[budget]\ncpuu = 4\n").unwrap_err();
        assert!(err.to_string().contains("cpuu"), "got: {err}");
    }

    #[test]
    fn bad_values_are_reported_with_the_offending_section() {
        let c: Config = toml::from_str("[budget]\nmem = \"lots\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[budget] mem"), "got: {err}");
    }

    #[test]
    fn overcommit_below_one_is_rejected() {
        let c: Config = toml::from_str("[enforce]\nmem_overcommit = 0.5\n").unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn env_capture_parses_from_cli_strings() {
        use std::str::FromStr;
        assert_eq!(EnvCapture::from_str("all").unwrap(), EnvCapture::All);
        assert_eq!(
            EnvCapture::from_str("MINIMAL").unwrap(),
            EnvCapture::Minimal
        );
        assert_eq!(EnvCapture::from_str(" none ").unwrap(), EnvCapture::None);
        assert!(EnvCapture::from_str("some").is_err());
    }
}
