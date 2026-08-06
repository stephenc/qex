//! This module selects the part of a log file that a command shows.
//!
//! `qex logs` and `qex status` share these options, so the two commands always
//! behave in the same way.
//!
//! The reader is frequently an agent with a limited context. A job can write
//! hundreds of megabytes, so every path here has a limit, and every path says
//! how many lines it did not show. A reader must never believe that a part of
//! the output is the whole output.

use clap::Args;

/// The maximum number of lines that a command shows without an option.
pub const DEFAULT_LINES: usize = 500;

/// The maximum number of matches of `--grep` that a command shows.
///
/// A wide pattern can match every line. Without this limit, a search would
/// give the whole file to a reader who asked for a small part of it.
pub const DEFAULT_MATCHES: usize = 100;

/// The number of lines that `--follow` shows before it waits for more.
///
/// A job can already have written a large file when the command starts. Without
/// this limit, `--follow` would write that whole file first.
pub const FOLLOW_LEAD_LINES: usize = 20;

/// The number of lines that `qex status` shows for a job that failed.
///
/// This value is small on purpose. The command shows these lines with no option
/// at all, and the usual reader has a limited context.
pub const STATUS_LINES: usize = 20;

#[derive(Debug, Args, Clone, Default)]
pub struct LogSelect {
    /// Show the standard output only.
    #[arg(long, conflicts_with = "stderr", help_heading = "Log selection")]
    pub stdout: bool,

    /// Show the standard error only.
    #[arg(long, help_heading = "Log selection")]
    pub stderr: bool,

    /// Show the last N lines.
    #[arg(long, value_name = "N", help_heading = "Log selection")]
    pub tail: Option<usize>,

    /// Show the first N lines. Use this option for a fault at the start.
    #[arg(long, value_name = "N", help_heading = "Log selection")]
    pub head: Option<usize>,

    /// Show the lines from N to M. Example: --lines 4200:4230.
    #[arg(long, value_name = "N:M", help_heading = "Log selection")]
    pub lines: Option<String>,

    /// Show the lines that match this regular expression.
    #[arg(long, value_name = "REGEX", help_heading = "Log selection")]
    pub grep: Option<String>,

    /// Read the --grep value as plain text, and not as a regular expression.
    #[arg(long, help_heading = "Log selection")]
    pub fixed: bool,

    /// Show N lines before and after each match of --grep.
    #[arg(long, short = 'C', value_name = "N", help_heading = "Log selection")]
    pub context: Option<usize>,

    /// Write the line number before each line.
    #[arg(long, short = 'n', help_heading = "Log selection")]
    pub number: bool,

    /// The maximum number of matches of --grep to show. The default is 100.
    ///
    /// qex reports the number of matches that it did not show. This option has
    /// no meaning with --follow, which reads a stream and has no total.
    #[arg(long, value_name = "N", help_heading = "Log selection")]
    pub max_matches: Option<usize>,

    /// Show every line. Without this option, qex shows the last 500 lines.
    #[arg(long, help_heading = "Log selection")]
    pub all: bool,
}

/// The result of a selection.
#[derive(Debug)]
pub struct Selected {
    pub text: String,
    /// The number of lines that the selection did not show.
    pub hidden: usize,
    /// True when a limit cut the result.
    pub truncated: bool,
    /// The number of lines that matched `--grep`, before any limit.
    ///
    /// This number is useful by itself. A pattern with 3000 matches is too
    /// wide, and the reader learns that from this number.
    pub matches: Option<usize>,
    /// The number of matches that the output holds.
    pub matches_shown: usize,
}

