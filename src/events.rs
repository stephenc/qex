//! This module holds the event stream of the coordinator.
//!
//! # The fault that this module removes
//!
//! An agent that drives twenty jobs asked about each job, one job at a time.
//! Twenty commands gave twenty answers, and the agent then asked again. The
//! agent thus learned of a result late, it used the machine to ask, and it
//! wrote a loop with a timer — the monitor script that qex exists to remove.
//!
//! This module gives one stream instead. The reader connects one time, and it
//! receives one line for each change. It needs no timer and no second command.
//!
//! # What counts as an event
//!
//! A CHANGE OF THE RECORD OF A JOB, and nothing else.
//!
//! * The state changed. This is the minimum, and it includes the first state:
//!   a job that arrives in the queue gives an event with the state `queued` and
//!   no previous state. That event IS the admission of the job, so qex needs no
//!   separate name for it.
//! * The reason that a queued job waits changed. Without this event, an agent
//!   sees a job that says `queued` and never learns that the job waits for
//!   memory, for a lock, or for a job that failed. The reason arrives a moment
//!   AFTER the admission, because the scheduler writes it, so an agent that
//!   reads the admission event alone reads `blocked_reason: null` and knows
//!   nothing. That is the event that makes the stream usable.
//!
//! The stream carries three lines that are not job events: the header of the
//! stream, a gap, and the goodbye of the coordinator. Each one answers a
//! question that the reader cannot answer for itself. See `Event`.
//!
//! # The stream reports what the coordinator holds
//!
//! Each event carries the whole record of the job, in the form that
//! `qex status --json` gives. A reader thus needs no second command to learn
//! the exit code, the measured use or the cause of a failure.
//!
//! The events come from ONE function, `publish_changes`, which compares the
//! records with the records that it reported before. Every path that changes a
//! job — the queue, the scheduler, `qex cancel`, `qex kill`, the supervisor —
//! thus gives an event, and a new path gives one as well with no change here.
//! The stream and `qex list` can never disagree, because they read one map.
//!
//! # The reader that does not read
//!
//! The coordinator holds the last `RETAINED` events in a ring. It never waits
//! for a reader, and its memory does not grow with a reader that stops reading.
//! When the ring passes a reader, the reader receives a `gap` line that COUNTS
//! the events that it lost. A silent gap is worse than a reported one: an agent
//! that loses the line "the job failed" and hears nothing waits for ever.
//!
//! # The coordinator still retires
//!
//! An attached reader does NOT hold the coordinator open. The coordinator stops
//! when no job operates and no command arrived for the idle time, exactly as
//! before. A stream that keeps a coordinator alive for ever is a leak.
//!
//! The reader hears the retirement: the coordinator writes a `bye` line before
//! it closes the socket. A stream that dies in silence under its reader is a
//! fault, because the reader cannot tell it from a broken socket.

use crate::daemon::{Coordinator, State};
use crate::job::{JobState, JobStatus};
use crate::proto::Response;
use crate::sys;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The number of events that the coordinator keeps.
///
/// This number is the memory limit of the stream. Each event holds one job
/// record, which is about one kilobyte, so the ring costs about half of one
/// megabyte and it never grows. A reader that stops reading thus costs the
/// coordinator nothing.
pub const RETAINED: usize = 512;

/// The name of the variable that changes the size of the ring. The tests use it.
///
/// A test of the gap line must make the coordinator drop events. With the usual
/// size that test needs more than five hundred events, and a test that takes
/// minutes is a test that nobody runs.
const RETAINED_VAR: &str = "QEX_EVENTS_RETAINED";

/// The time between two tests of the log, for a reader that waits.
///
/// The coordinator signals the condition variable at each change, so this value
/// is a guard only: it is the longest time between the stop of the coordinator
/// and the `bye` line that the reader must receive.
const POLL: Duration = Duration::from_millis(250);

/// The longest time that one write to a reader may take.
///
/// A reader that never reads fills the socket buffer, and the write then waits
/// for ever. The ring already protects the memory of the coordinator, so this
/// limit protects the thread and the file handle only. qex closes such a
/// connection, and the reader meets the end of the stream.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the reader wants the stream to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    /// Every event that the coordinator still holds, then the new events.
    Start,
    /// The new events only.
    Now,
    /// The events after this sequence number.
    ///
    /// An agent that stops and starts again gives the last number that it read.
    /// It thus loses nothing, which is the reason for the numbers.
    After(u64),
}

