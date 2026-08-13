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

/// The live state of the page that a person watches.
struct View {
    selected: Option<uuid::Uuid>,
    scroll: usize,
    /// How many job lines the last page could hold.
    body_rows: usize,
    show_info: bool,
    prompt: Option<Prompt>,
    message: Option<String>,
    dirty: bool,
    need_refresh: bool,
}

enum Prompt {
    Stop(uuid::Uuid),
    LeaveQueue(uuid::Uuid),
}

impl View {
    fn new() -> Self {
        Self {
            selected: None,
            scroll: 0,
            body_rows: 12,
            show_info: false,
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
            crate::keys::restore();
            return Ok(0);
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
        let rows = sys::terminal_rows();
        let page = paint(
            &ordered,
            hidden,
            info.as_ref(),
            &mut previous,
            !reached,
            Some(&mut view),
            rows,
        );

        print!("{CLEAR}{page}");
        use std::io::Write;
        std::io::stdout().flush().ok();

        // Sleep in short steps, so a key moves the selection at once and not
        // after the whole time between two refreshes.
        let until = Instant::now() + interval;
        while Instant::now() < until {
            if keys && apply_keys(&mut view, &ordered) {
                crate::keys::restore();
                return Ok(0);
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
                    !reached,
                    Some(&mut view),
                    sys::terminal_rows(),
                );
                print!("{CLEAR}{page}");
                std::io::stdout().flush().ok();
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
    paint(&ordered, hidden, info, previous, unreachable, None, None)
}

fn paint(
    ordered: &[JobStatus],
    hidden: usize,
    info: Option<&Response>,
    previous: &mut HashMap<uuid::Uuid, Previous>,
    unreachable: bool,
    mut live: Option<&mut View>,
    rows: Option<usize>,
) -> String {
    let mut out = String::new();

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
        ..
    }) = info
    {
        out.push_str(&format!(
            "qex   budget {cpu_claimed}/{cpu_budget} cores, {}/{} memory   \
             {jobs_running} running, {jobs_queued} queued\n",
            format_size(*mem_claimed),
            format_size(*mem_budget),
        ));
        // Give both versions. A coordinator can hold the code of an earlier
        // build, and that difference caused a fault that named no cause.
        let mine = crate::version::VERSION;
        if version != mine {
            out.push_str(&crate::style::warning(&format!(
                "      WARNING: the coordinator is version {version} and this command is {mine}"
            )));
            out.push('\n');
        } else {
            out.push_str(&crate::style::faint(&format!("      version {version}")));
            out.push('\n');
        }
        if *program_replaced {
            out.push_str(
                "      the qex program changed; this coordinator stops when no job operates\n",
            );
        }

        // Say the pause on the page. A person who watches a queue that starts
        // nothing must read the cause here, and not look for it.
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
                // The pid of the PAUSER. This page gave 0 for every pause,
                // which `pause::who` prints as "an unknown process" — the
                // honest answer for a coordinator that does not report it, and
                // the wrong answer for one that does.
                by_pid: paused_by_pid.unwrap_or(0),
                reason: paused_reason.clone(),
                until: *paused_until,
                fault,
            };
            out.push_str(&crate::style::warning(&format!(
                "      QUEUE PAUSED: qex starts no job. {}",
                crate::pause::queue_line(&record, now)
            )));
            out.push('\n');
        }
        for lock in paused_locks.iter().flatten() {
            out.push_str(&crate::style::warning(&format!(
                "      {}",
                crate::pause::lock_line(&lock.name, &lock.record, lock.held_by.as_deref(), now)
            )));
            out.push('\n');
        }

