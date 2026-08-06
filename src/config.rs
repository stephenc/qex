//! This module reads the config file `~/.config/qex.toml`.
//!
//! Each field has a default value. The config file is thus optional. If the
//! file does not exist, qex uses the default values and does not give an error.

use crate::{paths, sys, units};
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

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
/// A field such as `[budget] cpu` takes an integer OR a percentage, so the
/// field is text in this program. TOML then refused `cpu = 2` with
/// ``invalid type: integer `2`, expected a string``, while `[defaults] cpu = 1`
/// accepted an integer in the same file, and `qex help config` shows both
/// forms. A user who wrote the obvious thing received an error that named a
/// type in the program and gave no remedy.
///
/// A number and its text are the same value here, so this function takes
/// either. The units module reads a bare number: a size with no unit is bytes,
/// and a duration with no unit is seconds.
///
/// Use this function on a text field that holds a number, a size, a duration or
/// a percentage. Do not use it on a field that holds a name or a path, because
/// a number there is a fault that the user must see.
fn text_or_number<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct Either;

    impl Visitor<'_> for Either {
        type Value = String;

        // A type that is neither a number nor text stops here, and this text is
        // the remedy that the user reads. Name the two forms, and not the type
        // in the program.
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a number such as 2, or text such as \"75%\" or \"8GB\"")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }
        // An integer ABOVE `i64::MAX`, such as 9223372036854775808. The `toml`
        // crate reads an integer as i64 where it fits, and it goes to u64 for a
        // larger one. Without this method such a value gives `invalid type`.
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }
        // A whole number that TOML read as a float, such as `2.0`, becomes `2`.
        // Rust writes a float with no fraction in that form already.
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
            Ok(v.to_string())
        }
    }

    d.deserialize_any(Either)
}

/// The same as [`text_or_number`], for a field that the user can leave out.
fn text_or_number_opt<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    text_or_number(d).map(Some)
}

/// Reads a whole number that a person can write inside quotation marks.
///
/// # Why the tolerance goes both ways
///
/// [`text_or_number`] lets the user write a number where the field is text.
/// This function is the mirror: it lets the user write text where the field is
/// a number. `[defaults] cpu = "1"` refused the file with
/// `invalid type: string "1", expected u64`, which is the same fault in the
/// other direction, with the same type name and the same absent remedy.
///
/// The two together give one rule that the documentation can state: the
/// quotation marks make no difference. A user does not have to remember which
/// direction each field forgives.
///
/// A percentage stops here. `[budget] cpu` takes a percentage because it names
/// a part of the machine. `[defaults] cpu` names the cores for ONE job, so a
/// percentage there has no meaning that qex can defend, and qex must not choose
/// one in silence.
fn whole_number_opt<'de, D>(d: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct WholeNumber;

    impl Visitor<'_> for WholeNumber {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a whole number such as 1, with or without quotation marks")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            u64::try_from(v).map_err(|_| {
                de::Error::custom(format!(
                    "the number is {v}, and a count cannot be below zero. qex cannot \
                     calculate a size from it, and it stops. Write a whole number of \
                     0 or more."
                ))
            })
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            let t = v.trim();
            if t.ends_with('%') {
                return Err(de::Error::custom(format!(
                    "the value is `{v}`, and this field does not take a percentage. It \
                     gives the cores for ONE job, and a part of the machine names no \
                     number of cores. Write a whole number, such as 1. To give a part of \
                     the machine, use `[budget] cpu`, which controls all the jobs \
                     together."
                )));
            }
            t.parse::<u64>().map_err(|_| {
                de::Error::custom(format!(
                    "the value is `{v}`, and qex cannot read a whole number from it. qex \
                     cannot calculate the size of a job, and it stops. Write a whole \
                     number, such as 1."
                ))
            })
        }
    }

    d.deserialize_any(WholeNumber).map(Some)
}

/// Reads a whole number that can be below zero, with or without quotation
/// marks.
///
/// [`whole_number_opt`] takes a count, so it refuses a number below zero.
/// `[politeness] nice` and `[politeness] oom_score_adj` are the two fields that
/// need a number below zero, so they need this function.
///
/// The rule of the documentation is that the quotation marks make no
/// difference. Without this function `nice = "10"` refused the file with
/// ``invalid type: string "10", expected i32``, which names a type in the
/// program and gives no remedy, while `cpu = "1"` in the same file was
/// accepted.
fn signed_number<'de, D>(d: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    /// The message for a value from which qex can read no whole number.
    fn refuse<E: de::Error>(wrote: &dyn fmt::Display) -> E {
        de::Error::custom(format!(
            "the value is `{wrote}`, and qex cannot read a whole number from it. Write a \
             whole number, such as 10, with or without quotation marks. A number below \
             zero takes a minus sign, such as -5."
        ))
    }

    struct SignedNumber;

    impl Visitor<'_> for SignedNumber {
        type Value = i32;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a whole number such as 10, with or without quotation marks")
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i32, E> {
            i32::try_from(v).map_err(|_| refuse(&v))
        }
        // An integer above `i64::MAX`. See the same method in `text_or_number`.
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i32, E> {
            i32::try_from(v).map_err(|_| refuse(&v))
        }
        // A whole number that TOML read as a float, such as `10.0`. A value
        // with a fraction stops here, because qex cannot obey half a step of
        // priority and it must not choose one of the two neighbours in silence.
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<i32, E> {
            if v.fract() == 0.0 && v >= i32::MIN as f64 && v <= i32::MAX as f64 {
                Ok(v as i32)
            } else {
                Err(refuse(&v))
            }
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<i32, E> {
            v.trim().parse::<i32>().map_err(|_| refuse(&v))
        }
    }

    d.deserialize_any(SignedNumber)
}

