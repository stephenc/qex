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
//! is one path and no table of earlier versions. `CAPABILITY_FLOOR` is what makes
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
//! # The capability floor
//!
//! `CAPABILITY_FLOOR` is the first version that ANSWERS the question above. A
//! coordinator below it gives no answer, so the CLI cannot learn which options
//! it obeys, and it must not let a user believe a rule holds when it may not.
//! The CLI refuses such a coordinator and gives the way to correct it.
//!
//! The number says nothing about which versions get a correction: qex corrects
//! a fault in the NEXT version and never in an earlier one. Read the constant
//! itself for the whole of the reasoning.
//!
//! # The carve-out for a development build
//!
//! A DEVELOPMENT BUILD GETS A WARNING, AND NOT A REFUSAL.
//!
//! The floor is a backstop for a coordinator so early that it came before the
//! capability handshake. Such a build cannot say what it cannot do, so the only
//! safe answer is to refuse it.
//!
//! A development build is not that. It answers `Capabilities` like every other
//! build, so `check` still refuses an option that the coordinator cannot obey,
//! by name. The floor is the coarse gate and `check` is the exact one, so a
//! warning here takes no protection away.
//!
//! A refusal would make every build that a person makes unusable by its own
//! CLI: `main` holds `0.0.0-dev`, and a build with no git and no tag reports
//! that number. That fault is worse than the fault that the floor guards
//! against.
//!
//! `version::is_development` holds the rule for what a development build is,
//! and it is the only place that holds it.

use crate::spec::JobSpec;
use crate::version::is_development;

/// The first version that ANSWERS THE CAPABILITY HANDSHAKE.
///
/// # This number says nothing about which versions get a correction
///
/// qex corrects a fault in the NEXT version, and never in an earlier one. So
/// "the versions that qex supports" is always "the newest one", and no number
/// could say it.
///
/// This number means one thing: **below it, a coordinator does not report what
/// it can do.** The CLI asks with the `Capabilities` request, and a coordinator
/// from before this version gives no answer — so the CLI cannot learn whether
/// that coordinator obeys `--lock`, or `--nice`, or anything else. It must not
/// let a user believe a rule holds when it may not, so it refuses.
///
/// A coordinator AT or ABOVE this number needs no such refusal, whatever its
/// age. It names what it can do, and `check` then refuses the exact option that
/// it cannot obey, by name.
///
/// A build below this number also never reached a release — no version below
/// 0.6.0 went out anywhere — so the only programs that report such a number are
/// ones that somebody built.
///
/// # TWO DIFFERENT PROGRAMS NOW REPORT A NUMBER BELOW THIS ONE
///
/// They need two different answers, and the constant alone does not show that.
/// Read this before you decide that the carve-out in `check_floor` is a fault:
///
///   * A binary that somebody built from a checkout BEFORE 0.6.0 reports a real
///     number, such as `0.5.2`. It came before the capability handshake and
///     before `build.rs`, so it cannot say what it cannot do. REFUSE IT.
///   * A binary that somebody builds from `main` today reports
///     `0.0.0-dev+g98513e2`, because `main` holds `0.0.0-dev` and `build.rs`
///     adds the commit. It answers every request that this build answers.
///     WARN ABOUT IT.
///
/// `(0, 0, 0) < (0, 6, 0)` is true for the second one as well, so the three
/// numbers cannot tell the two apart. `version::is_development` tells them
/// apart, and `check_floor` reads it.
pub const CAPABILITY_FLOOR: (u32, u32, u32) = (0, 6, 0);

/// The answer of the floor test.
///
/// The three answers are separate values, and not a `Result`, because the
/// middle one is neither. A caller that could only say yes or no would have to
/// choose between refusing a build that a person made and saying nothing at all
/// about it.
pub enum Floor {
    /// The coordinator is at or above the floor. Say nothing.
    Supported,
    /// The coordinator is a development build. Warn the user, and continue.
    Development(String),
    /// The coordinator is below the floor. Refuse it, with this message.
    Below(String),
}

