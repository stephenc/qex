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
use std::os::unix::fs::OpenOptionsExt;
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
#[derive(Debug)]
pub struct Answer {
    pub newest: String,
    pub source: String,
}

pub fn read_record() -> Record {
    let Ok(dir) = paths::state_dir() else {
        return Record::default();
    };
    read_record_in(&dir)
}

/// The same, for one directory. A test gives its own.
fn read_record_in(dir: &std::path::Path) -> Record {
    let path = dir.join("update.json");
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_record_in(dir: &std::path::Path, record: &Record) -> Result<()> {
    paths::ensure_dir(dir, 0o700)?;
    let bytes = serde_json::to_vec_pretty(record)?;
    crate::job::write_atomic(&dir.join("update.json"), &bytes, 0o600)
}

/// Reads the record, changes it, and writes it, with nobody else in between.
///
/// TWO WRITERS SHARE THIS FILE. The coordinator writes what a service said,
/// and a command writes the version that it named to a reader. Each one holds
/// the WHOLE record, so a write of one can undo a write of the other: a
/// command that says "0.24.0 exists" while the coordinator waits for the
/// network would lose that word when the coordinator writes, and a reader
/// would then meet the same line again.
///
/// A lock file makes the three steps one operation. The kernel gives the lock
/// back when a process stops, so a process that dies here leaves nothing
/// locked.
fn with_the_record(change: impl FnOnce(&mut Record)) -> Result<Record> {
    with_the_record_in(&paths::state_dir()?, change)
}

/// The same, for one directory. A test gives its own.
fn with_the_record_in(dir: &std::path::Path, change: impl FnOnce(&mut Record)) -> Result<Record> {
    use std::os::unix::io::AsRawFd;

    paths::ensure_dir(dir, 0o700)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.join("update.lock"))?;
    let held = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0;

    let mut record = read_record_in(dir);
    change(&mut record);
    let answer = write_record_in(dir, &record);

    if held {
        unsafe {
            libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
        }
    }
    answer.map(|_| record)
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
    if !is_an_address(&url) {
        bail!(
            "[update] url must start with `https://`, `http://` or `file://`, and it holds \
             `{url}`. An address that starts with a dash becomes an OPTION of the program \
             that asks."
        );
    }
    let limit = parse_duration(&cfg.update.timeout)
        .map_err(|e| anyhow::anyhow!("[update] timeout: {e}"))?
        .unwrap_or(Duration::from_secs(5));

    let body = fetch(&url, limit)?;
    let newest = tag_of(&body)?;
    Ok(Answer {
        newest,
        source: url,
    })
}

/// The most that qex reads from the service.
///
/// The answer is one small JSON object. A service that sends more than this is
/// not answering the question, and the coordinator must not hold it: a read
/// with no limit reached 8.3GB of memory in 5 seconds against a service that
/// floods, IN THE PROCESS WHOSE PURPOSE IS TO STOP AN OUT-OF-MEMORY KILL.
const MOST_BYTES: usize = 256 * 1024;

/// The schemes that `[update] url` may hold.
///
/// The address goes to `curl` as an argument, and a value that starts with a
/// dash becomes an OPTION of curl: `-K/path/to/curlrc` makes curl read a
/// configuration of its own, and that configuration can write a file. A user
/// pastes this field from the instructions of a mirror, so it takes the
/// narrow test and not a test for a leading dash.
const SCHEMES: [&str; 3] = ["https://", "http://", "file://"];

/// True when qex may ask this address.
pub fn is_an_address(url: &str) -> bool {
    SCHEMES.iter().any(|scheme| url.starts_with(scheme))
}

