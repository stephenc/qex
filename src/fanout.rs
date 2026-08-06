//! This module reads a file of input lines, and makes one job for each line.
//!
//! `qex submit --each-line inputs.txt -- ./process {}` is the shape. Every job
//! of one command shares a group id, so the user holds one handle for the whole
//! fan-out.
//!
//! # Why the substitution is on the argument list
//!
//! qex starts no shell. It gives the program an argument list, and the operating
//! system passes that list to the program with no further reading. This module
//! keeps that property: it puts the text of a line INSIDE one argument, and it
//! never divides the line into more arguments.
//!
//! A line of an input file is data, and frequently the data comes from a
//! directory listing, a database or another program. A line such as
//! `a b"; rm -rf ~; echo "` therefore becomes exactly one argument, and the
//! characters in it have no meaning. This is the security property of the
//! feature, and `tests/e2e.rs` measures it.

use anyhow::{bail, Result};
use std::path::Path;

/// The text that each line replaces.
pub const PLACEHOLDER: &str = "{}";

/// The largest fan-out that qex accepts with no option.
///
/// A file with 100000 lines would make 100000 job directories and fill the
/// state directory. A limit stops that mistake at the moment of the command,
/// and the message says how to raise it.
///
/// The limit does not ask a question. The main user of qex is an agent, and an
/// agent cannot answer a prompt: a limit that waits for a key gives that user
/// no way forward at all.
pub const DEFAULT_MAX_JOBS: usize = 1000;

/// The longest name that qex makes for one job of a fan-out.
const MAX_NAME: usize = 48;
/// The part of the name that comes from the line.
const MAX_SLUG: usize = 24;
/// The part of the name that comes from the command or from `--name`.
const MAX_BASE: usize = 16;

/// The lines of an input file, and a count of the lines that qex passed over.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// The lines that become jobs, in the order of the file.
    pub lines: Vec<String>,
    /// The number of empty lines.
    pub blank: usize,
    /// The number of comment lines.
    pub comments: usize,
}

impl Input {
    /// True when qex passed over one line or more.
    pub fn skipped(&self) -> usize {
        self.blank + self.comments
    }
}

/// Divides the text of an input file into the lines that become jobs.
///
/// The rules, and the reason for each one:
///
/// * A line ends at a newline. `str::lines` also removes a `\r`, so a file with
///   CRLF endings gives the same jobs as a file with LF endings. A file that
///   came from Windows is common, and a job name that ends in a control
///   character is never what a user wanted.
/// * qex removes the space at the start and at the end of a line. A space at
///   the end of a line is invisible, and it is almost always an accident of the
///   program that wrote the file.
/// * An empty line becomes no job.
/// * A line that starts with `#` is a comment and becomes no job. An input file
///   is frequently written by a person, and a person writes notes in it.
/// * The last line counts, also when the file has no final newline.
///
/// The caller REPORTS the count of each kind that qex passed over. A line that
/// a user expected to run and qex passed over in silence is the quiet wrong
/// answer that this project exists to prevent.
pub fn parse(text: &str) -> Input {
    let mut input = Input::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            input.blank += 1;
        } else if line.starts_with('#') {
            input.comments += 1;
        } else {
            input.lines.push(line.to_string());
        }
    }
    input
}

/// Reads the input file, or standard input for the name `-`.
///
/// `qex submit` reads nothing from standard input for any other purpose, so the
/// name `-` is free and it has its usual meaning.
pub fn read(path: &Path) -> Result<Input> {
    let bytes = if path == Path::new("-") {
        use std::io::Read;
        let mut buffer = Vec::new();
        std::io::stdin().read_to_end(&mut buffer).map_err(|e| {
            anyhow::anyhow!(
                "qex cannot read the lines from standard input: {e}\n\n\
                 `--each-line -` needs those lines, and one line makes one job, so qex \
                 submits nothing.\n\n\
                 Send the lines through a pipe, or give the path of a file:\n\
                 \x20   ls *.csv | qex submit --each-line - -- ./process {PLACEHOLDER}"
            )
        })?;
        buffer
    } else {
        std::fs::read(path).map_err(|e| {
            anyhow::anyhow!(
                "qex cannot read the input file {}: {e}\n\n\
                 `--each-line` needs that file, and one line makes one job, so qex \
                 submits nothing.\n\n\
                 Give the path of a file that exists, or use `-` to read the lines from \
                 another program.",
                path.display()
            )
        })?
    };

    // The text must be UTF-8, and qex refuses the whole file when it is not.
    //
    // A command of a job goes into `spec.json` as text. A byte that is not
    // UTF-8 cannot go there, so qex cannot run such a line at all. To pass over
    // the line would hide the fault, and to submit the other lines would give a
    // fan-out that is not the file.
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            let at = e.utf8_error().valid_up_to();
            let line = e.as_bytes()[..at].iter().filter(|b| **b == b'\n').count() + 1;
            let name = if path == Path::new("-") {
                "the input from standard input".to_string()
            } else {
                format!("the input file {}", path.display())
            };
            bail!(
                "{name} is not UTF-8 text. The first incorrect byte is on line {line}.\n\n\
                 The command of a job is text, so qex cannot run that line, and qex \
                 submits no job at all.\n\n\
                 Correct line {line}, or write the file again with UTF-8."
            );
        }
    };
    Ok(parse(&text))
}

