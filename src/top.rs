//! This module holds the `qex top` command.
//!
//! The command shows the queue and refreshes it. For each job it shows the
//! claim and the true use at this moment, so a person can see immediately that
//! a claim is much larger than the need.
//!
//! The command uses simple terminal codes and no library. It clears the screen
//! and writes the page again for each refresh. The page that a person watches
//! fits the screen: the list scrolls, and a selection names the job that a
//! key will act on.
//!
//! `--once` is a query. It writes every job that this page names, and it does
//! not wait for a key.

use crate::client::Client;
use crate::job::{JobState, JobStatus};
use crate::keys::Key;
use crate::proto::{Request, Response};
use crate::sys;
use crate::units::{format_duration, format_size};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Moves the cursor to the corner and clears the screen.
const CLEAR: &str = "\x1b[2J\x1b[H";

/// The first signal that `x` sends, and the wait before KILL. The same values
/// as `qex kill` with no options.
const STOP_SIGNAL: i32 = 15;
const STOP_GRACE_SECS: u64 = 10;

/// One measurement of a job, to calculate the CPU use between two refreshes.
struct Previous {
    cpu_secs: f64,
    at: Instant,
}

/// How to draw one page.
struct PageHow<'a> {
    update_cpu: bool,
    unreachable: bool,
    live: Option<&'a mut View>,
    size: Option<(usize, usize)>,
}

/// The live state of the page that a person watches.
struct View {
    selected: Option<uuid::Uuid>,
    scroll: usize,
    /// How many job lines the last page could hold.
    body_rows: usize,
    show_info: bool,
    show_tail: bool,
    prompt: Option<Prompt>,
    message: Option<String>,
    dirty: bool,
    need_refresh: bool,
}

enum Prompt {
    Stop(uuid::Uuid),
    LeaveQueue(uuid::Uuid),
    Clean(uuid::Uuid),
}

impl View {
    fn new() -> Self {
        Self {
            selected: None,
            scroll: 0,
            body_rows: 12,
            show_info: false,
            show_tail: false,
            prompt: None,
            message: None,
            dirty: false,
            need_refresh: false,
        }
    }
}

pub fn run(args: crate::cli::TopArgs) -> Result<i32> {
    if args.no_color {
        crate::style::turn_off();
    }
    let interval = Duration::from_secs_f64(args.interval.max(0.2));
    let mut previous: HashMap<uuid::Uuid, Previous> = HashMap::new();
    // Say the cause ONE TIME. The page refreshes every second, and a line for
    // each refresh would push the page off the screen of the reader.
    let mut said_the_cause = false;

    // Read the keys, so the person can move and act. This step also puts the
    // terminal back when a signal stops the process.
    let keys = if args.once {
        false
    } else {
        crate::keys::watch()
    };
    let mut view = View::new();
    let mut ordered: Vec<JobStatus> = Vec::new();

    loop {
        if keys && apply_keys(&mut view, &ordered) {
            return Ok(leave());
        }

        // Read the queue from the coordinator when one operates, and from the
        // state directory when none does.
        //
        // This command never starts a coordinator. A command that watches must
        // not change the thing that it watches. It must also give an answer
        // when no coordinator operates: the supervisor of each job writes its
        // own record, so those records hold the truth at every moment, and a
        // job that operates can still be measured by its process group.
        // NO COORDINATOR IS AN ANSWER. A coordinator that does not answer is
        // not. The first is the true state of a machine with an empty queue,
        // and the records on the disk describe it. The second means that qex
        // could not read the state at all, and the page then shows the disk
        // alone while a coordinator holds jobs that the page does not name.
        let mut reached = true;
        let (jobs, info) = match Client::connect_existing_result() {
            Ok(Some(mut client)) => match read_the_queue(&mut client) {
                Ok(answer) => answer,
                Err(e) => {
                    say_once(&mut said_the_cause, &e);
                    reached = false;
                    (crate::job::read_all_from_disk(), None)
                }
            },
            Ok(None) => (crate::job::read_all_from_disk(), None),
            Err(e) => {
                say_once(&mut said_the_cause, &e);
                reached = false;
                (crate::job::read_all_from_disk(), None)
            }
        };

        if args.once {
            // The CPU column is the change in the CPU time between two
            // measurements, so one page needs two of them. Take the first
            // measurement, wait a short time, then write the page.
            render(&jobs, info.as_ref(), &mut previous, !reached);
            std::thread::sleep(Duration::from_millis(400));
            print!("{}", render(&jobs, info.as_ref(), &mut previous, !reached));
            // THE TWO FORMS MAKE TWO PROMISES, AND THEY ARE NOT THE SAME PAGE.
            //
            // The form that a person watches is a DISPLAY: its promise is to
            // keep drawing in every state, and a display that drew succeeded.
            // It fits the screen and it holds a selection. `--once` is a
            // QUERY, and an agent scripts it. It writes every job that this
            // page names, so a script does not lose a job that did not fit.
            // The code 0 from a query says that qex answered the question, so
            // a page that qex could not fill must not carry it: an agent then
            // reads an empty page as the state of the machine and acts on it,
            // and a false success is worse than a wait, because nobody sees it.
            return Ok(if reached {
                0
            } else {
                crate::commands::EXIT_TIMEOUT
            });
        }

        let hidden;
        (ordered, hidden) = arrange(&jobs);
        let page = paint(
            &ordered,
            hidden,
            info.as_ref(),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: !reached,
                live: Some(&mut view),
                size: sys::terminal_size(),
            },
        );

        show_page(&page);

        // Sleep in short steps, so a key moves the selection at once and not
        // after the whole time between two refreshes.
        let until = Instant::now() + interval;
        while Instant::now() < until {
            if keys && apply_keys(&mut view, &ordered) {
                return Ok(leave());
            }
            if view.need_refresh {
                view.need_refresh = false;
                break;
            }
            if view.dirty {
                view.dirty = false;
                let page = paint(
                    &ordered,
                    hidden,
                    info.as_ref(),
                    &mut previous,
                    PageHow {
                        update_cpu: false,
                        unreachable: !reached,
                        live: Some(&mut view),
                        size: sys::terminal_size(),
                    },
                );
                show_page(&page);
            }
            std::thread::sleep(Duration::from_millis(50).min(interval));
        }
    }
}

fn render(
    jobs: &[JobStatus],
    info: Option<&Response>,
    previous: &mut HashMap<uuid::Uuid, Previous>,
    unreachable: bool,
) -> String {
    let (ordered, hidden) = arrange(jobs);
    paint(
        &ordered,
        hidden,
        info,
        previous,
        PageHow {
            update_cpu: true,
            unreachable,
            live: None,
            size: None,
        },
    )
}

fn paint(
    ordered: &[JobStatus],
    hidden: usize,
    info: Option<&Response>,
    previous: &mut HashMap<uuid::Uuid, Previous>,
    how: PageHow<'_>,
) -> String {
    let PageHow {
        update_cpu,
        unreachable,
        live,
        size,
    } = how;
    let header = header_lines(ordered, info, unreachable);
    let heading = crate::style::heading(&format!(
        "{:<8}  {:<9}  {:<14}  {:>9}  {:>7}  {:>17}  {:>7}  {:>6}  {}",
        "ID",
        "STATE",
        "NAME",
        "CPU CLAIM",
        "CPU NOW",
        "MEMORY CLAIM/NOW",
        "RUNTIME",
        "SINCE",
        "NOTE"
    ));

    let mut lines: Vec<(uuid::Uuid, String)> = Vec::with_capacity(ordered.len());
    for job in ordered {
        lines.push((job.id, job_line(job, previous, update_cpu)));
    }

    let Some(view) = live else {
        return paint_once(&header, &heading, &lines, hidden);
    };

    let (rows, cols) = size.unwrap_or((24, 80));
    let width = cols.max(40);
    let ids: Vec<uuid::Uuid> = lines.iter().map(|l| l.0).collect();
    // The selection may still be empty on the first page. The info pane then
    // takes the first job, so reserve that height before the list is clipped.
    let preview = view.selected.or(ids.first().copied());
    let selected_job = preview.and_then(|id| ordered.iter().find(|j| j.id == id));
    let extra = usize::from(view.message.is_some() && view.prompt.is_none());
    let inner = width.saturating_sub(2).max(1);
    // Top bar, header, jobs bar, heading, optional extra, bottom bar.
    let chrome = 1 + header.len() + 1 + 1 + extra + 1;
    let remaining = rows.saturating_sub(chrome);
    let mut detail = detail_lines(view, selected_job, rows, remaining, inner);
    // One job row and the detail bar must still fit. A long note that wraps
    // past that budget would push the header off the screen.
    let max_detail = remaining.saturating_sub(2);
    if max_detail == 0 {
        detail.clear();
    } else if detail.len() > max_detail {
        detail.truncate(max_detail);
    }
    let detail_n = if detail.is_empty() {
        0
    } else {
        1 + detail.len()
    };
    view.body_rows = remaining.saturating_sub(detail_n);
    sync_view(view, &ids, view.body_rows);

    let selected = view.selected;
    let selected_job = selected.and_then(|id| ordered.iter().find(|j| j.id == id));
    let above = view.scroll;
    let below = lines.len().saturating_sub(view.scroll + view.body_rows);
    let mut jobs_title = "jobs".to_string();
    if hidden > 0 {
        jobs_title = format!("jobs · {hidden} stopped not shown");
    }
    let jobs_extra = match (above, below) {
        (0, 0) => String::new(),
        (a, b) => format!("{a} above, {b} below"),
    };

    let mut out = String::new();
    out.push_str(&pane_bar(
        '┌',
        '┐',
        "qex",
        &sys::clock_text(sys::now_secs()),
        width,
    ));
    for line in &header {
        out.push_str(&pane_row(line, width, false));
    }
    out.push_str(&pane_bar('├', '┤', &jobs_title, &jobs_extra, width));
    out.push_str(&pane_row(&format!(" {heading}"), width, false));
    let mut drawn = 0;
    if lines.is_empty() {
        if view.body_rows > 0 {
            out.push_str(&pane_row("no jobs", width, false));
            drawn = 1;
        }
    } else {
        let end = (view.scroll + view.body_rows).min(lines.len());
        for (id, styled) in &lines[view.scroll..end] {
            let marked = highlight(Some(*id) == selected, styled);
            out.push_str(&pane_row(&marked, width, Some(*id) == selected));
            drawn += 1;
        }
    }
    // Empty job rows pin the detail pane and the command bar to the bottom
    // of the screen.
    while drawn < view.body_rows {
        out.push_str(&pane_row("", width, false));
        drawn += 1;
    }
    if !detail.is_empty() {
        let title = if view.show_tail { "tail" } else { "info" };
        out.push_str(&pane_bar('├', '┤', title, "", width));
        for line in &detail {
            out.push_str(&pane_row(line, width, false));
        }
    }
    if view.prompt.is_none() {
        if let Some(message) = &view.message {
            out.push_str(&pane_row(message, width, false));
        }
    }
    // A y/n prompt blocks every other action. Replace the command bar so
    // the keys that do not operate are not on the screen.
    let keys = match prompt_keys(view, selected_job) {
        Some(text) => text,
        None => action_keys(selected_job),
    };
    out.push_str(&command_bar(&keys, width));
    // No newline after the last row. A page of N lines plus a newline on a
    // terminal of N rows scrolls the header off the screen.
    if out.ends_with('\n') {
        out.pop();
    }
    clip_to_rows(&out, rows)
}

