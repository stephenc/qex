//! This module connects the CLI to the coordinator.
//!
//! If no coordinator operates, the CLI starts one. Many CLI processes can do
//! this at the same time, so the code uses a lock file. One coordinator starts,
//! and the other CLI processes wait for it.

use crate::paths;
use crate::proto::{Request, Response};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// The maximum time to wait for a new coordinator to open its socket.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The maximum time to wait for the COORDINATOR to answer.
///
/// # Two kinds of wait, and only one of them takes a limit
///
/// A command waits for two different things, and they are not one promise:
///
/// * **The coordinator answers.** A connect, the spawn lock, and the answer to
///   a request that costs no work. A wait with no end is never correct here.
///   The coordinator answers at once, or something is wrong and the reader
///   must be told.
/// * **A job finishes.** `qex wait`, `qex status --wait`, `qex logs --follow`.
///   A wait of four hours is the POINT there, because the reader asked for it.
///
/// This limit belongs to the FIRST kind only. **Do not give the second kind a
/// limit of its own.** A ceiling on a job wait breaks the thing that qex is
/// for.
///
/// The code keeps the two apart: `call` sends one request and reads one
/// answer, so it takes this limit; `send` with `recv_opt` or `wait_readable`
/// leaves the limit to the caller that waits for a job.
///
/// The value is the limit for a coordinator that starts, so a reader who
/// learns one number learns them all.
const COORDINATOR_LIMIT: Duration = SPAWN_TIMEOUT;

