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

/// Reads a value that a person can write as a number or as text.
///
/// # The fault that this removes
///
/// `[budget] cpu` takes an integer or a percentage, so the field is text. TOML
/// then refuses `cpu = 2` with `invalid type: integer, expected a string`,
/// while `[defaults] cpu = 1` accepts an integer in the same file — and
/// `qex help config` shows both forms. A user who writes the obvious thing gets
/// an error that names a Rust type and gives no remedy.
///
/// A number and its text are the same value here. This function takes either.
fn text_or_number_opt<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    text_or_number(d).map(Some)
}

fn text_or_number<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct Either;

    impl Visitor<'_> for Either {
        type Value = String;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a number such as 2, or text such as \"75%\" or \"8GB\"")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
            // A whole number that TOML read as a float, such as `2.0`.
            if v.fract() == 0.0 && v.abs() <= i64::MAX as f64 {
                Ok((v as i64).to_string())
            } else {
                Ok(v.to_string())
            }
        }
    }

    d.deserialize_any(Either)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// The number of cores that qex can use.
    ///
    /// Give an integer, or a percentage of the machine.
    #[serde(deserialize_with = "text_or_number")]
    pub cpu: String,
    /// The quantity of memory that qex can use.
    ///
    /// Give a size, or a percentage of the machine.
    #[serde(deserialize_with = "text_or_number")]
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
    #[serde(deserialize_with = "text_or_number")]
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
    #[serde(deserialize_with = "text_or_number")]
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
    #[serde(deserialize_with = "text_or_number")]
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
    #[serde(default, deserialize_with = "text_or_number_opt")]
    pub mem: Option<String>,
    /// The time limit for a job. The default is `0`, which sets no limit.
    #[serde(default, deserialize_with = "text_or_number_opt")]
    pub timeout: Option<String>,
}

/// Controls the command that collects the old records.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GcConfig {
    /// The age of a record that `qex gc` deletes.
    ///
    /// `qex gc` works on every directory, so this value is larger than the one
    /// hour of `qex clean --auto`, which works on one directory tree.
    #[serde(deserialize_with = "text_or_number")]
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
    #[serde(deserialize_with = "text_or_number")]
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
}

/// How much of the message about the config file the reader needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    /// A person at a terminal, whose command stopped. This reader must learn
    /// the cause and the remedy, because nothing else will tell them.
    Full,
    /// A place that keeps the message and shows it again later. The record of a
    /// job is such a place, and `qex status` prints it.
    ///
    /// The long message is advice about an upgrade. In the `error:` field of a
    /// job that ran, it reads as a fault in qex, and it hides the one line that
    /// the reader of that field needs. The short form gives the answer of the
    /// parser and nothing else.
    Short,
}

