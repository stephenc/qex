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
//! The CLI reads the version first, because every version answers `Info`. It
//! then asks the coordinator what it can do.
//!
//! Every version that qex supports answers the `Capabilities` request, so there
//! is one path and no table of earlier versions. `SUPPORT_FLOOR` is what makes
//! that true: a coordinator below it is refused before any question about one
//! job.
//!
//! # The rule for the wire format: IT IS ADDITIVE ONLY
//!
//! The design above rests on one promise, and this is the statement of it:
//!
//!   * A new version can ADD a field to a request or to a response.
//!   * A new version can ADD a request name.
//!   * A new version MUST NOT change the meaning of a field that exists, and it
//!     MUST NOT change the meaning of a request name that exists.
//!   * A change that cannot obey the two rules above takes A NEW NAME, and a
//!     capability gates it.
//!
//! Each side thus ignores what it does not know, and it is correct to do that:
//! a field that a program does not know is a field that did not exist when
//! somebody made that program, so no earlier behaviour depends on it.
//!
//! This promise is the reason that the CLI does not refuse a coordinator that
//! is NEWER than itself. An early CLI sends the fields that it knows, and the
//! new coordinator understands each of them. The opposite direction is the
//! dangerous one, and the capability handshake covers it: a new CLI asks the
//! coordinator what it can do, and it refuses a job that the coordinator cannot
//! obey.
//!
//! # The support floor
//!
//! `SUPPORT_FLOOR` is the first version that qex published. A coordinator below
//! it comes from a build that no release holds, and qex has no promise about
//! it. The CLI refuses such a coordinator, and it gives the way to correct the
//! problem.

use crate::spec::JobSpec;

/// The first version that qex published, and thus the earliest version that
/// qex supports.
///
/// A build below this number never reached a release. Two agents that share a
/// machine can each hold a different build, and a coordinator can operate for
/// hours, so a mixture of versions is normal and qex must say which mixtures it
/// supports. The answer is: this number and above.
pub const SUPPORT_FLOOR: (u32, u32, u32) = (0, 6, 0);

/// Tests the version of the coordinator against the support floor.
///
/// The message gives the remedy, because a user cannot correct a fault that has
/// no instruction: the coordinator stops when no job operates, and the next
/// command starts a new one from the program that the user has now.
pub fn check_floor(coordinator_version: &str, coordinator_pid: i32) -> Result<(), String> {
    if parse(coordinator_version) >= SUPPORT_FLOOR {
        return Ok(());
    }
    let (major, minor, patch) = SUPPORT_FLOOR;
    Err(format!(
        "the coordinator (pid {coordinator_pid}) is version {coordinator_version}, and qex \
         supports {major}.{minor}.{patch} and above.\n\n\
         That coordinator comes from a build that no release holds, so qex gives no promise \
         about it.\n\n\
         The coordinator stops when no job operates, and the next command then starts one from \
         the program that you have now. To change it now:\n\
         \x20   kill {coordinator_pid}\n\n\
         The jobs that operate now continue; a new coordinator reads the same records."
    ))
}

/// Everything that this build can do.
pub const ALL: &[&str] = &[
    "dependencies",
    "groups",
    "history",
    "learn",
    "locks",
    "pause",
    "retries",
];

/// Reads a version such as `0.5.1`.
///
/// A version that this function cannot read gives the lowest value. Such a
/// coordinator is thus below the support floor and the CLI refuses it, which is
/// the safe direction.
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

/// Tests one COMMAND against a coordinator.
///
/// `required_by` and `check` test a job specification. A command is the other
/// half: `qex pause` is a request name, and not a field of a job.
///
/// An earlier coordinator already refuses a request that it cannot read, but it
/// answers "qex could not read this request", which states a condition and
/// gives no remedy. The failure is also dangerous: the person believes that the
/// machine is quiet, and the coordinator continues to start jobs. This function
/// gives the cause and the remedy instead.
///
/// `name` is the capability, and `command` is the words that the user typed.
pub fn require(
    have: &[String],
    coordinator_version: &str,
    coordinator_pid: i32,
    name: &str,
    command: &str,
) -> Result<(), String> {
    if have.iter().any(|h| h == name) {
        return Ok(());
    }
    Err(format!(
        "the coordinator (pid {coordinator_pid}) is version {coordinator_version}, and it \
         cannot obey `{command}`.\n\n\
         qex refuses this command. The coordinator would start the jobs of the queue, and you \
         would believe that the machine is quiet.\n\n\
         The coordinator stops when no job operates, and the next command then starts one that \
         can obey. To change it now:\n\
         \x20   kill {coordinator_pid}\n\n\
         The jobs that operate now continue; a new coordinator reads the same records."
    ))
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

    /// A coordinator below the first published version must be refused.
    ///
    /// Two agents on one machine can hold different builds, and a coordinator
    /// operates for hours. qex must therefore say which mixtures of versions it
    /// supports, and the answer is: the first release and above.
    #[test]
    fn a_coordinator_below_the_floor_is_refused() {
        let err = check_floor("0.5.2", 4321).unwrap_err();
        assert!(
            err.contains("0.6.0"),
            "the message must name the floor: {err}"
        );
        assert!(
            err.contains("kill 4321"),
            "the message must give the remedy: {err}"
        );

        assert!(
            check_floor("0.6.0", 1).is_ok(),
            "the floor itself is supported"
        );
        assert!(check_floor("0.7.3", 1).is_ok());
        assert!(check_floor("1.0.0", 1).is_ok());

        // A version that this code cannot read gives the lowest value, so such
        // a coordinator is refused. That is the safe direction.
        assert!(check_floor("", 1).is_err());
    }

    /// This build must never be below its own floor. A release that qex itself
    /// would refuse cannot exist.
    #[test]
    fn this_build_is_not_below_the_floor() {
        assert!(
            parse(env!("CARGO_PKG_VERSION")) >= SUPPORT_FLOOR,
            "this build is below the support floor"
        );
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

        // A coordinator that answers with everything except the locks.
        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "locks")
            .map(|c| c.to_string())
            .collect();
        let err = check(&old, "0.6.0", 4321, &s).unwrap_err();
        assert!(
            err.contains("--lock"),
            "the message must name the option: {err}"
        );
        assert!(
            err.contains("in silence"),
            "the message must give the danger: {err}"
        );
        assert!(
            err.contains("kill 4321"),
            "the message must give the remedy: {err}"
        );

        // A coordinator that has locks accepts the job.
        let new: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(check(&new, "0.6.0", 4321, &s).is_ok());
    }

    /// `qex pause` must be refused by a coordinator that cannot obey it.
    ///
    /// This is the dangerous direction: an earlier coordinator accepts nothing
    /// and continues to start jobs, and the person believes that the machine is
    /// quiet. The message must therefore give the cause and the remedy.
    #[test]
    fn a_pause_is_refused_by_a_coordinator_that_cannot_pause() {
        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "pause")
            .map(|c| c.to_string())
            .collect();

        let err = require(&old, "0.7.1", 3507877, "pause", "qex pause").unwrap_err();
        assert!(
            err.contains("qex pause"),
            "the message must name the command: {err}"
        );
        assert!(
            err.contains("believe that the machine is quiet"),
            "the message must give the danger: {err}"
        );
        assert!(
            err.contains("kill 3507877"),
            "the message must give the remedy: {err}"
        );

        let new: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(require(&new, "0.8.0", 1, "pause", "qex pause").is_ok());
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
        assert!(
            err.contains("--lock") && err.contains("--retries"),
            "got: {err}"
        );
    }
}
