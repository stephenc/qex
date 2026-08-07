//! Gives the build the version that it reports.
//!
//! # Why this file exists
//!
//! `main` holds `version = "0.0.0-dev"` in Cargo.toml, and no pull request
//! changes it. The number of a release lives on the tag, and on the one commit
//! that the tag names.
//!
//! `CARGO_PKG_VERSION` is therefore `0.0.0-dev` in every build that a person
//! makes, and a version that never changes makes two faults:
//!
//!   * qex compares the version of the CLI against the version of the
//!     coordinator, and it warns when the two differ. That warning exists to
//!     catch a person who builds qex and installs it WHILE THEY WORK. With one
//!     number for every build it can never fire again.
//!   * A fault report from a build that no release holds would name no commit.
//!
//! This file gives each build a name of its own, and it puts the commit in it.
//!
//! # It says which commit it is, and it claims NOTHING else
//!
//!     0.0.0-dev+g98513e2          this commit
//!     0.0.0-dev+g98513e2.dirty    this commit, with changes that are not committed
//!
//! An earlier version of this file wrote `0.7.3-alpha-2`. That number CLAIMS to
//! be a preview of the release 0.7.3, and to make the claim this file held a
//! second copy of the rules for the tags and the bump — a copy that can disagree
//! with the copy that makes the release. `0.0.0-dev+g98513e2` claims only which
//! commit it is. That is the one thing that a development build can say
//! honestly, and it is the thing that a fault report needs.
//!
//! The text after `+` is SemVer build metadata, which a comparison of versions
//! ignores. It never reaches Cargo.toml, so cargo has no opinion about it.
//! `capabilities::parse` reads the whole string as `(0, 0, 0)`, which is below
//! the capability floor, and `version::is_development` names it a development
//! build so that qex warns about it and does not refuse it.
//!
//! # Two rules, and the file says which one it is
//!
//!   1. `CARGO_PKG_VERSION` IS NOT `0.0.0-dev` — return it, and never run git.
//!      The commit that a tag names holds the real number in its Cargo.toml, so
//!      this covers a release build, the crates.io tarball, and any archive of
//!      a tag. Each of them states its own version, and no repository above it
//!      can say otherwise.
//!   2. IT IS `0.0.0-dev` — a development build, and the commit says which one:
//!
//!          0.0.0-dev+g98513e2          the repository whose root is this package
//!          0.0.0-dev+g98513e2.dirty    the same, with changes that nobody committed
//!          0.0.0-dev+unknown           anything else
//!
//! NOTHING COMES IN THROUGH THE ENVIRONMENT. An earlier version of this file
//! read `QEX_VERSION`, which the release workflow set from the tag. That was
//! one more way for a build to report a number that its own files do not hold.
//! The number is a fact about the tree, and rule 1 reads it from the tree.
//!
//! # GIT WALKS UP THE DIRECTORY TREE, SO IT ANSWERS FOR A STRANGER
//!
//! `git rev-parse HEAD` answers from the first repository ABOVE the current
//! directory, and it says nothing about which package that repository holds. A
//! package that is unpacked inside somebody else's repository therefore takes
//! the commit of that repository, and the binary then reports a hash that names
//! a commit in a project that its user never saw. A fault report from it sends
//! a reader to a commit that does not exist.
//!
//! This is not a rare shape. Each of these puts the package inside a repository:
//!
//!   * `cargo install --git https://github.com/stephenc/qex --tag v0.8.0`. THE
//!     CHECKOUT THAT CARGO MAKES IS ITSELF A GIT REPOSITORY, so without a test
//!     every such install reports the hash of cargo's own clone.
//!   * `cargo publish`, which builds the package again in
//!     `target/package/qex-X.Y.Z/`, inside the checkout that it came from.
//!   * `cargo install qex` for a user whose `$HOME` is a repository of dotfiles.
//!     The registry unpacks under `~/.cargo/registry/src/`, so git finds `$HOME`.
//!
//! TWO RULES CLOSE THIS, and each of them alone leaves a hole:
//!
//!   A. Look for git only when `CARGO_PKG_VERSION` is `0.0.0-dev`. Every case
//!      above with a real number in Cargo.toml — the crates.io tarball, any
//!      build of a commit that a tag names — then never runs git at all.
//!   B. When you do look, the repository must be OURS: the root of the
//!      repository must be the root of this package. `cargo install --git`
//!      unpacks a source tree that says `0.0.0-dev`, so rule A does not cover
//!      it and rule B does.
//!
//! Rule A costs a development build nothing, because `main` holds `0.0.0-dev`.
//! Rule B costs it nothing either, because the root of a checkout of qex IS the
//! root of the package. A git WORKTREE also passes: `--show-toplevel` gives the
//! root of the worktree, and our worktrees hold the package at their root.
//!
//! # `0.0.0-dev+unknown`, and why it is not a bare `0.0.0-dev`
//!
//! `+unknown` says what is true: this is a development build, and it could not
//! learn which commit it holds. It is ONE answer for two states — no git at
//! all, and a repository that is not ours — because a reader of a fault report
//! does the same thing for both of them.
//!
//! `capabilities::parse` reads it as `(0, 0, 0)` and `version::is_development`
//! names it a development build, because the rule is three numbers and then
//! `-dev`, and everything after that is build metadata. Such a build is thus
//! warned about, and never refused.
//!
//! # It must never stop a build
//!
//! git can be absent and the directory can hold no repository. Every path below
//! gives an answer, and none of them fails.

