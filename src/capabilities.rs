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
    "abort",
    "dedupe",
    "dependencies",
    "events",
    "groups",
    "history",
    "learn",
    "locks",
    "max-queue-time",
    "own-job",
    "pause",
    "politeness",
    "pools",
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
    // A lock does NOT need `pools`. It travels in its own field, and a
    // coordinator that has `locks` and not `pools` reads that field and obeys
    // it. A test here would refuse a job that the coordinator can run.
    if !spec.claims.is_empty() {
        out.push("pools");
    }
    if spec.retries > 0 {
        out.push("retries");
    }
    // A coordinator without this capability keeps the field, gives a job id, and
    // lets the job wait for ever. That is exactly the fault that the option
    // removes, so qex must refuse the job.
    if spec.max_queue_time.is_some() {
        out.push("max-queue-time");
    }
    if spec.group.is_some() {
        out.push("groups");
    }
    // A coordinator that does not know `nice` keeps the field, gives back a job
    // id, and runs the job at the usual priority. The user asked for a job that
    // gives way, and the job does not give way, and nothing said so.
    //
    // The support floor is 0.6.0, so such a coordinator passes the floor test.
    // The name is therefore the only thing that separates the two builds.
    if spec.nice.is_some() {
        out.push("politeness");
    }
    // A coordinator that does not know this field ignores it, starts a second
    // copy of the work, and gives a new id. That is exactly the fault that the
    // option removes, and the caller would see no message.
    if spec.dedupe_key.is_some() {
        out.push("dedupe");
    }
    out
}

