//! This module tells the CLI what the coordinator can do.
//!
//! # The fault that this module removes
//!
//! A coordinator can operate for hours, and a new build replaces the program.
//! The CLI then holds code that the coordinator does not have. A job
//! specification travels as JSON, and a field that the coordinator does not
//! know is IGNORED, in silence.
//!
//! That silence is dangerous. A user who writes `--lock target` against an
//! earlier coordinator receives an id, and the job starts beside the job that
//! the lock should have excluded. Two builds then destroy each other in one
//! directory, and no message says why. The version warning does not stop this,
//! because a warning is easy to read past and the job appears to work.
//!
//! # How the CLI asks
//!
//! The `Capabilities` request did not exist in the first versions, and an
//! earlier coordinator gives an error for a request that it cannot read. The
//! CLI thus asks only a coordinator that is new enough to answer, and it reads
//! the version first, because every version answers `Info`.
//!
//! For an earlier coordinator, the CLI uses the table below. A version says
//! what that build could do.

use crate::spec::JobSpec;

/// The first version that answers the `Capabilities` request.
///
/// The CLI does not send that request to an earlier coordinator, because an
/// earlier coordinator gives an error for a request that it cannot read.
pub const ASK_FROM_VERSION: (u32, u32, u32) = (0, 5, 1);

/// Everything that this build can do.
pub const ALL: &[&str] = &[
    "dependencies",
    "groups",
    "history",
    "learn",
    "locks",
    "retries",
];

/// Gives what a coordinator of one version could do.
///
/// The CLI uses this table for a coordinator that came before the
/// `Capabilities` request.
pub fn implied_by_version(version: &str) -> Vec<&'static str> {
    let v = parse(version);
    let mut out = Vec::new();

    // 0.2.0 added the job dependencies and the record of each job.
    if v >= (0, 2, 0) {
        out.push("dependencies");
        out.push("history");
    }
    // 0.3.0 added the pipeline file and the group of a pipeline.
    if v >= (0, 3, 0) {
        out.push("groups");
    }
    // 0.4.0 kept the measurement of each job and used it as the claim.
    if v >= (0, 4, 0) {
        out.push("learn");
    }
    // 0.5.0 added the locks and the retries.
    if v >= (0, 5, 0) {
        out.push("locks");
        out.push("retries");
    }
    out
}

/// Reads a version such as `0.5.1`.
///
/// A version that this function cannot read gives the lowest value, so the CLI
/// treats an unknown coordinator as an early one and asks for nothing.
pub fn parse(version: &str) -> (u32, u32, u32) {
    let mut parts = version.trim().split('.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Gives what one job needs from the coordinator.
pub fn required_by(spec: &JobSpec) -> Vec<&'static str> {
    let mut out = Vec::new();
    if !spec.needs.is_empty() || !spec.after.is_empty() {
        out.push("dependencies");
    }
    if !spec.locks.is_empty() {
        out.push("locks");
    }
    if spec.retries > 0 {
        out.push("retries");
    }
    if spec.group.is_some() {
        out.push("groups");
    }
    out
}

/// Tests one job against a coordinator.
///
/// The message names the option that the coordinator cannot obey, and it gives
/// the way to correct the problem. A user must never receive a job id for a job
/// that qex will run without the rule that the user asked for.
pub fn check(
    have: &[String],
    coordinator_version: &str,
    coordinator_pid: i32,
    spec: &JobSpec,
) -> Result<(), String> {
    let missing: Vec<&str> = required_by(spec)
        .into_iter()
        .filter(|need| !have.iter().any(|h| h == need))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let options: Vec<&str> = missing
        .iter()
        .map(|m| match *m {
            "locks" => "--lock",
            "retries" => "--retries",
            "dependencies" => "--needs and --after",
            "groups" => "qex pipeline",
            other => other,
        })
        .collect();

    Err(format!(
        "the coordinator (pid {coordinator_pid}) is version {coordinator_version}, and it \
         cannot obey {}.\n\n\
         qex refuses this job. The coordinator would ignore that option in silence, give \
         you a job id, and run the job without the rule that you asked for.\n\n\
         The coordinator stops when no job operates, and the next command then starts one \
         that can obey. To change it now:\n\
         \x20   kill {coordinator_pid}\n\n\
         The jobs that operate now continue; a new coordinator reads the same records.",
        options.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JobSpec {
        JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "t".into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu: 1,
            mem: 1 << 20,
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
    fn a_version_reads_into_three_numbers() {
        assert_eq!(parse("0.5.1"), (0, 5, 1));
        assert_eq!(parse("1.0.0"), (1, 0, 0));
        assert_eq!(parse("0.5"), (0, 5, 0));
        // A version that this code cannot read gives the lowest value, so the
        // CLI treats that coordinator as an early one.
        assert_eq!(parse("not-a-version"), (0, 0, 0));
        assert_eq!(parse(""), (0, 0, 0));
    }

    #[test]
    fn the_table_grows_with_the_version() {
        assert!(implied_by_version("0.1.0").is_empty());
        assert!(implied_by_version("0.2.0").contains(&"dependencies"));
        assert!(!implied_by_version("0.2.0").contains(&"locks"));
        assert!(implied_by_version("0.3.0").contains(&"groups"));
        assert!(implied_by_version("0.5.0").contains(&"locks"));
        assert!(implied_by_version("0.5.0").contains(&"retries"));
    }

    /// This build must be able to do everything that its own version implies.
    /// Without this test, the table and the code could disagree.
    #[test]
    fn this_build_can_do_what_its_version_implies() {
        let mine = env!("CARGO_PKG_VERSION");
        for name in implied_by_version(mine) {
            assert!(
                ALL.contains(&name),
                "the table says that {mine} can do `{name}`, and this build cannot"
            );
        }
    }

    #[test]
    fn a_job_that_needs_nothing_passes_every_coordinator() {
        assert!(required_by(&spec()).is_empty());
        assert!(check(&[], "0.1.0", 1, &spec()).is_ok());
    }

    /// A job with a lock must be refused by a coordinator that has no locks.
    /// Such a coordinator would ignore the lock in silence.
    #[test]
    fn a_lock_is_refused_by_a_coordinator_that_has_no_locks() {
        let mut s = spec();
        s.locks = vec!["target".into()];

        let old: Vec<String> = implied_by_version("0.3.0")
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = check(&old, "0.3.0", 4321, &s).unwrap_err();
        assert!(err.contains("--lock"), "the message must name the option: {err}");
        assert!(err.contains("in silence"), "the message must give the danger: {err}");
        assert!(err.contains("kill 4321"), "the message must give the remedy: {err}");

        // A coordinator that has locks accepts the job.
        let new: Vec<String> = implied_by_version("0.5.0")
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(check(&new, "0.5.0", 4321, &s).is_ok());
    }

    #[test]
    fn each_option_that_needs_the_coordinator_is_tested() {
        let mut s = spec();
        s.retries = 2;
        assert_eq!(required_by(&s), vec!["retries"]);

        let mut s = spec();
        s.needs = vec![uuid::Uuid::new_v4()];
        assert_eq!(required_by(&s), vec!["dependencies"]);

        let mut s = spec();
        s.group = Some(uuid::Uuid::new_v4());
        assert_eq!(required_by(&s), vec!["groups"]);

        // Two options together give two names in one message.
        let mut s = spec();
        s.locks = vec!["a".into()];
        s.retries = 1;
        let err = check(&[], "0.1.0", 7, &s).unwrap_err();
        assert!(err.contains("--lock") && err.contains("--retries"), "got: {err}");
    }
}