/// A connection to the coordinator.
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connects to the coordinator. Starts a coordinator if none operates.
    pub fn connect() -> Result<Self> {
        Self::connect_or_explain().map_err(name_the_sandbox)
    }

    fn connect_or_explain() -> Result<Self> {
        let socket = paths::socket_path()?;

        match try_connect(&socket)? {
            Some(stream) => return Client::with_stream(stream),
            None => {}
        }

        // Take the lock before the start. Two CLI processes can arrive here at
        // the same time. The lock lets one process start the coordinator.
        let _lock = SpawnLock::acquire()?;

        // Test the socket again. A different process can start the coordinator
        // while this process waits for the lock.
        match try_connect(&socket)? {
            Some(stream) => return Client::with_stream(stream),
            None => {}
        }

        spawn_daemon()?;
        let stream = wait_for_socket(&socket, SPAWN_TIMEOUT)?;
        Client::with_stream(stream)
    }

    /// Connects to the coordinator, but does not start one.
    ///
    /// `qex wait` uses this function. If no coordinator operates, that command
    /// reads the status file of the job instead.
    pub fn connect_existing() -> Option<Self> {
        // A socket that gives NO ANSWER is not the same as NO COORDINATOR, and
        // this function must not tell the caller that nothing is there when a
        // process holds the socket. The caller continues without a coordinator,
        // because that is the only thing it can do, and the reader gets the
        // cause in one line instead of a silent answer.
        //
        // No caller of this function starts a coordinator.
        match Self::connect_existing_result() {
            Ok(client) => client,
            Err(e) => {
                eprintln!("qex: {e:#}");
                None
            }
        }
    }

    /// Connects to a coordinator that operates, and keeps the timeout as an
    /// error.
    ///
    /// `qex info --no-start` answers the question "is a coordinator there".
    /// A socket that gives no answer is not a `no`, so that command must not
    /// print `no coordinator operates`: it reports the limit instead, and the
    /// exit code says that qex stopped waiting.
    pub fn connect_existing_result() -> Result<Option<Self>> {
        let Ok(socket) = paths::socket_path() else {
            return Ok(None);
        };
        match try_connect(&socket)? {
            Some(stream) => Ok(Some(Client::with_stream(stream)?)),
            None => Ok(None),
        }
    }

    fn with_stream(stream: UnixStream) -> Result<Self> {
        let reader = BufReader::new(stream.try_clone().context("copying the socket handle")?);
        Ok(Self { stream, reader })
    }

    /// Sends one request and reads one answer, inside the limit for a
    /// coordinator.
    ///
    /// This function is the FIRST kind of wait: the coordinator answers a
    /// question that costs it no work. A command that waits for a JOB does not
    /// use this function — it sends the request and holds its own limit, so
    /// this limit never shortens a wait that a reader asked for.
    pub fn call(&mut self, request: &Request) -> Result<Response> {
        self.send(request)?;
        let previous = self.stream.read_timeout().ok().flatten();
        // A caller that set its own limit keeps it. `qex logs --follow` and the
        // reporter of a wait each hold a limit that belongs to their own work.
        if previous.is_none() {
            self.set_read_timeout(Some(COORDINATOR_LIMIT)).ok();
        }
        let answer = self.recv();
        if previous.is_none() {
            self.set_read_timeout(None).ok();
        }
        answer.map_err(|e| name_a_silent_coordinator(e, previous.is_none()))
    }

    pub fn send(&mut self, request: &Request) -> Result<()> {
        let mut line = serde_json::to_string(request).context("writing the request")?;
        line.push('\n');
        self.stream
            .write_all(line.as_bytes())
            .context("sending the request to the coordinator")?;
        self.stream.flush().ok();
        Ok(())
    }

    /// Reads one response. This function blocks until the coordinator answers.
    pub fn recv(&mut self) -> Result<Response> {
        match self.recv_opt()? {
            Some(response) => Ok(response),
            None => bail!("the coordinator closed the connection without an answer"),
        }
    }

    /// Reads one response, and gives `None` at the end of the connection.
    ///
    /// A command that reads MANY answers needs this form. The end of the
    /// connection is a normal condition for such a command, and it is not the
    /// same fault as a connection that gives no answer at all.
    pub fn recv_opt(&mut self) -> Result<Option<Response>> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .context("reading the answer of the coordinator")?;
        if n == 0 {
            return Ok(None);
        }
        serde_json::from_str(&line)
            .map(Some)
            .with_context(|| format!("reading this answer of the coordinator: {}", line.trim()))
    }

    /// Waits until the coordinator has something to say, or until the time
    /// passes. Gives `true` when an answer is ready.
    ///
    /// A command that waits for a job must stay awake for two other events: a
    /// signal from the user, and a coordinator that stops. A blocking read sees
    /// neither, because the system restarts a read that a signal interrupts.
    ///
    /// This function looks at the socket and reads nothing, so a short limit
    /// costs one system call and it cannot divide an answer in two. A short
    /// read limit cannot give that: it takes the first part of a line and it
    /// loses that part.
    pub fn wait_readable(&self, timeout: Duration) -> std::io::Result<bool> {
        use std::os::unix::io::AsRawFd;

        let mut fds = libc::pollfd {
            fd: self.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // The system takes milliseconds, and it takes an `i32`.
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let rc = unsafe { libc::poll(&mut fds, 1, ms) };
        match rc {
            // The time passed and the coordinator said nothing.
            0 => Ok(false),
            // A signal stopped the call. The caller tests its own flag and
            // calls this function again, so this answer is not a fault.
            -1 => {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
            // The socket holds an answer, or the coordinator closed it. The
            // caller reads it, and the read reports the difference.
            _ => Ok(true),
        }
    }

    /// Removes the read timeout, for a request that waits a long time.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.stream
            .set_read_timeout(timeout)
            .context("setting the timeout of the socket")
    }
}

/// Marks an error that the limit for a COORDINATOR ended.
///
/// Every command answers 124 for this condition, and 124 says one thing: qex
/// stopped waiting because it reached a limit. That is a fact about the
/// COMMAND and not about a job, so it holds for `qex list` and `qex top` as
/// well as for `qex wait`.
#[derive(Debug)]
pub struct CoordinatorTimeout;

impl std::fmt::Display for CoordinatorTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the limit for an answer of the coordinator ended this wait")
    }
}

impl std::error::Error for CoordinatorTimeout {}

/// Gives an error that carries the mark, so that the command answers 124.
fn timed_out(message: String) -> anyhow::Error {
    anyhow::Error::new(CoordinatorTimeout).context(message)
}

/// Says whether the limit for a coordinator ended this error.
pub fn is_a_coordinator_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CoordinatorTimeout>().is_some()
}

/// Names the cause when a coordinator accepted and then said nothing.
///
/// The message says what qex TRIED and what did not happen. It does not say
/// that the coordinator is wedged, because qex cannot know that: a coordinator
/// that is busy and a coordinator that will never answer look the same from
/// here.
fn name_a_silent_coordinator(error: anyhow::Error, ours: bool) -> anyhow::Error {
    if !ours {
        return error;
    }
    let expired = error
        .downcast_ref::<std::io::Error>()
        .map(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
        .unwrap_or(false);
    if !expired {
        return error;
    }
    timed_out(format!(
        "the coordinator took the request and gave no answer in {} seconds.\n\
         Run `qex info --no-start` to see whether it answers now, and read {} \
         for what it did.",
        COORDINATOR_LIMIT.as_secs(),
        paths::daemon_log_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "the log of the coordinator".to_string())
    ))
}