/// Reads a decimal number that a person can write inside quotation marks.
///
/// The mirror of [`text_or_number`] for `[system] max_pressure`,
/// `[enforce] mem_overcommit` and `[learn] margin`. `margin = "1.5"` refused the
/// file with `invalid type: string "1.5", expected f64`. See
/// [`whole_number_opt`] for why the tolerance goes both ways.
///
/// A whole number is a decimal number too, so `max_pressure = 20` and
/// `max_pressure = "20"` and `max_pressure = 20.0` are one value.
///
/// # Why `nan` and `inf` stop here
///
/// TOML has the values `nan` and `inf`, and each field here is a limit that qex
/// compares against a measurement. A test against `nan` is false for EVERY
/// measurement, so the limit never operates: `max_pressure = nan` accepted the
/// file, passed `validate` (because `nan < 1.0` is false), and then held no job
/// back. `qex config show --json` wrote it as `null`, because JSON has no such
/// value. A limit that never operates and does not show itself is worse than a
/// file that qex refuses.
///
/// The text form already stopped, because `"nan"` reaches `visit_str`. The
/// number form did not, so the two forms disagreed. This function is part of a
/// change whose rule is that the quotation marks make no difference, so both
/// forms now stop.
fn decimal_number<'de, D>(d: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    /// The message for a value that is not a number that qex can compare.
    fn refuse<E: de::Error>(wrote: &dyn fmt::Display) -> E {
        de::Error::custom(format!(
            "the value is `{wrote}`, and qex cannot read a number from it. A limit that \
             is not a number is false against every measurement, so it never operates \
             and qex holds no job back. Write a number, such as 1.5."
        ))
    }

    struct DecimalNumber;

    impl Visitor<'_> for DecimalNumber {
        type Value = f64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a number such as 1.5, with or without quotation marks")
        }

        // `nan`, `inf` and `-inf` are TOML values, and they arrive here.
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<f64, E> {
            if v.is_finite() {
                Ok(v)
            } else {
                Err(refuse(&v))
            }
        }
        // Every i64 and every u64 becomes a finite f64, so these two methods
        // need no such test. `u64::MAX` becomes 1.8446744073709552e19.
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }
        // An integer above `i64::MAX`. See the same method in `text_or_number`.
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<f64, E> {
            match v.trim().parse::<f64>() {
                Ok(n) if n.is_finite() => Ok(n),
                _ => Err(refuse(&v)),
            }
        }
    }

    d.deserialize_any(DecimalNumber)
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
    #[serde(deserialize_with = "decimal_number")]
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
    #[serde(deserialize_with = "decimal_number")]
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

/// The names that `[politeness] io` accepts.
///
/// `none` leaves the class of the job as it is.
///
/// The supervisor names the same three classes again, because it needs the
/// NUMBER of each class for `ioprio_set` and this list holds the names only.
/// Keep the two together: a name here that the supervisor does not know would
/// leave every job in the usual class in silence.
pub const IO_CLASSES: [&str; 3] = ["none", "best-effort", "idle"];

/// How politely a job uses the machine.
///
/// # Why this exists
///
/// The queue controls HOW MANY cores a job uses. It does not control HOW RUDELY
/// the job uses them. A build inside its budget still makes an editor stutter
/// and a video call break up, because the job and the person ask the scheduler
/// for the same cores and the scheduler treats them alike.
///
/// qex knows something that the scheduler does not: this work is in a queue, so
/// nobody is waiting for the next second of it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolitenessConfig {
    /// The `nice` value of a job, from -20 to 19.
    ///
    /// A larger number gives the job less of the processor when something else
    /// wants it. 0 is the value of a command that you type.
    ///
    /// A user cannot LOWER this number without privilege, and the usual
    /// `RLIMIT_NICE` of 0 makes every number below zero such a number. qex
    /// still makes the call; the system refuses it; and the job then runs at
    /// the priority that it already had. Measured on Linux:
    /// `setpriority(PRIO_PROCESS, 0, -5)` from nice 0 gives EACCES, and a job
    /// submitted with `--nice -5` ran at nice 0 and completed.
    #[serde(deserialize_with = "signed_number")]
    pub nice: i32,
    /// The class of the job for the disk, on Linux.
    ///
    /// `idle` gives the disk to everything else first. `best-effort` is the
    /// usual class, and `none` leaves the class as it is.
    ///
    /// macOS has no equivalent, and qex ignores this value there.
    pub io: String,
    /// The value that qex adds to the out-of-memory score of a job, on Linux.
    ///
    /// The kernel chooses a victim when the machine has no memory left. A
    /// larger number makes the job a more likely victim, and 0 leaves the
    /// choice as it is.
    ///
    /// A background build should lose that competition before an editor with an
    /// hour of unsaved work. A user cannot LOWER this value without privilege.
    #[serde(deserialize_with = "signed_number")]
    pub oom_score_adj: i32,
}

impl Default for PolitenessConfig {
    fn default() -> Self {
        Self {
            // A background job gives way, and it still gets the whole machine
            // when nothing else wants it. This is the value that `nice` itself
            // gives with no argument.
            nice: 10,
            io: "none".into(),
            oom_score_adj: 0,
        }
    }
}

