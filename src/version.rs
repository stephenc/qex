//! The version that this build reports, and the rule for a development build.
//!
//! # Why the version does not come from Cargo.toml
//!
//! `main` holds `version = "0.0.0-dev"` for ever. The number of a release lives
//! on the tag, and on the one commit that the tag names. `build.rs` calculates
//! the number of THIS build and gives it here; read `build.rs` for the rules
//! and for the form.
//!
//! A build thus reports one of these:
//!
//!     0.7.3                       the number in Cargo.toml: a release
//!     0.0.0-dev+g98513e2          the commit that this build holds
//!     0.0.0-dev+g98513e2.dirty    the same, with changes that are not committed
//!     0.0.0-dev+unknown           a build that could not learn its commit
//!
//! Only the first of those is a release.

/// The version of this build.
pub const VERSION: &str = env!("QEX_BUILD_VERSION");

/// Says whether a version names a development build.
///
/// THIS IS THE ONE PLACE THAT DECIDES IT. `capabilities::check_floor` treats
/// such a coordinator differently, and a second rule in a second file would let
/// the two disagree about one build.
///
/// A development version is `0.0.0-dev`, and then build metadata that a `+`
/// starts. `build.rs` writes that form and no other, so this test names it
/// exactly.
///
/// # Why the test is exact, and not a family of shapes
///
/// The answer decides which of two things happens to a coordinator below the
/// capability floor: a development build gets a WARNING, and everything else gets
/// a REFUSAL. A test that accepts a family of shapes therefore hands the gentle
/// answer to a program that qex knows nothing about — `0.0.0-devil` is below
/// the floor, and it is not a build of qex.
///
/// Nothing forces the test to be loose. `build.rs` writes one form, so the test
/// is that form. A change to the form that `build.rs` writes must change this
/// line as well, and the tests below hold the two together.
///
/// Text that this function cannot read is NOT a development build. A version
/// that qex cannot read comes from a program that qex knows nothing about, and
/// the safe answer for such a program is the strict one.
pub fn is_development(version: &str) -> bool {
    let version = version.trim();
    version == DEVELOPMENT || version.starts_with(concat!("0.0.0-dev", "+"))
}

/// The version that `main` holds, and that a build with no release number
/// reports. `Cargo.toml` holds this same text.
pub const DEVELOPMENT: &str = "0.0.0-dev";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_not_a_development_build() {
        assert!(!is_development("0.7.3"));
        assert!(!is_development("1.0.0"));
        // Build metadata alone does not make a development build. A release
        // takes its number from Cargo.toml, and nothing adds `-dev` to it.
        assert!(!is_development("0.7.3+g98513e2"));
    }

    #[test]
    fn the_forms_that_build_rs_writes_are_development_builds() {
        assert!(is_development("0.0.0-dev"));
        assert!(is_development("0.0.0-dev+g98513e2"));
        assert!(is_development("0.0.0-dev+g98513e2.dirty"));
        // A build that could not learn its commit is still a development build,
        // so qex warns about it and never refuses it.
        assert!(is_development("0.0.0-dev+unknown"));
    }

    /// Text that qex cannot read must never pass as a development build. Such a
    /// coordinator comes from a program that qex knows nothing about.
    #[test]
    fn text_that_is_not_a_version_is_not_a_development_build() {
        assert!(!is_development(""));
        assert!(!is_development("not-a-version"));
        assert!(!is_development("-dev"));
        assert!(!is_development("0.0-dev"), "a version has three numbers");
        assert!(
            !is_development("0.0.0.0-dev"),
            "a version has three numbers"
        );
        assert!(!is_development("0.0.x-dev"));
        assert!(!is_development("0.7.3-rc1"), "a candidate is not a build");
        assert!(!is_development("0.7.3-alpha-5"), "qex writes no alpha");
    }

    /// A word that STARTS with `dev` is not `dev`.
    ///
    /// A version below the capability floor that reads as a development build gets
    /// a warning, and every other version below the floor gets a refusal. A
    /// test that accepts any word beginning with `dev` therefore gives the
    /// gentle answer to a program that qex knows nothing about, and each of the
    /// versions below is such a program.
    #[test]
    fn a_word_that_starts_with_dev_is_not_a_development_build() {
        assert!(!is_development("0.0.0-devil"));
        assert!(!is_development("0.0.0-development"));
        assert!(!is_development("0.0.0-devel"));
        assert!(!is_development("0.0.0-dev.1"), "qex writes no such form");
        assert!(!is_development("0.0.0-dev-1"), "qex writes no such form");
        // A different number with the same word is not the form that qex
        // writes. `build.rs` writes `0.0.0-dev` and nothing else.
        assert!(!is_development("0.1.0-dev"));
        assert!(!is_development("1.0.0-dev"));
    }

    /// The text in `Cargo.toml` and the text in this file must agree.
    ///
    /// `Cargo.toml` holds `0.0.0-dev` on `main`, and `set-version.sh` refuses
    /// to write a release number over anything else. If the two ever differ, a
    /// build of `main` reports a version that this file refuses to recognise,
    /// and every such coordinator is refused instead of warned about.
    #[test]
    fn the_development_version_agrees_with_cargo_toml() {
        assert!(
            is_development(DEVELOPMENT),
            "the constant must name a development build"
        );
        let cargo = include_str!("../Cargo.toml");
        let line = format!("version = \"{DEVELOPMENT}\"");
        assert!(
            cargo.lines().any(|l| l.trim() == line),
            "Cargo.toml must hold `{line}`, and it does not"
        );
    }

    /// This build must report a version that a reader can use.
    #[test]
    fn this_build_names_itself() {
        assert!(!VERSION.is_empty(), "a build must report a version");
    }
}
