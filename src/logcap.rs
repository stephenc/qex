//! This module holds the limit on the output of one stream of one job.
//!
//! A job wrote 386MB of standard output in a review, and nothing stopped it.
//! qex is made to be started and left, and that is the moment when nobody sees
//! a disk fill. The same disk holds `status.json`, so output with no limit can
//! destroy the record of every job on the machine.
//!
//! # The limit operates while the job writes
//!
//! The supervisor reads the output of the job through a pipe and writes it
//! here. It does not give the file to the job and correct the file afterwards.
//! A correction afterwards lets the disk fill first, and a full disk is the
//! fault that this module removes.
//!
//! # The reader keeps both ends
//!
//! The head of the output holds the start: the version, the configuration and
//! the arguments. The tail holds the failure. A reader who loses either end
//! loses the reason that the reader opened the file, so qex keeps both ends and
//! writes a line between them that says how much went.
//!
//! # The tail stays on the disk, and not in the memory
//!
//! The supervisor puts itself in the cgroup of the job, so memory that the
//! supervisor holds counts against the memory claim of the job. A tail of 24MB
//! in the memory of the supervisor would thus stop a job that operates at its
//! claim, and `qex status` would show an out-of-memory event that the job did
//! not cause. qex keeps the tail in a circular file: the file has a fixed size,
//! the writer returns to the start of it when it reaches the end, and this
//! module holds one buffer of 64KB in the memory.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The number of bytes that this module moves in one operation.
const CHUNK: usize = 64 * 1024;

/// The space that qex keeps in the limit for the two notes that it writes.
const NOTE_ROOM: u64 = 2048;

/// The smallest limit that qex accepts.
///
/// The limit must hold a head, a tail and the two notes. A limit of a few
/// hundred bytes holds nothing, and a user who writes one has an intention that
/// the file cannot satisfy. The configuration refuses such a value and says so.
pub const MIN_LIMIT: u64 = 16 * 1024;

/// The mark at the start of each line that qex itself writes into a log file.
///
/// A reader searches for this text, and `qex logs --grep` finds it.
pub const MARK: &str = "[qex]";

/// What qex removed from one stream of one job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    pub bytes: u64,
    pub lines: u64,
}

/// The parts of the limit.
#[derive(Debug, Clone, Copy)]
struct Parts {
    /// The bytes that the file keeps from the start of the output, after the
    /// output passes the limit.
    head: u64,
    /// The bytes that the circular file keeps from the end of the output.
    tail: u64,
    /// The bytes that go into the file itself before qex removes anything.
    fill: u64,
    /// The limit itself, for the notes that qex writes.
    max: u64,
}

/// Divides the limit between the head, the tail and the notes.
///
/// The head, the tail and the notes together are the limit. The head file and
/// the circular file are the only files, so the disk that one stream uses stays
/// below the limit at each moment, and not at the end only.
///
/// The tail is three times the head. The head needs the start of the output
/// only: a banner, the configuration and the first commands. The failure is at
/// the end, and the reader usually needs more of it.
///
/// # Why `fill` is not the same as `head`
///
/// qex removes nothing until the output passes the limit. A job that writes
/// less than `max_bytes` thus keeps every byte, in one piece, with no note.
/// This rule also holds for a second attempt, which adds to the same file: a
/// retry of a job that wrote 349KB against a limit of 1MB must not lose that
/// output, and an earlier version cut it at `head` and wrote a note that said
/// something untrue.
fn parts(max: u64) -> Parts {
    let room = NOTE_ROOM.min(max / 8);
    let head = max / 4;
    Parts {
        head,
        tail: max.saturating_sub(head + room).max(1),
        fill: max.saturating_sub(room).max(head),
        max,
    }
}

/// Writes one stream of one job, with a limit on the bytes that reach the disk.
pub struct CapWriter {
    file: File,
    /// `None` means that no limit operates.
    parts: Option<Parts>,
    /// The bytes in the file that a reader opens.
    head_len: u64,
    /// The bytes that went to the circular file.
    overflow_bytes: u64,
    /// The lines that went to the circular file.
    overflow_lines: u64,
    dropped: Dropped,
    tail: Option<Ring>,
    /// True after the output passed the limit one time.
    ///
    /// This flag is not the same test as `tail.is_some()`. A machine that
    /// refuses the circular file leaves no ring, and a test of the ring then
    /// starts the same operation again for each part of the output: the file
    /// received one note for each 64KB, and it grew with the output that the
    /// limit had to stop.
    overflowing: bool,
    tail_path: PathBuf,
    /// The first write fault, for the log of the supervisor.
    ///
    /// A job must never fail because of this module. A disk that refuses a
    /// write is a fault of the machine, and the job continues.
    fault: Option<String>,
}

impl CapWriter {
    /// Makes a writer for one open log file.
    ///
    /// `existing` is the size of that file now. A second attempt of a job adds
    /// to the file, and those bytes belong to the same limit.
    pub fn new(path: &Path, file: File, existing: u64, max_bytes: Option<u64>) -> Self {
        Self {
            file,
            parts: max_bytes.map(parts),
            head_len: existing,
            overflow_bytes: 0,
            overflow_lines: 0,
            dropped: Dropped::default(),
            tail: None,
            overflowing: false,
            tail_path: tail_path(path),
            fault: None,
        }
    }