/// Tries to connect inside the limit for a coordinator.
///
/// `Ok(None)` says that NOBODY LISTENS, and a caller may start a coordinator.
/// An `Err` says that a socket is there and gave no answer, and a caller must
/// NOT start a second coordinator: two coordinators on one state directory
/// each hold the whole budget, and the machine then gets the kill for memory
/// that qex exists to prevent.
fn try_connect(socket: &Path) -> Result<Option<UnixStream>> {
    match paths::connect_within(socket, COORDINATOR_LIMIT) {
        paths::Connected::Open(stream) => Ok(Some(stream)),
        paths::Connected::NobodyListens => Ok(None),
        paths::Connected::NoAnswer => Err(timed_out(format!(
            "the coordinator did not accept a connection in {} seconds.\n\
             The socket {} is there, so a process holds it.\n\
             Run `qex info --no-start` to see whether a coordinator answers, \
             and read {} for what it did.",
            COORDINATOR_LIMIT.as_secs(),
            socket.display(),
            paths::daemon_log_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "the log of the coordinator".to_string())
        ))),
    }
}

/// Waits until the new coordinator opens its socket.
fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_millis(2);
    // Test the socket itself after a SHORT wait, and not at the end.
    //
    // A coordinator starts in a few milliseconds. A sandbox that refuses the
    // socket never gives one, and the reader then learns the cause in one
    // second in place of ten.
    let probe_at = Instant::now() + Duration::from_secs(1);
    let mut probed = false;

    while Instant::now() < deadline {
        // A SHORT limit for each attempt. This loop holds the deadline, so an
        // attempt that waits the whole limit would make one attempt of the
        // whole wait. A socket that is there and gives no answer is not a
        // refusal here: the coordinator is starting, and the next attempt asks
        // again.
        if let paths::Connected::Open(stream) = paths::connect_within(socket, delay) {
            return Ok(stream);
        }
        if !probed && Instant::now() >= probe_at {
            probed = true;
            if let Some(message) = socket_is_refused() {
                bail!("{message}");
            }
        }
        std::thread::sleep(delay);
        // Increase the delay slowly. A coordinator usually starts in a few
        // milliseconds, and a long delay makes each command slow.
        delay = (delay * 2).min(Duration::from_millis(50));
    }

    let log = paths::daemon_log_path()?;

    // Say WHY, and do not send the reader to a file for the usual cause.
    //
    // A sandbox that refuses a Unix socket gives this same silence: the
    // coordinator starts, its `bind` fails, and it stops. The reader then has
    // "the coordinator did not start" and a log file, and nothing that names
    // the cause or the remedy.
    if let Some(message) = socket_is_refused() {
        bail!("{message}");
    }

    // Give the last lines of the log here as well. A reader that must open a
    // file to learn the cause frequently does not open it.
    let tail = last_lines_of(&log, 5);
    bail!(
        "the coordinator did not start in {} seconds.\n\
         Its log file is {}{}",
        timeout.as_secs(),
        log.display(),
        tail
    )
}

/// Tests whether this process may make a Unix socket at all.
///
/// Gives the message for a reader when it may not. qex needs one socket for the
/// coordinator, and a sandbox that refuses `bind` or `connect` on a Unix socket
/// stops qex completely. bubblewrap, which Codex and other agent harnesses use,
/// can be configured in that way.
fn socket_is_refused() -> Option<String> {
    // Probe the directory that holds the SOCKET. That is the runtime directory
    // usually, and a short directory under /tmp when the path of the runtime
    // directory is longer than `sun_path` takes. A probe of a different
    // directory answers about different rules.
    let socket = paths::socket_path().ok()?;
    socket_refused_in(socket.parent()?)
}

/// The test above, for one directory. See `socket_is_refused`.
fn socket_refused_in(dir: &Path) -> Option<String> {
    classify_refusal(dir, probe_a_socket).map(|refusal| refusal.message(dir))
}

/// What stopped qex from making its socket.
enum Refusal {
    /// An ordinary file works, and a socket does not. That is the signature of
    /// a sandbox that filters the system calls of a socket.
    Socket(std::io::Error),
    /// qex cannot write in the directory at all.
    Directory(std::io::Error),
}