/// Counts the places where a line goes.
///
/// `{{}}` is the escape for a literal `{}`, and this function does not count
/// it. Nothing else in an argument changes.
pub fn count_placeholders(command: &[String]) -> usize {
    command.iter().map(|arg| scan(arg, None).1).sum()
}

/// Puts the text of one line into every place of the command.
///
/// Each argument gives exactly one argument. The line goes inside the argument
/// and never divides it, whatever the line holds: a space, a quotation mark, a
/// semicolon, a dollar sign or a newline.
pub fn substitute(command: &[String], line: &str) -> Vec<String> {
    command.iter().map(|arg| scan(arg, Some(line)).0).collect()
}

/// Reads one argument, and gives the new text and the number of places.
///
/// With `line` as `None` the text is not useful and the count is.
fn scan(arg: &str, line: Option<&str>) -> (String, usize) {
    let mut out = String::with_capacity(arg.len());
    let mut count = 0usize;
    let mut rest = arg;
    loop {
        let Some(at) = rest.find('{') else {
            out.push_str(rest);
            return (out, count);
        };
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        // The escape comes first. `{{}}` holds `{}`, and it is not a place.
        if let Some(next) = tail.strip_prefix("{{}}") {
            out.push_str(PLACEHOLDER);
            rest = next;
        } else if let Some(next) = tail.strip_prefix(PLACEHOLDER) {
            count += 1;
            if let Some(line) = line {
                out.push_str(line);
            }
            rest = next;
        } else {
            // A `{` with no `}` after it is ordinary text. A user writes one in
            // a format string, in JSON and in a shell brace, and qex must not
            // change it.
            out.push('{');
            rest = &tail[1..];
        }
    }
}

/// Tests the command that the user gave after `--`.
///
/// A command with no place for the line would give N jobs that are all the
/// same. That is never what a fan-out means, so qex refuses it. A user who
/// really wants N copies of one command writes a loop.
pub fn check_command(command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!(
            "no command.\n\n\
             Write the command after `--`, and put `{PLACEHOLDER}` where each line goes:\n\
             \x20   qex submit --each-line inputs.txt -- ./process {PLACEHOLDER}"
        );
    }
    if count_placeholders(command) == 0 {
        bail!(
            "the command holds no `{PLACEHOLDER}`, so every job would be the same command.\n\n\
             `--each-line` runs one job for each line, and `{PLACEHOLDER}` says where the \
             line goes. With no `{PLACEHOLDER}` the lines have no effect, and qex submits \
             nothing.\n\n\
             Put `{PLACEHOLDER}` in the command:\n\
             \x20   qex submit --each-line inputs.txt -- ./process {PLACEHOLDER}\n\n\
             `{PLACEHOLDER}` goes in any argument, in the program name, or inside an \
             argument such as `--out={PLACEHOLDER}.log`. Write `{{{{}}}}` for a literal \
             `{PLACEHOLDER}`."
        );
    }
    Ok(())
}

/// Tests the number of lines against the limit.
pub fn check_count(count: usize, max: usize) -> Result<()> {
    if count == 0 {
        bail!(
            "the input holds no line that gives a job, so qex submits nothing.\n\n\
             qex passes over an empty line and a line that starts with `#`. Add one \
             line with the input for a job."
        );
    }
    if count > max {
        bail!(
            "the input holds {}, and the limit is {}.\n\n\
             Each job takes a directory in the state of qex, so a very large fan-out \
             fills the disk and makes `qex list` hard to read.\n\n\
             Give fewer lines, or raise the limit with `--max-jobs {count}`.",
            crate::units::count_of(count, "line"),
            crate::units::count_of(max, "job")
        );
    }
    Ok(())
}