fn clip_to_rows(page: &str, rows: usize) -> String {
    let mut lines: Vec<&str> = page.lines().collect();
    if lines.len() <= rows {
        return page.to_string();
    }
    lines.truncate(rows);
    lines.join("\n")
}

fn leave() -> i32 {
    crate::keys::restore();
    println!();
    0
}

/// Writes the live page and parks the cursor on the last row.
///
/// The cursor must not sit after a newline on the last row. That wrap is
/// what pushes the top pane off the display.
fn show_page(page: &str) {
    use std::io::Write;
    let rows = sys::terminal_size().map(|(r, _)| r).unwrap_or(24);
    print!("{CLEAR}{page}\x1b[{rows};1H");
    std::io::stdout().flush().ok();
}

fn paint_once(
    header: &[String],
    heading: &str,
    lines: &[(uuid::Uuid, String)],
    hidden: usize,
) -> String {
    let mut out = String::new();
    for line in header {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&sys::clock_text(sys::now_secs()));
    out.push('\n');
    out.push('\n');
    out.push_str(heading);
    out.push('\n');
    if lines.is_empty() {
        out.push_str("\nno jobs\n");
    } else {
        for (_, line) in lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    if hidden > 0 {
        out.push_str(&format!(
            "\n{hidden} more job(s) that stopped are not shown. Use `qex list`.\n"
        ));
    }
    out.push_str(&crate::style::faint(
        "\nCPU NOW gives cores in use. SINCE gives the time since the job was queued, \
         started or stopped.",
    ));
    out.push('\n');
    out
}

fn header_lines(ordered: &[JobStatus], info: Option<&Response>, unreachable: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(Response::Info {
        version,
        program_replaced,
        cpu_budget,
        mem_budget,
        cpu_claimed,
        mem_claimed,
        jobs_running,
        jobs_queued,
        queue_state,
        paused_at,
        paused_by_pid,
        paused_reason,
        paused_until,
        paused_locks,
        config_error,
        ..
    }) = info
    {
        if let Some(fault) = config_error {
            lines.push(crate::style::warning(&coordinator_config_line(fault)));
        }
        lines.push(format!(
            "budget {cpu_claimed}/{cpu_budget} cores, {}/{} memory   \
             {jobs_running} running, {jobs_queued} queued",
            format_size(*mem_claimed),
            format_size(*mem_budget),
        ));
        let mine = crate::version::VERSION;
        if version != mine {
            lines.push(crate::style::warning(&format!(
                "WARNING: the coordinator is version {version} and this command is {mine}"
            )));
        } else {
            lines.push(crate::style::faint(&format!("version {version}")));
        }
        if *program_replaced {
            lines.push(
                "the qex program changed; this coordinator stops when no job operates".into(),
            );
        }
        let now = sys::now_secs();
        let fault = queue_state.as_deref() == Some("paused-by-fault");
        if let (true, Some(at)) = (
            matches!(
                queue_state.as_deref(),
                Some("paused") | Some("paused-by-fault")
            ),
            paused_at,
        ) {
            let record = crate::pause::PauseRecord {
                paused_at: *at,
                by_pid: paused_by_pid.unwrap_or(0),
                reason: paused_reason.clone(),
                until: *paused_until,
                fault,
            };
            lines.push(crate::style::warning(&format!(
                "QUEUE PAUSED: qex starts no job. {}",
                crate::pause::queue_line(&record, now)
            )));
        }
        for lock in paused_locks.iter().flatten() {
            lines.push(crate::style::warning(&crate::pause::lock_line(
                &lock.name,
                &lock.record,
                lock.held_by.as_deref(),
                now,
            )));
        }
        if let Some(info) = info {
            let line = crate::commands::queue_line(info);
            if !line.is_empty() {
                lines.push(line);
            }
        }
    }

    if info.is_none() {
        let (cfg, fault) = config_for_header();
        if let Some(fault) = fault {
            lines.push(crate::style::warning(&fault));
        }
        let active: Vec<&JobStatus> = ordered.iter().filter(|j| j.state.is_active()).collect();
        let queued = ordered
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .count();
        let cpu: u64 = active.iter().map(|j| j.cpu).sum();
        let mem: u64 = active.iter().map(|j| j.mem).sum();
        lines.push(format!(
            "budget {cpu}/{} cores, {}/{} memory   {} running, {queued} queued",
            cfg.budget_cpu().unwrap_or(0),
            format_size(mem),
            format_size(cfg.budget_mem().unwrap_or(0)),
            active.len(),
        ));
        if unreachable {
            lines.push(
                "qex has no answer from a coordinator. These records come from the state \
                 directory, and a coordinator can hold jobs that this page does not name."
                    .into(),
            );
        } else {
            lines.push(
                "no coordinator operates. These records come from the state directory.".into(),
            );
            lines.push("qex starts a coordinator when you submit a job.".into());
        }
    }

    lines.push(format!(
        "machine  {} cores, {} free of {}",
        sys::cpu_count(),
        format_size(sys::available_memory()),
        format_size(sys::total_memory()),
    ));
    lines
}

/// A load fault must come back as a string, so the header can name the file.
/// The budget line otherwise looks like a number the user chose.
fn config_for_header() -> (crate::config::Config, Option<String>) {
    match crate::config::Config::load_short() {
        Ok(cfg) => (cfg, None),
        Err(_) => (
            crate::config::Config::default(),
            Some(format!(
                "WARNING: {} unreadable; this page uses the default values",
                config_file_label()
            )),
        ),
    }
}

/// The file name, not the path. The live page clips a long path from the right.
fn config_file_label() -> String {
    crate::paths::config_file()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "qex.toml".to_string())
}

/// `config_error` is a wait as well as a fault. A wait must not read as
/// "the file is unreadable", because the coordinator just accepted a save.
fn coordinator_config_line(error: &str) -> String {
    let file = config_file_label();
    if error == crate::daemon::WAITING_FOR_A_WRITER {
        format!("WARNING: {file}: waiting for a writer; keeping prior values")
    } else {
        format!("WARNING: {file} unreadable; coordinator keeps its values")
    }
}

fn pane_bar(left: char, right: char, title: &str, extra: &str, width: usize) -> String {
    let inner = width.saturating_sub(2).max(1);
    let mut core = format!("─ {title} ");
    if !extra.is_empty() {
        let tail = format!(" {extra} ─");
        let fill = inner.saturating_sub(visible_len(&core) + visible_len(&tail));
        core.push_str(&"─".repeat(fill));
        core.push_str(&tail);
    } else {
        let fill = inner.saturating_sub(visible_len(&core));
        core.push_str(&"─".repeat(fill));
    }
    core = fit(&core, inner);
    let pad = inner.saturating_sub(visible_len(&core));
    core.push_str(&"─".repeat(pad));
    format!("{}\n", crate::style::faint(&format!("{left}{core}{right}")))
}

