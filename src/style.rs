//! This module holds the terminal codes for colour and weight.
//!
//! qex writes these codes only when the standard output is a terminal. A file
//! or a pipe thus receives clean text, and a command such as
//! `qex top --once > page.txt` gives a file that a program can read.
//!
//! The module also follows the `NO_COLOR` variable, which a user sets to ask
//! every program for text with no colour.

use std::sync::OnceLock;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";

/// Tests if qex may write terminal codes.
///
/// The answer stays the same for the life of the process, so this function
/// reads the conditions one time.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        // A user sets NO_COLOR to ask for text with no colour.
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
            return false;
        }
        unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
    })
}

/// A user can turn the colour off for one command.
static FORCED_OFF: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn turn_off() {
    FORCED_OFF.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn active() -> bool {
    !FORCED_OFF.load(std::sync::atomic::Ordering::SeqCst) && enabled()
}

fn wrap(codes: &str, text: &str) -> String {
    if active() {
        format!("{codes}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Writes a heading.
pub fn heading(text: &str) -> String {
    wrap(BOLD, text)
}

/// Writes text that matters less than the text around it.
pub fn faint(text: &str) -> String {
    wrap(DIM, text)
}

/// Writes text with the colour of a job state.
///
/// The colour follows the meaning: green for work that operates, yellow for
/// work that waits, red for a failure, and a faint colour for a job that
/// succeeded, because that job needs no attention.
pub fn state(name: &str, text: &str) -> String {
    let codes = match name {
        "running" => GREEN,
        "starting" => GREEN,
        "queued" => YELLOW,
        "completed" => DIM,
        "failed" => RED,
        "oom" => RED,
        "timeout" => MAGENTA,
        "killed" => MAGENTA,
        "cancelled" => DIM,
        "skipped" => BLUE,
        _ => return text.to_string(),
    };
    wrap(codes, text)
}

/// Writes a warning.
pub fn warning(text: &str) -> String {
    wrap(RED, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test has no terminal, so the text must hold no code. This behaviour is
    /// also what a pipe and a file need.
    #[test]
    fn a_command_with_no_terminal_writes_plain_text() {
        assert_eq!(heading("ID"), "ID");
        assert_eq!(faint("done"), "done");
        assert_eq!(state("running", "running"), "running");
        assert!(!warning("careful").contains('\x1b'));
    }

    #[test]
    fn an_unknown_state_gives_the_text_back() {
        assert_eq!(state("something-else", "text"), "text");
    }
}