        // The health of the queue, in the same words as `qex info`. A reader of
        // a screen that does not change must be able to see WHY it does not
        // change, and the reason of each job is not on this screen.
        if let Some(info) = info {
            let line = crate::commands::queue_line(info);
            if !line.is_empty() {
                out.push_str(&format!("      {line}\n"));
            }
        }
    }

    if info.is_none() {
        // No coordinator. Give the budget from the config file, and count the
        // jobs from their records.
        let cfg = crate::config::Config::load().unwrap_or_default();
        let active: Vec<&JobStatus> = ordered.iter().filter(|j| j.state.is_active()).collect();
        let queued = ordered
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .count();
        let cpu: u64 = active.iter().map(|j| j.cpu).sum();
        let mem: u64 = active.iter().map(|j| j.mem).sum();

        out.push_str(&format!(
            "qex   budget {cpu}/{} cores, {}/{} memory   {} running, {queued} queued\n",
            cfg.budget_cpu().unwrap_or(0),
            format_size(mem),
            format_size(cfg.budget_mem().unwrap_or(0)),
            active.len(),
        ));
        out.push_str(if unreachable {
            // A coordinator that did not answer is NOT a coordinator that is
            // absent, and this page must not say that nothing operates. The
            // screen clears at each refresh, so the cause on stderr is gone by
            // the next page and only this line carries it.
            // EVERY CAUSE THAT REACHES THIS LINE, and not one of them.
            //
            // A coordinator that did not answer reaches it, and so does one
            // that answered with words this version cannot read. The page must
            // not name a cause that it did not test: what is true for both is
            // that qex holds no answer from a coordinator.
            "      qex has no answer from a coordinator. These records come from the state \
             directory,\n\
             \x20     and a coordinator can hold jobs that this page does not name.\n"
        } else {
            "      no coordinator operates. These records come from the state directory.\n\
             \x20     qex starts a coordinator when you submit a job.\n"
        });
    }

    out.push_str(&format!(
        "machine  {} cores, {} free of {}     {}\n\n",
        sys::cpu_count(),
        format_size(sys::available_memory()),
        format_size(sys::total_memory()),
        // The time of the page. A reader of a screen that stopped refreshing
        // must be able to see that it is old.
        sys::clock_text(sys::now_secs()),
    ));

    out.push_str(&crate::style::heading(&format!(
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
    )));
    out.push('\n');

    let mut lines: Vec<(uuid::Uuid, String)> = Vec::with_capacity(ordered.len());
    for job in ordered {
        lines.push((job.id, job_line(job, previous)));
    }

    if let (Some(view), Some(rows)) = (live.as_deref_mut(), rows) {
        // Reserve the footer before we know the exact window counts. The
        // footer is a few short lines, and one extra reserved line is better
        // than a page that writes past the last row.
        let header_n = out.lines().count();
        let reserved = reserved_footer_lines(view, hidden);
        view.body_rows = rows.saturating_sub(header_n + reserved).max(1);
        let ids: Vec<uuid::Uuid> = lines.iter().map(|l| l.0).collect();
        sync_view(view, &ids, view.body_rows);
    }

    let selected = live.as_ref().and_then(|v| v.selected);
    let selected_job = selected.and_then(|id| ordered.iter().find(|j| j.id == id));
    let footer = footer_text(
        hidden,
        live.as_deref(),
        selected_job,
        ordered.len(),
        sys::stdin_is_terminal(),
    );

    if lines.is_empty() {
        out.push_str("\nno jobs\n");
        out.push_str(&footer);
        return out;
    }

    let shown = if let Some(view) = live.as_deref() {
        let end = (view.scroll + view.body_rows).min(lines.len());
        &lines[view.scroll..end]
    } else {
        lines.as_slice()
    };

    let mark = live.is_some();
    for (id, styled) in shown {
        let line = if mark {
            highlight(Some(*id) == selected, styled)
        } else {
            styled.clone()
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(&footer);
    out
}

fn reserved_footer_lines(view: &View, hidden: usize) -> usize {
    // CPU NOW legend, the keys, and a blank line before them.
    let mut n = 3;
    if hidden > 0 {
        n += 2;
    }
    if view.show_info {
        n += 3;
    }
    if view.prompt.is_some() || view.message.is_some() {
        n += 1;
    }
    n
}

fn job_line(job: &JobStatus, previous: &mut HashMap<uuid::Uuid, Previous>) -> String {
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
            previous.insert(
                job.id,
                Previous {
                    cpu_secs: usage.cpu_secs,
                    at: now,
                },
            );
            (cores, Some(usage.rss))
        }
        _ => {
            previous.remove(&job.id);
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

    let note = note_for(job);

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
    let marked = format!("{mark}{line}");
    if selected {
        crate::style::inverse(&marked)
    } else {
        marked
    }
}

fn footer_text(
    hidden: usize,
    view: Option<&View>,
    selected: Option<&JobStatus>,
    total: usize,
    tty: bool,
) -> String {
    let mut out = String::new();
    if hidden > 0 {
        out.push_str(&format!(
            "\n{hidden} more job(s) that stopped are not shown. Use `qex list`.\n"
        ));
    }

    if let (Some(view), Some(job)) = (view, selected) {
        if view.show_info {
            out.push_str(&info_text(job));
        }
    }

    if let Some(view) = view {
        if let Some(text) = prompt_text(view, selected) {
            out.push_str(&crate::style::warning(&text));
            out.push('\n');
        } else if let Some(message) = &view.message {
            out.push_str(message);
            out.push('\n');
        }
    }

    out.push_str(&crate::style::faint(
        "\nCPU NOW gives cores in use. SINCE gives the time since the job was queued, \
         started or stopped.",
    ));
    out.push('\n');

    if let Some(view) = view {
        let above = view.scroll;
        let below = total.saturating_sub(view.scroll + view.body_rows);
        let window = match (above, below) {
            (0, 0) => String::new(),
            (a, b) => format!("{a} above, {b} below.  "),
        };
        out.push_str(&format!(
            "{window}j/k move   x stop   c cancel   i info   q quit\n"
        ));
    } else if tty {
        out.push_str("Press q to stop.\n");
    }
    out
}

fn info_text(job: &JobStatus) -> String {
    let command = job
        .command
        .iter()
        .map(|a| crate::job::safe_name(a))
        .collect::<Vec<_>>()
        .join(" ");
    let cwd = crate::job::safe_name(&job.cwd);
    let locks = if job.locks.is_empty() {
        String::new()
    } else {
        format!(
            "  locks {}",
            job.locks
                .iter()
                .map(|n| crate::job::safe_name(n))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!("\n{}  {}\n{}{}\n", job.id, command, cwd, locks)
}

fn prompt_text(view: &View, selected: Option<&JobStatus>) -> Option<String> {
    let prompt = view.prompt.as_ref()?;
    let id = match prompt {
        Prompt::Stop(id) | Prompt::LeaveQueue(id) => *id,
    };
    let name = selected
        .filter(|j| j.id == id)
        .map(|j| j.display_name())
        .unwrap_or_else(|| id.to_string()[..8].to_string());
    let short = &id.to_string()[..8];
    Some(match prompt {
        Prompt::Stop(_) => {
            format!("Stop the job {short} {name}? Press y to stop, or n to keep it.")
        }
        Prompt::LeaveQueue(_) => {
            format!("Take {short} {name} out of the queue? Press y to cancel, or n to keep it.")
        }
    })
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
    if let Some(id) = view.prompt.as_ref().map(|p| match p {
        Prompt::Stop(id) | Prompt::LeaveQueue(id) => *id,
    }) {
        let stop = matches!(view.prompt, Some(Prompt::Stop(_)));
        match key {
            Key::Char(b'y') | Key::Char(b'Y') => {
                view.prompt = None;
                view.message = Some(if stop { act_stop(id) } else { act_cancel(id) });
                view.need_refresh = true;
                view.dirty = true;
            }
            Key::Char(b'n') | Key::Char(b'N') | Key::Esc => {
                view.prompt = None;
                view.dirty = true;
            }
            Key::Char(b'q') | Key::Char(b'Q') => return true,
            _ => {}
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
            view.dirty = true;
        }
        Key::Char(b'x') | Key::Char(b'K') => ask_stop(view, jobs),
        Key::Char(b'c') => ask_cancel(view, jobs),
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
    if view.selected.is_none_or(|id| !ids.iter().any(|x| *x == id)) {
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
        // Name the cause, and not the symptom. This column holds one short
        // line, and the reader must learn what to correct: the claim.
        JobState::Oom => "the memory claim was too small".to_string(),
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
            false,
            Some(&mut view),
            Some(20),
        );
        assert!(
            page.lines().count() <= 20,
            "the page has {} lines: {page}",
            page.lines().count()
        );
        assert!(
            page.contains("below"),
            "the page must say that more jobs exist: {page}"
        );
        assert!(page.contains("j/k move"), "the keys are missing: {page}");
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
        handle_key(&mut view, Key::Char(b'x'), &[running.clone()]);
        assert!(matches!(view.prompt, Some(Prompt::Stop(id)) if id == running.id));

        let mut queued = job(JobState::Queued, 1, 1 << 20);
        queued.started_at = None;
        view.prompt = None;
        view.selected = Some(queued.id);
        handle_key(&mut view, Key::Char(b'x'), &[queued.clone()]);
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
    fn i_shows_the_command_of_the_selected_job() {
        let mut j = job(JobState::Running, 1, 1 << 20);
        j.command = vec!["uv".into(), "run".into(), "train.py".into()];
        j.cwd = "workdir".into();
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
            false,
            Some(&mut view),
            Some(24),
        );
        assert!(page.contains("uv run train.py"), "got: {page}");
        assert!(page.contains("workdir"), "got: {page}");
    }
}