/// The command bar is bold, and each key is inverse, so the reader sees
/// which characters operate.
fn command_bar(title: &str, width: usize) -> String {
    let inner = width.saturating_sub(2).max(1);
    let mut core = format!("─ {title} ");
    let fill = inner.saturating_sub(visible_len(&core));
    core.push_str(&"─".repeat(fill));
    core = fit(&core, inner);
    let pad = inner.saturating_sub(visible_len(&core));
    core.push_str(&"─".repeat(pad));
    format!("{}\n", crate::style::heading(&format!("└{core}┘")))
}

fn key_chip(key: &str) -> String {
    crate::style::inverse_span(key)
}

fn pane_row(text: &str, width: usize, selected: bool) -> String {
    let inner = width.saturating_sub(2).max(1);
    let body = if selected {
        // A state colour's reset would end inverse, so invert the plain text.
        let plain = strip_sgr(text);
        let mut fitted: String = plain.chars().take(inner).collect();
        let pad = inner.saturating_sub(fitted.chars().count());
        fitted.push_str(&" ".repeat(pad));
        crate::style::inverse(&fitted)
    } else {
        let fitted = fit(text, inner);
        let pad = inner.saturating_sub(visible_len(&fitted));
        format!("{fitted}{}", " ".repeat(pad))
    };
    format!(
        "{}{body}{}\n",
        crate::style::faint("│"),
        crate::style::faint("│")
    )
}

fn visible_len(s: &str) -> usize {
    strip_sgr(s).chars().count()
}

fn strip_sgr(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for x in chars.by_ref() {
                if ('@'..='~').contains(&x) {
                    break;
                }
            }
        } else if c != '\n' && c != '\r' {
            out.push(c);
        }
    }
    out
}

fn fit(s: &str, width: usize) -> String {
    if visible_len(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut n = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            out.push(c);
            out.push(chars.next().unwrap());
            for x in chars.by_ref() {
                out.push(x);
                if ('@'..='~').contains(&x) {
                    break;
                }
            }
            continue;
        }
        if n >= width {
            break;
        }
        out.push(c);
        n += 1;
    }
    out.push_str("\x1b[0m");
    out
}

fn job_line(
    job: &JobStatus,
    previous: &mut HashMap<uuid::Uuid, Previous>,
    update_cpu: bool,
) -> String {
    // Measure the job now, for a job that operates.
    let (cpu_now, mem_now) = match (job.state.is_active(), job.pid) {
        (true, Some(pid)) => {
            let usage = sys::group_usage(pid);
            let now = Instant::now();
            // The CPU use is the change in the CPU time, divided by the
            // time between the two measurements. The result is a number of
            // cores, so 2.0 means two cores in full use.
            let cores = previous.get(&job.id).map(|p| {
                let seconds = now.duration_since(p.at).as_secs_f64();
                if seconds > 0.0 {
                    ((usage.cpu_secs - p.cpu_secs) / seconds).max(0.0)
                } else {
                    0.0
                }
            });
            // A key redraw must not reseed this sample. j/k would then
            // measure CPU over 50ms and the next page would spike.
            if update_cpu {
                previous.insert(
                    job.id,
                    Previous {
                        cpu_secs: usage.cpu_secs,
                        at: now,
                    },
                );
            }
            (cores, Some(usage.rss))
        }
        _ => {
            if update_cpu {
                previous.remove(&job.id);
            }
            (None, None)
        }
    };

    let cpu_text = match cpu_now {
        // The first refresh has no earlier measurement to compare with.
        None if job.state.is_active() => "...".to_string(),
        None => "-".to_string(),
        Some(c) => format!("{c:.1}"),
    };

    let mem_text = match mem_now {
        Some(rss) => format!("{} / {}", format_size(job.mem), format_size(rss)),
        None if job.usage.max_rss > 0 => {
            format!(
                "{} / {}",
                format_size(job.mem),
                format_size(job.usage.max_rss)
            )
        }
        None => format!("{} / -", format_size(job.mem)),
    };

    let elapsed = job
        .elapsed()
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string());

    let note = crate::job::printable(&note_for(job));

    // Write the state in its colour, and make a line of a job that
    // succeeded faint. That job needs no attention, and the eye must go to
    // the jobs that operate and to the failures.
    let line = format!(
        "{:<8}  {:<9}  {:<14.14}  {:>9}  {:>7}  {:>17}  {:>7}  {:>6}  {:.40}",
        &job.id.to_string()[..8],
        job.state.as_str(),
        // The SAFE name. A name that holds an ESC byte would move the
        // cursor of the terminal and write over this page.
        job.display_name(),
        job.cpu,
        cpu_text,
        mem_text,
        elapsed,
        since_text(job),
        note
    );

    match job.state {
        JobState::Completed | JobState::Cancelled => crate::style::faint(&line),
        JobState::Running | JobState::Starting => {
            // Colour the state word only, so the numbers stay easy to read.
            line.replacen(
                job.state.as_str(),
                &crate::style::state(job.state.as_str(), job.state.as_str()),
                1,
            )
        }
        _ => line.replacen(
            job.state.as_str(),
            &crate::style::state(job.state.as_str(), job.state.as_str()),
            1,
        ),
    }
}

fn highlight(selected: bool, line: &str) -> String {
    let mark = if selected { '>' } else { ' ' };
    format!("{mark}{line}")
}

fn detail_lines(
    view: &View,
    job: Option<&JobStatus>,
    rows: usize,
    remaining: usize,
    inner: usize,
) -> Vec<String> {
    let max_content = remaining.saturating_sub(2);
    if max_content == 0 {
        return Vec::new();
    }
    if view.show_info {
        let mut lines = info_lines(job, inner);
        lines.truncate(max_content);
        return lines;
    }
    if view.show_tail {
        let want = (rows / 2).max(1);
        return tail_lines(job, want.min(max_content));
    }
    Vec::new()
}

/// Width of an info label plus the spaces that follow it: `note     `.
const INFO_LABEL: usize = 9;

fn info_lines(job: Option<&JobStatus>, width: usize) -> Vec<String> {
    let Some(job) = job else {
        return vec!["no job is selected".into()];
    };
    // The command and the directory are not handles. `safe_name` would turn
    // `/home/me/project` into `_home_me_project` and `--epochs` into `_epochs`,
    // and the reader could not act on either. `visible` keeps the path and
    // the flags, and it turns a control byte into an escape that a person
    // can read.
    let command = job
        .command
        .iter()
        .map(|a| crate::job::visible(a))
        .collect::<Vec<_>>()
        .join(" ");
    let cwd = crate::job::visible(&job.cwd);
    let mut lines = Vec::new();
    lines.extend(wrap_field("command", &command, width));
    lines.extend(wrap_field("cwd", &cwd, width));
    lines.extend(wrap_field("id", &job.id.to_string(), width));
    lines.extend(wrap_field(
        "queue",
        &format_duration(Duration::from_secs(queued_secs(job))),
        width,
    ));
    lines.extend(wrap_field("run", &run_text(job), width));
    lines.extend(wrap_field(
        "note",
        &crate::job::printable(&note_for(job)),
        width,
    ));
    if !job.locks.is_empty() {
        let locks = job
            .locks
            .iter()
            .map(|n| crate::job::safe_name(n))
            .collect::<Vec<_>>()
            .join(", ");
        lines.extend(wrap_field("locks", &locks, width));
    }
    if let Some(err) = &job.error {
        let err = crate::job::printable(err);
        if !err.is_empty() && !note_for(job).contains(&err) {
            lines.extend(wrap_field("error", &err, width));
        }
    }
    lines
}

fn wrap_field(label: &str, text: &str, width: usize) -> Vec<String> {
    let pad = format!("{label:<INFO_LABEL$}");
    let indent = " ".repeat(INFO_LABEL);
    let body_w = width.saturating_sub(INFO_LABEL).max(1);
    let wrapped = wrap_words(text, body_w);
    if wrapped.is_empty() {
        return vec![pad];
    }
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("{pad}{line}")
            } else {
                format!("{indent}{line}")
            }
        })
        .collect()
}

fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        if wlen > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let chars: Vec<char> = word.chars().collect();
            for chunk in chars.chunks(width) {
                lines.push(chunk.iter().collect());
            }
            continue;
        }
        if cur.is_empty() {
            cur = word.to_string();
        } else if cur.chars().count() + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn queued_secs(job: &JobStatus) -> u64 {
    let end = job.started_at.unwrap_or_else(sys::now_secs);
    end.saturating_sub(job.submitted_at)
}

fn run_text(job: &JobStatus) -> String {
    job.elapsed()
        .map(format_duration)
        .unwrap_or_else(|| "-".to_string())
}

fn tail_lines(job: Option<&JobStatus>, n: usize) -> Vec<String> {
    let mut lines = match job {
        None => vec!["no job is selected".into()],
        Some(job) => read_log_tail(job, n),
    };
    if lines.is_empty() {
        lines.push("no output yet".into());
    }
    while lines.len() < n {
        lines.push(String::new());
    }
    lines.truncate(n);
    lines
}

fn read_log_tail(job: &JobStatus, n: usize) -> Vec<String> {
    let Ok(dir) = crate::paths::job_dir(&job.id) else {
        return vec!["no log directory".into()];
    };
    let mut chunks = Vec::new();
    for (label, name) in [("stderr", "stderr.log"), ("stdout", "stdout.log")] {
        let got = last_file_lines(&dir.join(name), n);
        if !got.is_empty() {
            chunks.push(format!("{label}:"));
            chunks.extend(got);
        }
    }
    if chunks.len() > n {
        chunks.split_off(chunks.len() - n)
    } else {
        chunks
    }
}

