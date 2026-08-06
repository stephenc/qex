//! This module reads the size of a job claim.
//!
//! A claim is a number, such as `2` cores or `8GB`. A claim can also be a word:
//!
//! - `half` gives one half of the budget.
//! - `guess` has the same meaning as `half`. Use it when you do not know the
//!   size of the task.
//! - `full` gives the full budget.
//! - `max` has the same meaning as `full`.
//!
//! An agent that starts an unknown task can thus give a safe claim. Two jobs
//! with the claim `half` operate together, and a third job waits. A job with
//! the claim `full` operates alone, and every other job waits for it.
//!
//! qex measures the true use of each job. Read `qex status <id>` after the job,
//! then give an accurate claim the next time.

use crate::config::Config;
use serde::{Deserialize, Deserializer, Serialize};

/// The size of a claim, before qex calculates the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Claim {
    /// An exact value, such as `2` cores or `8GB`.
    Exact(u64),
    /// One half of the budget.
    Half,
    /// The full budget. A job with this claim operates alone.
    Full,
}

impl Claim {
    /// Reads a claim from the text that a user or an agent wrote.
    ///
    /// The `is_size` parameter selects the unit. A memory claim accepts `8GB`.
    /// A core claim accepts an integer only.
    pub fn parse(s: &str, is_size: bool) -> Result<Self, String> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "half" | "guess" | "auto" => Ok(Self::Half),
            "full" | "max" | "all" => Ok(Self::Full),
            _ if is_size => crate::units::parse_size(&t).map(Self::Exact),
            _ => t.parse::<u64>().map(Self::Exact).map_err(|_| {
                format!(
                    "incorrect core count `{s}`. Give an integer, or one of these words: \
                     `half` and `guess` for one half of the budget, `full` and `max` for \
                     the full budget."
                )
            }),
        }
    }

    /// Gives the number of cores for this claim.
    pub fn cores(&self, cfg: &Config) -> u64 {
        match self {
            Self::Exact(n) => (*n).max(1),
            // Two jobs of this size operate together, and a third job waits.
            Self::Half => (cfg.budget_cpu().unwrap_or(2) / 2).max(1),
            // A job of this size operates alone. It fills the budget exactly,
            // so qex starts it by the usual rule and does not force it.
            Self::Full => cfg.budget_cpu().unwrap_or(1).max(1),
        }
    }

    /// Gives the quantity of memory for this claim.
    pub fn bytes(&self, cfg: &Config) -> u64 {
        match self {
            Self::Exact(n) => *n,
            Self::Half => (cfg.budget_mem().unwrap_or(0) / 2).max(1 << 20),
            Self::Full => cfg.budget_mem().unwrap_or(1 << 20).max(1 << 20),
        }
    }
}

