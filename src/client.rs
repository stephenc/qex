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
    bail!(
        "the coordinator did not start in {} seconds.\n\
         Read its log file: {}",
        timeout.as_secs(),
        log.display()
    )
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