fn last_file_lines(path: &std::path::Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 64 * 1024;
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(WINDOW);
    if start > 0 && file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if std::io::Read::take(&mut file, WINDOW)
        .read_to_end(&mut buf)
        .is_err()
    {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(crate::job::printable).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > n {
        lines.split_off(lines.len() - n)
    } else {
        lines
    }
}

fn action_keys(job: Option<&JobStatus>) -> String {
    let mut keys = vec![key_hint("j/k", "move")];
    match job.map(|j| j.state) {
        Some(state) if state.is_active() => keys.push(key_hint("x", "stop")),
        Some(JobState::Queued) => keys.push(key_hint("c", "cancel")),
        Some(state) if state.is_terminal() => keys.push(key_hint("C", "clean")),
        _ => {}
    }
    // `t tail` is always valid. A job in the queue has no output yet, and
    // the reader may still open the pane to watch the first lines when it
    // starts. A key that appeared only after the start would be too late.
    keys.extend([
        key_hint("i", "info"),
        key_hint("t", "tail"),
        key_hint("q", "quit"),
    ]);
    keys.join("   ")
}

fn key_hint(key: &str, label: &str) -> String {
    format!("{} {label}", key_chip(key))
}

fn prompt_keys(view: &View, selected: Option<&JobStatus>) -> Option<String> {
    let prompt = view.prompt.as_ref()?;
    let id = match prompt {
        Prompt::Stop(id) | Prompt::LeaveQueue(id) | Prompt::Clean(id) => *id,
    };
    let name = selected
        .filter(|j| j.id == id)
        .map(|j| j.display_name())
        .unwrap_or_else(|| id.to_string()[..8].to_string());
    let short = &id.to_string()[..8];
    let verb = match prompt {
        Prompt::Stop(_) => "stop",
        Prompt::LeaveQueue(_) => "cancel",
        Prompt::Clean(_) => "clean",
    };
    Some(format!(
        "{}   {}   {}   {verb} {short} {name}?",
        key_hint("y", "yes"),
        key_hint("n", "no"),
        key_hint("q", "quit"),
    ))
}

/// Applies every key that arrived. Gives `true` when the person asked to leave.
fn apply_keys(view: &mut View, jobs: &[JobStatus]) -> bool {
    for key in crate::keys::take() {
        if handle_key(view, key, jobs) {
            return true;
        }
    }
    false
}

fn handle_key(view: &mut View, key: Key, jobs: &[JobStatus]) -> bool {
    if let Some(prompt) = view.prompt.take() {
        let id = match prompt {
            Prompt::Stop(id) | Prompt::LeaveQueue(id) | Prompt::Clean(id) => id,
        };
        match key {
            Key::Char(b'y') | Key::Char(b'Y') => {
                view.message = Some(match prompt {
                    Prompt::Stop(_) => act_stop(id),
                    Prompt::LeaveQueue(_) => act_cancel(id),
                    Prompt::Clean(_) => act_clean(id),
                });
                view.need_refresh = true;
                view.dirty = true;
            }
            Key::Char(b'n') | Key::Char(b'N') | Key::Esc => {
                view.dirty = true;
            }
            Key::Char(b'q') | Key::Char(b'Q') => return true,
            _ => {
                view.prompt = Some(prompt);
            }
        }
        return false;
    }

    match key {
        Key::Char(b'q') | Key::Char(b'Q') => return true,
        Key::Char(b'j') | Key::Down => move_sel(view, jobs, 1),
        Key::Char(b'k') | Key::Up => move_sel(view, jobs, -1),
        Key::PageDown => move_sel(view, jobs, view.body_rows as isize),
        Key::PageUp => move_sel(view, jobs, -(view.body_rows as isize)),
        Key::Char(b'g') | Key::Home => jump(view, jobs, 0),
        Key::Char(b'G') | Key::End => {
            if !jobs.is_empty() {
                jump(view, jobs, jobs.len() - 1);
            }
        }
        Key::Char(b'i') | Key::Enter => {
            view.show_info = !view.show_info;
            if view.show_info {
                view.show_tail = false;
            }
            view.dirty = true;
        }
        Key::Char(b't') => {
            view.show_tail = !view.show_tail;
            if view.show_tail {
                view.show_info = false;
            }
            view.dirty = true;
        }
        Key::Char(b'x') | Key::Char(b'K') => ask_stop(view, jobs),
        Key::Char(b'c') => ask_cancel(view, jobs),
        Key::Char(b'C') => ask_clean(view, jobs),
        _ => {}
    }
    false
}

fn ids_of(jobs: &[JobStatus]) -> Vec<uuid::Uuid> {
    jobs.iter().map(|j| j.id).collect()
}

fn selected_index(view: &View, jobs: &[JobStatus]) -> Option<usize> {
    view.selected
        .and_then(|id| jobs.iter().position(|j| j.id == id))
}

fn sync_view(view: &mut View, ids: &[uuid::Uuid], height: usize) {
    if ids.is_empty() {
        view.selected = None;
        view.scroll = 0;
        if view.prompt.is_some() {
            view.prompt = None;
            view.message = Some("that job is no longer on the page".into());
        }
        return;
    }
    if view.selected.is_none_or(|id| !ids.contains(&id)) {
        let fallback = view.scroll.min(ids.len() - 1);
        view.selected = Some(ids[fallback]);
    }
    let idx = view
        .selected
        .and_then(|id| ids.iter().position(|x| *x == id))
        .unwrap_or(0);
    if height == 0 {
        return;
    }
    if idx < view.scroll {
        view.scroll = idx;
    } else if idx >= view.scroll + height {
        view.scroll = idx + 1 - height;
    }
    let max_scroll = ids.len().saturating_sub(height);
    if view.scroll > max_scroll {
        view.scroll = max_scroll;
    }
}

fn move_sel(view: &mut View, jobs: &[JobStatus], delta: isize) {
    if jobs.is_empty() {
        return;
    }
    let idx = selected_index(view, jobs).unwrap_or(0);
    let next = if delta < 0 {
        idx.saturating_sub(delta.unsigned_abs())
    } else {
        idx.saturating_add(delta as usize).min(jobs.len() - 1)
    };
    view.selected = Some(jobs[next].id);
    view.message = None;
    view.dirty = true;
    sync_view(view, &ids_of(jobs), view.body_rows.max(1));
}

fn jump(view: &mut View, jobs: &[JobStatus], index: usize) {
    if jobs.is_empty() {
        return;
    }
    view.selected = Some(jobs[index.min(jobs.len() - 1)].id);
    view.message = None;
    view.dirty = true;
    sync_view(view, &ids_of(jobs), view.body_rows.max(1));
}

fn ask_stop(view: &mut View, jobs: &[JobStatus]) {
    let Some(job) = view
        .selected
        .and_then(|id| jobs.iter().find(|j| j.id == id))
    else {
        view.message = Some("no job is selected".into());
        view.dirty = true;
        return;
    };
    if job.state.is_active() {
        view.prompt = Some(Prompt::Stop(job.id));
        view.message = None;
    } else if job.state == JobState::Queued {
        view.message = Some("this job waits in the queue. Press c to take it out.".into());
    } else {
        view.message = Some("this job already stopped".into());
    }
    view.dirty = true;
}

fn ask_cancel(view: &mut View, jobs: &[JobStatus]) {
    let Some(job) = view
        .selected
        .and_then(|id| jobs.iter().find(|j| j.id == id))
    else {
        view.message = Some("no job is selected".into());
        view.dirty = true;
        return;
    };
    if job.state == JobState::Queued {
        view.prompt = Some(Prompt::LeaveQueue(job.id));
        view.message = None;
    } else if job.state.is_active() {
        view.message = Some("this job operates. Press x to stop it.".into());
    } else {
        view.message = Some("cancel takes a job out of the queue".into());
    }
    view.dirty = true;
}

fn ask_clean(view: &mut View, jobs: &[JobStatus]) {
    let Some(job) = view
        .selected
        .and_then(|id| jobs.iter().find(|j| j.id == id))
    else {
        view.message = Some("no job is selected".into());
        view.dirty = true;
        return;
    };
    if job.state.is_terminal() {
        view.prompt = Some(Prompt::Clean(job.id));
        view.message = None;
    } else {
        view.message = Some("clean deletes the record of a job that has stopped".into());
    }
    view.dirty = true;
}

fn act_stop(id: uuid::Uuid) -> String {
    let short = &id.to_string()[..8];
    match Client::connect_existing_result() {
        Ok(None) => "no coordinator operates. qex cannot stop the job from this page.".into(),
        Err(e) => format!("{e:#}"),
        Ok(Some(mut client)) => {
            match client.call(&Request::Kill {
                id,
                signal: STOP_SIGNAL,
                grace_secs: STOP_GRACE_SECS,
            }) {
                Ok(Response::Ok) => format!("{short} received the signal"),
                Ok(Response::Error { message, .. }) => message,
                Ok(_) => "the coordinator refused the request".into(),
                Err(e) => format!("{e:#}"),
            }
        }
    }
}

fn act_clean(id: uuid::Uuid) -> String {
    let short = &id.to_string()[..8];
    match Client::connect_existing_result() {
        Ok(None) => "no coordinator operates. qex cannot clean the job from this page.".into(),
        Err(e) => format!("{e:#}"),
        Ok(Some(mut client)) => match client.call(&Request::Clean { id }) {
            Ok(Response::Ok) => format!("{short} deleted"),
            Ok(Response::Error { message, .. }) => message,
            Ok(_) => "the coordinator refused the request".into(),
            Err(e) => format!("{e:#}"),
        },
    }
}

fn act_cancel(id: uuid::Uuid) -> String {
    let short = &id.to_string()[..8];
    match Client::connect_existing_result() {
        Ok(None) => "no coordinator operates. qex cannot cancel the job from this page.".into(),
        Err(e) => format!("{e:#}"),
        Ok(Some(mut client)) => match client.call(&Request::Cancel { id }) {
            Ok(Response::Ok) => format!("{short} left the queue"),
            Ok(Response::Error { message, .. }) => message,
            Ok(_) => "the coordinator refused the request".into(),
            Err(e) => format!("{e:#}"),
        },
    }
}

/// The number of jobs that stopped to show on the page.
///
/// A page must fit a screen. The jobs that operate and the jobs in the queue
/// always appear, because they are the state of the machine now. The live
/// page then scrolls that list so that it fits the rows of the terminal.
const RECENT_DONE: usize = 12;

/// Puts the jobs in the order for the page, and gives the number that it hides.
///
/// The jobs that operate come first, then the jobs in the queue, then the jobs
/// that stopped, with the most recent first. A reader looks at the top of the
/// page for the state of the machine now.
fn arrange(jobs: &[JobStatus]) -> (Vec<JobStatus>, usize) {
    let mut active: Vec<JobStatus> = jobs
        .iter()
        .filter(|j| j.state.is_active())
        .cloned()
        .collect();
    active.sort_by_key(|j| j.started_at.unwrap_or(j.submitted_at));

    let mut queued: Vec<JobStatus> = jobs
        .iter()
        .filter(|j| j.state == JobState::Queued)
        .cloned()
        .collect();
    queued.sort_by_key(|j| (j.submitted_at, j.sequence));

    let mut done: Vec<JobStatus> = jobs
        .iter()
        .filter(|j| j.state.is_terminal())
        .cloned()
        .collect();
    // The most recent first.
    done.sort_by_key(|j| std::cmp::Reverse(j.finished_at.unwrap_or(j.submitted_at)));

    let hidden = done.len().saturating_sub(RECENT_DONE);
    done.truncate(RECENT_DONE);

    let mut out = active;
    out.extend(queued);
    out.extend(done);
    (out, hidden)
}

/// Gives the time since the last change of a job.
///
/// The meaning follows the state: a job in the queue gives the time since its
/// submission, a job that operates gives the time since its start, and a job
/// that stopped gives the time since it stopped. A reader thus sees how old
/// each line is.
fn since_text(job: &JobStatus) -> String {
    let now = crate::sys::now_secs();
    let at = if job.state.is_terminal() {
        job.finished_at.unwrap_or(job.submitted_at)
    } else if job.state.is_active() {
        job.started_at.unwrap_or(job.submitted_at)
    } else {
        job.submitted_at
    };
    format_duration(Duration::from_secs(now.saturating_sub(at)))
}

/// Gives the short note for one job.
fn note_for(job: &JobStatus) -> String {
    if let Some(reason) = &job.blocked_reason {
        return reason.clone();
    }
    if job.forced {
        return "FORCED: larger than the budget".to_string();
    }
    match job.state {
        JobState::Completed => "ok".to_string(),
        JobState::Failed => match job.exit_code {
            Some(c) => format!("exit code {c}"),
            None => "failed".to_string(),
        },
        JobState::Skipped => "a job that it needed did not succeed".to_string(),
        // Say what qex saw, and no more. An earlier note said "the memory
        // claim was too small", and qex cannot say that: the kill count
        // covers every program of the user, so the machine can be full while
        // the claim is correct, and `docs/reference.md` promises that qex
        // never makes that call.
        JobState::Oom => "the kernel stopped it for memory".to_string(),
        JobState::Timeout => "reached its time limit".to_string(),
        JobState::Killed => "stopped by a command".to_string(),
        JobState::Cancelled => "left the queue".to_string(),
        JobState::Expired => "it never started; it waited too long".to_string(),
        // Every state that a job STOPS in must give a note. A reader of `qex
        // top` sees the state and the note together, and a note that is empty
        // makes the reader open a second command to learn the cause.
        _ => String::new(),
    }
}

/// Reads the queue and the state of the machine from a coordinator.
fn read_the_queue(client: &mut Client) -> Result<(Vec<crate::job::JobStatus>, Option<Response>)> {
    let Response::Jobs { mut jobs } = client.call(&Request::List)? else {
        bail!("the coordinator did not give the job list");
    };
    jobs.sort_by_key(|j| (j.submitted_at, j.sequence));
    let info = client.call(&Request::Info)?;
    Ok((jobs, Some(info)))
}

/// Writes the cause one time, whatever the number of refreshes.
fn say_once(said: &mut bool, error: &anyhow::Error) {
    if !*said {
        *said = true;
        eprintln!("qex: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Usage;

    fn job(state: JobState, cpu: u64, mem: u64) -> JobStatus {
        JobStatus {
            id: uuid::Uuid::new_v4(),
            name: "example".into(),
            command: vec!["true".into()],
            cwd: "/".into(),
            state,
            pid: Some(std::process::id() as i32),
            last_pid: None,
            supervisor_pid: None,
            boot_id: None,
            supervisor_start_token: None,
            exit_code: None,
            signal: None,
            submitted_at: 0,
            sequence: 1,
            started_at: Some(0),
            finished_at: None,
            cpu,
            mem,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            usage: Usage::default(),
            forced: false,
            forced_reason: None,
            queue_pause_secs: 0,
            blocked_reason: None,
            blocked_since: None,
            passed_by: 0,
            error: None,
            needs: vec![],
            after: vec![],
            locks: vec![],
            claims: Default::default(),
            assigned: Default::default(),
            attempts: 1,
            retries_left: 0,
            caused_by: None,
            logs_dropped: None,
            tags: vec![],
            dedupe_key: None,
        }
    }

    fn info() -> Response {
        Response::Info {
            config_error: None,
            pools: None,
            pid: 1,
            version: "test".into(),
            started_at: 0,
            program_replaced: false,
            jobs_running: 1,
            jobs_queued: 0,
            cpu_budget: 12,
            mem_budget: 20 << 30,
            cpu_claimed: 2,
            mem_claimed: 4 << 30,
            queue_state: Some("running".into()),
            paused_at: None,
            paused_by_pid: None,
            paused_reason: None,
            paused_until: None,
            paused_locks: Some(vec![]),
            health: Some(Box::new(crate::proto::QueueHealth {
                last_start_at: Some(crate::sys::now_secs()),
                peer_count: 0,
                peer_cpu: 0,
                peer_mem: 0,
                head_job: None,
                head_blocker: None,
                head_passed_by: None,
            })),
        }
    }

    /// A paused queue must say so on the page.
    ///
    /// A person who watches a queue that starts nothing must read the cause
    /// here. A queue that does nothing and does not say why is the fault that
    /// this tool exists to remove.
    #[test]
    fn a_paused_queue_says_so_on_the_page() {
        let Response::Info {
            pid,
            version,
            started_at,
            program_replaced,
            jobs_running,
            jobs_queued,
            cpu_budget,
            mem_budget,
            cpu_claimed,
            mem_claimed,
            ..
        } = info()
        else {
            panic!("expected an info response")
        };
        let paused = Response::Info {
            pools: None,
            pid,
            version,
            started_at,
            program_replaced,
            jobs_running,
            jobs_queued,
            cpu_budget,
            mem_budget,
            cpu_claimed,
            mem_claimed,
            config_error: None,
            queue_state: Some("paused".into()),
            paused_at: Some(crate::sys::now_secs() - 360),
            paused_by_pid: Some(3704694),
            paused_reason: Some("recording a demo".into()),
            paused_until: None,
            paused_locks: Some(vec![]),
            health: Some(Box::new(crate::proto::QueueHealth {
                last_start_at: None,
                peer_count: 0,
                peer_cpu: 0,
                peer_mem: 0,
                head_job: None,
                head_blocker: None,
                head_passed_by: None,
            })),
        };

        let mut previous = HashMap::new();
        let page = render(&[], Some(&paused), &mut previous, false);
        assert!(page.contains("QUEUE PAUSED"), "got: {page}");
        assert!(
            page.contains("recording a demo"),
            "the page must give the reason: {page}"
        );
        assert!(
            page.contains("6m"),
            "the page must say how long the pause has lasted: {page}"
        );
        assert!(
            page.contains("NO END"),
            "a pause with no end must be loud: {page}"
        );
    }

    #[test]
    fn the_page_holds_the_budget_and_the_jobs() {
        let jobs = vec![job(JobState::Running, 2, 4 << 30)];
        let mut previous = HashMap::new();
        let page = render(&jobs, Some(&info()), &mut previous, false);

        assert!(page.contains("2/12 cores"), "the budget is missing: {page}");
        assert!(page.contains("example"), "the job name is missing");
        assert!(page.contains("4GB"), "the memory claim is missing");
    }

    /// EVERY STATE THAT A JOB STOPS IN MUST GIVE A NOTE.
    ///
    /// A reader of `qex top` sees the state and the note together. A note that
    /// is empty makes that reader open a second command to learn the cause,
    /// and `expired` is the state with the least in the record: no exit code,
    /// no output and no log file.
    #[test]
    fn each_final_state_gives_a_note() {
        for state in [
            JobState::Completed,
            JobState::Failed,
            JobState::Killed,
            JobState::Timeout,
            JobState::Expired,
            JobState::Oom,
            JobState::Cancelled,
            JobState::Skipped,
        ] {
            let note = note_for(&job(state, 1, 1 << 30));
            assert!(!note.is_empty(), "the state {state} gives no note");
        }
    }

    /// The first refresh cannot give a CPU value, because there is no earlier
    /// measurement to compare with. The second refresh gives one.
    #[test]
    fn the_cpu_column_needs_two_measurements() {
        let jobs = vec![job(JobState::Running, 1, 1 << 30)];
        let mut previous = HashMap::new();

        let first = render(&jobs, Some(&info()), &mut previous, false);
        assert!(first.contains("..."), "the first page has no earlier value");

        std::thread::sleep(Duration::from_millis(50));
        let second = render(&jobs, Some(&info()), &mut previous, false);
        assert!(
            !second.contains("..."),
            "the second page must give a number: {second}"
        );
    }

    /// A job that stopped shows its measured peak, and not a live value.
    #[test]
    fn a_job_that_stopped_shows_its_measurement() {
        let mut j = job(JobState::Completed, 1, 1 << 30);
        j.usage.max_rss = 500 << 20;
        j.finished_at = Some(10);

        let mut previous = HashMap::new();
        let page = render(&[j], Some(&info()), &mut previous, false);
        assert!(page.contains("500MB"), "the measurement is missing: {page}");
        assert!(page.contains("ok"), "the result is missing");
    }

    /// The command must give the jobs when no coordinator operates.
    ///
    /// A command that watches must not depend on the thing that it watches, and
    /// it must not start it either.
    #[test]
    fn the_page_holds_the_jobs_with_no_coordinator() {
        let _lock = crate::testutil::env_lock();
        let mut j = job(JobState::Completed, 2, 1 << 30);
        j.usage.max_rss = 100 << 20;
        j.finished_at = Some(5);

        let mut previous = HashMap::new();
        let page = render(&[j], None, &mut previous, false);

        assert!(page.contains("example"), "the job is missing: {page}");
        assert!(
            page.contains("no coordinator"),
            "the page must say that no coordinator operates: {page}"
        );
        assert!(
            page.contains("state directory"),
            "the page must say where the records come from: {page}"
        );
        // The budget still comes from the config file.
        assert!(page.contains("cores"), "the budget is missing: {page}");
    }

    /// Isolates `XDG_CONFIG_HOME` so a test of the header can write a file
    /// without reading the config of the person who runs the suite.
    fn isolated_config(
        tag: &str,
        text: Option<&str>,
    ) -> (std::path::PathBuf, crate::testutil::EnvVar) {
        let dir = std::env::temp_dir().join(format!(
            "qex-top-cfg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = text {
            std::fs::write(dir.join("qex.toml"), text).unwrap();
        }
        let guard = crate::testutil::EnvVar::set("XDG_CONFIG_HOME", dir.to_str().unwrap());
        (dir, guard)
    }

    fn paint_live(info: Option<&Response>, cols: usize) -> String {
        let (ordered, hidden) = arrange(&[]);
        let mut previous = HashMap::new();
        let mut view = View::new();
        paint(
            &ordered,
            hidden,
            info,
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((24, cols)),
            },
        )
    }

    fn info_with_config_error(error: Option<String>) -> Response {
        let Response::Info {
            pid,
            version,
            started_at,
            program_replaced,
            jobs_running,
            jobs_queued,
            cpu_budget,
            mem_budget,
            cpu_claimed,
            mem_claimed,
            queue_state,
            paused_at,
            paused_by_pid,
            paused_reason,
            paused_until,
            paused_locks,
            health,
            pools,
            ..
        } = info()
        else {
            panic!("expected an info response")
        };
        Response::Info {
            pid,
            version,
            started_at,
            program_replaced,
            jobs_running,
            jobs_queued,
            cpu_budget,
            mem_budget,
            cpu_claimed,
            mem_claimed,
            config_error: error,
            queue_state,
            paused_at,
            paused_by_pid,
            paused_reason,
            paused_until,
            paused_locks,
            health,
            pools,
        }
    }

    /// A file that qex cannot read must not become the default values in
    /// silence. The page must keep working, and it must name the file.
    #[test]
    fn a_config_file_that_qex_cannot_read_is_named_on_the_page() {
        let _lock = crate::testutil::env_lock();
        let (_dir, _cfg) =
            isolated_config("bad", Some("[budget]\ncpu = \"1\"\n\n[unknown]\nfoo = 1\n"));

        let mut previous = HashMap::new();
        let page = render(&[], None, &mut previous, false);

        assert!(
            page.contains("default values"),
            "the page must say that the numbers are not the user's: {page}"
        );
        assert!(
            page.contains("qex.toml"),
            "the page must name the file: {page}"
        );
        assert!(
            page.contains("WARNING"),
            "a silent fallback is the fault: {page}"
        );
        assert!(page.contains("budget"), "the page must still work: {page}");
        assert!(
            page.contains("no coordinator"),
            "the page must still say that no coordinator operates: {page}"
        );
    }

    /// The live page clips from the right. The file name must still be on
    /// an 80-column screen, which is the form a person watches.
    #[test]
    fn a_live_page_still_names_a_config_file_that_qex_cannot_read() {
        let _lock = crate::testutil::env_lock();
        let (_dir, _cfg) = isolated_config(
            "badlive",
            Some("[budget]\ncpu = \"1\"\n\n[unknown]\nfoo = 1\n"),
        );

        let page = paint_live(None, 80);
        assert!(
            page.contains("qex.toml"),
            "an 80-column page must still name the file: {page}"
        );
        assert!(
            page.contains("default values"),
            "the page must say that the numbers are not the user's: {page}"
        );
        for line in page.lines() {
            assert!(
                visible_len(line) <= 80,
                "a line is wider than the screen ({}) : {line}",
                visible_len(line)
            );
        }
    }

    /// A file that is gone is not a fault. qex uses the default values and
    /// says nothing: that is the usual start, not a file that qex refused.
    #[test]
    fn a_missing_config_file_is_not_a_fault_on_the_page() {
        let _lock = crate::testutil::env_lock();
        let (_dir, _cfg) = isolated_config("gone", None);

        let mut previous = HashMap::new();
        let page = render(&[], None, &mut previous, false);

        assert!(
            !page.contains("default values"),
            "a missing file must not look like a file that qex refused: {page}"
        );
        assert!(
            !page.contains("WARNING"),
            "a missing file is the usual start: {page}"
        );
        assert!(page.contains("budget"), "the page must still work: {page}");
    }

    /// A number that the user wrote must reach the header. A change that
    /// always used the defaults would pass the warning tests and still hide
    /// the file.
    #[test]
    fn the_header_uses_the_budget_from_a_file_that_qex_can_read() {
        let _lock = crate::testutil::env_lock();
        let (_dir, _cfg) = isolated_config("ok", Some("[budget]\ncpu = \"7\"\n"));

        let mut previous = HashMap::new();
        let page = render(&[], None, &mut previous, false);

        assert!(
            page.contains("0/7 cores"),
            "the page must use the budget from the file: {page}"
        );
        assert!(
            !page.contains("default values"),
            "a file that qex can read is not a fault: {page}"
        );
    }

    /// A coordinator that cannot read the file already holds the last good
    /// values. The page must say so, or a person who watches `qex top` while
    /// they edit the file never learns that the numbers are old.
    #[test]
    fn a_coordinator_config_fault_is_named_on_the_page() {
        let broken = info_with_config_error(Some(
            "parsing config file /x/qex.toml: unknown field `foo`".into(),
        ));
        let page = paint_live(Some(&broken), 80);
        assert!(
            page.contains("WARNING"),
            "the page must name the fault: {page}"
        );
        assert!(
            page.contains("qex.toml"),
            "the page must name the file: {page}"
        );
        assert!(
            page.contains("coordinator keeps its values"),
            "the page must say that the numbers are the ones the coordinator already had: {page}"
        );
        assert!(
            !page.contains("default values"),
            "the coordinator did not take the defaults: {page}"
        );
        assert!(
            !page.contains("waiting for a writer"),
            "a parse fault is not a wait: {page}"
        );
        assert!(
            page.contains("2/12 cores"),
            "the budget of the coordinator must stay: {page}"
        );
    }

    /// A young file is a wait, not a fault. The page must not say that qex
    /// cannot use a file it is only ageing.
    #[test]
    fn a_coordinator_wait_for_a_writer_is_not_an_unreadable_file() {
        let waiting = info_with_config_error(Some(crate::daemon::WAITING_FOR_A_WRITER.to_string()));
        let page = paint_live(Some(&waiting), 80);
        assert!(
            page.contains("waiting for a writer"),
            "the page must say that qex is waiting: {page}"
        );
        assert!(
            page.contains("qex.toml"),
            "the page must name the file: {page}"
        );
        assert!(
            !page.contains("unreadable"),
            "a wait must not read as a broken file: {page}"
        );
        assert!(
            !page.contains("cannot use"),
            "a wait must not read as a broken file: {page}"
        );
        assert!(
            page.contains("2/12 cores"),
            "the budget of the coordinator must stay: {page}"
        );
    }

    /// The page must give the jobs that operate first, then the queue, then the
    /// jobs that stopped. A reader looks at the top for the state now.
    #[test]
    fn the_page_gives_the_running_jobs_first() {
        let mut running = job(JobState::Running, 1, 1 << 30);
        running.name = "runs-now".into();

        let mut queued = job(JobState::Queued, 1, 1 << 30);
        queued.name = "in-queue".into();
        queued.started_at = None;

        let mut done = job(JobState::Completed, 1, 1 << 30);
        done.name = "finished".into();
        done.finished_at = Some(5);

        // Give them in the wrong order for the page.
        let (ordered, hidden) = arrange(&[done, queued, running]);
        let names: Vec<&str> = ordered.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["runs-now", "in-queue", "finished"]);
        assert_eq!(hidden, 0);
    }

    /// The page shows the most recent jobs that stopped, and it says how many
    /// it did not show.
    #[test]
    fn the_page_limits_the_jobs_that_stopped() {
        let mut jobs = Vec::new();
        for i in 0..(RECENT_DONE + 5) {
            let mut j = job(JobState::Completed, 1, 1 << 20);
            j.name = format!("job-{i}");
            j.finished_at = Some(i as u64);
            jobs.push(j);
        }

        let (ordered, hidden) = arrange(&jobs);
        assert_eq!(ordered.len(), RECENT_DONE);
        assert_eq!(hidden, 5);
        // The most recent comes first.
        assert_eq!(ordered[0].name, format!("job-{}", RECENT_DONE + 4));
    }

    /// The SINCE column follows the state of the job.
    #[test]
    fn the_since_column_follows_the_state() {
        let now = crate::sys::now_secs();

        let mut queued = job(JobState::Queued, 1, 1 << 20);
        queued.submitted_at = now - 120;
        queued.started_at = None;
        assert_eq!(
            since_text(&queued),
            "2m",
            "a job in the queue: since it arrived"
        );

        let mut running = job(JobState::Running, 1, 1 << 20);
        running.submitted_at = now - 600;
        running.started_at = Some(now - 60);
        assert_eq!(
            since_text(&running),
            "1m",
            "a job that operates: since it started"
        );

        let mut done = job(JobState::Completed, 1, 1 << 20);
        done.started_at = Some(now - 600);
        done.finished_at = Some(now - 30);
        assert_eq!(
            since_text(&done),
            "30s",
            "a job that stopped: since it stopped"
        );
    }

    /// The page must give the time, so a reader sees that a screen is old.
    #[test]
    fn the_page_gives_the_time() {
        let mut previous = HashMap::new();
        let page = render(&[], Some(&info()), &mut previous, false);
        let clock = crate::sys::clock_text(crate::sys::now_secs());
        assert!(page.contains(&clock[..5]), "the time is missing: {page}");
    }

    #[test]
    fn an_empty_queue_says_so() {
        let mut previous = HashMap::new();
        let page = render(&[], Some(&info()), &mut previous, false);
        assert!(page.contains("no jobs"));
    }

    /// A job that waits must show the reason in the note.
    #[test]
    fn a_job_that_waits_shows_the_reason() {
        let mut j = job(JobState::Queued, 4, 1 << 30);
        j.blocked_reason = Some("waits for cores: 12 of 12 are in use".into());
        j.started_at = None;

        let mut previous = HashMap::new();
        let page = render(&[j], Some(&info()), &mut previous, false);
        assert!(page.contains("waits for cores"), "got: {page}");
    }

    fn many_jobs(n: usize) -> Vec<JobStatus> {
        (0..n)
            .map(|i| {
                let mut j = job(JobState::Queued, 1, 1 << 20);
                j.name = format!("job-{i}");
                j.started_at = None;
                j.submitted_at = i as u64;
                j.sequence = i as u64;
                j
            })
            .collect()
    }

    /// A page that a person watches must fit the screen. A list that is longer
    /// than the screen used to write past the last row, and the header left
    /// the display.
    #[test]
    fn a_live_page_fits_the_screen() {
        let jobs = many_jobs(40);
        let (ordered, hidden) = arrange(&jobs);
        let mut previous = HashMap::new();
        let mut view = View::new();
        let page = paint(
            &ordered,
            hidden,
            Some(&info()),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((20, 72)),
            },
        );
        assert_eq!(
            page.lines().count(),
            20,
            "the page must fill the screen: {page}"
        );
        assert!(
            !page.ends_with('\n'),
            "a newline after the last row scrolls the header off"
        );
        for line in page.lines() {
            assert!(
                visible_len(line) <= 72,
                "a line is wider than the screen ({}) : {line}",
                visible_len(line)
            );
        }
        assert!(
            page.contains("below"),
            "the page must say that more jobs exist: {page}"
        );
        assert!(page.contains("┌"), "the header pane is missing: {page}");
        assert!(page.contains("jobs"), "the jobs pane is missing: {page}");
        assert!(
            !page.contains("─ info "),
            "the info pane must be off until the reader asks: {page}"
        );
        assert!(page.contains("j/k move"), "the keys are missing: {page}");
    }

    /// A short list must still fill the terminal. The jobs pane grows, so the
    /// info pane and the keys stay on the last rows.
    #[test]
    fn a_short_list_still_fills_the_screen() {
        let jobs = many_jobs(2);
        let (ordered, hidden) = arrange(&jobs);
        let mut previous = HashMap::new();
        let mut view = View::new();
        let page = paint(
            &ordered,
            hidden,
            Some(&info()),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((20, 72)),
            },
        );
        assert_eq!(
            page.lines().count(),
            20,
            "the jobs pane must grow to fill the screen: {page}"
        );
        let text: Vec<&str> = page.lines().collect();
        assert!(
            text[text.len() - 1].contains("j/k move"),
            "the keys must sit on the last row: {page}"
        );
    }

    /// The query form writes every job that the page names. A script that
    /// counts lines must not lose a job that did not fit a screen.
    #[test]
    fn once_writes_every_job_on_the_page() {
        let jobs = many_jobs(40);
        let mut previous = HashMap::new();
        let page = render(&jobs, Some(&info()), &mut previous, false);
        for i in 0..40 {
            assert!(
                page.contains(&format!("job-{i}")),
                "the query lost job-{i}: {page}"
            );
        }
        assert!(
            !page.contains("j/k move"),
            "the query must not wait for a key: {page}"
        );
        assert!(
            !page.contains('┌'),
            "the query must stay a plain page: {page}"
        );
        assert!(
            !page.contains("Press q"),
            "the query must not name a key that does not operate: {page}"
        );
    }

    /// Moving past the last visible row must bring that job onto the page.
    #[test]
    fn the_selection_brings_a_job_onto_the_page() {
        let jobs = many_jobs(20);
        let ids: Vec<uuid::Uuid> = jobs.iter().map(|j| j.id).collect();
        let mut view = View::new();
        view.body_rows = 5;
        sync_view(&mut view, &ids, 5);
        assert_eq!(view.scroll, 0);
        assert_eq!(view.selected, Some(ids[0]));

        move_sel(&mut view, &jobs, 6);
        assert_eq!(view.selected, Some(ids[6]));
        assert_eq!(view.scroll, 2, "the window must follow the selection");
    }

    /// A job that leaves the page must not leave the selection pointing at it.
    #[test]
    fn a_job_that_leaves_gives_the_selection_to_another() {
        let jobs = many_jobs(3);
        let mut view = View::new();
        view.selected = Some(jobs[1].id);
        let remaining: Vec<uuid::Uuid> = vec![jobs[0].id, jobs[2].id];
        sync_view(&mut view, &remaining, 5);
        assert_eq!(view.selected, Some(jobs[0].id));
    }

    #[test]
    fn x_asks_to_stop_a_job_that_operates() {
        let running = job(JobState::Running, 1, 1 << 20);
        let mut view = View::new();
        view.selected = Some(running.id);
        handle_key(&mut view, Key::Char(b'x'), std::slice::from_ref(&running));
        assert!(matches!(view.prompt, Some(Prompt::Stop(id)) if id == running.id));

        let mut queued = job(JobState::Queued, 1, 1 << 20);
        queued.started_at = None;
        view.prompt = None;
        view.selected = Some(queued.id);
        handle_key(&mut view, Key::Char(b'x'), std::slice::from_ref(&queued));
        assert!(view.prompt.is_none());
        assert!(
            view.message.as_deref().is_some_and(|m| m.contains("queue")),
            "got {:?}",
            view.message
        );

        handle_key(&mut view, Key::Char(b'c'), &[queued]);
        assert!(matches!(view.prompt, Some(Prompt::LeaveQueue(_))));
    }

    #[test]
    fn the_keys_follow_the_state_of_the_selected_job() {
        let running = job(JobState::Running, 1, 1 << 20);
        let queued = {
            let mut j = job(JobState::Queued, 1, 1 << 20);
            j.started_at = None;
            j
        };
        let done = job(JobState::Completed, 1, 1 << 20);
        let run = action_keys(Some(&running));
        assert!(run.contains("x stop"), "{run}");
        assert!(!run.contains("c cancel"), "{run}");
        assert!(!run.contains("C clean"), "{run}");
        let wait = action_keys(Some(&queued));
        assert!(wait.contains("c cancel"), "{wait}");
        assert!(!wait.contains("x stop"), "{wait}");
        assert!(!wait.contains("C clean"), "{wait}");
        let finished = action_keys(Some(&done));
        assert!(finished.contains("C clean"), "{finished}");
        assert!(!finished.contains("x stop"), "{finished}");
        assert!(!finished.contains("c cancel"), "{finished}");
        for bar in [&run, &wait, &finished] {
            assert!(bar.contains("t tail"), "tail must stay on the bar: {bar}");
        }
    }

    #[test]
    fn shift_c_asks_to_clean_a_job_that_stopped() {
        let done = job(JobState::Completed, 1, 1 << 20);
        let mut view = View::new();
        view.selected = Some(done.id);
        handle_key(&mut view, Key::Char(b'C'), std::slice::from_ref(&done));
        assert!(matches!(view.prompt, Some(Prompt::Clean(id)) if id == done.id));

        let running = job(JobState::Running, 1, 1 << 20);
        view.prompt = None;
        view.selected = Some(running.id);
        handle_key(&mut view, Key::Char(b'C'), &[running]);
        assert!(view.prompt.is_none());
        assert!(
            view.message
                .as_deref()
                .is_some_and(|m| m.contains("stopped")),
            "got {:?}",
            view.message
        );
    }

    /// A y/n prompt blocks the other keys. The bar must name only y, n and q,
    /// or the reader presses a key that the page still shows and nothing
    /// happens.
    #[test]
    fn a_confirm_replaces_the_command_bar() {
        let running = job(JobState::Running, 1, 1 << 20);
        let (ordered, hidden) = arrange(std::slice::from_ref(&running));
        let mut previous = HashMap::new();
        let mut view = View::new();
        view.selected = Some(running.id);
        handle_key(&mut view, Key::Char(b'x'), &ordered);
        let page = paint(
            &ordered,
            hidden,
            Some(&info()),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((20, 72)),
            },
        );
        assert!(page.contains("y yes"), "the bar must offer y: {page}");
        assert!(page.contains("n no"), "the bar must offer n: {page}");
        assert!(
            !page.contains("x stop"),
            "a blocked key must leave the bar: {page}"
        );
        assert!(
            !page.contains("j/k move"),
            "move is blocked during confirm: {page}"
        );
    }

    #[test]
    fn i_shows_the_command_of_the_selected_job() {
        let mut j = job(JobState::Running, 1, 1 << 20);
        j.command = vec![
            "uv".into(),
            "run".into(),
            "train.py".into(),
            "--epochs".into(),
            "50".into(),
        ];
        j.cwd = "/home/me/project".into();
        let (ordered, hidden) = arrange(std::slice::from_ref(&j));
        let mut previous = HashMap::new();
        let mut view = View::new();
        view.selected = Some(j.id);
        view.show_info = true;
        let page = paint(
            &ordered,
            hidden,
            Some(&info()),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((24, 80)),
            },
        );
        assert!(
            page.contains("command  uv run train.py --epochs 50"),
            "the info must give the command line: {page}"
        );
        assert!(
            page.contains("cwd      /home/me/project"),
            "the info must give the working directory: {page}"
        );
        assert!(
            page.contains("queue    "),
            "the info must give the wait: {page}"
        );
        assert!(
            page.contains("run      "),
            "the info must give the run: {page}"
        );
        assert!(
            page.contains("note     "),
            "the info must give the full note: {page}"
        );
    }

    /// A long note must break on a word, and the next line must line up with
    /// the text, not with the label.
    #[test]
    fn a_long_note_wraps_on_a_word_and_indents() {
        let text = "waits for cores: this job needs 1 core, and the jobs of this queue \
                    hold 9 of the 9 cores in the budget.";
        let lines = wrap_field("note", text, 50);
        assert!(lines.len() >= 2, "the note must wrap: {lines:?}");
        assert!(
            lines[0].starts_with("note     "),
            "the first line keeps the label: {lines:?}"
        );
        assert!(
            lines[1].starts_with("         "),
            "a wrapped line is indented: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("bu\n") || l.ends_with("bu")),
            "a wrap must not split budget: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("budget")),
            "budget must stay one word: {lines:?}"
        );
    }

    /// A state colour carries a reset. Inverse around that line used to end
    /// at the state word, or before the pad, so the highlight stopped short.
    #[test]
    fn the_selection_covers_the_whole_row() {
        let mut previous = HashMap::new();
        let killed = job_line(&job(JobState::Killed, 1, 1 << 20), &mut previous, true);
        let completed = job_line(&job(JobState::Completed, 1, 1 << 20), &mut previous, true);
        for line in [killed, completed] {
            let row = pane_row(&highlight(true, &line), 80, true);
            assert_eq!(
                visible_len(&row),
                80,
                "the highlight must reach the border: {row}"
            );
        }
    }

    #[test]
    fn t_opens_a_tail_pane_on_the_lower_half() {
        let jobs = many_jobs(8);
        let (ordered, hidden) = arrange(&jobs);
        let mut previous = HashMap::new();
        let mut view = View::new();
        handle_key(&mut view, Key::Char(b't'), &ordered);
        assert!(view.show_tail);
        assert!(!view.show_info);
        let page = paint(
            &ordered,
            hidden,
            Some(&info()),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((20, 72)),
            },
        );
        assert!(page.contains("tail"), "the tail pane is missing: {page}");
        assert_eq!(page.lines().count(), 20, "got: {page}");
        let tail_rows = page
            .lines()
            .skip_while(|l| !l.contains("tail"))
            .skip(1)
            .take_while(|l| !l.contains("j/k move"))
            .count();
        assert!(
            tail_rows >= 1,
            "the tail pane must keep at least one line: {page}"
        );
        assert!(
            tail_rows >= 8,
            "the tail pane must take about half of a 20-row screen: {tail_rows} {page}"
        );
    }

    /// A long note must not make the page taller than the terminal.
    #[test]
    fn a_long_info_note_still_fits_the_screen() {
        let mut j = job(JobState::Queued, 1, 1 << 20);
        j.started_at = None;
        j.blocked_reason = Some("word ".repeat(200).trim().into());
        let (ordered, hidden) = arrange(std::slice::from_ref(&j));
        let mut previous = HashMap::new();
        let mut view = View::new();
        view.selected = Some(j.id);
        view.show_info = true;
        let page = paint(
            &ordered,
            hidden,
            Some(&info()),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((24, 80)),
            },
        );
        assert!(
            page.lines().count() <= 24,
            "info overflowed the screen ({} lines): {page}",
            page.lines().count()
        );
    }

    /// The confirm chips must stay visible when the job name is long.
    #[test]
    fn a_long_name_does_not_hide_the_confirm_keys() {
        let mut j = job(JobState::Completed, 1, 1 << 20);
        j.name = "n".repeat(128);
        let (ordered, hidden) = arrange(std::slice::from_ref(&j));
        let mut previous = HashMap::new();
        let mut view = View::new();
        view.selected = Some(j.id);
        handle_key(&mut view, Key::Char(b'C'), &ordered);
        let page = paint(
            &ordered,
            hidden,
            Some(&info()),
            &mut previous,
            PageHow {
                update_cpu: true,
                unreachable: false,
                live: Some(&mut view),
                size: Some((20, 80)),
            },
        );
        assert!(page.contains("y yes"), "y was clipped: {page}");
        assert!(page.contains("n no"), "n was clipped: {page}");
        assert!(page.contains("q quit"), "q was clipped: {page}");
    }

    #[test]
    fn a_sideways_arrow_does_not_cancel_a_confirm() {
        let running = job(JobState::Running, 1, 1 << 20);
        let mut view = View::new();
        view.selected = Some(running.id);
        handle_key(&mut view, Key::Char(b'x'), std::slice::from_ref(&running));
        assert!(matches!(view.prompt, Some(Prompt::Stop(_))));
        handle_key(&mut view, Key::Left, std::slice::from_ref(&running));
        assert!(
            matches!(view.prompt, Some(Prompt::Stop(_))),
            "left must not act as Esc"
        );
        handle_key(&mut view, Key::Right, std::slice::from_ref(&running));
        assert!(matches!(view.prompt, Some(Prompt::Stop(_))));
    }

    #[test]
    fn last_file_lines_reads_a_window_and_survives_bad_utf8() {
        let dir = std::env::temp_dir().join(format!("qex-top-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.log");
        let mut data = vec![0xffu8; 80 * 1024];
        data.extend_from_slice(b"\nTHE_END\n");
        std::fs::write(&path, &data).unwrap();
        let lines = last_file_lines(&path, 5);
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            lines.iter().any(|l| l.contains("THE_END")),
            "the window missed the end: {lines:?}"
        );
    }
}