/// Makes the message for a config file that qex cannot read.
///
/// # Why this is not the message of the parser alone
///
/// `Config` refuses a field that it does not know, and that rule is correct: a
/// value with a spelling fault must not be ignored in silence. But it has a
/// second cause that the parser cannot see. A user who writes an option of a
/// NEWER qex, and then runs an OLDER qex, gets "unknown field" for an option
/// that is not a fault at all.
///
/// The commands that need the config file then stop: `qex submit`, `qex run`,
/// `qex pipeline`, `qex gc`, `qex du` and `qex config show`. `daemon` reads it
/// too, so qex cannot START a coordinator.
///
/// # Why the message names so few commands that continue
///
/// Two questions decide whether a command continues, and the second one is easy
/// to miss. Test BOTH before you change this text.
///
/// 1. Does the command read the config file? `grep -n 'Config::load' src/`. A
///    call with `?` stops. `qex top` (`unwrap_or_default`) and the supervisor
///    of a job (it takes the default values) continue.
/// 2. Does the command need a COORDINATOR? `Client::connect` starts one when
///    none operates, and a coordinator cannot start from a file that qex
///    cannot read. `qex info`, `qex list`, `qex status`, `qex kill`, `qex
///    cancel`, `qex clean` and `qex rerun` therefore continue only WHILE a
///    coordinator operates. With none, each waits 10 seconds and then reports
///    that the coordinator did not start — a message that names no cause.
///
/// This message tells the reader to `kill` the coordinator, so it must never
/// promise a command that the `kill` takes away. Only `qex wait`, `qex top`,
/// `qex logs` and `qex version` continue in every state.
///
/// The order matters, and the message gives it. A new option belongs in the
/// config file only after the COORDINATOR is the new build. The program on the
/// disk is not sufficient: a coordinator operates for hours, it holds the code
/// that started it, and it reads the config file ONCE, when it starts
/// (`daemon::run`, and then `State.cfg` for ever). A new option in the file thus
/// has no effect until a new coordinator reads it, and qex ignores it in
/// silence, which is the fault that this message must stop.
fn config_error(path: &std::path::Path, error: toml::de::Error, detail: Detail) -> anyhow::Error {
    let short = anyhow::anyhow!("parsing config file {}: {error}", path.display());
    if detail == Detail::Short || !error.message().contains("unknown field") {
        return short;
    }

    anyhow::anyhow!(
        "{short}\n\n\
         qex refuses a field that it does not know, because a name with a spelling \
         fault must not be ignored in silence.\n\n\
         This qex is version {}. If that name is an option of a NEWER qex, then the \
         file and the program do not agree. Each command that needs this file stops \
         until they agree, and qex cannot start a coordinator, so a queue whose \
         coordinator retires stays where it is. Your jobs continue, and `qex wait` \
         and `qex top` continue. `qex info`, `qex list` and `qex status` continue \
         while a coordinator operates. Read `qex help config`.\n\n\
         Put a new option in the config file only AFTER the coordinator is the new \
         build. The program on the disk is not sufficient: a coordinator operates for \
         hours, it holds the code that started it, and it reads this file once, when \
         it starts. A new option that you write before that moment has no effect, and \
         qex ignores it in silence.\n\n\
         To correct it now:\n\
         \x20   1. Install the new qex. Do this FIRST: while the old qex is the \
         program on the disk, no coordinator can start from this file.\n\
         \x20   2. Run `qex info` for the version and the pid of the coordinator. A \
         coordinator stops when no job operates; `kill <pid>` changes it at once, and \
         the jobs that operate continue.\n\
         \x20   3. Run `qex info` again. The version must be the new one.\n\n\
         To go back instead, remove that section from {}.",
        // The version of the BUILD, and not the number in Cargo.toml. `main`
        // holds `0.0.0-dev` for ever, so `CARGO_PKG_VERSION` says nothing about
        // which build reads the file. `version::VERSION` names the commit, and
        // this message is about a file and a program that do not agree.
        crate::version::VERSION,
        path.display()
    )
}

impl Config {
    /// Reads `~/.config/qex.toml`.
    ///
    /// If the file does not exist, this function gives the default values.
    ///
    /// A fault gives the long message, for a person at a terminal. Use
    /// `load_for_job_record` where the message goes into a record.
    pub fn load() -> Result<Self> {
        Self::read(Detail::Full)
    }

    /// Reads the config file for the supervisor of a job.
    ///
    /// The supervisor puts the fault in the record of the job, and `qex status`
    /// prints that record. The long message is advice about an upgrade, so in
    /// an `error:` field it reads as a fault in qex. This form gives the answer
    /// of the parser only; `qex config show` gives the long message to the
    /// person who asks for it.
    pub fn load_for_job_record() -> Result<Self> {
        Self::read(Detail::Short)
    }