impl Selected {
    /// Gives one line that says what the output does not hold.
    ///
    /// A reader must never believe that a part of the output is the whole
    /// output, so every command writes this line when something is missing.
    pub fn notice(&self) -> Option<String> {
        match (self.matches, self.truncated) {
            (Some(found), _) if found > self.matches_shown => Some(format!(
                "... {} line(s) match, and qex shows the first {}. \
                 Use `--max-matches N`, `--all`, or a narrower pattern.",
                found,
                self.matches_shown
            )),
            (Some(found), _) => Some(format!("... {found} line(s) match.")),
            (None, true) => Some(format!(
                "... {} earlier line(s) are not shown. Use `--all`, `--head N` or \
                 `--tail N`.",
                self.hidden
            )),
            (None, false) => None,
        }
    }
}

impl LogSelect {
    /// Tests the options against each other.
    ///
    /// `--follow` reads a stream, so an option that counts a total has no
    /// meaning with it.
    pub fn check_with_follow(&self) -> Result<(), String> {
        if self.max_matches.is_some() {
            return Err(
                "--max-matches has no meaning with --follow. A stream has no total, so qex \
                 cannot say how many matches come after a limit. Use --grep alone to filter \
                 the stream."
                    .to_string(),
            );
        }
        // `--tail N` DOES have a meaning with `--follow`: give the last N lines
        // that the job already wrote, then continue with the new lines. That is
        // the behaviour of `tail -f -n N`.
        for (name, given) in [
            ("--head", self.head.is_some()),
            ("--lines", self.lines.is_some()),
        ] {
            if given {
                return Err(format!(
                    "{name} has no meaning with --follow, because the output continues. \
                     Use --grep to filter the stream."
                ));
            }
        }
        Ok(())
    }

    /// Tests if the user gave any option that selects lines.
    pub fn is_explicit(&self) -> bool {
        self.tail.is_some()
            || self.head.is_some()
            || self.lines.is_some()
            || self.grep.is_some()
            || self.all
    }

    /// Chooses the part of the text to show.
    ///
    /// `default_limit` is the number of last lines to show when the user gives
    /// no option.
    pub fn apply(&self, text: &str, default_limit: usize) -> Result<Selected, String> {
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        // A search comes first. The other options then act on the matches.
        let (chosen, match_count, matches_shown) = match &self.grep {
            Some(pattern) => {
                let (lines_to_keep, found, shown) = self.search(&lines, pattern)?;
                (lines_to_keep, Some(found), shown)
            }
            None => ((0..total).collect(), None, 0),
        };

        // Choose a range of the chosen lines.
        //
        // Count the lines that each option leaves out. A reader must never
        // believe that a part of the output is the whole output.
        let available = chosen.len();
        let (picked, hidden): (Vec<usize>, usize) = if let Some(range) = &self.lines {
            let (from, to) = parse_range(range)?;
            // A person counts from 1.
            let kept: Vec<usize> = chosen
                .into_iter()
                .filter(|i| *i + 1 >= from && *i + 1 <= to)
                .collect();
            let hidden = available.saturating_sub(kept.len());
            (kept, hidden)
        } else if let Some(n) = self.head {
            let kept: Vec<usize> = chosen.into_iter().take(n).collect();
            let hidden = available.saturating_sub(kept.len());
            (kept, hidden)
        } else if let Some(n) = self.tail {
            let start = available.saturating_sub(n);
            (chosen[start..].to_vec(), start)
        } else if self.all {
            (chosen, 0)
        } else {
            // No option. Show the last lines, and never the whole file.
            let start = available.saturating_sub(default_limit);
            (chosen[start..].to_vec(), start)
        };

        let mut out = String::new();
        let mut previous: Option<usize> = None;
        for i in picked.iter().copied() {
            // Show a mark where the selection leaves a gap, so a reader does not
            // read two lines as if they were together.
            if let Some(p) = previous {
                if i > p + 1 {
                    out.push_str("--\n");
                }
            }
            if self.number {
                out.push_str(&format!("{:>6}  ", i + 1));
            }
            out.push_str(lines[i]);
            out.push('\n');
            previous = Some(i);
        }

        Ok(Selected {
            text: out,
            hidden,
            truncated: hidden > 0,
            matches: match_count,
            matches_shown,
        })
    }