impl Refusal {
    fn message(&self, dir: &Path) -> String {
        // Both messages end in the same way, because the remedy for both
        // belongs to the same person.
        let page = "IF YOU ARE AN AGENT: tell the person that you work with. They must let qex \
                    run outside the sandbox, or give the sandbox permission for that \
                    directory. The page for them is:\n\
                    \x20   https://github.com/stephenc/qex/blob/main/docs/sandbox.md";
        match self {
            Refusal::Socket(e) => format!(
                "qex cannot make a Unix socket in {}: {e}. An ordinary file in that directory \
                 works, so this is the socket and not the directory.\n\n\
                 qex keeps one coordinator process, and every qex command talks to it through \
                 a Unix socket. A sandbox that refuses that socket stops qex completely, and \
                 that is the usual cause of this message.\n\n{page}",
                dir.display()
            ),
            Refusal::Directory(e) => format!(
                "qex cannot write in {}: {e}.\n\n\
                 qex keeps its records, its log files and the socket of its coordinator in \
                 that directory. A sandbox that gives it a read-only mount, or no mount at \
                 all, gives this fault.\n\n{page}",
                dir.display()
            ),
        }
    }
}

/// Tries to make a socket, and says what stopped it.
///
/// The FILE comes first. A sandbox that filters system calls can answer a
/// `bind` with "operation not permitted" before the kernel looks at the path,
/// so the error of the socket alone cannot say whether the directory exists. A
/// file that qex can write proves that the directory is there and writable, and
/// a socket that fails after that names the socket exactly.
///
/// `bind` is a parameter, so a test gives each answer with no sandbox: the
/// environment that this code exists for is the one that a test cannot make.
fn classify_refusal(dir: &Path, bind: impl Fn(&Path) -> std::io::Result<()>) -> Option<Refusal> {
    let name = format!("probe-{}", std::process::id());
    let file = dir.join(format!("{name}.tmp"));
    if let Err(e) = std::fs::write(&file, b"qex") {
        return a_refusal(e).map(Refusal::Directory);
    }
    std::fs::remove_file(&file).ok();

    let socket = dir.join(name);
    std::fs::remove_file(&socket).ok();
    let answer = bind(&socket);
    std::fs::remove_file(&socket).ok();
    answer.err().and_then(a_refusal).map(Refusal::Socket)
}

/// Keeps the errors that mean "you may not", and no others.
///
/// The messages of this module tell a reader that a SANDBOX stopped qex, and
/// that a different person must correct it. A disk that filled, or a process
/// that holds too many files, makes the same SHAPE — a write that fails, or a
/// socket that fails after a file that worked — and the words would then assert
/// a cause that the error itself contradicts. An agent acts on the words.
///
/// A fault that is not in this list gives no message here, and the caller then
/// reports the error that it has.
fn a_refusal(error: std::io::Error) -> Option<std::io::Error> {
    let refused = matches!(
        error.raw_os_error(),
        Some(libc::EACCES)
            | Some(libc::EPERM)
            | Some(libc::EROFS)
            | Some(libc::EAFNOSUPPORT)
            | Some(libc::EPROTONOSUPPORT)
            | Some(libc::EOPNOTSUPP)
            | Some(libc::ENOSYS)
    );
    // A test gives an error with no number of the system, so take the kind as
    // well. `PermissionDenied` is EACCES or EPERM by definition.
    let refused = refused
        || (error.raw_os_error().is_none()
            && matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ));
    refused.then_some(error)
}

/// Binds a socket and connects to it.
///
/// qex needs BOTH halves: the coordinator binds, and every command connects. A
/// sandbox can allow the first and refuse the second.
fn probe_a_socket(path: &Path) -> std::io::Result<()> {
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    let answer = UnixStream::connect(path).map(|_| ());
    drop(listener);
    answer
}