/// Tests the version of the coordinator against the capability floor.
///
/// Each message gives the remedy, because a user cannot correct a fault that
/// has no instruction: the coordinator stops when no job operates, and the next
/// command starts a new one from the program that the user has now.
pub fn check_floor(coordinator_version: &str, coordinator_pid: i32) -> Floor {
    // A development build is not an early build. Read the note at the top of
    // this file for the reason that it gets a warning and not a refusal.
    if is_development(coordinator_version) {
        if parse(coordinator_version) >= CAPABILITY_FLOOR {
            // The number is above the floor already, so the carve-out changes
            // nothing and there is nothing to say.
            return Floor::Supported;
        }
        return Floor::Development(format!(
            "the coordinator (pid {coordinator_pid}) is version {coordinator_version}, which is \
             a development build and not a release.\n\n\
             qex gives no promise about which options such a coordinator obeys. It still says \
             what it can do, and qex still refuses a job with an option that it cannot obey, \
             so this is a warning and not an error.\n\n\
             The coordinator stops when no job operates, and the next command then starts one \
             from the program that you have now. `qex info` gives its pid. To change it now:\n\
             \x20   kill {coordinator_pid}\n\n\
             The jobs that operate now continue; a new coordinator reads the same records."
        ));
    }

    if parse(coordinator_version) >= CAPABILITY_FLOOR {
        return Floor::Supported;
    }

    let (major, minor, patch) = CAPABILITY_FLOOR;
    Floor::Below(format!(
        "the coordinator (pid {coordinator_pid}) is version {coordinator_version}, and a \
         coordinator says what it can do from {major}.{minor}.{patch} and above.\n\n\
         That coordinator does not answer the question, so qex cannot learn which options it \
         obeys, and it must not let you believe a rule holds when it may not.\n\n\
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
    "retries",
];

/// Reads a version such as `0.5.1`.
///
/// A version that this function cannot read gives the lowest value. Such a
/// coordinator is thus below the capability floor and the CLI refuses it, which is
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

        // The forms that `build.rs` writes. The text after `-` and after `+`
        // says which build it is, and it must not change the three numbers.
        assert_eq!(parse("0.0.0-dev"), (0, 0, 0));
        assert_eq!(parse("0.0.0-dev+g98513e2"), (0, 0, 0));
        assert_eq!(parse("0.0.0-dev+g98513e2.dirty"), (0, 0, 0));

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
    fn below(version: &str, pid: i32) -> String {
        match check_floor(version, pid) {
            Floor::Below(message) => message,
            Floor::Development(m) => panic!("`{version}` gave a warning and not a refusal: {m}"),
            Floor::Supported => panic!("`{version}` passed the floor"),
        }
    }

    fn supported(version: &str) -> bool {
        matches!(check_floor(version, 1), Floor::Supported)
    }

    /// A coordinator below the first published version must be refused.
    ///
    /// Two agents on one machine can hold different builds, and a coordinator
    /// operates for hours. qex must therefore say which mixtures of versions it
    /// supports, and the answer is: the first release and above.
    ///
    /// THIS TEST HOLDS THE RULE THAT THE CARVE-OUT MUST NOT SWALLOW. A build
    /// that says `0.5.2` came before the capability handshake, so it cannot say
    /// what it cannot do, and a warning would not be enough.
    #[test]
    fn a_coordinator_below_the_floor_is_refused() {
        let err = below("0.5.2", 4321);
        assert!(
            err.contains("0.6.0"),
            "the message must name the floor: {err}"
        );
        assert!(
            err.contains("kill 4321"),
            "the message must give the remedy: {err}"
        );

        assert!(supported("0.6.0"), "the floor itself is supported");
        assert!(supported("0.7.3"));
        assert!(supported("1.0.0"));

        // A version that this code cannot read gives the lowest value, so such
        // a coordinator is refused. That is the safe direction.
        below("", 1);
        below("not-a-version", 1);
    }

    /// A build that a person made must be usable by its own CLI.
    ///
    /// It gets a WARNING and not a refusal. `check` still refuses each option
    /// that such a coordinator cannot obey, by name, so nothing is lost. See
    /// the note at the top of this file.
    #[test]
    fn a_development_build_is_warned_about_and_not_refused() {
        let warning = match check_floor("0.0.0-dev", 4321) {
            Floor::Development(message) => message,
            Floor::Below(m) => panic!("a development build must not be refused: {m}"),
            Floor::Supported => panic!("a development build below the floor must be named"),
        };
        assert!(
            warning.contains("development build"),
            "the message must say what happened: {warning}"
        );
        assert!(
            warning.contains("no promise"),
            "the message must say why it matters: {warning}"
        );
        assert!(
            warning.contains("kill 4321") && warning.contains("qex info"),
            "the message must give the remedy: {warning}"
        );

        // Every form that `build.rs` writes gets the same answer.
        for form in [
            "0.0.0-dev",
            "0.0.0-dev+g98513e2",
            "0.0.0-dev+g98513e2.dirty",
            "0.0.0-dev+unknown",
        ] {
            assert!(
                matches!(check_floor(form, 1), Floor::Development(_)),
                "`{form}` must give a warning"
            );
        }

        // A release takes its number from Cargo.toml, and it needs no warning
        // at all.
        assert!(supported("0.7.3"));
    }

    /// This build must never be below its own floor without saying so.
    ///
    /// A release that qex itself would refuse cannot exist. `main` holds
    /// `0.0.0-dev`, so a build of `main` with no git IS below the floor, and it
    /// must then be a build that qex names as a development build.
    ///
    /// The test fails for a build that is below the floor and is NOT a
    /// development build, which is exactly the state that must not exist.
    /// `build.rs` holds this number as well, and the two must agree.
    ///
    /// A build script cannot read a constant of the crate that it builds, so
    /// the number is written in both places. `build.rs` refuses a build whose
    /// `Cargo.toml` holds a release number below its own copy — so if the two
    /// went apart, that refusal would use one number while `check_floor` used
    /// another, and a release could be made that qex then refuses.
    ///
    /// `build.rs` writes its copy into the build, so this test reads it.
    #[test]
    fn the_floor_of_the_build_agrees_with_the_floor_of_the_code() {
        let written = env!("QEX_BUILD_FLOOR");
        let (major, minor, patch) = CAPABILITY_FLOOR;
        assert_eq!(
            written,
            format!("{major}.{minor}.{patch}"),
            "build.rs holds {written} and capabilities.rs holds {major}.{minor}.{patch}"
        );
        assert_eq!(parse(written), CAPABILITY_FLOOR);
    }

    #[test]
    fn this_build_is_not_below_the_floor() {
        let mine = crate::version::VERSION;
        assert!(
            parse(mine) >= CAPABILITY_FLOOR || crate::version::is_development(mine),
            "this build reports `{mine}`, which is below the capability floor and is not a \
             development build"
        );

        // The same rule, through the code that a user meets.
        assert!(
            !matches!(check_floor(mine, 1), Floor::Below(_)),
            "this build reports `{mine}`, which its own CLI would refuse"
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