    /// Gives the numbers of the lines to show, with the context lines.
    ///
    /// The result also gives the number of lines that matched before the limit,
    /// and the number of matches in the output. A pattern that matches 3000
    /// lines is too wide, and the reader must learn that from the output.
    fn search(&self, lines: &[&str], pattern: &str) -> Result<(Vec<usize>, usize, usize), String> {
        let matcher = Matcher::new(pattern, self.fixed)?;
        let context = self.context.unwrap_or(0);

        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| matcher.matches(line))
            .map(|(i, _)| i)
            .collect();
        let found = hits.len();

        // Keep the FIRST matches. In a log, the first failure is usually the
        // cause, and the failures after it are the result of that cause.
        let limit = if self.all {
            found
        } else {
            self.max_matches.unwrap_or(DEFAULT_MATCHES)
        };
        let kept: Vec<usize> = hits.into_iter().take(limit).collect();
        let shown = kept.len();

        let mut keep = vec![false; lines.len()];
        for i in kept {
            let from = i.saturating_sub(context);
            let to = (i + context).min(lines.len().saturating_sub(1));
            for item in keep.iter_mut().take(to + 1).skip(from) {
                *item = true;
            }
        }

        Ok((
            keep.iter()
                .enumerate()
                .filter(|(_, k)| **k)
                .map(|(i, _)| i)
                .collect(),
            found,
            shown,
        ))
    }
}

/// Reads a range such as `4200:4230`.
fn parse_range(s: &str) -> Result<(usize, usize), String> {
    let (a, b) = s.split_once(':').ok_or_else(|| {
        format!("incorrect range `{s}`. Use the form N:M, for example --lines 100:200.")
    })?;

    let from: usize = a
        .trim()
        .parse()
        .map_err(|_| format!("incorrect first line `{a}` in `{s}`"))?;
    let to: usize = b
        .trim()
        .parse()
        .map_err(|_| format!("incorrect last line `{b}` in `{s}`"))?;

    if from == 0 || to == 0 {
        return Err("a line number starts at 1".to_string());
    }
    if from > to {
        return Err(format!(
            "the first line {from} is after the last line {to} in `{s}`"
        ));
    }
    Ok((from, to))
}

/// Tests one line against a pattern.
///
/// qex has no regular expression library, so this type reads a small language:
/// `.`, `*`, `+`, `?`, `[abc]`, `^`, `$` and `|`. A pattern that uses more than
/// this gives a clear error, and `--fixed` accepts any text.
struct Matcher {
    alternatives: Vec<Vec<Token>>,
    anchored_start: bool,
    anchored_end: bool,
    fixed: Option<String>,
}

#[derive(Debug, Clone)]
enum Token {
    Char(char),
    Any,
    Class { chars: Vec<char>, negated: bool },
    Star(Box<Token>),
    Plus(Box<Token>),
    Optional(Box<Token>),
}

impl Matcher {
    fn new(pattern: &str, fixed: bool) -> Result<Self, String> {
        if fixed {
            return Ok(Self {
                alternatives: Vec::new(),
                anchored_start: false,
                anchored_end: false,
                fixed: Some(pattern.to_string()),
            });
        }

        let mut alternatives = Vec::new();
        let mut anchored_start = false;
        let mut anchored_end = false;

        for part in pattern.split('|') {
            let mut body = part;
            if let Some(rest) = body.strip_prefix('^') {
                anchored_start = true;
                body = rest;
            }
            if let Some(rest) = body.strip_suffix('$') {
                anchored_end = true;
                body = rest;
            }
            alternatives.push(parse_tokens(body)?);
        }

        Ok(Self {
            alternatives,
            anchored_start,
            anchored_end,
            fixed: None,
        })
    }