/// Gives the option that a reader writes for one capability.
///
/// The reader knows the option and not the name of the capability, so every
/// message names the option. One place holds this table, because two places
/// give a reader two names for one thing.
pub fn option_for(capability: &'static str) -> &'static str {
    match capability {
        "locks" => "--lock",
        "max-queue-time" => "--max-queue-time",
        "pools" => "--gpu, --vram and --claim",
        "retries" => "--retries",
        "dependencies" => "--needs and --after",
        "groups" => "qex pipeline",
        "politeness" => "--nice",
        "dedupe" => "--dedupe-key",
        // A capability that no option names keeps its own name. Every
        // capability that a JOB asks for has an option above. A name that
        // reaches this arm gates a COMMAND, and `check_command` names the
        // command itself.
        other => other,
    }
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

    let options: Vec<&str> = missing.iter().map(|m| option_for(m)).collect();

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

/// What one stage of a pipeline asks of the coordinator.
///
/// The name is the name in the file, because that is what the reader edits.
#[derive(Debug)]
pub struct StageNeeds {
    pub stage: String,
    pub needs: Vec<&'static str>,
}

/// What asks for one capability, so that the message names the right place.
///
/// A reader corrects the thing that asked. A stage names a line of the file, the
/// file itself names the shape of a pipeline, and the configuration names a file
/// that the pipeline file never mentions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every pipeline asks for it, whatever the file holds.
    EveryPipeline,
    /// The configuration of qex fills the field, and no stage names it.
    Configuration,
}

/// Tests a WHOLE pipeline file against a coordinator.
///
/// A pipeline submits many jobs, and each stage carries its own options. A test
/// of one stage therefore speaks for that stage alone: the coordinator takes a
/// later stage, ignores the option, and gives an id. The user asked for a stage
/// that gives way, or that holds a lock, and the stage does NEITHER. That
/// silence is the fault that this module exists to remove.
///
/// This function tests every stage, and it names the stage beside the option,
/// because the reader corrects a LINE of a file.
///
/// It gives ONE message for the whole file. A file with the same fault in three
/// stages gives one refusal that names the three, and not three refusals: the
/// reader corrects every line in one pass, and a command that writes three
/// refusals for one file teaches a reader to read none of them.
///
/// A message names ONLY the source that the caller computed. A capability that
/// a default of the configuration fills belongs to that file, and a reader who
/// is sent to a line that does not exist looks for a fault that is not there.
pub fn check_pipeline(
    have: &[String],
    coordinator_version: &str,
    coordinator_pid: i32,
    elsewhere: &[(&'static str, Source)],
    stages: &[StageNeeds],
) -> Result<(), String> {
    let absent = |need: &'static str| !have.iter().any(|h| h == need);

    // The option, and what asks for it. A `Source` says that no line of the
    // file asks for it, so no stage is more responsible than another.
    let mut missing: std::collections::BTreeMap<&'static str, Result<Vec<String>, Source>> =
        Default::default();

    for (need, source) in elsewhere.iter().filter(|(n, _)| absent(n)) {
        missing.entry(need).or_insert(Err(*source));
    }
    for stage in stages {
        for need in stage.needs.iter().copied().filter(|n| absent(n)) {
            match missing.entry(need).or_insert_with(|| Ok(Vec::new())) {
                Ok(names) => names.push(stage.stage.clone()),
                // Every pipeline asks for this one whatever the file holds, so
                // the name of a stage adds nothing.
                Err(Source::EveryPipeline) => {}
                // A stage that names the option is the better answer, because
                // the reader corrects a line of the file.
                slot @ Err(Source::Configuration) => *slot = Ok(vec![stage.stage.clone()]),
            }
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    // One column for the options, so a reader compares the names down the page.
    let width = missing
        .keys()
        .map(|need| option_for(need).len())
        .max()
        .unwrap_or(0);

    let mut lines = String::new();
    for (need, who) in &missing {
        let option = option_for(need);
        let reason = match who {
            Err(Source::EveryPipeline) => String::from("every pipeline asks for it"),
            Err(Source::Configuration) => {
                String::from("a default of the configuration, and no stage of this file")
            }
            Ok(names) if names.len() == 1 => format!("the stage `{}`", names[0]),
            Ok(names) => format!(
                "the stages {}",
                names
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        lines.push_str(&format!("\x20   {option:width$}   {reason}\n"));
    }

    Err(format!(
        "the coordinator (pid {coordinator_pid}) is version {coordinator_version}, and it \
         cannot obey:\n\n\
         {lines}\n\
         qex refuses this pipeline, and it started no job. The coordinator would ignore each \
         option in silence, give you an id for every stage, and run the pipeline without the \
         rules that you asked for.\n\n\
         The coordinator stops when no job operates, and the next command then starts one \
         that can obey. To change it now:\n\
         \x20   kill {coordinator_pid}\n\n\
         The jobs that operate now continue; a new coordinator reads the same records."
    ))
}

/// Tests one COMMAND against a coordinator.
///
/// `check` covers a job that carries an option. This function covers a command
/// that needs a request name, such as `qex events`. The two faults are the same
/// fault: a coordinator that cannot obey must refuse, and it must never leave
/// the user with a command that gives nothing and says nothing.
///
/// `danger` says why the refusal matters, and the caller gives it. It is not a
/// fixed sentence, because one capability can gate two commands whose dangers
/// are opposite: a `qex pause` that an earlier coordinator ignores starts work
/// while the person believes that the machine is quiet, and a `qex resume` that
/// it ignores changes nothing at all. One sentence for both would state a
/// reason that is not true for one of them, and a reader who acts on it acts on
/// a fault that does not exist.
pub fn check_command(
    have: &[String],
    coordinator_version: &str,
    coordinator_pid: i32,
    capability: &str,
    command: &str,
    danger: &str,
) -> Result<(), String> {
    if have.iter().any(|h| h == capability) {
        return Ok(());
    }
    Err(format!(
        "the coordinator (pid {coordinator_pid}) is version {coordinator_version}, and it \
         cannot obey `{command}`.\n\n\
         qex refuses this command. {danger}\n\n\
         The coordinator stops when no job operates, and the next command then starts one \
         that can obey. To change it now:\n\
         \x20   kill {coordinator_pid}\n\n\
         The jobs that operate now continue; a new coordinator reads the same records."
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
            dedupe_key: None,
            dedupe_window: 0,
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

    /// A job with `--nice` must be refused by a coordinator that cannot obey
    /// it.
    ///
    /// The support floor is 0.6.0, so a 0.6.x coordinator passes the floor
    /// test. It keeps the field, gives back a job id, and runs the job at the
    /// usual priority. The user asked for a job that gives way, the job does
    /// not give way, and nothing says so.
    ///
    /// The default from `[politeness] nice` is NOT this case: the supervisor
    /// reads that value, so a job that the user did not give a value stays
    /// without one and works with every coordinator.
    #[test]
    fn nice_is_refused_by_a_coordinator_that_cannot_obey_it() {
        let mut s = spec();
        s.nice = Some(19);

        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "politeness")
            .map(|c| c.to_string())
            .collect();
        let err = check(&old, "0.6.0", 4321, &s).unwrap_err();
        assert!(
            err.contains("--nice"),
            "the message must name the option: {err}"
        );
        assert!(
            err.contains("kill 4321"),
            "the message must give the remedy: {err}"
        );

        // A coordinator of this build obeys it.
        let now: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(check(&now, env!("CARGO_PKG_VERSION"), 1, &s).is_ok());

        // A job with no value of its own needs nothing.
        assert!(required_by(&spec()).is_empty());
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

    /// A dedupe key must be refused by a coordinator that has no dedupe.
    ///
    /// This is the most dangerous option to ignore in silence. The caller asks
    /// qex to start no second copy of the work. An earlier coordinator would
    /// start that second copy, give a new id, and say nothing, so the caller
    /// would get two four-hour runs and no message.
    #[test]
    fn a_dedupe_key_is_refused_by_a_coordinator_that_has_no_dedupe() {
        let mut s = spec();
        s.dedupe_key = Some("build:/x".into());
        assert_eq!(required_by(&s), vec!["dedupe"]);

        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "dedupe")
            .map(|c| c.to_string())
            .collect();
        let err = check(&old, "0.7.1", 4321, &s).unwrap_err();
        assert!(
            err.contains("--dedupe-key"),
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

        let new: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(check(&new, "0.8.0", 4321, &s).is_ok());
    }

    /// A lock must NOT need `pools`.
    ///
    /// `--lock` travels in its own wire field, and a coordinator that has
    /// `locks` reads that field and obeys it. A test for `pools` here would
    /// refuse a job that the coordinator can run correctly.
    #[test]
    fn a_lock_does_not_need_the_pools_capability() {
        let mut s = spec();
        s.locks = vec!["target".into()];
        assert_eq!(required_by(&s), vec!["locks"]);

        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "pools")
            .map(|c| c.to_string())
            .collect();
        assert!(
            check(&old, "0.7.1", 4321, &s).is_ok(),
            "a coordinator with `locks` and no `pools` must accept a lock"
        );
    }

    /// A job with a counted claim must be refused by a coordinator that has no
    /// pools. Such a coordinator would ignore the claim in silence, and the
    /// job would then run beside a job that holds the device.
    #[test]
    fn a_claim_is_refused_by_a_coordinator_that_has_no_pools() {
        let mut s = spec();
        s.claims.insert(
            "gpu".into(),
            crate::spec::PoolClaim {
                count: 1,
                size: None,
            },
        );
        assert_eq!(required_by(&s), vec!["pools"]);

        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "pools")
            .map(|c| c.to_string())
            .collect();
        let err = check(&old, "0.7.1", 4321, &s).unwrap_err();
        assert!(
            err.contains("--gpu"),
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

        let new: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(check(&new, "0.8.0", 4321, &s).is_ok());
    }

    /// A queue limit must be refused by a coordinator that cannot obey it.
    ///
    /// Such a coordinator keeps the field, gives a job id, and lets the job wait
    /// for ever. The user asked for the opposite of that, and a silent job id
    /// looks exactly like a job that obeys the limit.
    #[test]
    fn a_queue_limit_is_refused_by_a_coordinator_that_cannot_obey_it() {
        let mut s = spec();
        s.max_queue_time = Some(1800);

        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "max-queue-time")
            .map(|c| c.to_string())
            .collect();
        let err = check(&old, "0.7.1", 4321, &s).unwrap_err();
        assert!(
            err.contains("--max-queue-time"),
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

        let new: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(check(&new, "0.8.0", 4321, &s).is_ok());
    }

    /// A command that needs a request name must be refused by a coordinator
    /// that does not know that name.
    ///
    /// `qex events` against such a coordinator gave no line and no error. The
    /// reader then cannot tell "no job changed" from "this coordinator has no
    /// stream", and it waits for ever. That is the fault that qex exists to
    /// remove, in qex itself.
    #[test]
    fn a_command_is_refused_by_a_coordinator_that_does_not_know_it() {
        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "events")
            .map(|c| c.to_string())
            .collect();
        let err =
            check_command(&old, "0.7.1", 4321, "events", "qex events", "no reason").unwrap_err();
        assert!(
            err.contains("qex events"),
            "the message must name the command: {err}"
        );
        assert!(
            err.contains("kill 4321"),
            "the message must give the remedy: {err}"
        );

        let new: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(check_command(&new, "0.8.0", 4321, "events", "qex events", "no reason").is_ok());
    }

    /// `qex pause` must be refused by a coordinator that cannot obey it.
    ///
    /// The failure to prevent is the silent one: the person types `qex pause
    /// queue`, gets no clear reason, and believes that the machine is quiet
    /// while the coordinator continues to start jobs.
    #[test]
    fn a_pause_is_refused_by_a_coordinator_that_cannot_pause() {
        let old: Vec<String> = ALL
            .iter()
            .filter(|c| **c != "pause")
            .map(|c| c.to_string())
            .collect();
        let err = check_command(
            &old,
            "0.7.1",
            3507877,
            "pause",
            "qex pause queue",
            "The coordinator would start the jobs of the queue, and you would believe that the \
             machine is quiet.",
        )
        .unwrap_err();
        assert!(
            err.contains("qex pause queue"),
            "the message must name the command: {err}"
        );
        assert!(
            err.contains("kill 3507877"),
            "the message must give the remedy: {err}"
        );

        let new: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        assert!(check_command(&new, "0.8.0", 1, "pause", "qex pause queue", "no reason").is_ok());

        // The danger of a refused `qex resume` is the opposite of the danger of
        // a refused `qex pause`. One fixed sentence would state a reason that
        // is not true for one of the two commands.
        let resume = check_command(
            &old,
            "0.7.1",
            3507877,
            "pause",
            "qex resume queue",
            "That coordinator does not read the pause record, so it already starts the jobs of \
             the queue. This command would change nothing.",
        )
        .unwrap_err();
        assert!(
            !resume.contains("you would believe that the machine is quiet"),
            "the resume message must not give the danger of a pause: {resume}"
        );
    }

    /// This build must say that it can pause. Without the name in this list,
    /// every new CLI refuses every new coordinator.
    #[test]
    fn this_build_says_that_it_can_pause() {
        assert!(ALL.contains(&"pause"));
    }

    /// This build must say that it has the event stream. Without the name in
    /// this list, every new CLI refuses every new coordinator.
    #[test]
    fn this_build_says_that_it_has_the_event_stream() {
        assert!(ALL.contains(&"events"));
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

        let mut s = spec();
        s.max_queue_time = Some(60);
        assert_eq!(required_by(&s), vec!["max-queue-time"]);

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

    /// Gives a coordinator that answers with everything except one name.
    fn without(name: &str) -> Vec<String> {
        ALL.iter()
            .filter(|c| **c != name)
            .map(|c| c.to_string())
            .collect()
    }

    fn stage(name: &str, needs: &[&'static str]) -> StageNeeds {
        StageNeeds {
            stage: name.to_string(),
            needs: needs.to_vec(),
        }
    }

    /// An option on a stage AFTER the first must be refused.
    ///
    /// A test of the first stage alone speaks for that stage. The coordinator
    /// then takes the later stage, ignores the option, and gives an id.
    #[test]
    fn an_option_on_a_later_stage_is_refused() {
        // The first stage asks for nothing, and the second asks for politeness.
        let stages = [stage("build", &[]), stage("test", &["politeness"])];
        let err = check_pipeline(
            &without("politeness"),
            "0.6.0",
            4321,
            &[
                ("dependencies", Source::EveryPipeline),
                ("groups", Source::EveryPipeline),
            ],
            &stages,
        )
        .unwrap_err();

        assert!(
            err.contains("--nice"),
            "the message must name the option: {err}"
        );
        assert!(
            err.contains("`test`"),
            "the message must name the stage that the reader corrects: {err}"
        );
        assert!(
            !err.contains("`build`"),
            "the message must not name a stage that asks for nothing: {err}"
        );
        assert!(
            err.contains("kill 4321"),
            "the message must give the remedy: {err}"
        );
    }

    /// Every capability that a stage can carry is tested.
    ///
    /// A `Stage` holds the locks, the retries, the politeness, the queue limit
    /// and the claims, so five capabilities differ from one stage to the next.
    #[test]
    fn each_capability_of_a_later_stage_is_refused() {
        for (capability, option) in [
            ("locks", "--lock"),
            ("retries", "--retries"),
            ("politeness", "--nice"),
            ("max-queue-time", "--max-queue-time"),
            ("pools", "--gpu"),
        ] {
            let stages = [stage("one", &[]), stage("two", &[capability])];
            let err = check_pipeline(&without(capability), "0.6.0", 1, &[], &stages)
                .expect_err(&format!("{capability} on a later stage must be refused"));
            assert!(
                err.contains(option) && err.contains("`two`"),
                "the message must name {option} and the stage: {err}"
            );
        }
    }

    /// One message names every stage that has the same fault.
    ///
    /// Three refusals for one file teach a reader to read none of them.
    #[test]
    fn one_message_names_every_stage_with_the_same_fault() {
        let stages = [
            stage("build", &["politeness"]),
            stage("test", &["politeness"]),
            stage("ship", &["locks"]),
        ];
        let err = check_pipeline(&without("politeness"), "0.6.0", 1, &[], &stages).unwrap_err();

        assert_eq!(
            err.matches("--nice").count(),
            1,
            "one option gives one line: {err}"
        );
        assert!(
            err.contains("`build`") && err.contains("`test`"),
            "the line must name every stage that asks for it: {err}"
        );
        // The coordinator obeys the locks, so that stage is not named.
        assert!(
            !err.contains("--lock"),
            "a capability that the coordinator has must not appear: {err}"
        );
    }

    /// A capability that EVERY pipeline needs names no stage.
    ///
    /// No stage of the file is more responsible than another for it.
    #[test]
    fn a_capability_of_every_pipeline_names_no_stage() {
        let stages = [stage("only", &[])];
        let err = check_pipeline(
            &without("groups"),
            "0.6.0",
            1,
            &[("groups", Source::EveryPipeline)],
            &stages,
        )
        .unwrap_err();
        assert!(
            err.contains("every pipeline asks for it"),
            "the message must say that the file itself asks for it: {err}"
        );
        assert!(
            !err.contains("`only`"),
            "no stage is responsible for it: {err}"
        );
    }

    /// A pipeline that a coordinator can obey passes.
    #[test]
    fn a_pipeline_that_needs_nothing_passes_every_coordinator() {
        let now: Vec<String> = ALL.iter().map(|c| c.to_string()).collect();
        let stages = [stage("one", &[]), stage("two", &["locks"])];
        assert!(check_pipeline(&now, env!("CARGO_PKG_VERSION"), 1, &[], &stages).is_ok());
    }
}