use std::path::Path;
use std::process::Command;

/// The number that `main` holds, and that nothing else holds.
const DEVELOPMENT: &str = "0.0.0-dev";

/// The first version that answers the capability handshake.
///
/// A build below this number cannot say what it is unable to obey, so qex can
/// make no promise about it. That is the whole meaning of the number: it is not
/// a statement about which versions somebody wants to support.
///
/// This must agree with `capabilities::CAPABILITY_FLOOR`. A build script cannot
/// read a constant of the crate that it builds, so the number is here as well,
/// and `main` writes it into the build. The test
/// `the_floor_of_the_build_agrees_with_the_floor_of_the_code` then holds the
/// two together, so they cannot go apart in silence.
const CAPABILITY_FLOOR: (u32, u32, u32) = (0, 6, 0);

/// Stops a build that would give a binary that qex itself refuses.
///
/// `capabilities::check_floor` refuses a coordinator below `CAPABILITY_FLOOR`,
/// because a build below it comes from before the capability handshake and
/// cannot say what it is unable to do. A binary that carries such a number is
/// therefore of no use: its own CLI refuses it, and the person who installed it
/// meets that with no cause.
///
/// `0.0.0-dev` is NOT this case. Its numbers are below the floor, and
/// `version::is_development` names it, so qex warns about such a build and runs
/// it.
///
/// The fault that this prevents is a number that somebody writes into
/// `Cargo.toml` by hand, and a release that a fault in the version rules makes
/// too small. It stops at the build, where a person reads the reason, and not
/// at the first command after an install.
fn refuse_below_the_floor(version: &str) {
    let (major, minor, patch) = CAPABILITY_FLOOR;
    let numbers: Vec<u32> = version.split('.').filter_map(|p| p.parse().ok()).collect();

    if numbers.len() != 3 || version.split('.').count() != 3 {
        panic!(
            "the version `{version}` in Cargo.toml is neither three numbers nor \
             `{DEVELOPMENT}`.\n\n\
             qex compares this number against the first version that answers the \
             capability handshake,\
             and it cannot read this one, so the build would give a binary that qex \
             refuses.\n\n\
             Put three numbers in Cargo.toml, such as `{major}.{minor}.{patch}`, or \
             leave `{DEVELOPMENT}` for a build that is not a release."
        );
    }

    if (numbers[0], numbers[1], numbers[2]) < CAPABILITY_FLOOR {
        panic!(
            "the version `{version}` in Cargo.toml is below `{major}.{minor}.{patch}`, \
             which is the first version that answers the capability handshake.\n\n\
             A coordinator below that number does not say what it can do, so qex refuses \
             it. This build would therefore give a binary that its own CLI refuses, and \
             the person who installed it would meet that with no cause.\n\n\
             Put `{major}.{minor}.{patch}` or a later number in Cargo.toml, or leave \
             `{DEVELOPMENT}` for a build that is not a release."
        );
    }
}