/// Says why one job event exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// The job moved to a different state.
    State,
    /// The job stays in the queue, and the reason that it waits changed.
    Reason,
}

/// One line of the stream.
///
/// Each line is one JSON object, and the field `event` gives its type. A reader
/// that meets a type that it does not know must ignore that line and continue;
/// a later version of qex can add a type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The first line of every stream.
    ///
    /// It names the coordinator, so a reader can see that the coordinator
    /// restarted and that the sequence numbers start again. Without this line,
    /// a reader that keeps a number cannot know which coordinator gave it.
    Stream {
        time: u64,
        version: String,
        pid: i32,
        /// The time when this coordinator started. It identifies the numbers.
        coordinator_started_at: u64,
        /// The oldest event that the coordinator still holds.
        first_seq: u64,
        /// The newest event that the coordinator holds.
        last_seq: u64,
    },
    /// The record of one job changed.
    Job {
        seq: u64,
        time: u64,
        id: uuid::Uuid,
        name: String,
        state: JobState,
        /// The state before this event, or `null` for the first event of a job.
        previous: Option<JobState>,
        change: Change,
        /// The whole record, as `qex status --json` gives it.
        job: Box<JobStatus>,
    },
    /// The reader lost events, and this line counts them.
    Gap {
        time: u64,
        /// The number of events that the reader did not receive.
        missed: u64,
        /// The sequence number of the next event that the reader receives.
        next_seq: u64,
        reason: String,
    },
    /// The coordinator stops now.
    Bye { time: u64, reason: String },
}

/// The events that the coordinator holds, and the state that it reported.
pub struct EventLog {
    /// The last `RETAINED` events with their numbers, oldest first.
    ring: VecDeque<(u64, Event)>,
    /// The number for the next event. The first event has the number 1.
    next_seq: u64,
    /// The number of events that left the ring.
    dropped: u64,
    /// The state and the queue reason that the log reported for each job.
    reported: BTreeMap<uuid::Uuid, (JobState, Option<String>)>,
    /// The number of connections that read the stream now.
    readers: usize,
    /// The number of events that the ring holds.
    capacity: usize,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self::with_capacity(
            std::env::var(RETAINED_VAR)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(RETAINED),
        )
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ring: VecDeque::new(),
            next_seq: 1,
            dropped: 0,
            reported: BTreeMap::new(),
            readers: 0,
            capacity,
        }
    }

    /// The number of the oldest event that the coordinator holds.
    ///
    /// For an empty ring this is the number of the next event, so a reader that
    /// starts there loses nothing.
    pub fn first_seq(&self) -> u64 {
        self.ring.front().map(|(s, _)| *s).unwrap_or(self.next_seq)
    }

    /// The number of the newest event that the coordinator holds.
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn push(&mut self, make: impl FnOnce(u64) -> Event) {
        let seq = self.next_seq;
        let event = make(seq);
        self.next_seq += 1;
        if self.ring.len() == self.capacity {
            self.ring.pop_front();
            self.dropped += 1;
        }
        self.ring.push_back((seq, event));
    }

    /// Gives the events after the number `after`, with their numbers.
    pub fn after(&self, after: u64) -> Vec<(u64, Event)> {
        self.ring
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect()
    }
}

impl State {
    /// Makes one event for each job record that changed.
    ///
    /// This function is the ONLY writer of the stream. Call it while the lock is
    /// held, at every place that changes a job. It compares and it writes
    /// nothing when nothing changed, so a call too many costs a comparison and
    /// gives no repeated line.
    pub fn publish_changes(&mut self) {
        let now = sys::now_secs();

        for (id, job) in self.jobs.iter() {
            let state = job.status.state;
            // The reason belongs to a job that waits. A job that started or
            // stopped carries no reason, and a comparison with the reason of
            // its queued period would give an event with no information.
            let reason = if state == JobState::Queued {
                job.status.blocked_reason.clone()
            } else {
                None
            };

            let previous = self.events.reported.get(id).cloned();
            let change = match &previous {
                None => Change::State,
                Some((s, _)) if *s != state => Change::State,
                Some((_, r)) if *r != reason => Change::Reason,
                Some(_) => continue,
            };

            let status = job.status.clone();
            let name = status.name.clone();
            let previous_state = previous.map(|(s, _)| s);
            self.events.push(|seq| Event::Job {
                seq,
                time: now,
                id: *id,
                name,
                state,
                previous: previous_state,
                change,
                job: Box::new(status),
            });
            self.events.reported.insert(*id, (state, reason));
        }

        // Forget a job that `qex clean` deleted. Without this step the map
        // grows for as long as the coordinator operates.
        //
        // The removal gives NO event. A record that a reader deleted itself is
        // not news, and an id names one job for ever, so no later event can
        // arrive for it.
        if self.events.reported.len() > self.jobs.len() {
            self.events
                .reported
                .retain(|id, _| self.jobs.contains_key(id));
        }
    }
}