/// Tells the job how large its claim is.
///
/// A claim controls the QUEUE. It does not control the job, and a job that
/// asks the machine how large it is gets the size of the MACHINE. A build with
/// a claim of 2 cores on a machine of 16 then starts 16 threads, and the claim
/// that qex made becomes a promise that the job breaks.
///
/// Most runtimes read a variable in place of the machine, so qex writes those
/// variables from the claim. This is the nearest thing to a limit that operates
/// on macOS as well as on Linux, and it needs no cgroup and no privilege.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClaimsConfig {
    /// Write the size of the claim into the environment of the job.
    pub export_env: bool,
    /// The variables that qex does not write without a request.
    ///
    /// `java` writes `JAVA_TOOL_OPTIONS`, and each JVM then writes a line to
    /// ITS STANDARD ERROR: `Picked up JAVA_TOOL_OPTIONS: ...`. That line goes
    /// into the log of the job, and a test that compares the error output
    /// fails because of it.
    ///
    /// `make` writes `MAKEFLAGS=-jN`. This changes only a Makefile that gives
    /// no `-j` of its own, because the Makefile wins. It thus makes a Makefile
    /// parallel that its author never ran in parallel, and a Makefile with an
    /// incomplete dependency graph then fails.
    pub also: Vec<ClaimHint>,
}

/// A variable of `[claims] also`.
///
/// This is an enumeration and not text, so a name with a spelling fault gives
/// an error from the config file. A silent no-op is the wrong answer for a
/// feature whose purpose is to stop a job from taking more than it claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimHint {
    /// `JAVA_TOOL_OPTIONS`, for the JVM.
    Java,
    /// `MAKEFLAGS`, for GNU Make.
    Make,
}

impl ClaimHint {
    /// The name that the config file uses. `qex config show` prints it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Make => "make",
        }
    }
}

impl<'de> Deserialize<'de> for ClaimHint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "java" => Ok(Self::Java),
            "make" => Ok(Self::Make),
            // The three parts: what happened, why it matters, what to do.
            _ => Err(serde::de::Error::custom(format!(
                "unknown name `{s}` in `[claims] also`. qex writes no variable \
                 for that name, so each job of a build receives the size of the \
                 machine and not the size of its claim. Give `java`, or `make`, \
                 or both."
            ))),
        }
    }
}