fn main() {
    let (version, from_git) = version();

    for path in rerun_paths(from_git) {
        println!("cargo:rerun-if-changed={path}");
    }

    println!("cargo:rustc-env=QEX_BUILD_VERSION={version}");
    let (major, minor, patch) = CAPABILITY_FLOOR;
    println!("cargo:rustc-env=QEX_BUILD_FLOOR={major}.{minor}.{patch}");
}

/// The files that change the answer.
///
/// A build script that names any file at all takes the whole responsibility:
/// cargo then stops looking at the rest of the package. The list must therefore
/// hold the source as well, and not the git files only.
///
/// The answer holds two facts: which commit HEAD names, and whether the tree
/// holds changes that nobody committed.
///
///   * `.git/HEAD` changes when a person moves to another branch or commit.
///   * The refs change when a person makes a commit, because HEAD then still
///     names the same branch and that branch names a new commit. `packed-refs`
///     holds the same refs after `git gc`.
///   * The source changes when a person edits it, which is what makes the tree
///     dirty.
///
/// A CHANGE TO A FILE THAT IS NOT IN THIS LIST CAN LEAVE `.dirty` BEHIND. A
/// change to README.md makes the tree dirty, cargo does not run this script
/// again, and the binary keeps the string that it had. That is the correct
/// answer: the PROGRAM did not change, so the binary that reports the earlier
/// string is the same program that reported it. The string names a build, and
/// two builds of one program are one build.
fn rerun_paths(from_git: bool) -> Vec<String> {
    let mut paths = vec![
        "src".to_string(),
        "tests".to_string(),
        "Cargo.toml".to_string(),
        "build.rs".to_string(),
    ];

    // A version that did not come from git does not change when git changes.
    // Naming the files of a repository that gave nothing would tie this package
    // to a repository that it does not belong to.
    if from_git {
        // A worktree keeps its HEAD beside itself and its refs with the main
        // repository, so ask git for both places instead of taking `.git` for a
        // directory.
        if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
            paths.push(format!("{dir}/HEAD"));
        }
        if let Some(common) = git(&["rev-parse", "--git-common-dir"]) {
            paths.push(format!("{common}/refs"));
            paths.push(format!("{common}/packed-refs"));
        }
    }

    paths.retain(|p| Path::new(p).exists());
    paths
}

/// The version, and whether git gave it.
fn version() -> (String, bool) {
    // RULE 1, and it comes BEFORE GIT. A package with a real number in its
    // Cargo.toml states its own version, and no repository above it can say
    // otherwise.
    let packaged = env!("CARGO_PKG_VERSION");
    if packaged != DEVELOPMENT {
        refuse_below_the_floor(packaged);
        return (packaged.to_string(), false);
    }

    // RULE 2. The repository must hold THIS package.
    let ours = git(&["rev-parse", "--show-toplevel"]).is_some_and(|top| is_this_package(&top));
    if !ours {
        return (format!("{DEVELOPMENT}+unknown"), false);
    }

    let Some(commit) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        // Our repository, and it holds no commit yet. `from_git` stays true, so
        // the answer changes when the first commit arrives.
        return (format!("{DEVELOPMENT}+unknown"), true);
    };

    // A tree with changes that nobody committed is a different program from the
    // commit that it started from, and it must not take that name.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let mark = if dirty { ".dirty" } else { "" };
    (format!("{DEVELOPMENT}+g{commit}{mark}"), true)
}

/// Says whether the repository at `toplevel` is the one that holds this package.
///
/// CANONICALISE BOTH NAMES. A symbolic link anywhere in the path gives two
/// names for one directory, and a comparison of the text would then refuse a
/// repository that is ours. A name that this code cannot resolve gives `false`,
/// which is the safe direction: an unknown answer is better than a hash from a
/// repository that belongs to somebody else.
fn is_this_package(toplevel: &str) -> bool {
    let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") else {
        return false;
    };
    match (
        std::fs::canonicalize(toplevel),
        std::fs::canonicalize(manifest),
    ) {
        (Ok(repository), Ok(package)) => repository == package,
        _ => false,
    }
}

/// Runs one git command, and gives nothing when git cannot answer.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