/// Reads a claim from a job file.
///
/// The field accepts a number or a word, so both of these forms operate:
///
/// ```toml
/// [resources]
/// cpu = 2
/// mem = "8GB"
/// ```
///
/// ```toml
/// [resources]
/// cpu = "half"
/// mem = "guess"
/// ```
impl<'de> Deserialize<'de> for Claim {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u64),
            Text(String),
        }

        match Raw::deserialize(d)? {
            Raw::Number(n) => Ok(Claim::Exact(n)),
            // A job file gives a memory value as text, so this path accepts a
            // size. A core value of `"2"` also arrives here, and `parse_size`
            // reads a number without a unit as a count of bytes, which is the
            // same number.
            Raw::Text(s) => Claim::parse(&s, true).map_err(D::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        toml::from_str("[budget]\ncpu = \"8\"\nmem = \"16GB\"\n").unwrap()
    }

    #[test]
    fn a_number_gives_an_exact_claim() {
        assert_eq!(Claim::parse("4", false).unwrap(), Claim::Exact(4));
        assert_eq!(Claim::parse("8GB", true).unwrap(), Claim::Exact(8 << 30));
        assert_eq!(Claim::parse("512MB", true).unwrap(), Claim::Exact(512 << 20));
    }

    /// The words `half` and `guess` give one half of the budget. An agent uses
    /// them when it does not know the size of a task.
    #[test]
    fn the_words_give_one_half_of_the_budget() {
        let cfg = cfg();
        for word in ["half", "guess", "auto", "HALF", " guess "] {
            let cpu = Claim::parse(word, false).unwrap();
            let mem = Claim::parse(word, true).unwrap();
            assert_eq!(cpu, Claim::Half, "the word `{word}` must give a half claim");
            assert_eq!(cpu.cores(&cfg), 4, "one half of 8 cores is 4 cores");
            assert_eq!(mem.bytes(&cfg), 8 << 30, "one half of 16GB is 8GB");
        }
    }

    /// Two jobs with the claim `half` must operate together. This property is
    /// the reason for the value: an agent starts two unknown tasks safely.
    #[test]
    fn two_half_claims_fit_the_budget_together() {
        let cfg = cfg();
        let half = Claim::Half;
        assert!(half.cores(&cfg) * 2 <= cfg.budget_cpu().unwrap());
        assert!(half.bytes(&cfg) * 2 <= cfg.budget_mem().unwrap());
    }

    /// The words `full` and `max` give the full budget.
    #[test]
    fn the_words_full_and_max_give_the_full_budget() {
        let cfg = cfg();
        for word in ["full", "max", "all", "MAX"] {
            let cpu = Claim::parse(word, false).unwrap();
            let mem = Claim::parse(word, true).unwrap();
            assert_eq!(cpu, Claim::Full, "the word `{word}` must give a full claim");
            assert_eq!(cpu.cores(&cfg), 8);
            assert_eq!(mem.bytes(&cfg), 16 << 30);
        }
    }

    /// A job with the claim `full` must fit the budget exactly. If it were one
    /// byte larger, qex would treat it as a job that is too large, and it would
    /// force the job and give a warning. A job that asks for the budget is a
    /// normal job, so it must start by the usual rule.
    #[test]
    fn a_full_claim_fits_the_budget_exactly() {
        let cfg = cfg();
        assert_eq!(Claim::Full.cores(&cfg), cfg.budget_cpu().unwrap());
        assert_eq!(Claim::Full.bytes(&cfg), cfg.budget_mem().unwrap());
    }

    /// A job with the claim `full` must stop a second job of any size.
    #[test]
    fn a_full_claim_leaves_no_capacity_for_a_second_job() {
        let cfg = cfg();
        let used_cpu = Claim::Full.cores(&cfg);
        let used_mem = Claim::Full.bytes(&cfg);
        assert!(used_cpu + 1 > cfg.budget_cpu().unwrap());
        assert!(used_mem + 1 > cfg.budget_mem().unwrap());
    }

    /// A claim must never be zero. A zero claim lets qex start an unlimited
    /// number of jobs together, which is the fault that qex prevents.
    #[test]
    fn a_half_claim_is_never_zero() {
        let small: Config = toml::from_str("[budget]\ncpu = \"1\"\nmem = \"1MB\"\n").unwrap();
        assert!(Claim::Half.cores(&small) >= 1);
        assert!(Claim::Half.bytes(&small) > 0);
        assert!(Claim::Exact(0).cores(&small) >= 1);
    }

    #[test]
    fn an_unknown_word_gives_a_message_with_the_permitted_words() {
        let err = Claim::parse("lots", false).unwrap_err();
        assert!(err.contains("half"), "the message must name the words: {err}");
        assert!(err.contains("guess"), "the message must name the words: {err}");
    }

    /// A job file must accept a number and a word in the same field.
    #[test]
    fn a_job_file_accepts_a_number_or_a_word() {
        #[derive(Deserialize)]
        struct Res {
            cpu: Claim,
            mem: Claim,
        }

        let a: Res = toml::from_str("cpu = 2\nmem = \"8GB\"\n").unwrap();
        assert_eq!(a.cpu, Claim::Exact(2));
        assert_eq!(a.mem, Claim::Exact(8 << 30));

        let b: Res = toml::from_str("cpu = \"half\"\nmem = \"guess\"\n").unwrap();
        assert_eq!(b.cpu, Claim::Half);
        assert_eq!(b.mem, Claim::Half);

        // YAML and JSON must give the same result.
        let c: Res = serde_yaml_ng::from_str("cpu: half\nmem: 4GB\n").unwrap();
        assert_eq!(c.cpu, Claim::Half);
        assert_eq!(c.mem, Claim::Exact(4 << 30));

        let d: Res = serde_json::from_str(r#"{"cpu": 3, "mem": "half"}"#).unwrap();
        assert_eq!(d.cpu, Claim::Exact(3));
        assert_eq!(d.mem, Claim::Half);
    }
}