/// Makes the name that `qex list` shows for one job of the fan-out.
///
/// The name holds three parts: a base, the position in the file, and as much of
/// the line as fits. The position comes BEFORE the line, and it has the same
/// width for every job, so the names sort in the order of the file and two long
/// lines that start with the same text never give one name.
///
/// `index` counts from 1, and `count` is the number of jobs.
///
/// # Both parts of the name come from the file
///
/// The line is data of the file, and so is the BASE. `{}` can go in the program
/// position, which the documentation advertises, and the base is then the
/// program name of the first line.
///
/// A name goes to the terminal of the user, in the output of the submission and
/// in every later `qex list`. A line that holds a terminal control sequence
/// would move the cursor, change the colour, empty the screen or write the
/// title of the window, and rows of `qex list` would go away in front of the
/// reader. `slug` therefore cleans BOTH parts, and this function can give the
/// letters, the numbers, `.`, `_` and `-` only.
pub fn job_name(base: &str, index: usize, count: usize, line: &str) -> String {
    let width = count.to_string().len();
    let base = cut(&slug(base), MAX_BASE);
    let base = if base.is_empty() {
        "job"
    } else {
        base.as_str()
    };

    let mut name = format!("{base}-{index:0width$}");
    let slug = slug(line);
    if !slug.is_empty() {
        name.push('-');
        name.push_str(&cut(&slug, MAX_SLUG));
    }
    cut(&name, MAX_NAME)
}