/// Decrements the count of readers, whatever stops the stream.
///
/// A write to a reader that went away gives an error, and the stream must not
/// leave the count high: the coordinator waits for that count when it retires.
struct ReaderGuard(Arc<Coordinator>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.events.readers = state.events.readers.saturating_sub(1);
    }
}

/// Writes the stream to one connection. Gives control back when the stream ends.
///
/// The lines travel in the framing of the protocol, one `Response` for each
/// line. qex does not open a second socket and does not invent a second format.
pub fn stream(
    coord: &Arc<Coordinator>,
    out: &mut std::os::unix::net::UnixStream,
    cursor: Cursor,
) -> anyhow::Result<()> {
    // A reader that does not read must not hold this thread for ever.
    out.set_write_timeout(Some(WRITE_TIMEOUT)).ok();

    let mut lead: Vec<Event> = Vec::new();
    let mut next: u64;

    {
        let mut state = coord.state.lock().unwrap();
        // Report what the coordinator holds now, so a reader that connects
        // after a change still receives that change.
        state.publish_changes();
        state.events.readers += 1;

        let first = state.events.first_seq();
        let last = state.events.last_seq();
        let now = sys::now_secs();

        lead.push(Event::Stream {
            time: now,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id() as i32,
            coordinator_started_at: state.started_at,
            first_seq: first,
            last_seq: last,
        });

        next = match cursor {
            Cursor::Start => {
                // The coordinator holds the last events only. Say how many it
                // already dropped, so "everything that you have" never reads as
                // "everything that happened".
                if state.events.dropped() > 0 {
                    lead.push(Event::Gap {
                        time: now,
                        missed: state.events.dropped(),
                        next_seq: first,
                        reason: "the coordinator keeps the last events only, and these events \
                                 left it before you connected"
                            .to_string(),
                    });
                }
                first
            }
            Cursor::Now => last + 1,
            Cursor::After(n) if n > last => {
                // The number comes from a different coordinator. The records of
                // the jobs continue over a restart; the events do not.
                lead.push(Event::Gap {
                    time: now,
                    missed: 0,
                    next_seq: first,
                    reason: format!(
                        "you asked for the events after {n}, and this coordinator (pid {}, \
                         started at {}) holds {first} to {last}. It started after your number, \
                         so the stream continues with what it holds.",
                        std::process::id(),
                        state.started_at
                    ),
                });
                first
            }
            Cursor::After(n) if n + 1 < first => {
                lead.push(Event::Gap {
                    time: now,
                    missed: first - (n + 1),
                    next_seq: first,
                    reason: "the coordinator keeps the last events only, and these events left \
                             it before you connected"
                        .to_string(),
                });
                first
            }
            Cursor::After(n) => n + 1,
        };
    }

    let _guard = ReaderGuard(Arc::clone(coord));

    for event in lead {
        send(out, &event)?;
    }

    loop {
        let (batch, gap, stopping) = {
            let state = coord.state.lock().unwrap();
            let first = state.events.first_seq();

            // The ring passed this reader while it wrote. Count the loss and
            // continue from the oldest event that the coordinator still holds.
            let gap = if next < first {
                Some(Event::Gap {
                    time: sys::now_secs(),
                    missed: first - next,
                    next_seq: first,
                    reason: "you did not read the stream fast enough, and the coordinator \
                             dropped these events. Read the stream in a loop, and do the work \
                             in a different thread or process."
                        .to_string(),
                })
            } else {
                None
            };
            if gap.is_some() {
                next = first;
            }

            (state.events.after(next - 1), gap, state.stop)
        };

        if let Some(event) = gap {
            send(out, &event)?;
        }
        for (seq, event) in batch {
            next = seq + 1;
            send(out, &event)?;
        }

        if stopping {
            send(
                out,
                &Event::Bye {
                    time: sys::now_secs(),
                    reason: "the coordinator stops, because no job operates and no command \
                             arrived for the idle time. The records of the jobs stay on the \
                             disk. The next qex command starts a coordinator."
                        .to_string(),
                },
            )?;
            return Ok(());
        }

        // Sleep until something changes. This thread uses no CPU time while it
        // waits, and the coordinator signals it at each change.
        let state = coord.state.lock().unwrap();
        let _ = coord.changed.wait_timeout(state, POLL).unwrap();
    }
}