/// Adds the page for a person when a fault has the shape of a sandbox.
///
/// A sandbox gives an ordinary error: a directory that qex cannot make, a file
/// system that refuses a write, a system call that it does not permit. Each of
/// those messages names a file and nothing else, and the reader then does not
/// learn that the remedy belongs to a different person.
fn name_the_sandbox(error: anyhow::Error) -> anyhow::Error {
    // A message that names the page already keeps its words.
    if format!("{error:#}").contains("docs/sandbox.md") {
        return error;
    }
    if !looks_like_a_sandbox(&error) {
        return error;
    }
    // Put the CAUSE first and the remedy after it. `anyhow` writes a context
    // BEFORE the error that it covers, so a context here would give the remedy
    // and then the fault that it corrects.
    anyhow::anyhow!(
        "{error:#}\n\n\
         qex could not make the files that it needs. A sandbox that gives qex a read-only \
         mount, or no permission for that directory, gives this fault.\n\
         IF YOU ARE AN AGENT: tell the person that you work with. The page for them is \
         https://github.com/stephenc/qex/blob/main/docs/sandbox.md"
    )
}

/// True when an error has the shape that a sandbox gives.
fn looks_like_a_sandbox(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        let Some(io) = cause.downcast_ref::<std::io::Error>() else {
            continue;
        };
        // EROFS has no name in the stable library of every version that qex
        // builds with, so take the number.
        if io.raw_os_error() == Some(libc::EROFS) {
            return true;
        }
        if matches!(
            io.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
        ) {
            return true;
        }
    }
    false
}

/// Gives the last lines of a file, for a message.
fn last_lines_of(path: &Path, count: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    let tail = lines[lines.len().saturating_sub(count)..].join("\n  ");
    format!(", and it ends with:\n  {tail}")
}