    /// Writes one part of the output.
    pub fn write(&mut self, buf: &[u8]) {
        let Some(parts) = self.parts else {
            self.push_head(buf);
            return;
        };

        let mut rest = buf;
        // The bytes go into the file itself until the output passes the limit.
        // qex removes nothing before that moment, so output that fits in the
        // limit stays complete and in one piece.
        //
        // After that moment the head is closed for ever. Without the test of
        // `overflowing`, the cut to the head budget would make room, the next
        // bytes would fill the file a second time, and the file would hold the
        // middle of the output in place of its end.
        if !self.overflowing && self.head_len < parts.fill {
            let room = (parts.fill - self.head_len) as usize;
            let take = room.min(rest.len());
            self.push_head(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() {
            return;
        }
        self.push_overflow(rest);
    }

    /// Writes the head, and counts the bytes that reached the file.
    fn push_head(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        match self.file.write_all(data) {
            Ok(()) => self.head_len += data.len() as u64,
            Err(e) => self.record_fault(&format!("writing the output of the job: {e}")),
        }
    }

    /// Puts the bytes above the limit in the circular file.
    fn push_overflow(&mut self, data: &[u8]) {
        if !self.overflowing {
            self.start_overflow();
        }
        self.overflow_bytes += data.len() as u64;
        self.overflow_lines += count_lines(data);

        let result = match &mut self.tail {
            Some(ring) => ring.write(data),
            None => Ok(()),
        };
        if let Err(e) = result {
            self.record_fault(&format!("writing the last part of the output: {e}"));
        }
    }

    /// Prepares the file for a stream that is above the limit.
    ///
    /// This function writes a note into the file at that moment. A reader who
    /// opens the file while the job operates must not believe that the job
    /// wrote no more, and `qex logs --follow` gives this line as it arrives.
    fn start_overflow(&mut self) {
        let Some(parts) = self.parts else { return };
        // This operation happens one time for each stream, whatever follows.
        self.overflowing = true;

        // An earlier attempt of this job can have left more than the head in
        // the file. The limit belongs to the stream and not to the attempt, so
        // those bytes go now. The line count is exact, because this function
        // reads the part that it removes.
        //
        // The cut falls at a byte, and the output has lines, so qex moves the
        // cut back to the last line end. THIS IS THE SAME RULE AS THE TRIM OF
        // THE LAST PART, at the other end of the same loss. A head that stops
        // at a byte ends with a fragment of a line: the measured example wrote
        // the lines `1` to `500000`, and its head ended `3498` and then `3`,
        // which a reader takes for a line that the job wrote.
        let mut fragment = false;
        if self.head_len > parts.head {
            // `None` says that the cut back costs too much, in the same way as
            // the trim of the last part. The head then keeps the fragment, and
            // the note below says that it is not a whole line.
            let cut = head_cut(&mut self.file, parts.head).unwrap_or(None);
            fragment = cut.is_none();
            let keep = cut.unwrap_or(parts.head);
            let extra = self.head_len - keep;
            let lines = count_lines_in(&mut self.file, keep, extra).unwrap_or(0);
            match self.file.set_len(keep) {
                Ok(()) => {
                    self.dropped.bytes += extra;
                    self.dropped.lines += lines;
                    self.head_len = keep;
                }
                Err(e) => self.record_fault(&format!("cutting the earlier output: {e}")),
            }
            if let Err(e) = self.file.seek(SeekFrom::End(0)) {
                self.record_fault(&format!("moving to the end of the output: {e}"));
            }
        }

        // After the cut back the head ends at a line end, so the note needs no
        // line end before it. With a fragment, one line end separates the
        // fragment from the note, and the note says what the fragment is.
        let mut note = String::new();
        if fragment {
            note.push_str(&format!(
                "\n{MARK} The line above is not complete. qex removed the output after it.\n"
            ));
        }
        note.push_str(&format!(
            "{MARK} The output of this job reached the limit `[logs] max_bytes` = {}. \
             qex keeps the last part of the output beside this file. It writes that part \
             here when the job stops.\n",
            crate::units::format_size(parts.max)
        ));
        if let Err(e) = self.file.write_all(note.as_bytes()) {
            self.record_fault(&format!("writing the note about the limit: {e}"));
        }

        match Ring::create(&self.tail_path, parts.tail) {
            Ok(ring) => self.tail = Some(ring),
            Err(e) => self.record_fault(&format!("making the file for the last output: {e}")),
        }
    }

    /// Completes the file, and gives what qex removed.
    ///
    /// This function writes the line that says how much went, and then the
    /// tail. A reader thus finds the start of the output, one line that names
    /// the loss, and the end of the output, in that order.
    pub fn finish(mut self) -> Dropped {
        let (Some(ring), Some(parts)) = (self.tail.take(), self.parts) else {
            // The output passed the limit, and there is no circular file. The
            // machine refused that file, so those bytes reached no disk at all.
            // The count must say so: a record that says that nothing went, for
            // a file that is not complete, is worse than no record.
            self.dropped.bytes += self.overflow_bytes;
            self.dropped.lines += self.overflow_lines;

            // THE FILE MUST NOT KEEP A PROMISE THAT IT CANNOT KEEP. The note
            // that `start_overflow` wrote says that qex holds the last part of
            // the output beside this file, and that it writes that part here
            // when the job stops. On this path there is no such file and no
            // last part, so the reader who stops at the end of this file must
            // learn it here. Measured with a directory in the place of
            // `stdout.log.tail`: the file ended with that promise and gave no
            // count at all.
            if let Some(parts) = self.parts.filter(|_| self.overflowing) {
                let note = format!(
                    "{MARK} ---- {} and {} line(s) of the output are not in this file ----\n\
                     {MARK} qex could not make the file for the last part of the output, so \
                     the last part is not here. The limit is `[logs] max_bytes` = {}. Read \
                     `supervisor.log` in this directory for the fault of the machine.\n",
                    crate::units::format_size(self.dropped.bytes),
                    self.dropped.lines,
                    crate::units::format_size(parts.max),
                );
                if let Err(e) = self.file.write_all(note.as_bytes()) {
                    self.record_fault(&format!("writing the line about the removed output: {e}"));
                }
            }
            self.file.flush().ok();
            return self.dropped;
        };
        let mut ring = ring;

        // The tail starts in the middle of a line when qex removed bytes
        // between the head and the tail. A reader must not read that fragment
        // as a whole line, so qex removes the bytes before the first line end.
        //
        // THAT RULE HAS A LIMIT. The first line end can be far into the tail,
        // or there can be no line end at all: one JSON document of 30MB, a
        // base64 block, or a progress display that uses `\r` (dd, curl, docker,
        // apt) all give output of that form. To remove the fragment then is to
        // remove almost all of the space that the reader paid for: a measure
        // with a 64KB limit kept 13 bytes of a tail of 46KB, and the words
        // before the failure went with it.
        //
        // qex therefore removes the fragment only when it is a small part of
        // the tail. In each other case it keeps the fragment and writes a line
        // that says that the text starts in the middle of a line.
        let gap = ring.wrapped || self.dropped.bytes > 0;
        let first_end = ring.first_line_end().unwrap_or(None);
        let trim = gap && matches!(first_end, Some(n) if n <= parts.tail / 4);
        let cut_line = gap && !trim;

        // The first pass measures. The line that says how much went must be
        // before the tail, so qex needs the numbers before it writes anything.
        // Neither pass holds the tail in the memory.
        let (kept_bytes, kept_lines) = ring.walk(None, trim).unwrap_or((0, 0));
        self.dropped.bytes += self.overflow_bytes.saturating_sub(kept_bytes);
        self.dropped.lines += self.overflow_lines.saturating_sub(kept_lines);

        let mut note = format!(
            "{MARK} ---- {} and {} line(s) of the output are not in this file ----\n\
             {MARK} The limit is `[logs] max_bytes` = {}. qex kept the first {} and the last {}. \
             To keep more, make max_bytes larger in the configuration file.\n",
            crate::units::format_size(self.dropped.bytes),
            self.dropped.lines,
            crate::units::format_size(parts.max),
            crate::units::format_size(parts.head),
            crate::units::format_size(kept_bytes),
        );
        if cut_line {
            note.push_str(&format!(
                "{MARK} The text that follows starts in the middle of a line.\n"
            ));
        }
        if let Err(e) = self.file.write_all(note.as_bytes()) {
            self.record_fault(&format!("writing the line about the removed output: {e}"));
        }

        if let Err(e) = ring.walk(Some(&mut self.file), trim) {
            self.record_fault(&format!("writing the last part of the output: {e}"));
        }
        self.file.flush().ok();
        ring.remove();
        self.dropped
    }

    /// Keeps the first fault, and writes it to the log of the supervisor.
    fn record_fault(&mut self, message: &str) {
        if self.fault.is_some() {
            return;
        }
        self.fault = Some(message.to_string());
        eprintln!("qex: {message}. The job continues, and its output is not complete.");
    }
}

/// A file of a fixed size that holds the last bytes of a stream.
///
/// The writer returns to the start of the file when it reaches the end. The
/// file thus holds the last `size` bytes at each moment, and it never grows.
struct Ring {
    path: PathBuf,
    file: File,
    size: u64,
    /// The position for the next byte.
    pos: u64,
    /// True after the writer returned to the start one time.
    wrapped: bool,
}

impl Ring {
    fn create(path: &Path, size: u64) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            // The output of a job holds a token as frequently as its
            // environment, so this file uses the mode of the log file.
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
            pos: 0,
            wrapped: false,
        })
    }

    fn write(&mut self, buf: &[u8]) -> std::io::Result<()> {
        // Bytes that a later byte of the same call would cover need no write.
        let mut buf = if buf.len() as u64 > self.size {
            self.wrapped = true;
            &buf[buf.len() - self.size as usize..]
        } else {
            buf
        };

        while !buf.is_empty() {
            let room = (self.size - self.pos) as usize;
            let take = room.min(buf.len());
            self.file.seek(SeekFrom::Start(self.pos))?;
            self.file.write_all(&buf[..take])?;
            self.pos += take as u64;
            if self.pos == self.size {
                self.pos = 0;
                self.wrapped = true;
            }
            buf = &buf[take..];
        }
        Ok(())
    }

    /// Gives the order of the two parts of the file.
    fn segments(&self) -> [(u64, u64); 2] {
        if self.wrapped {
            [(self.pos, self.size - self.pos), (0, self.pos)]
        } else {
            [(0, self.pos), (0, 0)]
        }
    }

    /// Gives the position of the first line end, in the order of the output.
    ///
    /// The result is `None` when the contents hold no line end. The caller
    /// removes the incomplete first line, and it uses this position to decide
    /// if that operation costs too much. The function stops at the first line
    /// end, so the usual stream costs one read.
    fn first_line_end(&mut self) -> std::io::Result<Option<u64>> {
        let mut buf = vec![0u8; CHUNK];
        let mut seen = 0u64;
        for (start, len) in self.segments() {
            if len == 0 {
                continue;
            }
            self.file.seek(SeekFrom::Start(start))?;
            let mut left = len;
            while left > 0 {
                let want = (left as usize).min(buf.len());
                let n = self.file.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                left -= n as u64;
                if let Some(i) = buf[..n].iter().position(|b| *b == b'\n') {
                    return Ok(Some(seen + i as u64));
                }
                seen += n as u64;
            }
        }
        Ok(None)
    }

    /// Reads the contents in the order that the job wrote them.
    ///
    /// With `out`, this function also writes the contents there. It gives the
    /// number of bytes and the number of lines that it read. The caller calls
    /// it two times: one time to measure, and one time to write, and it gives
    /// the same `trim` value both times.
    ///
    /// With `trim`, this function removes the bytes before the first line end.
    /// The first line is incomplete when the writer returned to the start, and
    /// a reader must not meet one half of a line as if it were a whole line.
    fn walk(&mut self, mut out: Option<&mut File>, trim: bool) -> std::io::Result<(u64, u64)> {
        let segments = self.segments();
        let mut trim = trim;
        let mut bytes = 0u64;
        let mut lines = 0u64;
        let mut buf = vec![0u8; CHUNK];

        for (start, len) in segments {
            if len == 0 {
                continue;
            }
            self.file.seek(SeekFrom::Start(start))?;
            let mut left = len;
            while left > 0 {
                let want = (left as usize).min(buf.len());
                let n = self.file.read(&mut buf[..want])?;
                if n == 0 {
                    break;
                }
                left -= n as u64;
                let mut data = &buf[..n];
                if trim {
                    match data.iter().position(|b| *b == b'\n') {
                        Some(i) => {
                            data = &data[i + 1..];
                            trim = false;
                        }
                        None => data = &[],
                    }
                }
                bytes += data.len() as u64;
                lines += count_lines(data);
                if let Some(file) = out.as_deref_mut() {
                    file.write_all(data)?;
                }
            }
        }
        Ok((bytes, lines))
    }

    fn remove(&self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// Gives the name of the file that holds the tail of one stream.
///
/// The name is beside the log file, so `qex clean` deletes it with the record
/// of the job. The supervisor uses this function as well, to remove the file
/// when a copy did not complete.
pub fn tail_path(log: &Path) -> PathBuf {
    log.with_extension("log.tail")
}

fn count_lines(data: &[u8]) -> u64 {
    data.iter().filter(|b| **b == b'\n').count() as u64
}

/// Gives the position that makes the head end at a line end.
///
/// The head keeps `head` bytes, and that byte is in the middle of a line. This
/// function looks back for the last line end before it, so that the head holds
/// whole lines only.
///
/// The result is `None` when there is no line end near the cut. The caller then
/// keeps the fragment and writes a note, because a cut back would otherwise
/// remove the whole head of a stream with no line end — one JSON document, one
/// base64 block, or a progress display that uses `\r`. This is the rule that
/// [`CapWriter::finish`] uses at the other end of the same loss.
///
/// The function reads a window and not the head, so the memory that it uses
/// does not grow with `max_bytes`. The supervisor is in the cgroup of the job,
/// so memory that it holds counts against the claim of the job.
fn head_cut(file: &mut File, head: u64) -> std::io::Result<Option<u64>> {
    if head == 0 {
        return Ok(None);
    }
    let window = (head / 4).clamp(1, CHUNK as u64);
    let start = head - window.min(head);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (head - start) as usize];
    file.read_exact(&mut buf)?;
    Ok(buf
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| start + i as u64 + 1))
}

