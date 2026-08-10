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

    while Instant::now() < deadline {
        if let Some(stream) = try_connect(socket) {
            return Ok(stream);
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
    socket_refused_in(&paths::runtime_dir().ok()?)
}

/// The test above, for one directory. See `socket_is_refused`.
fn socket_refused_in(dir: &Path) -> Option<String> {
    let probe = dir.join(format!("probe-{}", std::process::id()));
    std::fs::remove_file(&probe).ok();

    // Test BOTH halves. A sandbox can allow `bind` and refuse `connect`, and
    // qex needs the two: the coordinator binds, and every command connects.
    let error = match std::os::unix::net::UnixListener::bind(&probe) {
        Ok(listener) => {
            let answer = UnixStream::connect(&probe).err();
            drop(listener);
            std::fs::remove_file(&probe).ok();
            answer?
        }
        Err(e) => {
            std::fs::remove_file(&probe).ok();
            e
        }
    };

    // A directory that qex cannot write is a different fault, and the message
    // for it is the one that the caller already gives.
    if !matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
    ) {
        return None;
    }

    Some(format!(
        "qex cannot make a Unix socket in {}: {error}.\n\n\
         qex keeps one coordinator process, and every qex command talks to it through a Unix \
         socket. A sandbox that refuses that socket stops qex completely, and this is the \
         usual cause of this message.\n\n\
         IF YOU ARE AN AGENT: tell the person that you work with. They must let qex run \
         outside the sandbox, or give the sandbox permission for a Unix socket in that \
         directory. The page for them is:\n\
         \x20   https://github.com/stephenc/qex/blob/main/docs/sandbox.md",
        dir.display()
    ))
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

    /// A directory that refuses a socket must give the page for a person, and
    /// not the message about a coordinator that did not start.
    ///
    /// This is the fault that a sandbox gives. An agent inside one meets it on
    /// its FIRST qex command, so the message must name the cause and say who
    /// can correct it.
    #[test]
    fn a_directory_that_refuses_a_socket_names_the_cause() {
        let dir = std::env::temp_dir().join(format!("qx-sock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Take the write permission away. A `bind` then gives EACCES, which is
        // the same class of fault as a sandbox that refuses the socket.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let answer = socket_refused_in(&dir);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).ok();
        std::fs::remove_dir_all(&dir).ok();

        // A test that runs as root writes anywhere, so it cannot make this
        // fault. Say so, and do not report a failure that is not one.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let message = answer.expect("a directory that refuses a socket must give a message");
        assert!(
            message.contains("docs/sandbox.md"),
            "the message must send the person to the page: {message}"
        );
        assert!(
            message.contains("Unix socket"),
            "the message must name the cause: {message}"
        );
    }

    /// A directory that does not exist is a different fault, and the caller
    /// gives its own message for it. A wrong message here would send every
    /// reader to a page about sandboxes.
    #[test]
    fn a_directory_that_does_not_exist_is_not_a_sandbox() {
        let dir = std::env::temp_dir().join(format!("qx-none-{}", std::process::id()));
        assert!(socket_refused_in(&dir).is_none());
    }
}
