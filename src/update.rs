//! Looks for a newer release of qex, and says so one time.
//!
//! # Who asks
//!
//! THE COORDINATOR ASKS. THE CLI READS A FILE.
//!
//! A check must never delay a command and must never fail one. The one way to
//! keep both rules absolutely is to take the network out of the path of a
//! command: the coordinator already lives across commands, so it asks on its
//! own time, in its own thread, and a `qex submit` waits for the queue and for
//! nothing else. One call also serves every agent on the machine, which is the
//! property that this queue exists for.
//!
//! `qex version --check` is the one exception. A person asked for an answer
//! now, so that command asks now.
//!
//! # How qex asks
//!
//! It runs `curl`, and then `wget`. qex holds nine dependencies and an HTTP
//! client would bring a TLS stack; both of these programs are on Linux and on
//! macOS already. A machine with neither gets the message that says so, and
//! nothing else changes.
//!
//! # What qex never does
//!
//! It never installs anything. A tool that replaces its own program needs a
//! decision from the person who installed it, and that person may have a
//! package manager that owns the file.

use crate::config::Config;
use crate::paths;
use crate::units::parse_duration;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The word that turns the check off completely.
pub const NEVER: &str = "never";

/// What qex knows about the newest release.
///
/// The coordinator writes this file and the CLI reads it, so a command needs
/// no network and no coordinator to say that a newer release exists.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// When qex last asked, in seconds. Zero says that it never asked.
    pub last_checked: u64,
    /// The newest release that the service named.
    pub newest: Option<String>,
    /// The service that answered.
    pub source: Option<String>,
    /// What went wrong the last time qex asked.
    pub error: Option<String>,
    /// The version that qex last named to a reader.
    ///
    /// One message for each release. A line on every command teaches a reader
    /// to read no line at all.
    pub told: Option<String>,
}

/// The answer of the service.
pub struct Answer {
    pub newest: String,
    pub source: String,
}

fn record_path() -> Result<std::path::PathBuf> {
    Ok(paths::state_dir()?.join("update.json"))
}

pub fn read_record() -> Record {
    let Ok(path) = record_path() else {
        return Record::default();
    };
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_record(record: &Record) -> Result<()> {
    let path = record_path()?;
    paths::ensure_dir(&paths::state_dir()?, 0o700)?;
    let bytes = serde_json::to_vec_pretty(record)?;
    crate::job::write_atomic(&path, &bytes, 0o600)
}

/// Gives the time between two checks, and `None` for `never`.
pub fn interval(cfg: &Config) -> Result<Option<Duration>> {
    let value = cfg.update.check.trim();
    if value.eq_ignore_ascii_case(NEVER) {
        return Ok(None);
    }
    match parse_duration(value) {
        // `0` means the same as `never` here, in the same way as every other
        // time in the config file.
        Ok(None) => Ok(None),
        Ok(Some(d)) => Ok(Some(d)),
        Err(e) => bail!("[update] check: {e}"),
    }
}

/// Asks the service now.
///
/// This function talks to the network. Only the coordinator and
/// `qex version --check` call it.
pub fn ask(cfg: &Config) -> Result<Answer> {
    let url = cfg.update.url.trim().to_string();
    if url.is_empty() {
        bail!("[update] url is empty, so qex has nothing to ask");
    }
    let limit = parse_duration(&cfg.update.timeout)
        .map_err(|e| anyhow::anyhow!("[update] timeout: {e}"))?
        .unwrap_or(Duration::from_secs(5));
    let seconds = limit.as_secs().max(1).to_string();

    let body = fetch(&url, &seconds)?;
    let newest = tag_of(&body)?;
    Ok(Answer {
        newest,
        source: url,
    })
}

/// Runs the program that talks to the network.
fn fetch(url: &str, seconds: &str) -> Result<String> {
    let attempts: [(&str, Vec<String>); 2] = [
        (
            "curl",
            vec![
                "-fsSL".into(),
                "--max-time".into(),
                seconds.into(),
                "-H".into(),
                "Accept: application/vnd.github+json".into(),
                url.into(),
            ],
        ),
        (
            "wget",
            vec![
                "-qO-".into(),
                format!("--timeout={seconds}"),
                "--tries=1".into(),
                url.into(),
            ],
        ),
    ];

    let mut missing = Vec::new();
    for (program, args) in attempts {
        let answer = std::process::Command::new(program).args(&args).output();
        match answer {
            Ok(out) if out.status.success() => {
                return Ok(String::from_utf8_lossy(&out.stdout).to_string());
            }
            Ok(out) => {
                // The program ran and the request failed. Give the words of
                // the program: they name the proxy, the certificate or the
                // limit, and qex cannot say it better.
                let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let code = out.status.code().unwrap_or(-1);
                bail!(
                    "{program} could not reach {url}: exit code {code}{}",
                    if said.is_empty() {
                        String::new()
                    } else {
                        format!(": {said}")
                    }
                );
            }
            Err(_) => missing.push(program),
        }
    }

    bail!(
        "qex asks a web service with `curl` or `wget`, and this machine has neither ({}). \
         Install one, or set `[update] check = \"never\"` in your config file.",
        missing.join(" and ")
    )
}

/// Takes the version from the answer of the service.
fn tag_of(body: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: Option<String>,
    }
    let release: Release = serde_json::from_str(body)
        .context("the service gave an answer that qex could not read as JSON")?;
    let tag = release
        .tag_name
        .filter(|t| !t.trim().is_empty())
        .context("the answer of the service holds no `tag_name`")?;
    Ok(tag.trim().trim_start_matches('v').to_string())
}

