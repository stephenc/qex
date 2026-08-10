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

        if let Some(stream) = try_connect(&socket) {
            return Client::with_stream(stream);
        }

        // Take the lock before the start. Two CLI processes can arrive here at
        // the same time. The lock lets one process start the coordinator.
        let _lock = SpawnLock::acquire()?;

        // Test the socket again. A different process can start the coordinator
        // while this process waits for the lock.
        if let Some(stream) = try_connect(&socket) {
            return Client::with_stream(stream);
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
        let socket = paths::socket_path().ok()?;
        let stream = try_connect(&socket)?;
        Client::with_stream(stream).ok()
    }

    fn with_stream(stream: UnixStream) -> Result<Self> {
        let reader = BufReader::new(stream.try_clone().context("copying the socket handle")?);
        Ok(Self { stream, reader })
    }

    /// Sends one request and reads one response.
    pub fn call(&mut self, request: &Request) -> Result<Response> {
        self.send(request)?;
        self.recv()
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

/// Tries to connect. Gives `None` if no coordinator listens.
fn try_connect(socket: &Path) -> Option<UnixStream> {
    UnixStream::connect(socket).ok()
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
        if let Some(stream) = try_connect(socket) {
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
        return Some(Refusal::Directory(e));
    }
    std::fs::remove_file(&file).ok();

    let socket = dir.join(name);
    std::fs::remove_file(&socket).ok();
    let answer = bind(&socket);
    std::fs::remove_file(&socket).ok();
    answer.err().map(Refusal::Socket)
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
        // This call blocks until the lock is free.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", path.display()));
        }

        Ok(Self { file })
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

    /// A directory that qex cannot write gives the message of a DIRECTORY.
    ///
    /// A sandbox that filters system calls answers `bind` with "operation not
    /// permitted" before the kernel looks at the path, so the error of the
    /// socket alone cannot separate the two faults. The FILE separates them.
    #[test]
    fn a_directory_that_qex_cannot_write_is_not_the_socket() {
        // A path that does not exist, which no permission can make writable.
        let dir = std::env::temp_dir().join(format!("qx-none-{}/deeper", std::process::id()));
        let message = classify_refusal(&dir, |_| Ok(()))
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