impl Default for ClaimsConfig {
    fn default() -> Self {
        Self {
            export_env: true,
            also: Vec::new(),
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
    #[serde(default, deserialize_with = "whole_number_opt")]
    pub cpu: Option<u64>,
    /// The quantity of memory for a job.
    ///
    /// The default is the machine memory divided by the number of cores.
    #[serde(default, deserialize_with = "text_or_number_opt")]
    pub mem: Option<String>,
    /// The time limit for a job. The default is `0`, which sets no limit.
    #[serde(default, deserialize_with = "text_or_number_opt")]
    pub timeout: Option<String>,
    /// The time that a job may wait in the queue. The default is no limit.
    ///
    /// qex has NO built-in value here, and that is deliberate. A job that
    /// reaches this limit never runs. A built-in value would thus discard the
    /// work of a user who did not ask for the rule, on the day that the machine
    /// is busy. A user who wants the rule for every job writes it here one time.
    #[serde(default, deserialize_with = "text_or_number_opt")]
    pub max_queue_time: Option<String>,
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

/// Controls the limit on the output that qex keeps for each job.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogsConfig {
    /// The bytes that qex keeps for each stream of each job.
    ///
    /// qex keeps the first part and the last part of the output, and it writes
    /// a line between them that says how much went. Use `"0"` for no limit.
    ///
    /// A size with no unit is a count of bytes, so `max_bytes = 65536` and
    /// `max_bytes = "64KB"` are one value. See [`text_or_number`] for why the
    /// field accepts a number and text.
    #[serde(deserialize_with = "text_or_number")]
    pub max_bytes: String,
}

impl Default for LogsConfig {
    fn default() -> Self {
        // A job wrote 386MB of standard output in a review, and nothing stopped
        // it. qex is made to be started and left, so nobody sees a disk fill,
        // and the same disk holds the record of each job. No limit is thus not
        // an acceptable default.
        //
        // 32MB for each stream is far above the output of a real build or a
        // real test run, which is a few megabytes. It is also small enough that
        // a day of runaway jobs costs some gigabytes and not some hundreds of
        // gigabytes. A user who needs more writes one line in the config file.
        Self {
            max_bytes: "32MB".into(),
        }
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
    #[serde(deserialize_with = "decimal_number")]
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
    pub politeness: PolitenessConfig,
    pub claims: ClaimsConfig,
    pub defaults: DefaultsConfig,
    pub learn: LearnConfig,
    pub history: HistoryConfig,
    pub gc: GcConfig,
    pub logs: LogsConfig,
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
/// disk is not sufficient: a coordinator operates for hours, and it holds the
/// code that started it. `daemon::reload_config` reads the file again when the
/// content changes, but it reads that file with the code that the coordinator
/// holds, and that code does not know the new option. The read gives this same
/// error, the coordinator keeps `State.cfg`, and `qex info` reports the fault.
/// The new option thus has no effect until a NEW coordinator reads it, which is
/// the fault that this message must stop.
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
         hours, and it holds the code that started it. A coordinator reads this file \
         again when it changes, but it reads the file with the code that it holds. It \
         gives this same error, it keeps the values that it had, and `qex info` reports \
         the fault. A new option that you write before that moment thus has no \
         effect.\n\n\
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

/// What one look at the configuration file gave.
pub enum ConfigFile {
    /// The bytes of the file. An empty file gives an empty value here.
    Text(Vec<u8>),
    /// The file does not exist. That is not a fault: every field is optional.
    Missing,
    /// The path is there, and it is not a regular file.
    NotRegular,
    /// The path is there, and the read of it failed.
    Unreadable(std::io::Error),
}

/// Looks at the configuration file, and reads it when it is a regular file.
///
/// # Call this WITHOUT the state mutex
///
/// The scheduler runs this on every turn. A read of a file can take any length
/// of time — a network file system that stops answering holds it for minutes —
/// and the mutex of the state must stay free, or `qex info`, `qex list` and
/// `qex submit` all wait with it.
///
/// # Why the type of the file matters
///
/// A FIFO at this path stops the OPEN until somebody writes to the FIFO. A
/// review measured that: `qex info` gave no answer in 15 seconds, three threads
/// waited for the mutex that the fourth held, and only `kill -9` ended it. A
/// device node can do the same.
///
/// The test is `stat` and then `open`, and not `open` with `O_NOFOLLOW`. A
/// SYMLINK to a regular file must continue to work, because a user who keeps
/// the file in a repository puts a link at this path. `stat` follows the link
/// and gives the type of the target, and it gives that answer at once even for
/// a FIFO.
///
/// # Why the file that the OPEN gave is tested as well
///
/// The second test is not for a FIFO: the open of a FIFO waits, so this code
/// never reaches the test. It is for a path that becomes a device node or a
/// directory between the `stat` and the `open`. The open of `/dev/zero`
/// succeeds at once, and `read_to_end` on it NEVER ENDS. One `fstat` bounds an
/// unbounded read, which is the right trade in a change whose whole subject is
/// unbounded reads.
pub fn read_config_file() -> ConfigFile {
    let Ok(path) = paths::config_file() else {
        return ConfigFile::Missing;
    };
    match std::fs::metadata(&path) {
        Ok(m) if !m.is_file() => return ConfigFile::NotRegular,
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConfigFile::Missing,
        Err(e) => return ConfigFile::Unreadable(e),
    }
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConfigFile::Missing,
        Err(e) => return ConfigFile::Unreadable(e),
    };
    match file.metadata() {
        Ok(m) if !m.is_file() => return ConfigFile::NotRegular,
        Ok(_) => {}
        Err(e) => return ConfigFile::Unreadable(e),
    }
    let mut bytes = Vec::new();
    match std::io::Read::read_to_end(&mut { file }, &mut bytes) {
        Ok(_) => ConfigFile::Text(bytes),
        Err(e) => ConfigFile::Unreadable(e),
    }
}

impl Config {
    /// Reads `~/.config/qex.toml`.
    ///
    /// If the file does not exist, this function gives the default values.
    ///
    /// A fault gives the long message, for a person at a terminal. Use
    /// `load_short` where the message goes into a record or over the wire.
    pub fn load() -> Result<Self> {
        Self::read(Detail::Full)
    }

    /// Reads the config file where the message becomes DATA.
    ///
    /// Two callers need this form. The supervisor of a job puts the fault in
    /// the record of the job, and `qex status` prints that record. The
    /// coordinator puts the fault of a reload in `config_error`, and every
    /// `qex info` then carries it, in the text form and in the JSON form.
    ///
    /// The long message is advice about an upgrade. In an `error:` field it
    /// reads as a fault in qex, and in a warning of 20 lines it hides the one
    /// line that matters. This form gives the answer of the parser only, and
    /// `qex config show` gives the long message to the person who asks for it.
    pub fn load_short() -> Result<Self> {
        Self::read(Detail::Short)
    }

    /// Takes the values from text that the CALLER read.
    ///
    /// The coordinator reads the file itself, for two reasons: it must test
    /// the type of the file before it opens it, and it must not hold the state
    /// mutex while it reads. It must then take the values from the bytes that
    /// IT read. A second read of the same path can give different bytes, and
    /// the coordinator would record the name of bytes that it never installed.
    ///
    /// The message is the short form, because it becomes the value of
    /// `config_error` and travels to every client.
    pub fn parse_short(path: &std::path::Path, text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| config_error(path, e, Detail::Short))
    }

    fn read(detail: Detail) -> Result<Self> {
        let path = paths::config_file()?;
        match read_config_file() {
            ConfigFile::Text(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| {
                    anyhow::anyhow!(
                        "the configuration file {} is not text. qex cannot take any value \
                         from it. Write the file again as UTF-8 text.",
                        path.display()
                    )
                })?;
                toml::from_str(&text).map_err(|e| config_error(&path, e, detail))
            }
            ConfigFile::Missing => Ok(Self::default()),
            // EVERY CALLER GETS THIS GUARD, and not the coordinator only.
            //
            // `qex config show` and `qex submit` read this file for
            // themselves. A review put a FIFO at this path and measured both
            // of them with no answer at all, because the OPEN of a FIFO waits
            // for a writer. The warning of `qex info` names `qex config show`
            // as the way to see the whole message, so without this guard that
            // advice walks the reader into the same wait.
            ConfigFile::NotRegular => Err(anyhow::anyhow!(
                "the configuration file {} is not a regular file. qex takes no value \
                 from a path of another kind, because such a path can stop qex for \
                 ever: the open of a FIFO waits for a writer, and a read of a device \
                 gives bytes without end. Put a regular file at that path, or a link \
                 to one.",
                path.display()
            )),
            ConfigFile::Unreadable(e) => {
                Err(e).with_context(|| format!("reading config file {}", path.display()))
            }
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

    /// Gives the bytes that qex keeps for each stream of each job.
    ///
    /// `None` means that no limit operates. The values `0`, `none`, `never` and
    /// `unlimited` give that result. These are the four words that
    /// [`units::parse_duration`] takes for a time with no limit, so one
    /// vocabulary covers both.
    pub fn log_max_bytes(&self) -> Result<Option<u64>> {
        let text = self.logs.max_bytes.trim().to_ascii_lowercase();
        if matches!(text.as_str(), "0" | "none" | "never" | "unlimited") {
            return Ok(None);
        }
        let bytes = units::parse_size(&text)
            .map_err(|e| anyhow::anyhow!("config [logs] max_bytes: {e}"))?;
        if bytes == 0 {
            return Ok(None);
        }
        // A limit must hold a head, a tail and the line that says what went. A
        // smaller value gives a file that answers no question, and a user who
        // writes one has an intention that the file cannot satisfy.
        //
        // The message gives the RAW BYTE COUNT of both numbers. `format_size`
        // rounds, so `max_bytes = "16383"` gave "max_bytes is 16KB. Use 16KB or
        // more", which asks the user for the value that the user wrote.
        if bytes < crate::logcap::MIN_LIMIT {
            anyhow::bail!(
                "config [logs] max_bytes is {bytes} bytes. Use {} bytes ({}) or more, or use \
                 \"0\" for no limit. A smaller limit does not hold the start of the output, \
                 the end of the output and the line that says how much went.",
                crate::logcap::MIN_LIMIT,
                units::format_size(crate::logcap::MIN_LIMIT)
            );
        }
        Ok(Some(bytes))
    }

    /// Reads the `[politeness]` values, and refuses one that the system cannot
    /// obey.
    ///
    /// Each of these values goes to the system between the fork and the exec of
    /// a job. That code cannot report a fault: it has no lock and no allocation
    /// available, so it gives up in silence. A value with a fault would then
    /// give every job something that nobody asked for, and say nothing.
    /// Measured on Linux, from nice 0 and with no privilege: `nice = 100` gives
    /// a job at nice 19, because `setpriority` takes 19 for any number above
    /// the range and reports success; `nice = -21` gives EACCES and the job
    /// keeps the priority that it had, because the number is below the range
    /// AND below the number that the process has; `io = "iddle"` reads as
    /// `io = "none"`; and the kernel refuses a write of
    /// `oom_score_adj = 90000`, so the job keeps the score that it had.
    ///
    /// The test therefore belongs here, where qex can still name the file, the
    /// value and the remedy.
    pub fn politeness_values(&self) -> Result<()> {
        let p = &self.politeness;
        if !(-20..=19).contains(&p.nice) {
            anyhow::bail!(
                "config [politeness] nice is {}. Use a number from -20 to 19. The system \
                 takes no other number, and it does not tell qex: above the range it uses \
                 19 and reports success, and below the range it refuses the change on a \
                 machine with no privilege. Each job then gets a priority that you did not \
                 ask for, and nothing says so.",
                p.nice
            );
        }
        if !IO_CLASSES.contains(&p.io.as_str()) {
            anyhow::bail!(
                "config [politeness] io is `{}`. Use one of: {}. qex sets the class of a job \
                 between the fork and the exec, where it cannot report a fault, so a name \
                 with a fault would leave every job in the usual class in silence.",
                p.io,
                IO_CLASSES.join(", ")
            );
        }
        if !(-1000..=1000).contains(&p.oom_score_adj) {
            anyhow::bail!(
                "config [politeness] oom_score_adj is {}. Use a number from -1000 to 1000. \
                 The kernel refuses every other number, so this value changes nothing, and \
                 the job stays as likely a victim as an editor of the user.",
                p.oom_score_adj
            );
        }
        Ok(())
    }

    /// Gives the default queue limit for a job.
    ///
    /// If the config file gives no value, the result is `None`. A job then waits
    /// for capacity with no end, which is the behaviour of every earlier qex.
    pub fn default_max_queue_time(&self) -> Result<Option<std::time::Duration>> {
        match &self.defaults.max_queue_time {
            Some(s) => units::parse_duration(s)
                .map_err(|e| anyhow::anyhow!("config [defaults] max_queue_time: {e}")),
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
        self.default_max_queue_time()?;
        self.log_max_bytes()?;
        if self.learn.margin < 1.0 {
            anyhow::bail!(
                "config [learn] margin is {}. Use a value of 1.0 or more. A smaller value \
                 gives a claim below the measurement, and the job would then stop.",
                self.learn.margin
            );
        }
        self.politeness_values()?;
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

    /// A number must be accepted where the field takes a number or text.
    ///
    /// `[budget] cpu = 2` gave ``invalid type: integer `2`, expected a string``
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
    }

    /// Every other field of the same shape must take a number too.
    ///
    /// A user who learns that `[budget] cpu = 2` operates writes `settle = 5`
    /// next. One field that refuses a number keeps the fault, so this test
    /// reads each of them. A duration with no unit is seconds, which
    /// `units::parse_duration` gives. `[gc] keep` is a float, because TOML
    /// reads `3600.0` as a float and the value must still be one hour.
    #[test]
    fn each_duration_and_size_field_accepts_a_number() {
        let c: Config = toml::from_str(
            "[queue]\nsettle = 5\n\
             [peers]\nstale_after = 45\n\
             [gc]\nkeep = 3600.0\n\
             [history]\nkeep = 7200\n\
             [defaults]\nmem = 536870912\ntimeout = 90\nmax_queue_time = 120\n",
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.settle().unwrap(), std::time::Duration::from_secs(5));
        assert_eq!(
            c.peer_stale_after().unwrap(),
            std::time::Duration::from_secs(45)
        );
        assert_eq!(c.gc_keep().unwrap(), std::time::Duration::from_secs(3600));
        assert_eq!(
            c.history_keep().unwrap(),
            std::time::Duration::from_secs(7200)
        );
        assert_eq!(c.default_mem().unwrap(), 512 << 20);
        assert_eq!(
            c.default_timeout().unwrap(),
            Some(std::time::Duration::from_secs(90))
        );
        assert_eq!(
            c.default_max_queue_time().unwrap(),
            Some(std::time::Duration::from_secs(120))
        );
    }

    /// A value that means nothing must still give an error that names the
    /// field, and not the type of the value in the program.
    #[test]
    fn a_value_that_means_nothing_gives_an_error_that_names_the_field() {
        let c: Config = toml::from_str("[budget]\ncpu = \"two\"\n").unwrap();
        let e = c.validate().unwrap_err().to_string();
        assert!(
            e.contains("[budget] cpu"),
            "the error must name the field: {e}"
        );
        assert!(
            e.contains("integer or a percentage"),
            "the error must say what to write: {e}"
        );

        // A type that is neither a number nor text must name the two forms
        // that the field takes.
        let e = toml::from_str::<Config>("[budget]\ncpu = true\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("a number such as 2") && e.contains("75%"),
            "the error must say which forms the field takes: {e}"
        );
    }

    /// An integer above `i64::MAX` must reach `visit_u64` and not stop the file.
    ///
    /// The `toml` crate reads an integer as i64 where it fits, and it goes to
    /// u64 for a larger one. An earlier comment in this file said that TOML
    /// never calls `visit_u64`. That statement was incorrect, and this test
    /// holds the measurement that corrects it: with `visit_u64` removed, the
    /// value below gives `invalid type: integer`.
    #[test]
    fn an_integer_above_the_signed_limit_is_read_as_text() {
        let c: Config = toml::from_str("[budget]\ncpu = 9223372036854775808\n").unwrap();
        assert_eq!(c.budget.cpu, "9223372036854775808");
        let c: Config = toml::from_str("[system]\nmax_pressure = 9223372036854775808\n").unwrap();
        assert_eq!(c.system.max_pressure, 9223372036854775808f64);
        // `[defaults] cpu` is a u64 field, so the value stays a number.
        let c: Config = toml::from_str("[defaults]\ncpu = 18446744073709551615\n").unwrap();
        assert_eq!(c.default_cpu(), u64::MAX);
    }

    /// The quotation marks must make no difference in EITHER direction.
    ///
    /// `[defaults] cpu = "1"` gave `invalid type: string "1", expected u64`,
    /// and `[learn] margin = "1.5"` gave the same fault for a float. That is
    /// the fault of this pull request in the mirror direction. The
    /// documentation states one rule, so the code must obey it for every
    /// numeric field.
    #[test]
    fn a_number_inside_quotation_marks_gives_the_same_value() {
        let quoted: Config = toml::from_str(
            "[defaults]\ncpu = \"3\"\n\
             [system]\nmax_pressure = \"30\"\n\
             [learn]\nmargin = \"2.5\"\n\
             [enforce]\nmem_overcommit = \"2.0\"\n",
        )
        .unwrap();
        quoted.validate().unwrap();

        let bare: Config = toml::from_str(
            "[defaults]\ncpu = 3\n\
             [system]\nmax_pressure = 30\n\
             [learn]\nmargin = 2.5\n\
             [enforce]\nmem_overcommit = 2.0\n",
        )
        .unwrap();
        bare.validate().unwrap();

        assert_eq!(quoted.default_cpu(), bare.default_cpu());
        assert_eq!(quoted.default_cpu(), 3);
        assert_eq!(quoted.system.max_pressure, bare.system.max_pressure);
        assert_eq!(quoted.system.max_pressure, 30.0);
        assert_eq!(quoted.learn.margin, bare.learn.margin);
        assert_eq!(quoted.learn.margin, 2.5);
        assert_eq!(quoted.enforce.mem_overcommit, bare.enforce.mem_overcommit);
        assert_eq!(quoted.enforce.mem_overcommit, 2.0);

        // A whole number in a decimal field is the same value again.
        let c: Config = toml::from_str("[learn]\nmargin = 2\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.learn.margin, 2.0);
    }

    /// A percentage in `[defaults] cpu` must give an error, and not a guess.
    ///
    /// `[budget] cpu` takes a percentage because it names a part of the
    /// machine. `[defaults] cpu` names the cores for ONE job. qex has no
    /// defensible meaning for a part of the machine there, so it must not
    /// choose one in silence.
    #[test]
    fn a_percentage_is_refused_where_it_has_no_meaning() {
        let e = toml::from_str::<Config>("[defaults]\ncpu = \"50%\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("does not take a percentage"),
            "the error must say what happened: {e}"
        );
        assert!(
            e.contains("cores for ONE job"),
            "the error must say why it matters: {e}"
        );
        assert!(
            e.contains("Write a whole number") && e.contains("[budget] cpu"),
            "the error must say what to do: {e}"
        );

        // Text that is not a number at all must also give a remedy.
        for (text, want) in [
            ("[defaults]\ncpu = \"many\"\n", "Write a whole number"),
            ("[defaults]\ncpu = -1\n", "cannot be below zero"),
            ("[learn]\nmargin = \"one\"\n", "Write a number"),
        ] {
            let e = toml::from_str::<Config>(text).unwrap_err().to_string();
            assert!(e.contains(want), "{text} gave: {e}");
        }

        // A space around the value is not a fault. A user who writes ` 2` in a
        // quoted field means 2.
        let c: Config = toml::from_str("[defaults]\ncpu = \" 2 \"\n").unwrap();
        assert_eq!(c.default_cpu(), 2);
        let c: Config = toml::from_str("[learn]\nmargin = \" 2.5 \"\n").unwrap();
        assert_eq!(c.learn.margin, 2.5);
    }

    /// A limit that is not a number must stop the file, in EITHER form.
    ///
    /// TOML has `nan` and `inf`. A test against `nan` is false for every
    /// measurement, so the limit never operates: `max_pressure = nan` passed
    /// `validate`, because `nan < 1.0` is false, and then held no job back.
    /// `qex config show --json` wrote it as `null`. The text form `"nan"`
    /// already stopped, so the two forms disagreed, and the rule of this change
    /// is that the quotation marks make no difference.
    #[test]
    fn a_limit_that_is_not_a_number_stops_the_file() {
        for text in [
            "[system]\nmax_pressure = nan\n",
            "[system]\nmax_pressure = \"nan\"\n",
            "[system]\nmax_pressure = inf\n",
            "[system]\nmax_pressure = -inf\n",
            "[learn]\nmargin = nan\n",
            "[learn]\nmargin = inf\n",
            "[learn]\nmargin = \"inf\"\n",
            "[enforce]\nmem_overcommit = nan\n",
            "[enforce]\nmem_overcommit = inf\n",
        ] {
            let e = toml::from_str::<Config>(text)
                .err()
                .unwrap_or_else(|| panic!("{text} was accepted, and it must not be"))
                .to_string();
            assert!(
                e.contains("false against every measurement"),
                "{text} must say why it matters, and it gave: {e}"
            );
            assert!(
                e.contains("Write a number"),
                "{text} must say what to write, and it gave: {e}"
            );
        }

        // A number that a person writes stays acceptable.
        let c: Config = toml::from_str("[system]\nmax_pressure = 20.5\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.system.max_pressure, 20.5);
    }

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

    /// A politeness value must accept quotation marks, like every other number
    /// in this file.
    ///
    /// The rule of the documentation is that the quotation marks make no
    /// difference. `nice = "10"` refused the file with
    /// ``invalid type: string "10", expected i32``, which names a type in the
    /// program and gives no remedy, while `cpu = "1"` in the same file was
    /// accepted.
    #[test]
    fn a_politeness_number_accepts_quotation_marks() {
        let quoted: Config =
            toml::from_str("[politeness]\nnice = \"15\"\noom_score_adj = \"-500\"\n").unwrap();
        let bare: Config =
            toml::from_str("[politeness]\nnice = 15\noom_score_adj = -500\n").unwrap();
        assert_eq!(quoted.politeness.nice, bare.politeness.nice);
        assert_eq!(
            quoted.politeness.oom_score_adj,
            bare.politeness.oom_score_adj
        );
        assert_eq!(quoted.politeness.nice, 15);
        assert_eq!(quoted.politeness.oom_score_adj, -500);

        // TOML reads `10.0` as a float, and a whole number is a whole number in
        // both forms.
        let float: Config = toml::from_str("[politeness]\nnice = 10.0\n").unwrap();
        assert_eq!(float.politeness.nice, 10);

        // A value from which qex can read no whole number must name the remedy
        // and not a type in the program.
        //
        // The last one is ABOVE `i64::MAX`. The `toml` crate reads an integer as
        // i64 where it fits and goes to u64 for a larger one, so that value is
        // the one form that reaches `visit_u64`. A review called that method
        // unreachable from TOML; this line is the file that reaches it.
        for text in [
            "[politeness]\nnice = \"ten\"\n",
            "[politeness]\nnice = 10.5\n",
            "[politeness]\noom_score_adj = 9999999999\n",
            "[politeness]\nnice = 9223372036854775808\n",
        ] {
            let err = toml::from_str::<Config>(text).unwrap_err().to_string();
            assert!(
                err.contains("Write a whole number"),
                "the message for `{text}` must give the remedy: {err}"
            );
        }

        // A type that is neither a number nor text reaches `expecting`, and
        // that text is the whole remedy that the user gets. serde writes
        // "invalid type: boolean `true`, expected <the text of `expecting`>",
        // so an empty or wrong `expecting` leaves the user with the name of a
        // type in the program and no answer.
        for text in [
            "[politeness]\nnice = true\n",
            "[politeness]\noom_score_adj = [1, 2]\n",
        ] {
            let err = toml::from_str::<Config>(text).unwrap_err().to_string();
            assert!(
                err.contains("a whole number such as 10, with or without quotation marks"),
                "the message for `{text}` must name the two forms that qex takes: {err}"
            );
        }
    }

    /// A politeness value that the system cannot obey must be refused here.
    ///
    /// qex applies these values between the fork and the exec of a job, where
    /// it cannot report a fault and gives up in silence. A value with a fault
    /// would thus give every job something that nobody asked for, and say
    /// nothing. Measured on Linux: `nice = 100` gives a job at nice 19, because
    /// `setpriority` moves the number into the range and reports success;
    /// `io = "iddle"` reads as `io = "none"`.
    #[test]
    fn a_politeness_value_that_the_system_refuses_is_refused_at_the_start() {
        for (text, word) in [
            ("[politeness]\nnice = 100\n", "-20 to 19"),
            ("[politeness]\nnice = -21\n", "-20 to 19"),
            ("[politeness]\nio = \"banana\"\n", "best-effort"),
            ("[politeness]\noom_score_adj = 5000\n", "-1000 to 1000"),
            ("[politeness]\noom_score_adj = 100000000\n", "-1000 to 1000"),
        ] {
            let c: Config = toml::from_str(text).unwrap();
            let err = c.validate().unwrap_err().to_string();
            assert!(
                err.contains(word),
                "the message for `{text}` must give the remedy `{word}`, and it said: {err}"
            );
        }

        // Every value that the system accepts must pass.
        for class in IO_CLASSES {
            let c: Config = toml::from_str(&format!(
                "[politeness]\nnice = 19\nio = \"{class}\"\noom_score_adj = 1000\n"
            ))
            .unwrap();
            c.validate().unwrap();
        }
        let c: Config =
            toml::from_str("[politeness]\nnice = -20\noom_score_adj = -1000\n").unwrap();
        c.validate().unwrap();
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

[logs]
max_bytes = "64MB"

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
        assert_eq!(c.log_max_bytes().unwrap(), Some(64 << 20));
    }

    /// The output of a job must have a limit with no configuration file.
    ///
    /// A job wrote 386MB in a review and nothing stopped it. The same disk
    /// holds the record of each job, so no limit is not an acceptable default.
    #[test]
    fn the_output_of_a_job_has_a_limit_with_no_configuration() {
        let c = Config::default();
        let limit = c.log_max_bytes().unwrap().expect("there is no limit");
        assert!(
            (crate::logcap::MIN_LIMIT..=1 << 30).contains(&limit),
            "the default limit {} is not plausible",
            units::format_size(limit)
        );
    }

    /// A user can remove the limit, and a user can give it in the size syntax
    /// of the other fields.
    #[test]
    fn the_limit_on_the_output_reads_sizes_and_the_word_for_no_limit() {
        let c: Config = toml::from_str("[logs]\nmax_bytes = \"64MB\"\n").unwrap();
        assert_eq!(c.log_max_bytes().unwrap(), Some(64 << 20));

        // These are the four words that a time with no limit takes, so the
        // documentation of one field is the documentation of the other.
        for text in ["\"0\"", "\"none\"", "\"never\"", "\"unlimited\""] {
            let c: Config = toml::from_str(&format!("[logs]\nmax_bytes = {text}\n")).unwrap();
            assert_eq!(c.log_max_bytes().unwrap(), None, "for {text}");
            c.validate().unwrap();
            assert_eq!(
                units::parse_duration(text.trim_matches('"')).unwrap(),
                None,
                "the two fields must take the same words for no limit"
            );
        }
    }

    /// The quotation marks make no difference on this field, as on each other
    /// size field.
    ///
    /// The name of the field says `bytes`, so a user writes `max_bytes = 65536`
    /// without the quotation marks. Measured before the fix: the file stopped
    /// with `invalid type: integer 65536, expected a string`, which names a type
    /// in the program and gives no remedy. A size with no unit is a count of
    /// bytes, so the two forms give one value.
    #[test]
    fn the_limit_on_the_output_reads_a_number_as_bytes() {
        let quoted: Config = toml::from_str("[logs]\nmax_bytes = \"65536\"\n").unwrap();
        let bare: Config = toml::from_str("[logs]\nmax_bytes = 65536\n").unwrap();
        assert_eq!(bare.log_max_bytes().unwrap(), Some(65536));
        assert_eq!(
            bare.log_max_bytes().unwrap(),
            quoted.log_max_bytes().unwrap()
        );

        // `0` with no quotation marks removes the limit, as `"0"` does.
        let none: Config = toml::from_str("[logs]\nmax_bytes = 0\n").unwrap();
        assert_eq!(none.log_max_bytes().unwrap(), None);
        none.validate().unwrap();
    }

    /// A limit that is too small to hold a head, a tail and the note must give
    /// an error. In silence, the user believes that a small file is the whole
    /// output.
    #[test]
    fn a_limit_on_the_output_that_is_too_small_is_refused() {
        let c: Config = toml::from_str("[logs]\nmax_bytes = \"100\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[logs] max_bytes"), "got: {err}");
        assert!(err.contains("0"), "the message must give the remedy: {err}");
    }

    /// The refusal must not ask the user for the value that the user wrote.
    ///
    /// One byte below the limit is the message that a user meets after the
    /// first correction. `format_size` rounds, so the message said
    /// "max_bytes is 16KB. Use 16KB or more", which names one value two times
    /// and gives the reader no way forward. The raw byte count separates them.
    #[test]
    fn the_refusal_of_a_small_limit_gives_the_raw_byte_count() {
        let one_below = crate::logcap::MIN_LIMIT - 1;
        let c: Config = toml::from_str(&format!("[logs]\nmax_bytes = {one_below}\n")).unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(
            err.contains(&format!("is {one_below} bytes")),
            "the message must give the value that the user wrote: {err}"
        );
        assert!(
            err.contains(&format!("Use {} bytes", crate::logcap::MIN_LIMIT)),
            "the message must give the value that qex needs: {err}"
        );
        assert!(
            !err.contains("is 16KB. Use 16KB"),
            "the message names one value two times: {err}"
        );

        // The value that the message asks for must be accepted.
        let ok: Config = toml::from_str(&format!(
            "[logs]\nmax_bytes = {}\n",
            crate::logcap::MIN_LIMIT
        ))
        .unwrap();
        ok.validate().unwrap();
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