/// Writes one line, and sends it now.
///
/// The reader is a program that waits for the line. A line that stays in a
/// buffer is a line that did not arrive, and the reader then waits for a change
/// that already happened.
fn send(out: &mut std::os::unix::net::UnixStream, event: &Event) -> anyhow::Result<()> {
    let response = Response::Event {
        event: Box::new(event.clone()),
    };
    let mut line = serde_json::to_string(&response)?;
    line.push('\n');
    out.write_all(line.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Waits for the readers of the stream to receive the goodbye.
///
/// The coordinator calls this function when it stops. Without it, the process
/// ends while the `bye` line is still in a buffer, and every reader meets a
/// socket that closed with no reason.
///
/// The wait has a limit. A reader that does not read must not stop the exit of
/// the coordinator.
pub fn wait_for_readers(coord: &Arc<Coordinator>, limit: Duration) {
    coord.changed.notify_all();
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if coord.state.lock().unwrap().events.readers == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: u64) -> Event {
        Event::Bye {
            time: seq,
            reason: String::new(),
        }
    }

    /// Every line must fit one line of JSON, and it must survive a round trip.
    /// A reader splits the stream on the newline character.
    #[test]
    fn each_event_fits_one_line_of_json() {
        let lines = [
            Event::Stream {
                time: 1,
                version: "0.8.0".into(),
                pid: 7,
                coordinator_started_at: 1,
                first_seq: 1,
                last_seq: 3,
            },
            Event::Gap {
                time: 2,
                missed: 4,
                next_seq: 9,
                reason: "a reason with \"quotation marks\"\nand a newline".into(),
            },
            Event::Bye {
                time: 3,
                reason: "the coordinator stops".into(),
            },
        ];
        for line in lines {
            let text = serde_json::to_string(&line).unwrap();
            assert!(!text.contains('\n'), "an event must fit one line: {text}");
            let back: Event = serde_json::from_str(&text).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), text);
        }
    }

    /// The ring must hold the last events only, and it must count the events
    /// that left it. A count that is wrong makes a gap line that lies.
    #[test]
    fn the_ring_holds_the_last_events_and_counts_the_others() {
        let mut log = EventLog::with_capacity(RETAINED);
        assert_eq!(log.first_seq(), 1, "an empty log starts at the next number");
        assert_eq!(log.last_seq(), 0);

        for _ in 0..RETAINED {
            log.push(event);
        }
        assert_eq!(log.first_seq(), 1);
        assert_eq!(log.last_seq(), RETAINED as u64);
        assert_eq!(log.dropped(), 0);

        // Ten more events push ten events out of the ring.
        for _ in 0..10 {
            log.push(event);
        }
        assert_eq!(log.dropped(), 10);
        assert_eq!(log.first_seq(), 11);
        assert_eq!(log.last_seq(), RETAINED as u64 + 10);
        assert_eq!(log.after(0).len(), RETAINED);
    }

    /// A reader asks for the events after a number, and it must receive each
    /// event after that number and no event before it.
    #[test]
    fn a_reader_receives_the_events_after_its_number_only() {
        let mut log = EventLog::with_capacity(RETAINED);
        for _ in 0..5 {
            log.push(event);
        }
        let got: Vec<u64> = log.after(2).iter().map(|(seq, _)| *seq).collect();
        assert_eq!(got, vec![3, 4, 5]);
        assert!(
            log.after(5).is_empty(),
            "a reader that is current gets nothing"
        );
    }

    /// The numbers must never repeat. A reader keeps the last number, and a
    /// repeated number would make it lose an event after a restart.
    #[test]
    fn the_numbers_never_repeat() {
        let mut log = EventLog::with_capacity(RETAINED);
        let mut seen = Vec::new();
        for _ in 0..(RETAINED + 20) {
            log.push(event);
            seen.push(log.last_seq());
        }
        let mut sorted = seen.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "a number repeated");
        assert!(seen.windows(2).all(|w| w[1] == w[0] + 1));
    }

    fn one_job_state() -> State {
        let spec = crate::spec::JobSpec {
            id: uuid::Uuid::new_v4(),
            name: "t".into(),
            cwd: "/".into(),
            command: vec!["true".into()],
            env: Default::default(),
            cpu: 1,
            mem: 1 << 20,
            timeout: None,
            tags: vec![],
            priority: 0,
            env_capture: crate::config::EnvCapture::None,
            claim_source: "explicit".into(),
            group: None,
            group_name: None,
            locks: vec![],
            retries: 0,
            needs: vec![],
            after: vec![],
            submitted_at: 0,
        };
        let status = JobStatus::new(&spec);
        let mut jobs = BTreeMap::new();
        jobs.insert(
            spec.id,
            crate::daemon::Job {
                spec,
                status,
                supervisor_pid: None,
            },
        );
        State {
            cfg: Default::default(),
            jobs,
            queue: Vec::new(),
            last_contact: Instant::now(),
            idle_since: None,
            next_sequence: 1,
            started_at: 0,
            events: EventLog::with_capacity(RETAINED),
            stop: false,
        }
    }

    /// One change must give ONE line, and a record that did not change must
    /// give none.
    ///
    /// Every path that changes a job calls this function, and several paths
    /// call it for one change. A line for each call would give an agent the
    /// same result many times, and an agent that acts on each line would do the
    /// work again.
    #[test]
    fn each_change_gives_one_line_and_a_repeat_gives_none() {
        let mut state = one_job_state();
        let id = *state.jobs.keys().next().unwrap();

        state.publish_changes();
        assert_eq!(state.events.last_seq(), 1, "the admission must give a line");
        state.publish_changes();
        state.publish_changes();
        assert_eq!(state.events.last_seq(), 1, "a repeat must give no line");

        match &state.events.after(0)[0].1 {
            Event::Job {
                state,
                previous,
                change,
                ..
            } => {
                assert_eq!(*state, JobState::Queued);
                assert_eq!(
                    *previous, None,
                    "the first line of a job has no previous state"
                );
                assert_eq!(*change, Change::State);
            }
            other => panic!("expected a job event, got {other:?}"),
        }

        // The reason that a queued job waits is a change of its own. Without
        // this line an agent sees `queued` and never learns what the job waits
        // for, because the scheduler writes that reason after the admission.
        state.jobs.get_mut(&id).unwrap().status.blocked_reason = Some("waits for memory".into());
        state.publish_changes();
        assert_eq!(state.events.last_seq(), 2);
        match &state.events.after(1)[0].1 {
            Event::Job { change, job, .. } => {
                assert_eq!(*change, Change::Reason);
                assert_eq!(job.blocked_reason.as_deref(), Some("waits for memory"));
            }
            other => panic!("expected a job event, got {other:?}"),
        }

        state.jobs.get_mut(&id).unwrap().status.state = JobState::Running;
        state.publish_changes();
        assert_eq!(state.events.last_seq(), 3);
        match &state.events.after(2)[0].1 {
            Event::Job {
                state,
                previous,
                change,
                ..
            } => {
                assert_eq!(*state, JobState::Running);
                assert_eq!(*previous, Some(JobState::Queued));
                assert_eq!(*change, Change::State);
            }
            other => panic!("expected a job event, got {other:?}"),
        }

        // A job that `qex clean` deleted gives no line, and it leaves nothing
        // behind. Without the second rule, the map grows for as long as the
        // coordinator operates.
        state.jobs.remove(&id);
        state.publish_changes();
        assert_eq!(state.events.last_seq(), 3, "a deleted record gives no line");
        assert!(state.events.reported.is_empty());
    }

    /// The cursor must survive the wire. The coordinator reads it from JSON.
    #[test]
    fn each_cursor_form_survives_the_wire() {
        for c in [Cursor::Start, Cursor::Now, Cursor::After(42)] {
            let text = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<Cursor>(&text).unwrap(), c);
        }
        assert_eq!(serde_json::to_string(&Cursor::Start).unwrap(), "\"start\"");
        assert_eq!(
            serde_json::to_string(&Cursor::After(3)).unwrap(),
            "{\"after\":3}"
        );
    }
}