    fn matches(&self, line: &str) -> bool {
        if let Some(text) = &self.fixed {
            return line.contains(text.as_str());
        }

        let chars: Vec<char> = line.chars().collect();
        for tokens in &self.alternatives {
            let starts: Vec<usize> = if self.anchored_start {
                vec![0]
            } else {
                (0..=chars.len()).collect()
            };
            for start in starts {
                if let Some(end) = match_here(tokens, &chars, start) {
                    if !self.anchored_end || end == chars.len() {
                        return true;
                    }
                }
            }
        }
        false
    }
}

fn parse_tokens(pattern: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let token = match chars[i] {
            '.' => Token::Any,
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    return Err("the pattern ends with a backslash".to_string());
                }
                Token::Char(chars[i])
            }
            '[' => {
                let close = chars[i..]
                    .iter()
                    .position(|c| *c == ']')
                    .ok_or_else(|| format!("the group that starts at `[` has no `]` in `{pattern}`"))?
                    + i;
                let mut set: Vec<char> = chars[i + 1..close].to_vec();
                let negated = set.first() == Some(&'^');
                if negated {
                    set.remove(0);
                }
                // Read a range such as a-z.
                let mut expanded = Vec::new();
                let mut k = 0;
                while k < set.len() {
                    if k + 2 < set.len() && set[k + 1] == '-' {
                        for c in set[k]..=set[k + 2] {
                            expanded.push(c);
                        }
                        k += 3;
                    } else {
                        expanded.push(set[k]);
                        k += 1;
                    }
                }
                i = close;
                Token::Class {
                    chars: expanded,
                    negated,
                }
            }
            '*' | '+' | '?' => {
                let previous = tokens
                    .pop()
                    .ok_or_else(|| format!("`{}` has nothing before it in `{pattern}`", chars[i]))?;
                let wrapped = match chars[i] {
                    '*' => Token::Star(Box::new(previous)),
                    '+' => Token::Plus(Box::new(previous)),
                    _ => Token::Optional(Box::new(previous)),
                };
                tokens.push(wrapped);
                i += 1;
                continue;
            }
            '(' | ')' => {
                return Err(format!(
                    "this search does not read a group `(` or `)` in `{pattern}`. \
                     Use `--fixed` for plain text, or use `|` for alternatives."
                ))
            }
            c => Token::Char(c),
        };
        tokens.push(token);
        i += 1;
    }
    Ok(tokens)
}

/// Tests the tokens against the text from one position.
fn match_here(tokens: &[Token], chars: &[char], position: usize) -> Option<usize> {
    let Some((first, rest)) = tokens.split_first() else {
        return Some(position);
    };

    match first {
        Token::Star(inner) => {
            // Try the longest match first, then give characters back.
            let mut ends = vec![position];
            let mut p = position;
            while let Some(next) = match_one(inner, chars, p) {
                ends.push(next);
                p = next;
            }
            for end in ends.into_iter().rev() {
                if let Some(done) = match_here(rest, chars, end) {
                    return Some(done);
                }
            }
            None
        }
        Token::Plus(inner) => {
            let first_end = match_one(inner, chars, position)?;
            let mut ends = vec![first_end];
            let mut p = first_end;
            while let Some(next) = match_one(inner, chars, p) {
                ends.push(next);
                p = next;
            }
            for end in ends.into_iter().rev() {
                if let Some(done) = match_here(rest, chars, end) {
                    return Some(done);
                }
            }
            None
        }
        Token::Optional(inner) => {
            if let Some(next) = match_one(inner, chars, position) {
                if let Some(done) = match_here(rest, chars, next) {
                    return Some(done);
                }
            }
            match_here(rest, chars, position)
        }
        simple => {
            let next = match_one(simple, chars, position)?;
            match_here(rest, chars, next)
        }
    }
}