/// Counts the lines in one part of a file, with no large read.
fn count_lines_in(file: &mut File, start: u64, len: u64) -> std::io::Result<u64> {
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; CHUNK];
    let mut left = len;
    let mut lines = 0u64;
    while left > 0 {
        let want = (left as usize).min(buf.len());
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        left -= n as u64;
        lines += count_lines(&buf[..n]);
    }
    Ok(lines)
}

/// What one copy of one stream reports to the supervisor.
#[derive(Debug, Clone, Copy)]
pub enum Report {
    /// The job closed this stream. No more output can arrive.
    Eof,
    /// The copy is complete. This is what qex removed.
    Done(Dropped),
}

/// Reads one stream of the job and writes it through the limit.
///
/// This function operates in a thread of its own, one thread for each stream.
/// It reads until the job closes the stream. A read fault stops the copy, and
/// it does not stop the job.
///
/// `on_eof` reports the end of the output, BEFORE the tail goes into the log
/// file. The two events are separate because the supervisor gives them
/// different times: the wait for the end of the output has a short limit,
/// because a process that left the process group can hold the pipe open for
/// ever, but the copy of the tail that follows is local work, and a limit on it
/// would cut the log file of a job that did nothing wrong.
pub fn pump(mut source: impl Read, mut writer: CapWriter, on_eof: impl FnOnce()) -> Dropped {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => writer.write(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    on_eof();
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "qex-logcap-{}-{}-{name}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn log(&self) -> PathBuf {
            self.0.join("stdout.log")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn open(path: &Path) -> File {
        std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap()
    }

    /// Writes numbered lines through a writer, one line in each call.
    fn write_lines(writer: &mut CapWriter, from: usize, to: usize) {
        for i in from..=to {
            writer.write(format!("line-{i}\n").as_bytes());
        }
    }

    /// The head and the tail must both stay, and the file must say what went.
    ///
    /// A job wrote 386MB in a review. A limit that keeps the tail only loses
    /// the start-up and the configuration, and a limit that keeps the head only
    /// loses the failure. The reader needs both ends.
    #[test]
    fn the_first_lines_and_the_last_lines_stay_and_the_file_says_what_went() {
        let dir = Dir::new("both-ends");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(64 * 1024));
        write_lines(&mut w, 1, 20000);
        let dropped = w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("line-1\n"), "the first line went");
        assert!(text.contains("line-20000\n"), "the last line went");
        assert!(
            !text.contains("line-10000\n"),
            "the middle must go, and it stayed"
        );
        assert!(dropped.bytes > 0 && dropped.lines > 0, "{dropped:?}");
        assert!(
            text.contains("are not in this file"),
            "the file must say what went: {text:.400}"
        );
        assert!(
            text.contains(&dropped.lines.to_string()),
            "the file must give the number of lines"
        );

        // The file must never be larger than the limit. That promise is the
        // reason for this module.
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size <= 64 * 1024, "the file is {size} bytes");
    }

    /// The disk must stay below the limit WHILE THE JOB WRITES.
    ///
    /// A limit that qex applies after the job stops lets the disk fill first,
    /// and a full disk is the fault that this module removes. The test measures
    /// the log file and the file beside it together, at each step.
    #[test]
    fn the_disk_stays_below_the_limit_while_the_job_writes() {
        let dir = Dir::new("during");
        let path = dir.log();
        let limit = 32 * 1024;
        let mut w = CapWriter::new(&path, open(&path), 0, Some(limit));

        for i in 1..=5000 {
            w.write(format!("line-{i} aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n").as_bytes());
            if i % 100 == 0 {
                let mut total = 0;
                for entry in std::fs::read_dir(&dir.0).unwrap() {
                    total += entry.unwrap().metadata().unwrap().len();
                }
                assert!(total <= limit, "the disk holds {total} bytes at line {i}");
            }
        }
        w.finish();
        let mut total = 0;
        for entry in std::fs::read_dir(&dir.0).unwrap() {
            total += entry.unwrap().metadata().unwrap().len();
        }
        assert!(total <= limit, "the disk holds {total} bytes at the end");
    }

    /// The file beside the log file must go. A file that stays holds disk space
    /// that `qex du` cannot explain.
    #[test]
    fn the_file_that_holds_the_tail_goes_at_the_end() {
        let dir = Dir::new("cleanup");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(32 * 1024));
        write_lines(&mut w, 1, 5000);
        w.finish();

        let left: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["stdout.log".to_string()], "a file stayed");
    }

    /// Output that is below the limit must arrive complete, with no note.
    #[test]
    fn output_below_the_limit_arrives_complete() {
        let dir = Dir::new("small");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(MIN_LIMIT));
        write_lines(&mut w, 1, 20);
        let dropped = w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(dropped, Dropped::default());
        assert!(!text.contains(MARK), "qex wrote a note with no reason");
        assert_eq!(text.lines().count(), 20);
    }

    /// A limit of `None` keeps every byte. A user who turns the limit off must
    /// get the behaviour of the earlier versions.
    #[test]
    fn no_limit_keeps_every_byte() {
        let dir = Dir::new("none");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, None);
        write_lines(&mut w, 1, 5000);
        let dropped = w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(dropped, Dropped::default());
        assert_eq!(text.lines().count(), 5000);
    }

    /// A reader that opens the file WHILE the job writes must see that the job
    /// wrote more. Without this note, a file that stops at the head reads as a
    /// complete file, and the reader believes that the job stopped there.
    #[test]
    fn a_reader_sees_the_limit_before_the_job_stops() {
        let dir = Dir::new("live");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(32 * 1024));
        write_lines(&mut w, 1, 5000);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("reached the limit"),
            "the file must say that the output continues: {text:.300}"
        );
    }

    /// The limit belongs to the stream of the job, and not to one attempt.
    ///
    /// A job with `--retries` adds to the same file. Without this rule, a job
    /// with three attempts holds three times the limit on the disk.
    #[test]
    fn a_second_attempt_shares_the_limit_of_the_first() {
        let dir = Dir::new("retry");
        let path = dir.log();
        let limit = 32 * 1024;

        let mut w = CapWriter::new(&path, open(&path), 0, Some(limit));
        write_lines(&mut w, 1, 5000);
        w.finish();

        let existing = std::fs::metadata(&path).unwrap().len();
        let again = std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .open(&path)
            .unwrap();
        let mut w = CapWriter::new(&path, again, existing, Some(limit));
        write_lines(&mut w, 5001, 10000);
        let dropped = w.finish();

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size <= limit, "two attempts hold {size} bytes");
        assert!(dropped.bytes > 0, "the second attempt removed nothing");
        assert!(
            dropped.lines > 0,
            "the count of the lines must hold the lines of the first attempt"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("line-1\n"), "the start of the job went");
        assert!(text.contains("line-10000\n"), "the last line went");
    }

    /// One write that is larger than the whole tail must not lose the end of
    /// that write. A program that writes one very long line meets this path.
    #[test]
    fn one_write_that_is_larger_than_the_tail_keeps_its_end() {
        let dir = Dir::new("big-write");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(MIN_LIMIT));
        w.write(b"first\n");
        let mut big = vec![b'x'; 200 * 1024];
        big.extend_from_slice(b"\nTHE-END\n");
        w.write(&big);
        w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("first"), "the head went");
        assert!(text.contains("THE-END"), "the end of the write went");
        assert!(std::fs::metadata(&path).unwrap().len() <= MIN_LIMIT);
    }

    /// Output with no line end must keep its last part.
    ///
    /// qex removes the incomplete first line of the tail, so that a reader
    /// never meets one half of a line. An earlier version applied that rule
    /// when the tail held NO line end at all, and it then removed the whole
    /// tail: a 4GB stream of one line left the head only. One JSON document,
    /// one base64 block, or a progress display that uses `\r` (dd, curl,
    /// docker, apt) all give a stream of that form.
    #[test]
    fn output_with_no_line_end_keeps_its_last_part() {
        let dir = Dir::new("no-line-end");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(MIN_LIMIT));

        w.write(b"the start of the output, and then one very long line: ");
        for _ in 0..40 {
            w.write(&vec![b'x'; 8 * 1024]);
        }
        w.write(b"THE-VERY-END");
        let dropped = w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("the start of the output"), "the head went");
        assert!(
            text.ends_with("THE-VERY-END"),
            "the end of the output went, and the file holds the head only"
        );
        // The tail must be a true tail, and not one line of the note.
        let kept = text.rfind(MARK).map(|i| text[i..].len()).unwrap_or(0);
        assert!(kept > 1000, "the tail holds {kept} bytes only");
        assert!(dropped.bytes > 0);
        assert!(
            text.contains("middle of a line"),
            "the file must say that the last part starts in the middle of a line"
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= MIN_LIMIT);
    }

    /// A second attempt must not remove output that fits in the limit.
    ///
    /// The limit belongs to the whole stream, but qex removes nothing before
    /// the stream passes the limit. An earlier version cut the file back to one
    /// quarter of the limit at the first byte of the second attempt: a retry of
    /// a job that wrote 349KB against a limit of 1MB lost 87KB, and the file
    /// said that the output had reached the limit, which was not true.
    #[test]
    fn a_second_attempt_that_fits_the_limit_loses_nothing() {
        let dir = Dir::new("retry-fits");
        let path = dir.log();
        let limit = 1 << 20;

        let mut w = CapWriter::new(&path, open(&path), 0, Some(limit));
        write_lines(&mut w, 1, 30000);
        let first = w.finish();
        assert_eq!(first, Dropped::default(), "the first attempt fits");
        let existing = std::fs::metadata(&path).unwrap().len();
        assert!(existing < limit, "the test needs an attempt that fits");

        // The supervisor writes this mark between two attempts.
        let mut again = std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .open(&path)
            .unwrap();
        again.write_all(b"\n--- attempt 2 ---\n").unwrap();
        let existing = std::fs::metadata(&path).unwrap().len();

        let mut w = CapWriter::new(&path, again, existing, Some(limit));
        w.write(b"the second attempt\n");
        let second = w.finish();

        assert_eq!(
            second,
            Dropped::default(),
            "the second attempt removed data"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(MARK), "qex wrote a note with no reason");
        assert!(
            text.contains("line-1\n"),
            "the first attempt lost its start"
        );
        assert!(
            text.contains("line-30000\n"),
            "the first attempt lost its end"
        );
        assert!(
            text.contains("--- attempt 2 ---"),
            "the mark between the attempts went"
        );
        assert!(text.contains("the second attempt"));
    }

    /// A line end that is far into the tail must not cost the whole tail.
    ///
    /// qex removes the incomplete first line of the tail. With one line end
    /// far into the tail, that rule removed almost everything: a measure with
    /// a 64KB limit kept 13 bytes of a tail of 46KB, and the words before the
    /// failure went with them. A progress display that uses `\r` and then one
    /// line at the end gives output of that form.
    #[test]
    fn a_line_end_that_is_far_into_the_tail_does_not_cost_the_tail() {
        let dir = Dir::new("late-line-end");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(MIN_LIMIT));

        w.write(b"the start\n");
        // A progress display: many parts, and no line end at all.
        for i in 0..4000 {
            w.write(format!("\rstep {i} of 4000").as_bytes());
        }
        w.write(b"\nBUILD FAILED: the compiler stopped\n");
        w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("BUILD FAILED"), "the last line went");
        // The tail must hold the work before the failure as well, and not the
        // last line alone.
        let tail = text.rsplit(MARK).next().unwrap_or("");
        assert!(
            tail.len() > 1000,
            "the tail holds {} bytes only, so the trim removed the output",
            tail.len()
        );
        assert!(
            text.contains("middle of a line"),
            "the file must say that the last part is not a whole line"
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= MIN_LIMIT);
    }

    /// A tail that starts in the middle of a line always says so.
    ///
    /// qex removes the bytes between the head and the tail, so the first bytes
    /// of the tail are the middle of a line. THAT IS TRUE WHETHER THE CIRCULAR
    /// FILE RETURNED TO ITS START OR NOT. An earlier version tested
    /// `wrapped && dropped > 0` and thus wrote the warning in the first case
    /// only: a reader then met `AAAAAAAAAAAAAAA-994` and had no reason to doubt
    /// it.
    ///
    /// The output here passes the limit by a small quantity, so the circular
    /// file does not return to its start, and `wrapped` alone is false.
    /// A head with no line end near its cut keeps the fragment AND says so.
    ///
    /// qex moves the cut of the head back to the last line end, so that the
    /// head holds whole lines only. That rule needs the same limit as the trim
    /// of the last part: with the last line end far back, a cut back removes
    /// almost all of the head. Measured with the window removed: a head of 4096
    /// bytes became 6 bytes, and the reader lost the start-up that the head
    /// exists to keep.
    ///
    /// qex therefore keeps the fragment in that case, and the note says that
    /// the line above it is not complete. In silence the reader takes the
    /// fragment for a line that the job wrote.
    #[test]
    fn a_head_that_cannot_cut_back_keeps_its_bytes_and_says_so() {
        let dir = Dir::new("head-fragment");
        let path = dir.log();
        let parts = parts(MIN_LIMIT);
        let mut w = CapWriter::new(&path, open(&path), 0, Some(MIN_LIMIT));

        // One line end at the start, and then one line that never ends. The
        // last line end is thus far outside the window that qex looks in.
        w.write(b"the start of the job\n");
        for _ in 0..40 {
            w.write(&vec![b'x'; 8 * 1024]);
        }
        w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        let head = &text[..text.find(MARK).expect("the file must hold a note of qex")];

        // The head must keep its space. A cut back to the line end at byte 21
        // would leave the reader with that line only.
        assert!(
            head.len() as u64 > parts.head / 2,
            "the head holds {} bytes of the {} that it must hold, so the cut back removed \
             the start of the output",
            head.len(),
            parts.head
        );
        assert!(head.starts_with("the start of the job\n"));

        // The head ends in the middle of a line, so the file must say it.
        assert!(
            text.contains("The line above is not complete"),
            "the head ends with a fragment of a line, and the file does not say so: \
             {text:.400}"
        );
        assert!(std::fs::metadata(&path).unwrap().len() <= MIN_LIMIT);
    }

    #[test]
    fn a_tail_that_starts_in_the_middle_of_a_line_says_so() {
        let dir = Dir::new("mid-line");
        let path = dir.log();
        let body = "A".repeat(59);
        let mut w = CapWriter::new(&path, open(&path), 0, Some(MIN_LIMIT));
        let mut written = 0u64;
        for i in 0..380 {
            let line = format!("{body}-{i}\n");
            written += line.len() as u64;
            w.write(line.as_bytes());
        }
        w.finish();

        // THE TEST MUST MEASURE THE CASE THAT IT NAMES. The output above the
        // limit must fit in the circular file, or that file returns to its
        // start, `wrapped` is true by itself, and this test stops measuring the
        // fault. An earlier version wrote 400 lines, which is 25490 bytes and
        // 914 bytes too many.
        let parts = parts(MIN_LIMIT);
        assert!(
            written > parts.fill && written - parts.fill <= parts.tail,
            "the job wrote {written} bytes; this test needs more than {} and not more \
             than {}",
            parts.fill,
            parts.fill + parts.tail
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let after = text.rsplit(MARK).next().unwrap_or("");
        let tail: Vec<&str> = after.lines().skip(1).collect();

        // THE ASSERTION IS NOT CONDITIONAL. A test that asks the question only
        // when the answer is already wrong measures nothing: it passed against
        // the code that this test exists to refuse.
        let first = tail.first().copied().unwrap_or("");
        let whole = first.starts_with(&body) && first[body.len()..].starts_with('-');
        assert!(
            whole || text.contains("middle of a line"),
            "the first line of the last part is `{first}`, which is the end of a line that \
             the reader cannot see, and the file does not say so"
        );
    }

    /// A machine that refuses the file for the tail must still stop the output,
    /// and the count must say that the output is not complete.
    ///
    /// An earlier version tested the ring and not the state, so it wrote the
    /// note one time for each 64KB and the file grew with the output that the
    /// limit had to stop. It also reported that nothing went.
    #[test]
    fn a_tail_file_that_the_machine_refuses_still_stops_the_output() {
        let dir = Dir::new("no-ring");
        let path = dir.log();
        // A directory with the name of the file. The machine then refuses the
        // file, in the same way as a disk that is full or a mode that stops it.
        std::fs::create_dir_all(tail_path(&path)).unwrap();

        let mut written = 0u64;
        let mut lines = 0u64;
        let mut w = CapWriter::new(&path, open(&path), 0, Some(MIN_LIMIT));
        for i in 1..=20000 {
            let line = format!("line-{i}\n");
            written += line.len() as u64;
            lines += 1;
            w.write(line.as_bytes());
        }
        let dropped = w.finish();

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size <= MIN_LIMIT, "the file holds {size} bytes");

        // THE COUNT MUST BALANCE TO THE BYTE. The bytes above the limit went to
        // a file that the machine refused, so they exist nowhere; the head is
        // the only output that a reader can still get. A count that is only
        // "large enough" hides the whole question: an earlier version of this
        // test asked for `dropped.bytes >= written - kept`, and the head cut
        // alone satisfied it, so a count that dropped the overflow, and a count
        // that multiplied it, both passed.
        // The head is every byte before the first note of qex. The head ends at
        // a line end here, because the lines are short, so qex writes no line
        // end of its own before that note.
        let text = std::fs::read_to_string(&path).unwrap();
        let kept_text = &text[..text.find(MARK).expect("the file must hold a note of qex")];
        assert!(
            kept_text.ends_with('\n'),
            "the head must end at a line end, and it ends `{}`",
            &kept_text[kept_text.len().saturating_sub(12)..]
        );
        let kept = kept_text.len() as u64;
        assert_eq!(
            dropped.bytes,
            written - kept,
            "the file holds {kept} byte(s) of the {written} that the job wrote, so exactly \
             {} went, and the count says {}",
            written - kept,
            dropped.bytes
        );
        assert_eq!(
            dropped.lines,
            lines - count_lines(kept_text.as_bytes()),
            "the file holds {} line(s) of the {lines} that the job wrote, and the count says \
             that {} went",
            count_lines(kept_text.as_bytes()),
            dropped.lines
        );

        assert_eq!(
            text.matches("reached the limit").count(),
            1,
            "the note must appear one time"
        );

        // The note that qex wrote when the output passed the limit says that
        // qex holds the last part beside this file, and that it writes that
        // part here at the end. There is no such file on this path, so the
        // file must say what it does hold.
        assert!(
            text.contains("are not in this file"),
            "the file promises a last part that never arrives, and it gives no count: \
             {text:.600}"
        );
        assert!(
            text.contains("could not make the file for the last part"),
            "the file must say why the last part is missing: {text:.600}"
        );
    }

    /// With short lines, the last part must start at a WHOLE line.
    ///
    /// qex removes the bytes between the head and the last part, so the first
    /// bytes of that part are the end of a line that the reader cannot see. For
    /// the usual output — short lines — that fragment is a few bytes, so qex
    /// removes it and the reader meets whole lines only. Without that
    /// operation, the first line of the last part reads as a true line, and a
    /// reader takes `-1234` for a value or `rror: no such file` for a message.
    ///
    /// The rule has a limit, and the two tests above hold the other side of it:
    /// a fragment that is a large part of the space must stay.
    #[test]
    fn the_last_part_of_short_lines_starts_at_a_whole_line() {
        let dir = Dir::new("trim");
        let path = dir.log();
        let mut w = CapWriter::new(&path, open(&path), 0, Some(64 * 1024));
        write_lines(&mut w, 1, 20000);
        w.finish();

        let text = std::fs::read_to_string(&path).unwrap();
        // The last note of qex is the last line before the tail.
        let tail = text
            .rsplit_once(&format!("{MARK} The limit is"))
            .expect("the file must hold the note about the limit")
            .1;
        let first = tail.lines().nth(1).expect("the last part is empty");
        assert!(
            first.starts_with("line-"),
            "the last part starts with `{first}`, which is the end of a line and not a \
             whole line"
        );
        assert!(
            !text.contains("middle of a line"),
            "qex removed the fragment, so the file must not say that one stayed"
        );

        // Every line of the last part is a line that the job wrote.
        for line in tail.lines().skip(1).filter(|l| !l.is_empty()) {
            let n: u64 = line
                .strip_prefix("line-")
                .unwrap_or("")
                .parse()
                .unwrap_or_else(|_| panic!("`{line}` is not a line that the job wrote"));
            assert!((1..=20000).contains(&n), "`{line}` is not in the output");
        }
    }

    /// The parts of the limit must never be larger than the limit itself. The
    /// two files exist together while the job operates.
    #[test]
    fn the_parts_of_the_limit_fit_the_limit() {
        for max in [MIN_LIMIT, 1 << 20, 32 << 20, 1 << 30] {
            let p = parts(max);
            assert!(
                p.head + p.tail <= max,
                "the parts of {max} are larger than the limit"
            );
            assert!(p.tail > p.head, "the tail must hold more than the head");
        }
    }
}