/// Changes text into the part of a job name that a person reads.
///
/// A line is a path, a URL or a record, and it holds characters that make a
/// name hard to read and hard to type in `qex status <name>`. A line can also
/// hold a terminal control sequence, and a name goes to the terminal of the
/// user. This function keeps the letters, the numbers, `.` and `_`, and each
/// run of the other characters becomes one dash.
fn slug(line: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in line.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Cuts text to a number of characters, and never inside a character.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => text[..at].trim_end_matches('-').to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The line goes inside one argument, and it never divides it. This is the
    /// property that keeps a line of an input file from becoming a command.
    #[test]
    fn a_line_with_shell_characters_stays_one_argument() {
        let out = substitute(&args(&["./p", "{}"]), "a b\"; rm -rf ~; echo $HOME");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], "a b\"; rm -rf ~; echo $HOME");
    }

    /// A newline in a line of the file cannot arrive from `parse`, and a caller
    /// can still give one. It must not divide the argument either.
    #[test]
    fn a_line_with_a_newline_stays_one_argument() {
        let out = substitute(&args(&["./p", "{}"]), "one\ntwo");
        assert_eq!(out, vec!["./p".to_string(), "one\ntwo".to_string()]);
    }

    #[test]
    fn the_place_can_be_the_program_or_a_part_of_an_argument() {
        let out = substitute(&args(&["./{}.sh", "--out={}.log", "-v"]), "run");
        assert_eq!(out, args(&["./run.sh", "--out=run.log", "-v"]));
    }

    /// Every place takes the line, because a command frequently names the same
    /// input twice: an input file and an output file.
    #[test]
    fn every_place_takes_the_line() {
        let out = substitute(&args(&["cp", "{}", "{}.bak"]), "a.txt");
        assert_eq!(out, args(&["cp", "a.txt", "a.txt.bak"]));
        assert_eq!(count_placeholders(&args(&["cp", "{}", "{}.bak"])), 2);
    }

    /// `{{}}` gives a literal `{}`, and it is not a place for the line.
    #[test]
    fn the_escape_gives_a_literal_placeholder() {
        let command = args(&["jq", "{{}}", "{}"]);
        assert_eq!(count_placeholders(&command), 1);
        assert_eq!(
            substitute(&command, "a.json"),
            args(&["jq", "{}", "a.json"])
        );
    }

    /// A `{` with no `}` after it is ordinary text.
    #[test]
    fn a_brace_that_is_not_a_place_does_not_change() {
        let command = args(&["fmt", "--style={indent: 2}", "{}"]);
        assert_eq!(count_placeholders(&command), 1);
        assert_eq!(
            substitute(&command, "x"),
            args(&["fmt", "--style={indent: 2}", "x"])
        );
    }

    /// A command with no place would give N jobs that are all the same.
    #[test]
    fn a_command_with_no_place_is_refused() {
        let err = check_command(&args(&["./process", "input"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("holds no `{}`"), "got: {err}");
        assert!(err.contains("--each-line"), "the error must say what to do");
        // The escape alone is not a place.
        assert!(check_command(&args(&["echo", "{{}}"])).is_err());
        assert!(check_command(&args(&["echo", "{}"])).is_ok());
        assert!(check_command(&[]).is_err());
    }

    #[test]
    fn an_empty_line_and_a_comment_line_give_no_job() {
        let input = parse("one\n\n# a note\ntwo\n   \n#\nthree\n");
        assert_eq!(input.lines, vec!["one", "two", "three"]);
        assert_eq!(input.blank, 2);
        assert_eq!(input.comments, 2);
        assert_eq!(input.skipped(), 4);
    }

    /// A file from Windows, and a file with no final newline, must give the
    /// same jobs as an ordinary file.
    #[test]
    fn crlf_endings_and_a_missing_final_newline_give_the_same_lines() {
        assert_eq!(parse("a\r\nb\r\n").lines, vec!["a", "b"]);
        assert_eq!(parse("a\nb").lines, vec!["a", "b"]);
        assert_eq!(parse("a").lines, vec!["a"]);
        assert_eq!(parse("").lines, Vec::<String>::new());
    }

    /// A space at the end of a line is invisible, and it is an accident of the
    /// program that wrote the file.
    #[test]
    fn the_space_at_each_end_of_a_line_goes_away() {
        let input = parse("  a b  \n\tc\t\n");
        assert_eq!(input.lines, vec!["a b", "c"]);
    }

    /// The limit stops a fan-out that would fill the state directory, and the
    /// message says how to raise it.
    #[test]
    fn a_count_above_the_limit_is_refused() {
        assert!(check_count(10, 10).is_ok());
        let err = check_count(11, 10).unwrap_err().to_string();
        assert!(err.contains("--max-jobs 11"), "got: {err}");
        let err = check_count(0, 10).unwrap_err().to_string();
        assert!(err.contains("no line that gives a job"), "got: {err}");
    }

    /// The name must show the position and the line, and it must be different
    /// for two lines that start with the same long text.
    #[test]
    fn a_job_name_holds_the_position_and_the_line() {
        assert_eq!(job_name("process", 1, 9, "a.txt"), "process-1-a.txt");
        // The width of the position is the width of the count, so the names
        // sort in the order of the file.
        assert_eq!(job_name("process", 7, 120, "a.txt"), "process-007-a.txt");

        let long = "/data/very/long/path/that/goes/on/and/on/file-000.parquet";
        let one = job_name("p", 1, 100, long);
        let two = job_name("p", 2, 100, long);
        assert_ne!(one, two);
        assert!(one.len() <= MAX_NAME, "{one} is too long");
        assert!(one.starts_with("p-001-"), "got: {one}");
    }

    /// A job name must hold no terminal control character, in EITHER part.
    ///
    /// The line is data of the file, and so is the base: `{}` can go in the
    /// program position, and the base is then the program name of the first
    /// line. A name goes to the terminal of the user at the submission and in
    /// every later `qex list`. An escape sequence there changes the colour,
    /// moves the cursor, empties the screen or writes the window title, and
    /// rows of `qex list` go away in front of the reader.
    #[test]
    fn a_job_name_holds_no_control_character_from_the_file() {
        // The base and the line both hold escape sequences.
        let name = job_name(
            "\u{1b}[31mBOOM\u{1b}[0m",
            1,
            9,
            "\u{1b}[2J\u{1b}]0;title\u{7}x",
        );
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'),
            "the name holds a character that must not reach a terminal: {name:?}"
        );
        assert!(!name.contains('\u{1b}'), "got: {name:?}");

        // A name that a file made must never hold a newline either. A newline
        // would make one row of `qex list` into two.
        let name = job_name("a\nb", 2, 9, "c\r\nd");
        assert!(
            !name.contains('\n') && !name.contains('\r'),
            "got: {name:?}"
        );
    }

    /// A line with no letter and no number still gives a name, because the
    /// position is always there.
    #[test]
    fn a_line_with_no_ordinary_character_still_gives_a_name() {
        let name = job_name("p", 3, 10, "!!! ???");
        assert_eq!(name, "p-03");
        assert!(name.parse::<uuid::Uuid>().is_err());
    }

    /// A job name must not have the form of a job id, and the position keeps
    /// that true.
    #[test]
    fn a_job_name_never_has_the_form_of_an_id() {
        let name = job_name("x", 1, 1, "550e8400-e29b-41d4-a716-446655440000");
        assert!(name.parse::<uuid::Uuid>().is_err(), "got: {name}");
    }
}