/// Runs the program that talks to the network.
///
/// qex holds the limit itself, and it does not trust the program to hold one.
/// `curl --max-time` covers the whole transfer, and the `--timeout` of `wget`
/// covers ONE operation: a service that sends one byte every two seconds keeps
/// wget for ever, and a coordinator that stops then leaves it behind. The
/// deadline below covers both, and a child that passes it takes a signal.
fn fetch(url: &str, limit: Duration) -> Result<String> {
    use std::io::Read;

    let seconds = limit.as_secs().max(1).to_string();
    let attempts: [(&str, Vec<String>); 2] = [
        (
            "curl",
            vec![
                "-fsSL".into(),
                "--max-time".into(),
                seconds.clone(),
                "--max-filesize".into(),
                MOST_BYTES.to_string(),
                "-H".into(),
                "Accept: application/vnd.github+json".into(),
                url.into(),
            ],
        ),
        (
            // THE FLAGS THAT EVERY `wget` TAKES.
            //
            // The `wget` of busybox is the one on Alpine and on most small
            // containers, which are the machines least likely to hold curl.
            // It does not take `--timeout=` or `--tries=`, and a command with
            // those gives a usage error that names an option and not the
            // fault. `-q`, `-O -` and `-T` are in busybox and in GNU wget
            // alike. The deadline of qex covers the time in either case.
            "wget",
            vec![
                "-q".into(),
                "-O".into(),
                "-".into(),
                "-T".into(),
                seconds.clone(),
                url.into(),
            ],
        ),
    ];

    let mut missing = Vec::new();
    for (program, args) in attempts {
        let child = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(_) => {
                missing.push(program);
                continue;
            }
        };

        // Read no more than the limit, and stop at the deadline.
        //
        // THE REASON FOR THE STOP COMES FROM THIS LOOP, and never from a test
        // of the child. A program that ends the moment before this loop ends
        // is not reaped yet, and `try_wait` then says that it operates: a
        // fault that arrived in six milliseconds read as a service that never
        // answered, and every answer that arrived was one race away from the
        // same message.
        enum Stop {
            /// The program closed its output. It is finishing now.
            Answered,
            /// The limit of the reader came first.
            TooSlow,
            /// The service sent more than an answer.
            TooMuch,
        }
        let deadline = std::time::Instant::now() + limit;
        let mut body = Vec::new();
        let mut stop = Stop::Answered;
        if let Some(out) = child.stdout.as_mut() {
            let mut buffer = [0u8; 8192];
            loop {
                if std::time::Instant::now() >= deadline {
                    stop = Stop::TooSlow;
                    break;
                }
                match out.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        body.extend_from_slice(&buffer[..n]);
                        if body.len() > MOST_BYTES {
                            stop = Stop::TooMuch;
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }

        // A program that qex stopped takes a signal. A program that answered
        // is closing already, so qex waits for it in the usual way.
        if !matches!(stop, Stop::Answered) {
            child.kill().ok();
        }
        let end = child.wait_with_output();

        match stop {
            Stop::TooMuch => bail!(
                "the answer of {url} passed {MOST_BYTES} bytes, and the answer of this service \
                 is one small object. qex stopped the read."
            ),
            Stop::TooSlow => bail!(
                "{program} did not answer for {url} in {seconds} seconds, and qex stopped it."
            ),
            Stop::Answered => {}
        }

        let Ok(end) = end else {
            bail!("qex could not wait for {program}");
        };
        if end.status.success() {
            return Ok(String::from_utf8_lossy(&body).to_string());
        }

        // The program ran and the request failed. Give the words of the
        // program: they name the proxy, the certificate or the limit, and qex
        // cannot say it better.
        let said = String::from_utf8_lossy(&end.stderr).trim().to_string();
        let code = end.status.code().unwrap_or(-1);
        bail!(
            "{program} could not reach {url}: exit code {code}{}",
            if said.is_empty() {
                String::new()
            } else {
                format!(": {said}")
            }
        );
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
    let record = read_record();
    let now = crate::sys::now_secs();

    // THE FIRST RUN DOES NOT ASK.
    //
    // A person who installed qex a moment ago holds the newest version nearly
    // by definition, so a call on the first day spends the network to say "you
    // are up to date". qex therefore writes the time and stays quiet, and it
    // opens no connection at all until the first interval passes.
    if record.last_checked == 0 {
        with_the_record(|r| r.last_checked = now).ok();
        return;
    }

    if now.saturating_sub(record.last_checked) < gap.as_secs() {
        return;
    }

    // ASK WITH NO LOCK. The service can take seconds, or never answer, and a
    // command that reads this file must not wait for it.
    let answer = ask(cfg);

    // Change THIS coordinator's fields, and leave the field that a command
    // owns. See `with_the_record`.
    with_the_record(|r| {
        r.last_checked = now;
        match &answer {
            Ok(answer) => {
                r.newest = Some(answer.newest.clone());
                r.source = Some(answer.source.clone());
                r.error = None;
            }
            // Keep the newest version that qex knows. A service that did not
            // answer today does not remove what it said last week.
            Err(e) => r.error = Some(format!("{e:#}")),
        }
    })
    .ok();
}

/// Gives the line that a command writes, and `None` when there is nothing new.
///
/// This function reads a file. It opens no connection, so a command that calls
/// it cannot wait for a service and cannot fail because of one.
pub fn note_for_a_command(cfg: &Config) -> Option<String> {
    let dir = paths::state_dir().ok()?;
    note_for_a_command_in(&dir, crate::version::VERSION, cfg)
}

/// The same, for one directory and one version. A test gives both.
fn note_for_a_command_in(dir: &std::path::Path, mine: &str, cfg: &Config) -> Option<String> {
    interval(cfg).ok().flatten()?;
    // DECIDE WITH NO LOCK AND NO WRITE FIRST.
    //
    // This runs at the end of EVERY command, and nearly every time it has
    // nothing to say. A lock and a write on that path would cost each command
    // a file operation, and it would make the file exist before the
    // coordinator ever asked anything.
    note(mine, &read_record_in(dir))?;

    // There is a line to give. Decide again on the record as it stands INSIDE
    // the lock, and claim the version in the same operation: the coordinator
    // writes this file as well, and a decision from before its write would
    // give the same line two times.
    let mut line = None;
    with_the_record_in(dir, |r| {
        line = note(mine, r);
        if line.is_some() {
            // Keep the version that qex named, so the next command is quiet.
            r.told.clone_from(&r.newest);
        }
    })
    .ok()?;
    line
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

    fn a_directory(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qx-upd-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The line reaches a command, and the record remembers that it did.
    ///
    /// The whole path is under test here: the decision, the lock and the
    /// write. A test of the decision alone passes when nothing calls it.
    #[test]
    fn a_command_gives_the_line_one_time_and_remembers_it() {
        let dir = a_directory("cmd");
        let cfg = Config::default();
        write_record_in(&dir, &record_of("0.24.0", None)).unwrap();

        let first = note_for_a_command_in(&dir, "0.23.0", &cfg);
        assert!(first.is_some(), "the first command must give the line");
        assert_eq!(
            read_record_in(&dir).told.as_deref(),
            Some("0.24.0"),
            "the record must remember the version that qex named"
        );

        // The second command says nothing, and the record keeps its other
        // fields.
        assert!(note_for_a_command_in(&dir, "0.23.0", &cfg).is_none());
        let record = read_record_in(&dir);
        assert_eq!(record.newest.as_deref(), Some("0.24.0"));
        assert_eq!(record.source.as_deref(), Some("a service"));

        // `never` stops the line, whatever the record says.
        let mut quiet = Config::default();
        quiet.update.check = "never".into();
        write_record_in(&dir, &record_of("0.25.0", None)).unwrap();
        assert!(note_for_a_command_in(&dir, "0.23.0", &quiet).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A command that has nothing to say writes NOTHING.
    ///
    /// This runs at the end of every command. A write there would cost each
    /// command a file operation, and it would make the record exist before the
    /// coordinator ever asked.
    #[test]
    fn a_command_with_nothing_to_say_writes_no_file() {
        let dir = a_directory("quiet");
        let cfg = Config::default();

        assert!(note_for_a_command_in(&dir, "0.23.0", &cfg).is_none());
        assert!(
            !dir.join("update.json").exists(),
            "a command with nothing to say must write no record"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The record keeps the field of the OTHER writer.
    ///
    /// The coordinator writes what a service said, and a command writes the
    /// version that it named. Each one holds the whole record, so a change of
    /// one must not undo a change of the other.
    #[test]
    fn a_writer_keeps_the_field_of_the_other_writer() {
        let dir = a_directory("both");
        write_record_in(&dir, &record_of("0.24.0", Some("0.24.0"))).unwrap();

        // The coordinator learns of a newer release.
        with_the_record_in(&dir, |r| {
            r.newest = Some("0.25.0".into());
            r.last_checked = 99;
        })
        .unwrap();
        let record = read_record_in(&dir);
        assert_eq!(
            record.told.as_deref(),
            Some("0.24.0"),
            "the coordinator must keep the word of a command"
        );

        // A command then names the new one.
        let line = note_for_a_command_in(&dir, "0.23.0", &Config::default());
        assert!(line.unwrap().contains("0.25.0"));
        let record = read_record_in(&dir);
        assert_eq!(record.last_checked, 99, "a command must keep the time");
        assert_eq!(record.told.as_deref(), Some("0.25.0"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An address that is not an address is refused before anything runs.
    ///
    /// `curl` takes a value that starts with a dash as an OPTION, and
    /// `-K/path` makes it read a configuration that can write a file.
    #[test]
    fn only_an_address_reaches_the_program_that_asks() {
        assert!(is_an_address("https://example.test/x"));
        assert!(is_an_address("http://example.test/x"));
        assert!(is_an_address("file:///tmp/x"));
        assert!(!is_an_address("-K/tmp/curlrc"));
        assert!(!is_an_address("--config=/tmp/curlrc"));
        assert!(!is_an_address("example.test/x"));
        assert!(!is_an_address(""));

        let mut cfg = Config::default();
        cfg.update.url = "-K/tmp/curlrc".into();
        let e = format!("{:#}", ask(&cfg).expect_err("qex must refuse this"));
        assert!(e.contains("must start with"), "got: {e}");
    }

    /// A service that floods must not fill the memory of the coordinator.
    ///
    /// A read with no limit reached 8.3GB in 5 seconds, in the process whose
    /// purpose is to stop an out-of-memory kill.
    #[test]
    fn an_answer_that_never_ends_stops_at_the_limit() {
        let mut cfg = Config::default();
        cfg.update.url = "file:///dev/zero".into();
        cfg.update.timeout = "10s".into();

        let started = std::time::Instant::now();
        let e = format!("{:#}", ask(&cfg).expect_err("an endless answer is a fault"));
        assert!(
            e.contains("passed") || e.contains("could not reach"),
            "got: {e}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the read must stop at the limit, and it took {:?}",
            started.elapsed()
        );
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