/// Gives the three numbers of a release, and `None` for anything else.
fn numbers_of(version: &str) -> Option<(u64, u64, u64)> {
    // A release is `X.Y.Z` and nothing more. A build that carries a suffix —
    // `0.0.0-dev+g98513e2` — is not a release and takes no place in the order.
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// True when `newest` is a release above `mine`.
///
/// A development build is never above and never below: `qex version --check`
/// says what it is, and the automatic check says nothing at all about it.
pub fn is_newer(mine: &str, newest: &str) -> bool {
    match (numbers_of(mine), numbers_of(newest)) {
        (Some(mine), Some(newest)) => newest > mine,
        _ => false,
    }
}

/// Asks when the interval passed, and writes what the service said.
///
/// The coordinator calls this. It gives no error to its caller: a service that
/// does not answer is not a fault of the queue, and a job must never stop for
/// it.
pub fn check_if_due(cfg: &Config) {
    let Ok(Some(gap)) = interval(cfg) else {
        return;
    };
    let mut record = read_record();
    let now = crate::sys::now_secs();

    // THE FIRST RUN DOES NOT ASK.
    //
    // A person who installed qex a moment ago holds the newest version nearly
    // by definition, so a call on the first day spends the network to say "you
    // are up to date". qex therefore writes the time and stays quiet, and it
    // opens no connection at all until the first interval passes.
    if record.last_checked == 0 {
        record.last_checked = now;
        write_record(&record).ok();
        return;
    }

    if now.saturating_sub(record.last_checked) < gap.as_secs() {
        return;
    }

    record.last_checked = now;
    match ask(cfg) {
        Ok(answer) => {
            record.newest = Some(answer.newest);
            record.source = Some(answer.source);
            record.error = None;
        }
        Err(e) => {
            // Keep the newest version that qex knows. A service that did not
            // answer today does not remove what it said last week.
            record.error = Some(format!("{e:#}"));
        }
    }
    write_record(&record).ok();
}

/// Gives the line that a command writes, and `None` when there is nothing new.
///
/// This function reads a file. It opens no connection, so a command that calls
/// it cannot wait for a service and cannot fail because of one.
pub fn note_for_a_command(cfg: &Config) -> Option<String> {
    interval(cfg).ok().flatten()?;
    let mut record = read_record();
    let line = note(crate::version::VERSION, &record)?;
    // Keep the version that qex named, so the next command says nothing.
    record.told.clone_from(&record.newest);
    write_record(&record).ok();
    Some(line)
}

/// The decision behind `note_for_a_command`, with no file and no clock.
///
/// It is a function of the version and the record alone, so a test gives it
/// every case: this build is a development build, the release is older, the
/// release is newer, and qex named that release already.
fn note(mine: &str, record: &Record) -> Option<String> {
    // A development build takes no place in the order of the releases, so it
    // gets no message. A person who builds qex knows what they built.
    if crate::version::is_development(mine) {
        return None;
    }
    let newest = record.newest.as_deref()?;
    if !is_newer(mine, newest) {
        return None;
    }
    // One message for each release.
    if record.told.as_deref() == Some(newest) {
        return None;
    }
    Some(format!(
        "qex: a newer qex exists: {newest}. This is {mine}. Run `qex version --check` for the \
         detail, or set `[update] check = \"never\"` to stop this message."
    ))
}

/// The answer of `qex version --check`, for a reader and for a program.
pub struct Report {
    pub mine: String,
    pub newest: Option<String>,
    pub source: Option<String>,
    pub development: bool,
    pub newer: bool,
    pub error: Option<String>,
}

/// Asks now, and describes the answer.
pub fn report(cfg: &Config) -> Report {
    let mine = crate::version::VERSION.to_string();
    let development = crate::version::is_development(&mine);
    match ask(cfg) {
        Ok(answer) => Report {
            newer: !development && is_newer(&mine, &answer.newest),
            newest: Some(answer.newest),
            source: Some(answer.source),
            mine,
            development,
            error: None,
        },
        Err(e) => Report {
            mine,
            newest: None,
            source: Some(cfg.update.url.clone()),
            development,
            newer: false,
            error: Some(format!("{e:#}")),
        },
    }
}

impl Report {
    /// The words for a person.
    pub fn text(&self) -> String {
        let mine = &self.mine;
        if let Some(error) = &self.error {
            return format!(
                "qex {mine}\nqex could not ask for the newest release: {error}\n\
                 The version that you have still operates. Nothing changed."
            );
        }
        let newest = self.newest.clone().unwrap_or_default();
        let source = self.source.clone().unwrap_or_default();

        // A DEVELOPMENT BUILD IS NEITHER UP TO DATE NOR OUT OF DATE.
        //
        // It carries the hash of a commit and no place in the order of the
        // releases, so a message that picks one of those two is wrong.
        if self.development {
            return format!(
                "This is a development build: {mine}.\n\
                 The newest release is {newest}, from {source}.\n\
                 A development build is neither newer nor older than a release. It holds the \
                 commit that you built."
            );
        }
        if self.newer {
            return format!(
                "qex {mine}\nA newer release exists: {newest}, from {source}.\n\
                 Take it from https://github.com/stephenc/qex/releases/latest , or run \
                 `cargo install qex`.\n\
                 qex installs nothing by itself."
            );
        }
        format!("qex {mine}\nThis is the newest release. The newest is {newest}, from {source}.")
    }

    /// The same answer for a program.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "version": self.mine,
            "newest": self.newest,
            "newer": self.newer,
            "development": self.development,
            "source": self.source,
            "error": self.error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_above_this_one_is_newer() {
        assert!(is_newer("0.23.0", "0.24.0"));
        assert!(is_newer("0.23.0", "1.0.0"));
        assert!(is_newer("0.23.0", "0.23.1"));
        assert!(!is_newer("0.23.0", "0.23.0"));
        assert!(!is_newer("0.23.1", "0.23.0"));
        // The order is by NUMBER, and not by text. "0.9.0" is below "0.23.0",
        // and a comparison of the text says the opposite.
        assert!(is_newer("0.9.0", "0.23.0"));
        assert!(!is_newer("0.23.0", "0.9.0"));
    }

    /// A development build has no place in the order.
    ///
    /// Nothing may offer a development build as an update, and nothing may
    /// call a development build old. `0.0.0-dev+g98513e2` is not a release.
    #[test]
    fn a_development_build_is_neither_newer_nor_older() {
        assert!(!is_newer("0.0.0-dev+g98513e2", "0.23.0"));
        assert!(!is_newer("0.23.0", "0.0.0-dev+g98513e2"));
        assert!(!is_newer("0.23.0", "0.24.0-rc1"));
    }

    #[test]
    fn the_tag_of_a_release_loses_its_v() {
        assert_eq!(tag_of(r#"{"tag_name":"v0.23.0"}"#).unwrap(), "0.23.0");
        assert_eq!(tag_of(r#"{"tag_name":"0.23.0"}"#).unwrap(), "0.23.0");
        assert!(tag_of(r#"{"tag_name":""}"#).is_err());
        assert!(tag_of(r#"{"other":1}"#).is_err());
        assert!(tag_of("not json").is_err());
    }

    fn record_of(newest: &str, told: Option<&str>) -> Record {
        Record {
            last_checked: 1,
            newest: Some(newest.to_string()),
            source: Some("a service".into()),
            error: None,
            told: told.map(|t| t.to_string()),
        }
    }

    /// The line arrives one time for each release, and never for a build that
    /// takes no place in the order.
    #[test]
    fn the_line_arrives_one_time_for_each_release() {
        // A newer release: say it.
        let record = record_of("0.24.0", None);
        let line = note("0.23.0", &record).expect("a newer release must give a line");
        assert!(line.contains("0.24.0") && line.contains("0.23.0"), "{line}");
        assert!(
            line.contains("never"),
            "the line must say how to stop it: {line}"
        );

        // The same release, once qex named it: say nothing.
        assert!(note("0.23.0", &record_of("0.24.0", Some("0.24.0"))).is_none());

        // A release that came AFTER the one that qex named: say it again.
        assert!(note("0.23.0", &record_of("0.25.0", Some("0.24.0"))).is_some());

        // The newest release is this one, or older: say nothing.
        assert!(note("0.24.0", &record_of("0.24.0", None)).is_none());
        assert!(note("0.25.0", &record_of("0.24.0", None)).is_none());

        // A development build: say nothing, whatever the service said.
        assert!(note("0.0.0-dev+g98513e2", &record_of("9.9.9", None)).is_none());

        // qex asked and learned nothing: say nothing.
        let mut empty = record_of("0.24.0", None);
        empty.newest = None;
        assert!(note("0.23.0", &empty).is_none());
    }

    /// `never` is absolute, and `0` says the same thing.
    #[test]
    fn never_gives_no_interval() {
        let mut cfg = Config::default();
        cfg.update.check = "never".into();
        assert!(interval(&cfg).unwrap().is_none());
        cfg.update.check = "NEVER".into();
        assert!(interval(&cfg).unwrap().is_none());
        cfg.update.check = "0".into();
        assert!(interval(&cfg).unwrap().is_none());
        cfg.update.check = "7d".into();
        assert_eq!(
            interval(&cfg).unwrap(),
            Some(Duration::from_secs(7 * 24 * 3600))
        );
    }
}