    fn read(detail: Detail) -> Result<Self> {
        let path = paths::config_file()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| config_error(&path, e, detail)),
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
    /// A number must be accepted where the field takes a number or text.
    ///
    /// `[budget] cpu = 2` gave `invalid type: integer, expected a string`
    /// while `[defaults] cpu = 1` accepted an integer in the same file, and
    /// `qex help config` shows both forms. A user who writes the obvious thing
    /// met an error that named a Rust type and gave no remedy.
    #[test]
    fn a_budget_accepts_a_number_and_text_alike() {
        let c: Config = toml::from_str("[budget]\ncpu = 2\nmem = 2147483648\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.budget.cpu, "2");
        assert_eq!(c.budget_cpu().unwrap(), 2);
        assert_eq!(c.budget_mem().unwrap(), 2 << 30);

        // The text forms still operate.
        let c: Config = toml::from_str("[budget]\ncpu = \"75%\"\nmem = \"8GB\"\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.budget_mem().unwrap(), 8 << 30);

        // And the reserve, which has the same shape.
        let c: Config = toml::from_str("[system]\nreserve_mem = 0\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.reserve_mem().unwrap(), 0);

        // A value that means nothing must still give an error, and that error
        // must name the field.
        let c: Config = toml::from_str("[budget]\ncpu = \"two\"\n").unwrap();
        let e = c.validate().unwrap_err().to_string();
        assert!(e.contains("budget"), "the error must name the field: {e}");
    }

    use super::*;

    /// A field that qex does not know must give the order of the steps.
    ///
    /// The usual cause is not a spelling fault. It is a user who wrote an
    /// option of a NEWER qex and then ran an OLDER one. That state stops the
    /// commands that need the file, and `qex submit` is the first one that the
    /// user meets. The message must therefore say which commands continue, and
    /// `qex info` is the one that gives the pid of the coordinator.
    ///
    /// It must also say that the program on the disk is not sufficient. A
    /// coordinator operates for hours and holds the code that started it, so a
    /// new option reaches a coordinator that cannot read it.
    #[test]
    fn a_field_that_qex_does_not_know_gives_the_order_of_the_steps() {
        let path = std::path::Path::new("/home/me/.config/qex.toml");
        let error = toml::from_str::<Config>("[hooks]\non_stop = [\"true\"]\n").unwrap_err();
        let text = config_error(path, error, Detail::Full).to_string();

        assert!(
            text.contains("unknown field"),
            "the message must keep the answer of the parser: {text}"
        );
        // The way back must be there as well. A user who cannot install a new
        // qex now needs a command that works now.
        assert!(
            text.contains("remove that section"),
            "the message must give the way back: {text}"
        );
        // The message must name what stops and what continues. A user who
        // reads "every command stops" looks for a fault that is not there.
        assert!(
            text.contains("qex info") && text.contains("continue"),
            "the message must say which commands continue: {text}"
        );
        // `qex info` needs a COORDINATOR, and step 2 of this same message tells
        // the reader to kill the coordinator. A promise with no qualifier would
        // therefore send that reader into a 10 second wait and a message that
        // names no cause.
        assert!(
            text.contains("while a coordinator operates"),
            "the promise about `qex info` must carry its qualifier: {text}"
        );
        // Step 1 must be first, and the message must say WHY. A reader who
        // kills the coordinator with the old qex still on the disk cannot start
        // a new one.
        assert!(
            text.contains("Do this FIRST"),
            "the message must say that the install comes before the kill: {text}"
        );
        assert!(
            text.contains("AFTER the coordinator is the new build"),
            "the message must give the order of the steps: {text}"
        );
        assert!(
            text.contains("not sufficient"),
            "the message must say that the program on the disk is not sufficient: {text}"
        );
        assert!(
            text.contains(crate::version::VERSION),
            "the message must name the version that reads the file: {text}"
        );

        // A fault that is NOT an unknown field keeps the short message. A user
        // who wrote incorrect TOML needs no lesson about a coordinator.
        let other = toml::from_str::<Config>("[budget\n").unwrap_err();
        let text = config_error(path, other, Detail::Full).to_string();
        assert!(
            !text.contains("coordinator"),
            "a fault of the form of the file must not give the version lesson: {text}"
        );
    }

    /// The record of a job takes the SHORT message.
    ///
    /// The supervisor puts this text in the `error:` field of the job, and `qex
    /// status` prints it. The long message is advice about an upgrade of the
    /// coordinator. In the record of a job that already ran it reads as a fault
    /// in qex, and it pushes the one line that matters — that no limit operates
    /// — off the screen.
    #[test]
    fn the_record_of_a_job_takes_the_short_message() {
        let path = std::path::Path::new("/home/me/.config/qex.toml");
        let error = toml::from_str::<Config>("[hooks]\non_stop = [\"true\"]\n").unwrap_err();
        let text = config_error(path, error, Detail::Short).to_string();

        assert!(
            text.contains("unknown field"),
            "the short message must still give the answer of the parser: {text}"
        );
        assert!(
            !text.contains("coordinator"),
            "the record of a job must not hold the lesson about the coordinator: {text}"
        );
        assert!(
            text.lines().count() <= 6,
            "the short message must fit in a record: {text}"
        );
    }

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
"#;
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.enforce.mode, EnforceMode::Soft);
        assert_eq!(c.submit.env_capture, EnvCapture::Minimal);
        assert_eq!(c.budget_mem().unwrap(), 20 << 30);
        assert_eq!(c.default_timeout().unwrap(), None);
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
