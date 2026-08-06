//! This module finds the monitor scripts that wait for a proxy.
//!
//! A monitor waits for evidence of the work: a pattern in the process list, a
//! line in a log file, a file that appears. Evidence stops arriving when the
//! work stops, and the monitor cannot see that. Such a monitor then sleeps for
//! ever. Four of them on one machine slept for 95 hours between them.
//!
//! # Why this command can do what a shell command cannot
//!
//! The usual way to look for these monitors is `pgrep -f pgrep`, and that
//! command matches itself: its own command line holds the letters that it looks
//! for. A user of that method finds the search and reads it as a monitor.
//!
//! This command knows its own process id, its own process group and its own
//! ancestors, so it removes them before it reports anything. It cannot find
//! itself.

use serde::Serialize;

/// One monitor that this command found.
#[derive(Debug, Clone, Serialize)]
pub struct Watcher {
    pub pid: i32,
    /// The time that this process has operated, in seconds.
    pub age_secs: u64,
    pub command: String,
    /// The kind of proxy that this monitor waits for.
    pub proxy: &'static str,
    /// What a reader should do about it.
    pub advice: &'static str,
}

/// Tests one command line, and gives the kind of proxy that it waits for.
///
/// This function looks for a loop that sleeps. A command that sleeps once is
/// not a monitor.
pub fn classify(command: &str) -> Option<(&'static str, &'static str)> {
    let lower = command.to_ascii_lowercase();

    // A monitor sleeps in a loop. Without a sleep, a command that holds these
    // words is doing its work and not waiting for a proxy.
    let sleeps = lower.contains("sleep");
    let loops = lower.contains("while ") || lower.contains("until ") || lower.contains("for ");
    if !(sleeps && loops) {
        return None;
    }

    if lower.contains("pgrep")
        || lower.contains("pidof")
        || lower.contains("ps -ef")
        || lower.contains("ps aux")
    {
        return Some((
            "a pattern in the process list",
            "This monitor can match its own command line. Use `qex submit` and \
             `qex status <id> --wait`.",
        ));
    }

    if lower.contains("grep") || lower.contains("tail ") {
        return Some((
            "a line in a log file",
            "The line never arrives when something stops the task that writes it. \
             Use `qex submit` and `qex status <id> --wait`.",
        ));
    }

    if lower.contains("test -f") || lower.contains("[ -f") || lower.contains("[ -e") {
        return Some((
            "a file that appears",
            "The file never appears when something stops the task that writes it. \
             Use `qex submit` and `qex status <id> --wait`.",
        ));
    }

    Some((
        "an unknown condition",
        "This loop sleeps and tests a condition. Use `qex submit` and \
         `qex status <id> --wait`, which waits for the process itself.",
    ))
}

/// Finds the monitors that operate now.
///
/// The result never holds this process, its process group, or any process that
/// started it. A command that looks for this fault must not find itself.
#[cfg(target_os = "linux")]
pub fn find() -> Vec<Watcher> {
    let mut out = Vec::new();

    let me = std::process::id() as i32;
    let ancestors = ancestors_of(me);

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };

    let uptime = read_uptime();
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };

        // Remove this process and the processes that started it. This step is
        // the reason that this command can look for a fault that a shell
        // command cannot look for without finding itself: the shell that runs
        // `qex watchers` holds those letters in its own command line.
        //
        // Remove these processes only. A monitor that a user started from the
        // same shell is a true result, and it is the usual case.
        if pid == me || ancestors.contains(&pid) {
            continue;
        }

        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        // The parts of a command line are separated by a zero byte.
        let command = String::from_utf8_lossy(&raw)
            .replace('\0', " ")
            .trim()
            .to_string();

        // A command that runs qex itself is not a monitor.
        if command.contains("qex watchers") {
            continue;
        }

        let Some((proxy, advice)) = classify(&command) else {
            continue;
        };

        out.push(Watcher {
            pid,
            age_secs: process_age(&entry.path(), uptime, ticks),
            command,
            proxy,
            advice,
        });
    }

    // The oldest first. A monitor that has slept for a day is the clearest
    // fault, and the most useful line for a reader.
    out.sort_by_key(|w| std::cmp::Reverse(w.age_secs));
    out
}

#[cfg(not(target_os = "linux"))]
pub fn find() -> Vec<Watcher> {
    // macOS has no `/proc`. Read the process list with `ps`, and remove this
    // process and its group in the same way.
    let mut out = Vec::new();
    let me = std::process::id() as i32;
    let my_group = unsafe { libc::getpgid(0) };

    let Ok(result) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,pgid=,etime=,command="])
        .output()
    else {
        return out;
    };

    for line in String::from_utf8_lossy(&result.stdout).lines() {
        let mut parts = line.trim().splitn(4, char::is_whitespace);
        let (Some(pid), Some(group), Some(elapsed), Some(command)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        if pid == me || group.parse::<i32>() == Ok(my_group) {
            continue;
        }
        let Some((proxy, advice)) = classify(command) else {
            continue;
        };
        out.push(Watcher {
            pid,
            age_secs: parse_elapsed(elapsed),
            command: command.to_string(),
            proxy,
            advice,
        });
    }

    out.sort_by_key(|w| std::cmp::Reverse(w.age_secs));
    out
}

#[cfg(not(target_os = "linux"))]
fn parse_elapsed(text: &str) -> u64 {
    // `ps` gives [[DD-]HH:]MM:SS.
    let (days, rest) = match text.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().unwrap_or(0), r),
        None => (0, text),
    };
    let mut seconds = 0u64;
    for part in rest.split(':') {
        seconds = seconds * 60 + part.parse::<u64>().unwrap_or(0);
    }
    days * 86400 + seconds
}

