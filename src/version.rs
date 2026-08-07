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
/// A development version is three numbers, then `-dev`. A release is three
/// numbers and nothing else, so the two can never be confused.
///
/// Text that this function cannot read is NOT a development build. A version
/// that qex cannot read comes from a program that qex knows nothing about, and
/// the safe answer for such a program is the strict one.
pub fn is_development(version: &str) -> bool {
    let Some((numbers, rest)) = version.trim().split_once('-') else {
        return false;
    };

    let mut count = 0;
    for part in numbers.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        count += 1;
    }

    count == 3 && rest.starts_with("dev")
}

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

    /// This build must report a version that a reader can use.
    #[test]
    fn this_build_names_itself() {
        assert!(!VERSION.is_empty(), "a build must report a version");
    }
}