/// Starts the coordinator as a separate process.
///
/// The new process calls `setsid`. It thus has a new session and no controlling
/// terminal. The system does not send it `SIGHUP` when the terminal closes.
/// This is the behaviour of `nohup`, but qex does not need a shell.
fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;

    let exe = paths::program_path()?;
    let log_path = paths::daemon_log_path()?;
    paths::ensure_dir(&paths::runtime_dir()?, 0o700)?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening the log file {}", log_path.display()))?;
    let log_err = log.try_clone().context("copying the log file handle")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        // The coordinator must not hold the directory of the CLI. That
        // directory can be a removable disk, or a user can delete it.
        .current_dir("/");

    unsafe {
        cmd.pre_exec(|| {
            // Make a new session. The coordinator then has no controlling
            // terminal, and it continues after the shell closes.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn().context("starting the coordinator")?;
    Ok(())
}

/// An exclusive lock on the spawn lock file.
///
/// The lock stops two CLI processes from starting two coordinators. The kernel
/// releases the lock if the process stops, so a lock file never stays locked
/// after a failure.
pub struct SpawnLock {
    file: std::fs::File,
}

impl SpawnLock {
    pub fn acquire() -> Result<Self> {
        let dir = paths::runtime_dir()?;
        paths::ensure_dir(&dir, 0o700)?;
        let path = paths::spawn_lock_path()?;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening the lock file {}", path.display()))?;

        use std::os::unix::io::AsRawFd;
        // Take the lock inside the limit for a coordinator.
        //
        // A lock that blocks with no end holds every later command behind the
        // one process that holds it. The lock covers the start of a
        // coordinator, so a coordinator that is slow to bind holds the others
        // for that time — and a process that stops while it holds the lock
        // held them for ever.
        //
        // A SIGNAL IS NOT A REFUSAL. `flock` ends with EINTR when a signal
        // arrives, and the caller tries again.
        let deadline = Instant::now() + COORDINATOR_LIMIT;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { file });
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EWOULDBLOCK) | Some(libc::EINTR) => {}
                _ => {
                    return Err(error).with_context(|| format!("locking {}", path.display()));
                }
            }
            if Instant::now() >= deadline {
                return Err(timed_out(format!(
                    "a different qex command held the lock {} for {} seconds.\n\
                     That lock covers the start of a coordinator, so a command \
                     that stopped while it held the lock keeps this one waiting.\n\
                     Run `qex info --no-start` to see whether a coordinator answers.",
                    path.display(),
                    COORDINATOR_LIMIT.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(kind: std::io::ErrorKind) -> impl Fn(&Path) -> std::io::Result<()> {
        move |_| Err(std::io::Error::new(kind, "refused"))
    }

    /// Makes a directory that the test can write, and gives `None` when the
    /// environment refuses even that.
    ///
    /// A sandbox that refuses a socket can refuse a file as well. A test that
    /// needs a writable directory then measures the sandbox and not qex, so it
    /// gives no verdict there. The two tests that need no directory still hold
    /// the rule.
    fn writable_dir(name: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("qx-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        match std::fs::write(dir.join("control"), b"qex") {
            Ok(()) => {
                std::fs::remove_file(dir.join("control")).ok();
                Some(dir)
            }
            Err(_) => {
                std::fs::remove_dir_all(&dir).ok();
                None
            }
        }
    }

    /// The words of each message, with no directory and no socket. These hold
    /// in every environment, including the sandbox that this code exists for.
    #[test]
    fn each_refusal_has_its_own_words() {
        let socket = Refusal::Socket(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .message(Path::new("/state/run"));
        assert!(socket.contains("Unix socket in /state/run"));
        assert!(socket.contains("docs/sandbox.md"));

        let directory =
            Refusal::Directory(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .message(Path::new("/state/run"));
        assert!(directory.contains("cannot write in /state/run"));
        assert!(
            !directory.contains("Unix socket in"),
            "the message of a directory must not blame the socket: {directory}"
        );
        assert!(directory.contains("docs/sandbox.md"));
    }

    /// A directory that takes a file and refuses a socket is a sandbox.
    #[test]
    fn a_socket_that_is_refused_names_the_cause_and_the_page() {
        let Some(dir) = writable_dir("sock") else {
            return;
        };
        let answer = classify_refusal(&dir, refused(std::io::ErrorKind::PermissionDenied));
        std::fs::remove_dir_all(&dir).ok();

        let message = answer
            .expect("a socket that is refused must give a message")
            .message(Path::new("/state/run"));
        assert!(message.contains("Unix socket"), "got: {message}");
        assert!(message.contains("docs/sandbox.md"), "got: {message}");
    }

    /// A socket that qex can make gives no message at all.
    #[test]
    fn a_socket_that_works_is_not_a_fault() {
        let Some(dir) = writable_dir("ok") else {
            return;
        };
        let answer = classify_refusal(&dir, |_| Ok(()));
        std::fs::remove_dir_all(&dir).ok();
        assert!(answer.is_none(), "a socket that works is not a fault");
    }

    /// A fault that is not a refusal gives no message at all.
    ///
    /// A disk that filled makes the same shape as a sandbox: a write that
    /// fails. The words of this module say that a sandbox stopped qex and that
    /// a person must change its permissions, and an agent acts on the words.
    #[test]
    fn a_disk_that_filled_is_not_a_sandbox() {
        let full = std::io::Error::from_raw_os_error(libc::ENOSPC);
        assert!(a_refusal(full).is_none());

        let too_many_files = std::io::Error::from_raw_os_error(libc::EMFILE);
        assert!(a_refusal(too_many_files).is_none());

        // The refusals stay.
        for code in [libc::EACCES, libc::EPERM, libc::EROFS, libc::EAFNOSUPPORT] {
            assert!(
                a_refusal(std::io::Error::from_raw_os_error(code)).is_some(),
                "the code {code} must count as a refusal"
            );
        }
    }

    /// A directory that qex cannot write gives the message of a DIRECTORY.
    ///
    /// A sandbox that filters system calls answers `bind` with "operation not
    /// permitted" before the kernel looks at the path, so the error of the
    /// socket alone cannot separate the two faults. The FILE separates them.
    #[test]
    fn a_directory_that_qex_cannot_write_is_not_the_socket() {
        // A path that does not exist, which no permission can make writable.
        // A directory inside a file, which the system refuses with ENOTDIR...
        // and that is not a refusal. Take a real one: a directory with no
        // permission to write.
        let Some(parent) = writable_dir("deny") else {
            return;
        };
        let dir = parent.join("locked");
        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let answer = classify_refusal(&dir, |_| Ok(()));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).ok();
        std::fs::remove_dir_all(&parent).ok();

        // A test that runs as root writes anywhere, so it cannot make this
        // fault.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let message = answer
            .expect("a directory that qex cannot write must give a message")
            .message(&dir);
        assert!(message.contains("cannot write"), "got: {message}");
        assert!(!message.contains("Unix socket in"), "got: {message}");
    }

    /// An error that is not a sandbox keeps its own words.
    #[test]
    fn an_ordinary_error_does_not_name_a_sandbox() {
        let error = anyhow::anyhow!(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(!format!("{:#}", name_the_sandbox(error)).contains("docs/sandbox.md"));

        let refused = anyhow::anyhow!(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(format!("{:#}", name_the_sandbox(refused)).contains("docs/sandbox.md"));
    }
}