/// Gives the process ids that started this process.
#[cfg(target_os = "linux")]
fn ancestors_of(mut pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    // A limit, so a strange process table cannot make an endless loop.
    for _ in 0..64 {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        let Some(rest) = stat.rsplit_once(") ") else {
            break;
        };
        let fields: Vec<&str> = rest.1.split_whitespace().collect();
        if fields.len() < 2 {
            break;
        }
        let Ok(parent) = fields[1].parse::<i32>() else {
            break;
        };
        if parent <= 1 {
            break;
        }
        out.push(parent);
        pid = parent;
    }
    out
}

#[cfg(target_os = "linux")]
fn read_uptime() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next().map(|s| s.to_string()))
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

#[cfg(target_os = "linux")]
fn process_age(dir: &std::path::Path, uptime: f64, ticks: f64) -> u64 {
    let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
        return 0;
    };
    let Some(rest) = stat.rsplit_once(") ") else {
        return 0;
    };
    let fields: Vec<&str> = rest.1.split_whitespace().collect();
    // After the command, field 20 is the time when the process started.
    if fields.len() < 20 {
        return 0;
    }
    let started: f64 = fields[19].parse().unwrap_or(0.0);
    (uptime - started / ticks).max(0.0) as u64
}

/// Writes the monitors that this command found.
pub fn report(json: bool) -> anyhow::Result<i32> {
    let found = find();

    if json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(if found.is_empty() { 0 } else { 1 });
    }

    if found.is_empty() {
        println!("no monitor script waits for a proxy on this machine.");
        return Ok(0);
    }

    let total: u64 = found.iter().map(|w| w.age_secs).sum();
    println!(
        "{} monitor script(s) wait for a proxy. Together they have waited {}.",
        found.len(),
        crate::units::format_duration(std::time::Duration::from_secs(total))
    );
    println!();

    for w in &found {
        println!(
            "{}  pid {}  waiting {}",
            crate::style::warning("MONITOR"),
            w.pid,
            crate::units::format_duration(std::time::Duration::from_secs(w.age_secs))
        );
        println!("  waits for: {}", w.proxy);
        println!("  command:   {:.120}", w.command);
        println!("  {}", w.advice);
        println!();
    }

    println!(
        "{}",
        crate::style::faint(
            "This command removes its own process and the processes that started it \
             before it reports anything, so it never finds itself.\nStop one with \
             `kill <pid>`. A kill of the shell does not stop the processes that the shell \
             started; those go to the init process and continue."
        )
    );
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three monitors that a user measured on one machine.
    #[test]
    fn the_real_monitors_are_recognised() {
        let (proxy, _) = classify("bash -c while pgrep -f solve.py; do sleep 60; done").unwrap();
        assert_eq!(proxy, "a pattern in the process list");

        let (proxy, _) =
            classify("bash -c until grep -q 'COOP written' run.log; do sleep 60; done").unwrap();
        assert_eq!(proxy, "a line in a log file");

        let (proxy, _) =
            classify("sh -c until [ -f /tmp/done.marker ]; do sleep 30; done").unwrap();
        assert_eq!(proxy, "a file that appears");
    }

    /// A command that does its work must not look like a monitor.
    #[test]
    fn ordinary_commands_are_not_monitors() {
        assert!(classify("cargo test --release").is_none());
        assert!(
            classify("grep -r pattern src/").is_none(),
            "a search is not a loop"
        );
        assert!(classify("sleep 60").is_none(), "one sleep is not a loop");
        assert!(
            classify("pgrep -f something").is_none(),
            "one search of the process list is not a monitor"
        );
        assert!(
            classify("python3 train.py --epochs 50").is_none(),
            "a long task is not a monitor"
        );
    }

    /// A loop that sleeps and tests something else is still a monitor.
    #[test]
    fn an_unknown_condition_is_still_a_monitor() {
        let (proxy, advice) =
            classify("bash -c while ! curl -sf localhost:8080; do sleep 5; done").unwrap();
        assert_eq!(proxy, "an unknown condition");
        assert!(advice.contains("qex"));
    }

    /// This command must never report itself.
    ///
    /// A user hunted for this fault with `pgrep -f pgrep`, and the search
    /// matched its own command line. That was the fourth occurrence of the
    /// fault in one day, inside the hunt for it.
    #[test]
    fn the_search_never_finds_itself() {
        let me = std::process::id() as i32;
        let found = find();
        assert!(
            !found.iter().any(|w| w.pid == me),
            "the search reported itself"
        );

        // It must not report the processes that started it either. The shell
        // that runs this test holds the words of the command in its own line.
        for parent in ancestors_of(me) {
            assert!(
                !found.iter().any(|w| w.pid == parent),
                "the search reported a process that started it"
            );
        }
    }
}