fn match_one(token: &Token, chars: &[char], position: usize) -> Option<usize> {
    if position >= chars.len() {
        return None;
    }
    let c = chars[position];
    let ok = match token {
        Token::Char(want) => c == *want,
        Token::Any => true,
        Token::Class { chars: set, negated } => set.contains(&c) != *negated,
        // A repeat inside a repeat is not necessary here.
        _ => return None,
    };
    ok.then_some(position + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel() -> LogSelect {
        LogSelect::default()
    }

    fn text() -> String {
        (1..=1000)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn with_no_option_the_last_lines_come_back() {
        let out = sel().apply(&text(), 500).unwrap();
        assert_eq!(out.text.lines().count(), 500);
        assert!(out.text.contains("line-1000"));
        assert!(!out.text.contains("line-1\n"), "the first line must be hidden");
        assert!(out.truncated);
        assert_eq!(out.hidden, 500);
    }

    #[test]
    fn head_gives_the_start_and_tail_gives_the_end() {
        let mut s = sel();
        s.head = Some(3);
        let out = s.apply(&text(), 500).unwrap();
        assert_eq!(out.text.lines().next().unwrap(), "line-1");
        assert_eq!(out.text.lines().count(), 3);

        let mut s = sel();
        s.tail = Some(3);
        let out = s.apply(&text(), 500).unwrap();
        assert_eq!(out.text.lines().last().unwrap(), "line-1000");
        assert_eq!(out.text.lines().count(), 3);
    }

    #[test]
    fn a_range_gives_the_lines_between_two_numbers() {
        let mut s = sel();
        s.lines = Some("10:12".into());
        let out = s.apply(&text(), 500).unwrap();
        assert_eq!(out.text, "line-10\nline-11\nline-12\n");
    }

    #[test]
    fn a_range_with_an_incorrect_form_gives_a_clear_error() {
        let mut s = sel();
        s.lines = Some("10".into());
        assert!(s.apply(&text(), 500).unwrap_err().contains("N:M"));

        s.lines = Some("50:10".into());
        assert!(s.apply(&text(), 500).unwrap_err().contains("after"));

        s.lines = Some("0:5".into());
        assert!(s.apply(&text(), 500).unwrap_err().contains("starts at 1"));
    }

    #[test]
    fn all_gives_every_line() {
        let mut s = sel();
        s.all = true;
        let out = s.apply(&text(), 500).unwrap();
        assert_eq!(out.text.lines().count(), 1000);
        assert!(!out.truncated);
    }

    #[test]
    fn the_line_numbers_appear_with_the_option() {
        let mut s = sel();
        s.tail = Some(1);
        s.number = true;
        let out = s.apply(&text(), 500).unwrap();
        assert!(out.text.contains("1000"), "got: {}", out.text);
    }

    #[test]
    fn a_search_gives_the_lines_that_match() {
        let body = "alpha ok\nbeta FAIL here\ngamma ok\ndelta FAIL too\n";
        let mut s = sel();
        s.grep = Some("FAIL".into());
        let out = s.apply(body, 500).unwrap();
        // The output holds a mark where the selection leaves a gap, so count
        // the log lines only.
        let shown: Vec<&str> = out.text.lines().filter(|l| *l != "--").collect();
        assert_eq!(shown, vec!["beta FAIL here", "delta FAIL too"]);
        assert_eq!(out.matches, Some(2));
    }

    /// A match alone is frequently not enough. The lines after a failure hold
    /// the reason for it.
    #[test]
    fn the_context_option_gives_the_lines_around_a_match() {
        let body = "a\nb\nFAIL\nc\nd\ne\n";
        let mut s = sel();
        s.grep = Some("FAIL".into());
        s.context = Some(1);
        let out = s.apply(body, 500).unwrap();
        assert_eq!(out.text, "b\nFAIL\nc\n");
    }

    /// A gap in the selection must be visible, or a reader joins two lines that
    /// are far apart.
    #[test]
    fn a_gap_between_matches_is_marked() {
        let body = "FAIL one\nx\nx\nx\nx\nFAIL two\n";
        let mut s = sel();
        s.grep = Some("FAIL".into());
        let out = s.apply(body, 500).unwrap();
        assert!(out.text.contains("--\n"), "got: {}", out.text);
    }

    #[test]
    fn the_search_reads_a_small_pattern_language() {
        let body = "error: one\nwarning: two\nERROR: three\nnote: four\n";

        let mut s = sel();
        s.grep = Some("error|warning".into());
        assert_eq!(s.apply(body, 500).unwrap().text.lines().count(), 2);

        let mut s = sel();
        s.grep = Some("^note".into());
        assert_eq!(s.apply(body, 500).unwrap().text.trim(), "note: four");

        let mut s = sel();
        s.grep = Some("e[rn]ror".into());
        assert_eq!(s.apply(body, 500).unwrap().text.lines().count(), 1);

        let mut s = sel();
        s.grep = Some("t.*ee$".into());
        assert_eq!(s.apply(body, 500).unwrap().text.trim(), "ERROR: three");
    }

    #[test]
    fn the_fixed_option_reads_the_value_as_plain_text() {
        let body = "cost is 3.50\ncost is 3x50\n";
        let mut s = sel();
        s.grep = Some("3.50".into());
        // As a pattern, the dot matches any character.
        assert_eq!(s.apply(body, 500).unwrap().text.lines().count(), 2);

        s.fixed = true;
        assert_eq!(s.apply(body, 500).unwrap().text.trim(), "cost is 3.50");
    }

    /// A pattern that this search cannot read must give a clear error, and it
    /// must name the option that always operates.
    #[test]
    fn an_unsupported_pattern_names_the_alternative() {
        let mut s = sel();
        s.grep = Some("(a|b)c".into());
        let err = s.apply("abc", 500).unwrap_err();
        assert!(err.contains("--fixed"), "got: {err}");
    }

    /// A search that matches every line must not give the whole file to a
    /// reader who gave no other option, and it must say how many lines matched.
    #[test]
    fn a_search_that_matches_everything_still_has_a_limit() {
        let mut s = sel();
        s.grep = Some("line".into());
        let out = s.apply(&text(), 500).unwrap();

        assert_eq!(out.matches, Some(1000), "the total must be the true total");
        assert_eq!(out.matches_shown, DEFAULT_MATCHES);
        assert_eq!(out.text.lines().count(), DEFAULT_MATCHES);
    }

    /// The limit keeps the FIRST matches. In a log, the first failure is
    /// usually the cause, and the later failures are the result of it.
    #[test]
    fn the_match_limit_keeps_the_first_matches() {
        let mut s = sel();
        s.grep = Some("line".into());
        s.max_matches = Some(3);
        let out = s.apply(&text(), 500).unwrap();
        assert_eq!(out.matches, Some(1000));
        assert_eq!(out.matches_shown, 3);
        assert_eq!(out.text, "line-1\nline-2\nline-3\n");
    }

    /// The context lines multiply the output, so the limit counts the matches
    /// and not the lines. Forty matches with three context lines are 280 lines.
    #[test]
    fn the_limit_counts_the_matches_and_not_the_lines() {
        let body: String = (1..=100)
            .map(|i| format!("pad-{i}\nFAIL-{i}\npad2-{i}\n"))
            .collect();
        let mut s = sel();
        s.grep = Some("FAIL".into());
        s.context = Some(1);
        s.max_matches = Some(5);
        let out = s.apply(&body, 500).unwrap();
        assert_eq!(out.matches, Some(100));
        assert_eq!(out.matches_shown, 5);
        // Five matches, and one line before and after each one.
        assert_eq!(out.text.lines().filter(|l| l.starts_with("FAIL")).count(), 5);
    }

    /// The option `--all` removes the match limit as well.
    #[test]
    fn all_removes_the_match_limit() {
        let mut s = sel();
        s.grep = Some("line".into());
        s.all = true;
        let out = s.apply(&text(), 500).unwrap();
        assert_eq!(out.matches, Some(1000));
        assert_eq!(out.matches_shown, 1000);
    }
}
