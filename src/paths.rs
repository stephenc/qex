//! This module gives the location of each file that qex uses.
//!
//! qex uses the XDG directories on Linux and on macOS. On macOS it does not use
//! `~/Library/Application Support`. A user who shares dotfiles between machines
//! then finds the config file in the same location on each machine.

use anyhow::{Context, Result};
use std::path::PathBuf;

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .context("HOME is not set; qex cannot locate its config or state directory")
}

fn xdg(var: &str, default_suffix: &str) -> Result<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Ok(home()?.join(default_suffix)),
    }
}

/// Gives the location of the config file: `~/.config/qex.toml`.
///
/// qex has one config file only.
pub fn config_file() -> Result<PathBuf> {
    Ok(xdg("XDG_CONFIG_HOME", ".config")?.join("qex.toml"))
}

/// Gives the location of the state directory: `~/.local/state/qex`.
///
/// This directory holds the job records and the log files. These files must
/// stay available after a restart of the machine.
pub fn state_dir() -> Result<PathBuf> {
    Ok(xdg("XDG_STATE_HOME", ".local/state")?.join("qex"))
}

/// `~/.local/state/qex/jobs`
pub fn jobs_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("jobs"))
}

/// Gives the location of the runtime directory.
///
/// This directory holds the socket, the spawn lock and the log of the
/// coordinator.
///
/// The directory is always inside the state directory. It is not inside
/// `$XDG_RUNTIME_DIR`.
///
/// The reason is important. The jobs of a user are in the state directory, and
/// there must be one coordinator for those jobs. A desktop terminal sets
/// `$XDG_RUNTIME_DIR`, but an ssh session, a cron job and a container
/// frequently do not set it. Two sessions with one home directory would then
/// use two sockets and start two coordinators. Each coordinator would hold the
/// full budget, and together they would start twice the permitted work. That
/// result is the fault that qex prevents.
///
/// The socket and the lock file are thus beside the jobs that they control.
pub fn runtime_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("run"))
}

/// The maximum length of a socket path.
///
/// The `sun_path` field of `sockaddr_un` holds 104 bytes on macOS and 108 bytes
/// on Linux. Use the smaller value on both systems, and keep space for the
/// final zero byte.
const MAX_SOCKET_PATH: usize = 100;

/// Gives the location of the control socket.
///
/// The file name has one character only, to keep the path short.
///
/// A long `$XDG_RUNTIME_DIR`, a long `$HOME`, or a test that uses a temporary
/// directory can still make the path too long. This function then uses a short
/// directory in `/tmp` instead. The name of that directory comes from the user
/// id and from the value of the long path, so the CLI and the coordinator
/// always calculate the same name.
pub fn socket_path() -> Result<PathBuf> {
    let preferred = runtime_dir()?.join("s");
    if preferred.as_os_str().len() <= MAX_SOCKET_PATH {
        return Ok(preferred);
    }
    Ok(short_socket_dir(&preferred)?.join("s"))
}

/// Gives the short directory for the socket, and makes it if necessary.
///
/// The directory is in `/tmp`, which every user can write. The function thus
/// tests the owner and the mode. If a different user owns the directory, the
/// function gives an error and qex stops. It does not use that directory.
fn short_socket_dir(preferred: &std::path::Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("qex-{uid}-{}", path_hash(preferred)));

    match std::fs::symlink_metadata(&dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                anyhow::bail!(
                    "qex needs the directory {} for its socket, but that path is a file",
                    dir.display()
                );
            }
            if meta.uid() != uid {
                anyhow::bail!(
                    "the directory {} belongs to the user {}, and qex will not use it",
                    dir.display(),
                    meta.uid()
                );
            }
            ensure_dir(&dir, 0o700)?;
        }
        Err(_) => ensure_dir(&dir, 0o700)?,
    }

    // MAKE THE PID FILE WITH THE DIRECTORY, AND NOT LATER.
    //
    // The sweep of a different coordinator takes the lock on this file and
    // holds it across the deletion of the directory. A directory with no such
    // file gives the two sides no common lock for a time.
    //
    // THIS FILE MAKES THAT TIME SMALL. It does not remove it: a sweep between
    // the new directory and this call still finds no file. The CLI calls this
    // function as well, and it takes no lock, so that time was long before.
    // `hold_pid_file` closes the window with a test of `st_nlink` after it takes
    // the lock.
    //
    // The file holds no number yet. The lock is the evidence, and the lock is
    // free until a coordinator takes it.
    let pid = dir.join(PID_FILE);
    if !pid.exists() {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&pid)
            .ok();
    }

    Ok(dir)
}

/// The time that qex waits for one socket to answer.
///
/// A socket on this machine answers in less than one millisecond. A longer wait
/// gives no more information. Without this limit, a connect to a socket that a
/// live process holds, but never accepts, waits for ever.
///
/// The limit gives the answer `Unknown`, and never the answer `NobodyListens`.
/// The caller thus keeps the socket, and the length of this limit cannot
/// destroy the work of a different coordinator.
const SOCKET_ANSWER_LIMIT: std::time::Duration = std::time::Duration::from_millis(100);

/// The name of the file that holds the process id of the coordinator.
///
/// The file is in the same directory as the socket.
pub const PID_FILE: &str = "pid";

/// The time that one sweep of the temporary directory can take.
///
/// The sweep stops at this limit and leaves the rest of the directories. The
/// next coordinator continues the work.
///
/// The limit is a time and not a count of directories, because the COST of a
/// directory is not the same for each one. A directory with a socket that
/// refuses a connection costs microseconds; a directory with a socket that
/// gives no answer costs the whole limit of one probe. A count of directories
/// thus gives no limit on the time that the sweep takes.
const SWEEP_LIMIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Deletes the short socket directories that no coordinator uses.
///
/// Each test run and each unusual state directory can make one of these
/// directories in `/tmp`. Without this step they stay for ever.
///
/// This function deletes a directory of this user only, and only when it has
/// PROOF that no coordinator uses it. The proof is a dead process and a socket
/// that refuses a connection. Every other answer keeps the directory.
///
/// The rule is one-sided for a reason. A directory that stays costs some space
/// in the temporary directory. A directory that goes takes the socket of a live
/// coordinator, and the commands of that user then start a second coordinator
/// on the same state directory. Two coordinators each hold the full budget, and
/// together they start twice the permitted work. That result is the fault that
/// qex prevents.
///
/// The coordinator calls this function on a thread, and after it opens its own
/// socket. The sweep touches the directories of other coordinators only, so no
/// command waits for it.
pub fn reap_stale_socket_dirs() {
    // Keep the directories of this process. The socket of this coordinator
    // answers, so the sweep keeps them anyway, but the rule must not depend on
    // that: a sweep must never delete the socket that it needs to operate.
    let mut own: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = runtime_dir() {
        own.push(dir);
    }
    if let Ok(socket) = socket_path() {
        if let Some(dir) = socket.parent() {
            own.push(dir.to_path_buf());
        }
    }
    sweep_socket_dirs(&std::env::temp_dir(), &own, SWEEP_LIMIT);
}

/// Sweeps one directory for the socket directories that no coordinator uses.
///
/// `own` holds the directories of this process, which the sweep keeps. `limit`
/// is the time that the whole sweep can take.
///
/// This function takes the directory as a parameter, and thus a test can give
/// it a directory of its own. `$TMPDIR` belongs to the whole process, and a
/// test that changes it changes the other tests at the same time.
fn sweep_socket_dirs(dir: &std::path::Path, own: &[PathBuf], limit: std::time::Duration) {
    use std::os::unix::fs::MetadataExt;
    let start = std::time::Instant::now();
    let uid = unsafe { libc::getuid() };
    let prefix = format!("qex-{uid}-");

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        if start.elapsed() >= limit {
            return;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) {
            continue;
        }
        if own.iter().any(|p| *p == entry.path()) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.is_dir() || meta.uid() != uid {
            continue;
        }

        // Delete a directory only with the proof that no coordinator uses it.
        //
        // The claim HOLDS THE LOCK while the deletion runs. A coordinator that
        // starts in the moment between the test and the deletion would lose its
        // directory, and the lock closes that moment.
        let Some(claim) = claim_unused_dir(&entry.path(), SOCKET_ANSWER_LIMIT) else {
            continue;
        };
        std::fs::remove_dir_all(entry.path()).ok();
        drop(claim);
    }
}

/// Holds the lock on a socket directory that no coordinator uses.
///
/// The result is `Some` only with the proof that no coordinator uses the
/// directory. Each other answer gives `None`, and the caller keeps the
/// directory.
///
/// THE RESULT HOLDS THE LOCK. This function makes the pid file if it is not
/// there, so the sweep and a coordinator that starts in the directory always
/// lock one inode. The caller deletes the directory and then drops the claim.
///
/// A LOCK ALONE DOES NOT KEEP THE DIRECTORY FOR A COORDINATOR. This function
/// can take the lock first, delete the file, and give the lock back. A
/// coordinator that then takes the lock holds it on a file with no name.
/// `hold_pid_file` tests `st_nlink` after it takes the lock, and it opens the
/// path again when the file is gone. The two parts together close the window.
///
/// The lock on the pid file is the strong test, and it comes first. A process
/// that holds that lock operates, so the directory stays and the socket needs no
/// probe. The answer of a socket is weaker: Linux and macOS give different
/// errors for a socket that a live coordinator holds but does not accept, and
/// the safety of this code must not depend on that difference.
fn claim_unused_dir(dir: &std::path::Path, limit: std::time::Duration) -> Option<Claim> {
    // MAKE THE FILE IF IT IS NOT THERE, AND DO NOT ONLY READ IT.
    //
    // The sweep and a coordinator that starts in this directory must lock ONE
    // inode. A sweep that reads only takes no lock in a directory with no pid
    // file, and the two then have no common lock at all. The file costs one
    // call of the system, and the sweep deletes it with the directory.
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join(PID_FILE))
    {
        Ok(file) => match try_lock(&file) {
            // This process holds the lock now, and it keeps the file to hold it.
            LockTry::Taken => Some(file),
            LockTry::Held | LockTry::Failed(_) => return None,
        },
        // qex cannot use the file, so it cannot say that the directory is free.
        Err(_) => return None,
    };

    if !matches!(
        ask_socket(&dir.join("s"), limit),
        SocketAnswer::NobodyListens
    ) {
        return None;
    }
    Some(Claim { _file: file })
}

/// The lock on a directory that the sweep is about to delete.
///
/// The lock goes back when this value goes out of scope, and the close of the
/// file gives it back as well.
struct Claim {
    _file: Option<std::fs::File>,
}

/// The answer of a socket to a connection.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SocketAnswer {
    /// A process accepted the connection. A coordinator operates there.
    Answers,
    /// Nobody listens at the socket. The system refused the connection, or the
    /// file is not there. This answer is the one proof of an unused socket.
    NobodyListens,
    /// The answer is not known. A process can hold the socket, and this machine
    /// gives no way to be sure at this moment.
    Unknown,
}

/// The state of the pid file of a socket directory.
///
/// The product decides with `try_lock` and with `hold_pid_file`. This value
/// gives the same answer in one word, for a test that reads the state.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PidFile {
    /// A process holds the lock on the file. That process operates.
    Held,
    /// No process holds the lock, and the file thus says nothing about a
    /// coordinator. A file that is not there gives this state as well.
    Free,
    /// qex cannot test the file.
    Unknown,
}

/// The time that a coordinator tries for the lock before it gives up.
///
/// A SWEEP TAKES THIS LOCK FOR A SHORT TIME, AND THAT TIME MUST NOT READ AS A
/// COORDINATOR. The sweep of a different COORDINATOR of this user takes the
/// lock of a directory that it can delete, and it keeps the lock across the
/// question to the socket and across the removal of the directory. A single try
/// can meet that time and report a coordinator that does not exist.
///
/// The budget: a question to a socket ends at `SOCKET_ANSWER_LIMIT`, and the
/// removal of a directory of two small files costs microseconds. This time is
/// thus ten times the longest hold that a sweep can make.
///
/// A coordinator holds the lock for its whole life, so this time separates a
/// coordinator from a sweep with no other mechanism.
/// The number of times that a coordinator opens the pid file again.
///
/// Each try can meet a sweep that deletes the file after this process locks it.
/// A small count is sufficient: a sweep deletes a directory one time, and it
/// then has no more work there.
const PID_LOCK_ATTEMPTS: u32 = 5;

pub const PID_LOCK_PATIENCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Holds the pid file of this coordinator, and the lock on it.
///
/// The coordinator keeps this value for its whole life. The kernel gives the
/// lock back when the process stops, so no process can hold this lock after it
/// stops.
pub struct PidFileLock {
    #[allow(dead_code)]
    file: std::fs::File,
}

#[cfg(test)]
impl PidFileLock {
    /// The count of the names that point to the file that this lock holds.
    ///
    /// A count of 0 says that the file is gone, and a lock on it is invisible
    /// to every later process.
    fn nlink(&self) -> u64 {
        use std::os::unix::fs::MetadataExt;
        self.file.metadata().map(|m| m.nlink()).unwrap_or(0)
    }

    /// The number of the file that this lock holds.
    fn ino(&self) -> u64 {
        use std::os::unix::fs::MetadataExt;
        self.file.metadata().map(|m| m.ino()).unwrap_or(0)
    }
}

/// The result of a try for the lock on the pid file of this coordinator.
pub enum PidHold {
    /// This process holds the lock, and it wrote its number into the file.
    Held(PidFileLock),
    /// A different process holds the lock. A coordinator operates.
    Busy,
    /// qex cannot use the file. The text says what the reader must do.
    Unusable(String),
}

/// The result of one try for the lock.
enum LockTry {
    /// This process holds the lock now. It stays while the caller keeps the
    /// file open.
    Taken,
    /// A different process holds the lock.
    Held,
    /// The try failed. The error says why.
    Failed(std::io::Error),
}

/// One try for the lock, with no wait.
///
/// The result carries the error, so the caller never reads `errno` again. A
/// second read gives the error of the last call of the system, and that call is
/// not always this one.
fn try_lock(file: &std::fs::File) -> LockTry {
    use std::os::unix::io::AsRawFd;
    // A SIGNAL IS NOT A REFUSAL. `flock` ends with EINTR when a signal arrives,
    // and that is an ordinary event and not a fault.
    for _ in 0..5 {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return LockTry::Taken;
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::WouldBlock => return LockTry::Held,
            std::io::ErrorKind::Interrupted => continue,
            _ => return LockTry::Failed(e),
        }
    }
    LockTry::Failed(std::io::Error::from(std::io::ErrorKind::Interrupted))
}

/// Takes the lock on the pid file beside the socket, and writes the number of
/// this process into it.
///
/// THE LOCK IS THE EVIDENCE OF LIFE, AND THE NUMBER IS NOT. The kernel gives an
/// `flock` back when the process stops, whatever stops it: a signal that a
/// process cannot catch, a kill at an out-of-memory condition, or a loss of
/// power. A number gives no such promise, because the system gives the number
/// of a process that stopped to a new process after some time. A test of the
/// number would then keep a state directory that no coordinator uses, and
/// nothing could correct it.
///
/// The number stays in the file for a reader, and for a message that names the
/// process. No decision uses it.
///
/// The coordinator calls this function BEFORE it tests the socket file. The
/// lock, and not the socket, says that a different coordinator operates: a
/// socket file can go while its coordinator operates, and the coordinator
/// deletes its own socket file one moment before it stops.
pub fn hold_pid_file(socket: &std::path::Path, patience: std::time::Duration) -> PidHold {
    use std::io::Write as _;

    let Some(dir) = socket.parent() else {
        return PidHold::Unusable(format!(
            "the socket path {} has no directory",
            socket.display()
        ));
    };
    let path = dir.join(PID_FILE);

    // TRY AGAIN WHEN THE FILE THAT THIS PROCESS LOCKED IS GONE.
    //
    // A sweep can hold the lock, then delete the file and the directory, and
    // then give the lock back. This process would take the lock on an inode
    // that no name points to. Such a lock is invisible to every later process,
    // and the one common lock that this design rests on would be gone.
    //
    // `st_nlink` of 0 is the test: no name points to the file. The answer is a
    // new open of the path, which gives the inode that the name points to now.
    let deadline = std::time::Instant::now() + patience;
    for _ in 0..PID_LOCK_ATTEMPTS {
        // OPEN WITH NO TRUNCATION. A file that this process empties before it
        // takes the lock is the file of the coordinator that owns the lock, and
        // that coordinator loses its number for a reader.
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => file,
            // A file that is not there, in a directory that is not there. A
            // deletion is a wrong instruction here, and the reader must not get
            // it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return PidHold::Unusable(format!(
                    "qex cannot make the file {}: {e}. Start qex again.",
                    path.display()
                ))
            }
            Err(e) => {
                return PidHold::Unusable(format!(
                    "qex cannot open the file {}: {e}. Delete that file if no coordinator \
                     operates.",
                    path.display()
                ))
            }
        };

        loop {
            match try_lock(&file) {
                LockTry::Taken => break,
                LockTry::Held => {
                    if std::time::Instant::now() >= deadline {
                        return PidHold::Busy;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                LockTry::Failed(e) => {
                    return PidHold::Unusable(format!(
                        "qex cannot lock the file {}: {e}. Delete that file if no \
                         coordinator operates.",
                        path.display(),
                    ))
                }
            }
        }

        // A file with no name is a file that a sweep deleted after this process
        // opened it. Open the path again and take the lock on the new file.
        let gone = matches!(file.metadata(), Ok(meta) if
            std::os::unix::fs::MetadataExt::nlink(&meta) == 0);
        if gone {
            continue;
        }

        // The lock is this process's now, so the number of the earlier
        // coordinator has no more use.
        if let Err(e) = file.set_len(0).and_then(|()| {
            file.write_all(std::process::id().to_string().as_bytes())?;
            file.flush()
        }) {
            return PidHold::Unusable(format!(
                "qex cannot write the file {}: {e}. Delete that file if no coordinator \
                 operates.",
                path.display()
            ));
        }

        return PidHold::Held(PidFileLock { file });
    }

    PidHold::Unusable(format!(
        "a sweep deleted the file {} while qex took the lock on it. Start qex again.",
        path.display()
    ))
}

/// Tests if a process holds the pid file of a directory.
///
/// The test takes the lock, and it gives the lock back at once. A lock that
/// this function cannot take belongs to a process that operates.
///
/// This test does not read the number in the file. See `hold_pid_file` for the
/// reason: the lock says that a process operates, and the number does not.
#[cfg(test)]
pub fn pid_file_state(dir: &std::path::Path) -> PidFile {
    use std::os::unix::io::AsRawFd;

    // OPEN THE FILE FOR WRITING AS WELL AS FOR READING. An exclusive lock needs
    // a descriptor that can write on some systems and on some file systems, and
    // this test takes an exclusive lock.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(PID_FILE))
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PidFile::Free,
        // qex cannot read the file, so it cannot say that the directory is
        // free. The caller keeps the directory.
        Err(_) => return PidFile::Unknown,
    };
    match try_lock(&file) {
        LockTry::Taken => {
            // Give the lock back at once. This process must not hold a lock on
            // the file of a different coordinator.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            PidFile::Free
        }
        LockTry::Held => PidFile::Held,
        LockTry::Failed(_) => PidFile::Unknown,
    }
}

/// Asks a socket to answer inside `limit`.
///
/// The standard library gives no connect with a time limit for a Unix socket.
/// This function thus uses a socket that does not block, and it tries again
/// until the limit.
///
/// The limit is necessary. A process that holds the socket, but never accepts a
/// connection, makes a connect without a limit wait for ever.
///
/// The limit gives `Unknown`. A socket that does not answer can belong to a
/// coordinator that is busy, so a caller must not read a slow answer as a dead
/// coordinator.
pub fn ask_socket(socket: &std::path::Path, limit: std::time::Duration) -> SocketAnswer {
    // A test of the LINK, and not of the target.
    //
    // A link with no target reaches `NobodyListens` either way: this test lets
    // it through, and the connect below then gives `ENOENT`. The one answer
    // that changes is a path too long for `sun_path`, which now gives
    // `Unknown`, and a caller keeps such a socket. That is the safe direction.
    if std::fs::symlink_metadata(socket).is_err() {
        return SocketAnswer::NobodyListens;
    }
    // A path that is too long gives no address. The socket can still belong to
    // a live coordinator, so the answer is not known.
    let Some(address) = unix_address(socket) else {
        return SocketAnswer::Unknown;
    };

    let deadline = std::time::Instant::now() + limit;
    loop {
        match connect_once(&address, deadline) {
            Some(answer) => return answer,
            None => {
                if std::time::Instant::now() >= deadline {
                    return SocketAnswer::Unknown;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

/// Gives the address of a Unix socket at `path`.
///
/// The result is `None` when the path is too long for `sun_path`.
fn unix_address(path: &std::path::Path) -> Option<libc::sockaddr_un> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= address.sun_path.len() {
        return None;
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    Some(address)
}

/// Reads the answer of a connect, or of `SO_ERROR`, as one of three states.
///
/// These are the errors of `connect(2)` that prove that NOBODY LISTENS at the
/// path. Each one is a statement about the path, and not about this process:
///
/// * `ECONNREFUSED` — a socket with no process that accepts. A system that
///   refuses when the queue of the socket is full gives this error as well, so
///   this answer alone cannot say that a coordinator stopped. The lock on the
///   pid file makes that decision.
/// * `ENOENT` — no file at the path.
/// * `ENOTSOCK` — a file that is not a socket. Nothing can listen at such a
///   path. One system gives `ECONNREFUSED` for it and another gives this error,
///   and the two must give one answer to qex.
///
/// Every other error gives `Unknown`, and a caller then keeps the socket.
fn answer_of(error: Option<i32>) -> SocketAnswer {
    match error {
        Some(libc::ECONNREFUSED) | Some(libc::ENOENT) | Some(libc::ENOTSOCK) => {
            SocketAnswer::NobodyListens
        }
        _ => SocketAnswer::Unknown,
    }
}

/// Waits for a connect that the system did not finish at once.
///
/// A non-blocking connect can give `EINPROGRESS`. The system then finishes the
/// connect later, and the caller reads the result from `SO_ERROR` after the
/// socket is ready to write. A caller that reads the answer of `connect` alone
/// never sees that result.
///
/// The result is `None` when the caller can try again.
fn finish_connect(fd: libc::c_int, deadline: std::time::Instant) -> Option<SocketAnswer> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    let mut waiting = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let ready = unsafe {
        libc::poll(
            &mut waiting,
            1,
            left.as_millis().min(i32::MAX as u128) as i32,
        )
    };
    if ready == 0 {
        // The connect did not finish inside the limit. A coordinator can hold
        // this socket, so the answer is not known.
        return Some(SocketAnswer::Unknown);
    }
    if ready < 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => None,
            other => Some(answer_of(other)),
        };
    }

    let mut error: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let read = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut error as *mut libc::c_int as *mut libc::c_void,
            &mut size,
        )
    };
    if read != 0 {
        return Some(SocketAnswer::Unknown);
    }
    if error == 0 {
        return Some(SocketAnswer::Answers);
    }
    if error == libc::EAGAIN || error == libc::EINTR {
        return None;
    }
    Some(answer_of(Some(error)))
}

/// Makes one attempt to connect, and never blocks.
///
/// The result is `Some(answer)` when the system gives an answer. The result is
/// `None` when the caller can try again: the socket is full at this moment, or
/// a signal stopped the call.
///
/// `deadline` bounds the wait for a connect that the system did not finish at
/// once. It does not make the caller wait for anything else.
fn connect_once(address: &libc::sockaddr_un, deadline: std::time::Instant) -> Option<SocketAnswer> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            // This process has no more file descriptors, or no more memory. The
            // socket of a different coordinator is not the cause.
            return Some(SocketAnswer::Unknown);
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            // A connect that blocks can wait for ever. Stop instead, and give
            // the answer that keeps the socket.
            libc::close(fd);
            return Some(SocketAnswer::Unknown);
        }
        let result = libc::connect(
            fd,
            address as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        if result == 0 {
            libc::close(fd);
            return Some(SocketAnswer::Answers);
        }
        let error = std::io::Error::last_os_error().raw_os_error();
        let answer = match error {
            // The system finishes this connect later. Wait for it, and read the
            // result that it gives.
            Some(libc::EINPROGRESS) => finish_connect(fd, deadline),
            // The queue of the socket is full at this moment. A process holds
            // the socket, so the caller tries again.
            Some(libc::EAGAIN) | Some(libc::EINTR) => None,
            other => Some(answer_of(other)),
        };
        libc::close(fd);
        answer
    }
}

/// The answer of a connect that KEEPS the connection.
///
/// `ask_socket` tests a socket and closes what it opened. A command needs the
/// connection itself, and it needs the three answers apart, because the answer
/// decides whether qex may start a coordinator.
pub enum Connected {
    /// A coordinator accepted, and this is the connection to it.
    Open(std::os::unix::net::UnixStream),
    /// Nobody listens at the socket. A caller may start a coordinator.
    NobodyListens,
    /// The socket gave no answer inside the limit. A process can hold that
    /// socket, so a caller MUST NOT start a second coordinator.
    NoAnswer,
}

/// Connects to a socket inside `limit`, and gives the connection back.
///
/// A connect with no limit waits for ever when a process holds the socket and
/// never accepts. The queue of a busy coordinator fills in the same way, so
/// this is not a rare state on a machine that many agents share.
///
/// `NoAnswer` is not `NobodyListens`. A caller that reads the two as one starts
/// a second coordinator beside a coordinator that operates, and two
/// coordinators on one state directory each hold the whole budget.
pub fn connect_within(socket: &std::path::Path, limit: std::time::Duration) -> Connected {
    use std::os::unix::io::FromRawFd;

    // ONLY "THERE IS NO FILE" MEANS THAT NOBODY LISTENS.
    //
    // This answer is the one that lets a caller START A COORDINATOR, so every
    // other error must not give it. A directory that this user cannot read, a
    // chain of links that has no end, a file system that answers nothing: none
    // of those says that no coordinator operates, and a second coordinator on
    // one state directory holds the whole budget again.
    //
    // An unknown answer authorises nothing.
    if let Err(e) = std::fs::symlink_metadata(socket) {
        return match e.kind() {
            std::io::ErrorKind::NotFound => Connected::NobodyListens,
            _ => Connected::NoAnswer,
        };
    }
    let Some(address) = unix_address(socket) else {
        return Connected::NoAnswer;
    };

    let deadline = std::time::Instant::now() + limit;
    loop {
        match open_once(&address, deadline) {
            Some(Ok(fd)) => {
                // Give the connection back in the blocking form. Every reader
                // of this stream expects a read that waits, and the limit of a
                // read belongs to the caller that makes the request.
                unsafe {
                    let flags = libc::fcntl(fd, libc::F_GETFL);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                        libc::close(fd);
                        return Connected::NoAnswer;
                    }
                    return Connected::Open(std::os::unix::net::UnixStream::from_raw_fd(fd));
                }
            }
            Some(Err(SocketAnswer::NobodyListens)) => return Connected::NobodyListens,
            Some(Err(_)) => return Connected::NoAnswer,
            None => {
                if std::time::Instant::now() >= deadline {
                    return Connected::NoAnswer;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

/// Makes one attempt to connect, and keeps the descriptor when it succeeds.
///
/// `Some(Ok(fd))` is a connection. `Some(Err(answer))` is an answer that ends
/// the attempts. `None` says that the caller can try again.
fn open_once(
    address: &libc::sockaddr_un,
    deadline: std::time::Instant,
) -> Option<Result<libc::c_int, SocketAnswer>> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Some(Err(SocketAnswer::Unknown));
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            libc::close(fd);
            return Some(Err(SocketAnswer::Unknown));
        }
        let result = libc::connect(
            fd,
            address as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        if result == 0 {
            return Some(Ok(fd));
        }
        let error = std::io::Error::last_os_error().raw_os_error();
        let answer = match error {
            Some(libc::EINPROGRESS) => match finish_connect(fd, deadline) {
                Some(SocketAnswer::Answers) => return Some(Ok(fd)),
                Some(other) => Some(Err(other)),
                None => None,
            },
            Some(libc::EAGAIN) | Some(libc::EINTR) => None,
            other => Some(Err(answer_of(other))),
        };
        libc::close(fd);
        answer
    }
}

/// Gives a short name for a long path.
///
/// This function uses the FNV-1a method. The result identifies the path. It is
/// not a secure hash, and qex does not use it for security.
fn path_hash(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.as_os_str().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:08x}")
}

/// Gives the path of the qex program file, in a form that qex can start again.
///
/// `current_exe` reads `/proc/self/exe`. When something replaces the program
/// file, the kernel keeps the running program but adds ` (deleted)` to that
/// name. A start of that name then fails with "No such file or directory".
///
/// This happens during development: a coordinator operates, `cargo install`
/// replaces the file, and every job after that fails at once with a message
/// that names no cause.
///
/// This function removes that mark and tests the true path. The program at that
/// path is the new one, and its records have the same format, so a job starts
/// correctly.
pub fn program_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding the qex program file")?;

    if exe.exists() {
        return Ok(exe);
    }

    let text = exe.to_string_lossy();
    if let Some(stripped) = text.strip_suffix(" (deleted)") {
        let path = PathBuf::from(stripped);
        if path.exists() {
            return Ok(path);
        }
        anyhow::bail!(
            "the qex program file {} no longer exists. Something replaced or deleted it \
             while the coordinator was operating. Stop the coordinator and start it again: \
             `qex info --no-start --json` gives its process id.",
            stripped
        );
    }

    anyhow::bail!(
        "the qex program file {} no longer exists. Stop the coordinator and start it again.",
        exe.display()
    )
}

/// Tests if something replaced the program file of this process.
///
/// The coordinator uses this test to stop when it becomes idle, so the next
/// command starts a coordinator with the new program.
pub fn program_file_changed() -> bool {
    match std::env::current_exe() {
        Ok(exe) => !exe.exists(),
        Err(_) => false,
    }
}

pub fn spawn_lock_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("spawn.lock"))
}

pub fn daemon_log_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.log"))
}

/// Gives the location of the directory for one job.
///
/// qex makes this directory with mode `0700`. The directory holds the captured
/// environment, which can contain secrets.
pub fn job_dir(id: &uuid::Uuid) -> Result<PathBuf> {
    Ok(jobs_dir()?.join(id.to_string()))
}

/// Makes a directory and its parent directories, then sets the mode.
///
/// This function sets the mode after it makes the directory. A permissive umask
/// thus cannot make the directory more open than the `mode` parameter.
pub fn ensure_dir(path: &std::path::Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("creating directory {}", path.display()))?;
    }
    // Set the mode here. `create_dir_all` subtracts the umask. With a
    // permissive umask, it keeps the group bits and the other bits.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode {mode:o} on {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {

    /// AN UNKNOWN ANSWER AUTHORISES NOTHING.
    ///
    /// `NobodyListens` is the answer that lets a caller start a coordinator. A
    /// socket that qex cannot even LOOK at says nothing about whether one
    /// operates, and a second coordinator on one state directory holds the
    /// whole budget again.
    #[test]
    fn a_socket_that_qex_cannot_look_at_does_not_say_that_nobody_listens() {
        let dir = std::env::temp_dir().join(format!("qex-lookat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shut = dir.join("shut");
        std::fs::create_dir_all(&shut).unwrap();
        let hidden = shut.join("s");

        // A directory that this user cannot enter. `symlink_metadata` then
        // gives `PermissionDenied`, which is not `NotFound`.
        std::fs::set_permissions(&shut, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();

        let answer = connect_within(&hidden, std::time::Duration::from_millis(50));
        let says_nobody = matches!(answer, Connected::NobodyListens);

        std::fs::set_permissions(&shut, std::os::unix::fs::PermissionsExt::from_mode(0o700)).ok();
        std::fs::remove_dir_all(&dir).ok();

        // The root user reads every directory, so this test cannot make the
        // state that it needs there. Say so, and hold nothing.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("this test did not run: the root user can look at every directory");
            return;
        }
        assert!(
            !says_nobody,
            "a socket that qex cannot look at must not say that nobody listens: \
             that answer lets a caller start a SECOND coordinator"
        );
    }

    use super::*;

    use crate::testutil::{env_lock, EnvVar};

    /// The integration tests set these variables to isolate each test.
    /// The tests thus depend on this behaviour.
    #[test]
    fn xdg_overrides_are_honoured() {
        let _guard = env_lock();
        let _c = EnvVar::set("XDG_CONFIG_HOME", "/tmp/qex-test-cfg");
        let _s = EnvVar::set("XDG_STATE_HOME", "/tmp/qex-test-state");
        assert_eq!(
            config_file().unwrap(),
            PathBuf::from("/tmp/qex-test-cfg/qex.toml")
        );
        assert_eq!(
            jobs_dir().unwrap(),
            PathBuf::from("/tmp/qex-test-state/qex/jobs")
        );
    }

    /// If the XDG variables are not set, each path must start at `$HOME`.
    #[test]
    fn defaults_fall_back_to_home() {
        let _guard = env_lock();
        let _h = EnvVar::set("HOME", "/home/example");
        let _c = EnvVar::unset("XDG_CONFIG_HOME");
        let _s = EnvVar::unset("XDG_STATE_HOME");
        let _r = EnvVar::unset("XDG_RUNTIME_DIR");
        assert_eq!(
            config_file().unwrap(),
            PathBuf::from("/home/example/.config/qex.toml")
        );
        assert_eq!(
            state_dir().unwrap(),
            PathBuf::from("/home/example/.local/state/qex")
        );
        // macOS does not set XDG_RUNTIME_DIR. The socket needs a location there.
        assert_eq!(
            runtime_dir().unwrap(),
            PathBuf::from("/home/example/.local/state/qex/run")
        );
    }

    #[test]
    fn socket_path_stays_within_sun_path_limits() {
        let _guard = env_lock();
        let _s = EnvVar::set("XDG_STATE_HOME", "/tmp/qex-sock-test");
        let p = socket_path().unwrap();
        assert_eq!(p, PathBuf::from("/tmp/qex-sock-test/qex/run/s"));
        assert!(p.as_os_str().len() <= MAX_SOCKET_PATH);
    }

    /// The socket must depend on the state directory only.
    ///
    /// A desktop terminal sets `$XDG_RUNTIME_DIR`, and an ssh session, a cron
    /// job and a container frequently do not. If the socket depended on that
    /// variable, two sessions with one home directory would start two
    /// coordinators. Each would hold the full budget, and together they would
    /// start twice the permitted work.
    #[test]
    fn the_socket_does_not_depend_on_the_runtime_variable() {
        let _guard = env_lock();
        let _s = EnvVar::set("XDG_STATE_HOME", "/tmp/qex-one-state");

        let with_variable = {
            let _r = EnvVar::set("XDG_RUNTIME_DIR", "/run/user/1000");
            socket_path().unwrap()
        };
        let without_variable = {
            let _r = EnvVar::unset("XDG_RUNTIME_DIR");
            socket_path().unwrap()
        };

        assert_eq!(
            with_variable, without_variable,
            "one state directory must give one socket, and thus one coordinator"
        );
    }

    /// A deep directory must not stop qex. A test harness and a long `$HOME`
    /// both give a path that does not fit in `sun_path`.
    #[test]
    fn a_long_state_directory_gives_a_short_socket_path() {
        let _guard = env_lock();
        // GIVE THIS TEST ITS OWN TEMPORARY DIRECTORY.
        //
        // The name of the short socket directory comes from the user and the
        // state directory, so every copy of this test calculates one name in
        // one shared directory. A second copy deletes it, and a sweep of a
        // coordinator on this machine deletes it as well.
        let own_tmp = TestDir::make("qex-shortdirtest");
        let _t = EnvVar::set("TMPDIR", own_tmp.path().to_str().unwrap());
        let long = format!("/tmp/{}", "very-long-directory-name/".repeat(8));
        let _s = EnvVar::set("XDG_STATE_HOME", &long);

        let p = socket_path().unwrap();
        assert!(
            p.as_os_str().len() <= MAX_SOCKET_PATH,
            "the socket path {} is still too long",
            p.display()
        );

        // The CLI and the coordinator must calculate the same name, or they
        // cannot find each other.
        assert_eq!(p, socket_path().unwrap());

        // The pid file comes with the directory. A sweep locks that file and
        // holds the lock across the deletion, so a directory with no such file
        // gives the sweep and a new coordinator no common lock.
        assert!(
            p.parent().unwrap().join(PID_FILE).exists(),
            "the short socket directory must hold a pid file from the moment it exists"
        );

        // A different state directory must give a different socket.
        let _s2 = EnvVar::set("XDG_STATE_HOME", &format!("{long}other/"));
        assert_ne!(p, socket_path().unwrap());

        // `own_tmp` removes the whole temporary directory of this test.
    }

    /// The directory that holds the directories of the tests.
    ///
    /// This function does NOT read `$TMPDIR`. One test gives the product its
    /// own `$TMPDIR`, and that variable belongs to the whole process. A helper
    /// that reads it would put the directories of every other test inside the
    /// directory of that one test, and that test then deletes them.
    fn test_root() -> PathBuf {
        PathBuf::from("/tmp")
    }

    /// Removes a directory when the test ends, and also when it panics.
    ///
    /// A test that leaves a directory in `/tmp` adds to the fault that the
    /// sweep corrects. The name of the directory is outside the `qex-<uid>-`
    /// set, so no sweep collects it.
    struct TestDir(PathBuf);

    impl TestDir {
        fn make(name: &str) -> Self {
            let path = test_root().join(format!("{name}-{}", std::process::id()));
            std::fs::remove_dir_all(&path).ok();
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Closes the sockets of a test when the test ends, and also when it panics.
    struct OpenSockets(Vec<libc::c_int>);

    impl Drop for OpenSockets {
        fn drop(&mut self) {
            for fd in self.0.drain(..) {
                unsafe { libc::close(fd) };
            }
        }
    }

    /// Opens a socket that accepts no connection, and fills its backlog.
    ///
    /// A connect to this socket does not complete. The standard `UnixListener`
    /// asks for a large backlog, so this test uses `libc` and asks for a
    /// backlog of one.
    ///
    /// The result holds the listener and the connections. The caller must keep
    /// it, because a closed socket answers at once with a refusal.
    ///
    /// THE RESULT IS `None` ON A SYSTEM THAT REFUSES A CONNECTION WHEN THE
    /// QUEUE OF THE SOCKET IS FULL. Such a system gives no way to hold a
    /// connect open, so a test that needs one cannot run there. The lock on the
    /// pid file, and not the answer of the socket, keeps the directory of a
    /// coordinator on such a system.
    fn a_socket_that_never_answers(path: &std::path::Path) -> Option<OpenSockets> {
        let address = unix_address(path).expect("the path of the test socket is short");
        let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let mut open = Vec::new();

        let can_wait = unsafe {
            let listener = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(listener >= 0, "the test cannot open a socket");
            open.push(listener);
            let bound = libc::bind(
                listener,
                &address as *const libc::sockaddr_un as *const libc::sockaddr,
                size,
            );
            assert_eq!(bound, 0, "the test cannot bind {}", path.display());
            assert_eq!(libc::listen(listener, 1), 0, "the test cannot listen");

            // Fill the backlog. The listener accepts nothing, so each
            // connection stays in the queue.
            //
            // The test reads the error of the system itself, and it does not
            // use `connect_once`. A test that measures the product with the
            // product fails at its own precondition when the product changes,
            // and that failure hides the property that the test holds.
            let mut full = false;
            for _ in 0..256 {
                let client = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
                assert!(client >= 0, "the test cannot open a socket");
                let flags = libc::fcntl(client, libc::F_GETFL);
                libc::fcntl(client, libc::F_SETFL, flags | libc::O_NONBLOCK);
                let result = libc::connect(
                    client,
                    &address as *const libc::sockaddr_un as *const libc::sockaddr,
                    size,
                );
                if result != 0 {
                    let error = std::io::Error::last_os_error().raw_os_error();
                    libc::close(client);
                    // A queue that is full gives one of these two answers. The
                    // first says that the connect can finish later, and the
                    // test can then hold it open. The second is a refusal, and
                    // this system gives no way to hold a connect open.
                    full = matches!(error, Some(libc::EAGAIN) | Some(libc::EINPROGRESS));
                    break;
                }
                open.push(client);
            }
            full
        };

        // Make the guard before the test of `full`, so that a system that
        // cannot hold a connect open still closes every socket of this test.
        let sockets = OpenSockets(open);
        if !can_wait {
            return None;
        }
        Some(sockets)
    }

    /// Makes a REAL socket file whose process is gone.
    ///
    /// This is the file that a coordinator leaves when a kill stops it. A bind
    /// makes the file, and the close of the socket leaves the file with no
    /// process behind it.
    ///
    /// A test must use this file, and not an ordinary file: one system refuses
    /// a connection to an ordinary file and another gives "that path is not a
    /// socket", and only a real socket file measures the state that a
    /// coordinator leaves.
    fn a_socket_file_with_no_owner(path: &std::path::Path) {
        let address = unix_address(path).expect("the path of the test socket is short");
        let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "the test cannot open a socket");
            let bound = libc::bind(
                fd,
                &address as *const libc::sockaddr_un as *const libc::sockaddr,
                size,
            );
            assert_eq!(bound, 0, "the test cannot bind {}", path.display());
            assert_eq!(libc::listen(fd, 1), 0, "the test cannot listen");
            libc::close(fd);
        }
        assert!(path.exists(), "the bind must leave the socket file");
    }

    /// Makes a pid file and holds the lock on it, as a coordinator does.
    ///
    /// The caller must keep the result. A file that closes gives the lock back,
    /// and the directory is then free.
    fn a_held_pid_file(dir: &std::path::Path) -> std::fs::File {
        use std::io::Write as _;
        use std::os::unix::io::AsRawFd;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dir.join(PID_FILE))
            .unwrap();
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "the test cannot lock the pid file");
        file.write_all(std::process::id().to_string().as_bytes())
            .unwrap();
        file.flush().unwrap();
        file
    }

    /// The sweep must keep the directory of a coordinator that operates.
    ///
    /// This is the central property. A coordinator that is busy does not answer
    /// at this moment, and a sweep that reads a slow answer as a dead
    /// coordinator deletes the socket of a coordinator that operates. The
    /// commands of that user then start a second coordinator on the same state
    /// directory, and the two together start twice the permitted work.
    #[test]
    fn the_sweep_keeps_the_directory_of_a_coordinator_that_operates() {
        let uid = unsafe { libc::getuid() };
        let base = TestDir::make("qex-reaptest");

        // THE DIRECTORIES THAT THE LOCK KEEPS. These run on every system, and
        // nothing below makes them conditional. The lock is the mechanism that
        // carries this property on a system where a socket cannot give "no
        // answer".
        let refuses_but_live = base.path().join(format!("qex-{uid}-livepid"));
        let own = base.path().join(format!("qex-{uid}-own"));
        std::fs::create_dir_all(&refuses_but_live).unwrap();
        std::fs::create_dir_all(&own).unwrap();

        // A socket that refuses a connection, and a process that operates. A
        // system that refuses a connection when the queue of the socket is full
        // gives this state for a coordinator that is busy. The test of the
        // process is thus the test that decides, and the socket cannot.
        a_socket_file_with_no_owner(&refuses_but_live.join("s"));
        let _held_live = a_held_pid_file(&refuses_but_live);

        // THE DIRECTORIES THAT NEED A SOCKET WHICH GIVES NO ANSWER.
        //
        // The making of each directory, its socket and its lock live together
        // in this one block. A system that cannot hold a connect open thus
        // leaves NO half-made directory, and no step after this block depends
        // on the directories or on the result.
        let busy_with_pid = base.path().join(format!("qex-{uid}-busypid"));
        let busy_no_pid = base.path().join(format!("qex-{uid}-busy"));
        let busy = (|| {
            std::fs::create_dir_all(&busy_with_pid).unwrap();
            std::fs::create_dir_all(&busy_no_pid).unwrap();
            let one = a_socket_that_never_answers(&busy_with_pid.join("s"))?;
            let two = a_socket_that_never_answers(&busy_no_pid.join("s"))?;
            let held = a_held_pid_file(&busy_with_pid);
            Some((one, two, held))
        })();
        if busy.is_none() {
            eprintln!(
                "this system refuses a connection when the queue of a socket is full, so \
                 the two directories with a socket that gives no answer do not take part"
            );
            std::fs::remove_dir_all(&busy_with_pid).ok();
            std::fs::remove_dir_all(&busy_no_pid).ok();
        }

        // Run the sweep on a thread. A sweep that waits for ever then gives a
        // failure, and it does not stop the whole test suite.
        let (sender, receiver) = std::sync::mpsc::channel();
        let directory = base.path().to_path_buf();
        let keep = own.clone();
        std::thread::spawn(move || {
            sweep_socket_dirs(&directory, &[keep], std::time::Duration::from_secs(3));
            sender.send(()).ok();
        });
        let finished = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok();

        assert!(
            finished,
            "the sweep waits for a socket that never answers; every qex command then waits"
        );
        if busy.is_some() {
            assert!(
                busy_with_pid.exists(),
                "the sweep deleted the socket directory of a coordinator that operates \
                 and that holds the lock on its pid file"
            );
            assert!(
                busy_no_pid.exists(),
                "the sweep deleted the socket directory of a coordinator that operates; \
                 a socket that gives no answer is not a socket that nobody holds"
            );
        }
        assert!(
            refuses_but_live.exists(),
            "the sweep deleted the directory of a process that operates; the answer of a \
             socket cannot overrule a process that is alive"
        );
        assert!(
            own.exists(),
            "the sweep must keep the directory of this process"
        );
    }

    /// The sweep must delete the directory of a coordinator that stopped.
    ///
    /// A sweep that keeps every directory does no work. Each shape here holds
    /// the proof that nobody listens: a socket that refuses a connection, and
    /// no socket at all.
    #[test]
    fn the_sweep_deletes_the_directory_of_a_coordinator_that_stopped() {
        let uid = unsafe { libc::getuid() };
        let base = TestDir::make("qex-deadtest");

        let no_socket = base.path().join(format!("qex-{uid}-nosocket"));
        let dead_socket = base.path().join(format!("qex-{uid}-dead"));
        let dead_pid = base.path().join(format!("qex-{uid}-deadpid"));
        for dir in [&no_socket, &dead_socket, &dead_pid] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A REAL socket file that no process holds. This is the file that a
        // coordinator leaves when a kill stops it, and a connect to it gives a
        // refusal on every system.
        a_socket_file_with_no_owner(&dead_socket.join("s"));
        a_socket_file_with_no_owner(&dead_pid.join("s"));
        // The number of a process that OPERATES, and no lock on the file.
        //
        // This is the case that a test of the number gets wrong. The number
        // names a live process, so such a test keeps this directory for ever.
        // Nothing holds the lock, so no coordinator uses the directory.
        std::fs::write(dead_pid.join(PID_FILE), std::process::id().to_string()).unwrap();

        sweep_socket_dirs(base.path(), &[], std::time::Duration::from_secs(3));

        assert!(
            !no_socket.exists(),
            "the sweep must delete a directory with no socket"
        );
        assert!(
            !dead_socket.exists(),
            "the sweep must delete a directory with a socket that refuses a connection"
        );
        assert!(
            !dead_pid.exists(),
            "the sweep must delete a directory that holds a number and no lock; a number \
             is not evidence that a coordinator operates"
        );
    }

    /// The sweep keeps its time limit, and each sweep continues the work.
    ///
    /// The limit is a time and not a count of directories, because one
    /// directory can cost microseconds and another can cost a whole probe. The
    /// property that matters is progress: a sweep with a small limit does part
    /// of the work, and the sweeps that follow do the rest.
    ///
    /// This test measures no split of the work. The cost of one entry changes
    /// with the load of the machine, so a test that names a number of
    /// directories fails when the machine is busy.
    #[test]
    fn each_sweep_continues_the_work_that_the_limit_stopped() {
        let uid = unsafe { libc::getuid() };
        let base = TestDir::make("qex-limittest");
        let count = 200;
        for n in 0..count {
            std::fs::create_dir_all(base.path().join(format!("qex-{uid}-{n}"))).unwrap();
        }
        let left = || std::fs::read_dir(base.path()).unwrap().count();

        // A limit of no time must stop the sweep before the first directory.
        sweep_socket_dirs(base.path(), &[], std::time::Duration::ZERO);
        assert_eq!(
            left(),
            count,
            "a sweep with no time must delete no directory"
        );

        // Sweeps with a small limit must finish the work between them. Each one
        // deletes what it can and leaves the rest.
        let mut sweeps = 0;
        while left() > 0 {
            sweeps += 1;
            assert!(
                sweeps <= 10_000,
                "the sweeps make no progress; {} directories stay",
                left()
            );
            sweep_socket_dirs(base.path(), &[], std::time::Duration::from_micros(500));
        }

        // A generous limit must do the whole work in one sweep.
        for n in 0..count {
            std::fs::create_dir_all(base.path().join(format!("qex-{uid}-{n}"))).unwrap();
        }
        sweep_socket_dirs(base.path(), &[], std::time::Duration::from_secs(30));
        assert_eq!(left(), 0, "a sweep with time must finish the work");
    }

    /// A socket that gives no answer must never say that nobody listens.
    ///
    /// The answer of the limit decides whether a caller deletes the socket of a
    /// different coordinator, so this test holds the meaning of that answer.
    #[test]
    fn a_socket_that_never_answers_gives_the_unknown_answer() {
        let base = TestDir::make("qex-answertest");
        let stuck = base.path().join("s");
        let Some(_sockets) = a_socket_that_never_answers(&stuck) else {
            // This system refuses a connection when the queue of a socket is
            // full, so no socket here can give "no answer". The state that this
            // test measures does not exist on it.
            eprintln!(
                "this test did not run: this system gives a refusal, and not a wait, for \
                 a socket with a full queue"
            );
            return;
        };

        // Ask on a thread. A question that gives no answer is then a failure of
        // this test, and it does not stop the whole test suite.
        let (sender, receiver) = std::sync::mpsc::channel();
        let path = stuck.clone();
        std::thread::spawn(move || {
            sender
                .send(ask_socket(&path, std::time::Duration::from_millis(50)))
                .ok();
        });
        let answer = receiver.recv_timeout(std::time::Duration::from_secs(10));

        assert_eq!(
            answer.ok(),
            Some(SocketAnswer::Unknown),
            "a socket that does not answer inside the limit can belong to a coordinator \
             that is busy, and the question must stop at that limit"
        );
    }

    /// A connect that the system finishes later must give the true answer.
    ///
    /// One system finishes a connect at once and another gives `EINPROGRESS`
    /// and finishes it later. The second answer arrives in `SO_ERROR`, and a
    /// caller that reads the answer of `connect` alone never sees it. Such a
    /// caller reads "not known" for a socket that nobody holds, and a
    /// coordinator that reads "not known" for its own socket stops for ever.
    ///
    /// THIS TEST USES A SOCKET OF THE NETWORK, because a Unix socket on this
    /// system finishes every connect at once. The code under test reads the
    /// result of the system, and it does not depend on the family of the
    /// socket.
    #[test]
    fn a_connect_that_finishes_later_gives_the_true_answer() {
        // A PORT THAT THIS SYSTEM NEVER GIVES TO A PROGRAM THAT ASKS FOR ONE.
        //
        // The port 1 needs a privilege, so no test can take it, and it is
        // outside the range that the system gives for a connection that a
        // program makes. A test that binds a port and closes it does not give
        // this promise: the system gives that number to the next program that
        // asks, and the connect of this test then reaches that program.
        let port: u16 = 1;

        let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        address.sin_family = libc::AF_INET as libc::sa_family_t;
        address.sin_port = port.to_be();
        address.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);

        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "the test cannot open a socket");
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            let result = libc::connect(
                fd,
                &address as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
            let error = std::io::Error::last_os_error().raw_os_error();
            if result == 0 || error != Some(libc::EINPROGRESS) {
                libc::close(fd);
                eprintln!(
                    "this test did not run: this system finished the connect at once, so \
                     it makes no `EINPROGRESS` to measure"
                );
                return;
            }

            let answer = finish_connect(
                fd,
                std::time::Instant::now() + std::time::Duration::from_secs(10),
            );
            libc::close(fd);

            if answer == Some(SocketAnswer::Answers) {
                // A program listens at this port. The test cannot measure a
                // refusal, and it must not report a fault of qex.
                eprintln!(
                    "this test did not run: a program listens at the port {port}, so this \
                     test cannot measure a refusal"
                );
                return;
            }
            assert_eq!(
                answer,
                Some(SocketAnswer::NobodyListens),
                "the answer of a connect that the system finishes later arrives in \
                 `SO_ERROR`, and nobody listens at this port"
            );
        }
    }

    /// A socket that no process holds must say that nobody listens.
    #[test]
    fn a_socket_with_no_process_says_that_nobody_listens() {
        let base = TestDir::make("qex-refusetest");
        let missing = base.path().join("s");
        assert_eq!(
            ask_socket(&missing, std::time::Duration::from_millis(50)),
            SocketAnswer::NobodyListens,
            "a socket file that is not there holds no coordinator"
        );

        // A REAL socket file whose process is gone. This is the state that a
        // coordinator leaves behind, and the answer decides whether a new
        // coordinator can start at all.
        a_socket_file_with_no_owner(&missing);
        assert_eq!(
            ask_socket(&missing, std::time::Duration::from_millis(50)),
            SocketAnswer::NobodyListens,
            "a socket file that no process holds refuses a connection"
        );

        // A file that is not a socket. One system refuses a connection to it
        // and another says that the path is not a socket. Nothing can listen at
        // such a path, so qex must give one answer for both.
        let plain = base.path().join("plain");
        std::fs::write(&plain, b"").unwrap();
        assert_eq!(
            ask_socket(&plain, std::time::Duration::from_millis(50)),
            SocketAnswer::NobodyListens,
            "a path that is not a socket holds no coordinator"
        );
    }

    /// Waits until the pid file reaches a state, and gives the last state it saw.
    ///
    /// A test cannot read the state one time. `Command::spawn` in a different
    /// test makes a copy of every open file descriptor of this process, and a
    /// lock stays while that copy is open. The copy closes when the new program
    /// starts, so the state settles in a moment.
    fn wait_for_pid_file(dir: &std::path::Path, want: PidFile) -> PidFile {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let state = pid_file_state(dir);
            if state == want || std::time::Instant::now() >= deadline {
                return state;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The lock says that a coordinator operates, and the number does not.
    ///
    /// A number goes back to the system when a process stops, and the system
    /// gives it to a new process later. A test of the number thus keeps a state
    /// directory that no coordinator uses, and nothing can correct it.
    #[test]
    fn a_number_in_the_pid_file_is_not_evidence_of_a_coordinator() {
        let base = TestDir::make("qex-pidtest");

        assert_eq!(
            pid_file_state(base.path()),
            PidFile::Free,
            "a directory with no pid file holds no coordinator"
        );

        // The number of THIS process, which operates, and no lock on the file.
        std::fs::write(base.path().join(PID_FILE), std::process::id().to_string()).unwrap();
        assert_eq!(
            pid_file_state(base.path()),
            PidFile::Free,
            "a number in a file is not evidence of a coordinator; only the lock is"
        );

        let held = a_held_pid_file(base.path());
        assert_eq!(
            pid_file_state(base.path()),
            PidFile::Held,
            "a process holds the lock, so a coordinator operates"
        );
        drop(held);
    }

    /// A try for the lock must leave the file of the owner as it was.
    ///
    /// A truncation before the lock empties the file of the coordinator that
    /// owns the lock, and that coordinator loses its number for a reader.
    #[test]
    fn a_try_that_fails_leaves_the_file_of_the_owner_as_it_was() {
        let base = TestDir::make("qex-truncatetest");
        let socket = base.path().join("s");
        let path = base.path().join(PID_FILE);
        std::fs::write(&path, b"12345").unwrap();
        let held = a_held_pid_file(base.path());

        // A short patience: this test asks for a lock that it cannot get.
        let answer = hold_pid_file(&socket, std::time::Duration::from_millis(50));
        assert!(
            matches!(answer, PidHold::Busy),
            "a lock that a different process holds must give Busy"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string(),
            "the try emptied the file of the process that holds the lock"
        );
        drop(held);
    }

    /// A coordinator must not hold its lock on a file that a sweep deleted.
    ///
    /// A sweep can hold the lock, delete the file and the directory, and then
    /// give the lock back. A coordinator that takes the lock at that moment
    /// holds it on an inode with no name. Every later process locks a different
    /// inode and sees nothing, so the one common lock of this design is gone.
    ///
    /// THIS TEST READS `st_nlink`, BECAUSE THE FAULT IS INVISIBLE FROM OUTSIDE.
    /// A lock on a file with no name gives no error and changes no answer of
    /// the system. The count of the names is the one mark that it leaves.
    #[test]
    fn the_lock_of_a_coordinator_is_on_a_file_that_a_name_points_to() {
        let base = TestDir::make("qex-nlinktest");
        let socket = base.path().join("s");
        let path = base.path().join(PID_FILE);
        std::fs::write(&path, b"").unwrap();

        // A sweep: it holds the lock, deletes the file, and gives the lock back.
        let dir = base.path().to_path_buf();
        let (ready, holds_the_lock) = std::sync::mpsc::channel();
        let sweep = std::thread::spawn(move || {
            let file = a_held_pid_file(&dir);
            ready.send(()).ok();
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::fs::remove_file(dir.join(PID_FILE)).ok();
            drop(file);
        });
        holds_the_lock
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the thread of the test did not take the lock");

        let answer = hold_pid_file(&socket, std::time::Duration::from_secs(5));
        sweep.join().unwrap();

        let PidHold::Held(lock) = answer else {
            panic!("the coordinator must take the lock after the sweep gives it back");
        };
        assert!(
            lock.nlink() >= 1,
            "the coordinator holds its lock on a file that no name points to"
        );
        assert_eq!(
            lock.ino(),
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap()),
            "the coordinator must hold the lock on the file that the path names now"
        );
    }

    /// A remedy must never name a step that the reader cannot take.
    ///
    /// A file that is not there cannot be deleted. An agent acts on the words
    /// of a message, so a wrong step costs it a command and gives no progress.
    #[test]
    fn a_file_that_is_not_there_does_not_ask_the_reader_to_delete_it() {
        let base = TestDir::make("qex-remedytest");
        // A directory that is not there. The open of the pid file gives
        // `NotFound`, and no deletion can correct that.
        let socket = base.path().join("no-such-directory").join("s");

        let answer = hold_pid_file(&socket, std::time::Duration::from_millis(50));
        let PidHold::Unusable(text) = answer else {
            panic!("a directory that is not there must give Unusable");
        };
        assert!(
            !text.contains("Delete that file"),
            "the message asks the reader to delete a file that is not there: {text}"
        );
        assert!(
            text.contains("Start qex again"),
            "the message must give a step that the reader can take: {text}"
        );
    }

    /// A coordinator must wait through the moment that a probe holds the lock.
    ///
    /// A sweep takes this lock to test it and gives it back at once. A single
    /// try can meet that moment, and a coordinator that stopped on it would
    /// refuse to start with no coordinator alive.
    #[test]
    fn a_coordinator_waits_through_the_moment_that_a_probe_holds_the_lock() {
        let base = TestDir::make("qex-patiencetest");
        let socket = base.path().join("s");
        std::fs::write(base.path().join(PID_FILE), b"").unwrap();

        // A different thread holds the lock for a moment, as a probe does.
        //
        // The thread SIGNALS when it holds the lock. A test that waits for a
        // time instead gives the lock to the main thread when the machine is
        // busy, and the failure then names the test and not the product.
        let dir = base.path().to_path_buf();
        let (ready, holds_the_lock) = std::sync::mpsc::channel();
        let probe = std::thread::spawn(move || {
            let file = a_held_pid_file(&dir);
            ready.send(()).ok();
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(file);
        });
        holds_the_lock
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the thread of the test did not take the lock");

        let answer = hold_pid_file(&socket, std::time::Duration::from_secs(5));
        probe.join().unwrap();
        assert!(
            matches!(answer, PidHold::Held(_)),
            "a moment of a probe must not read as a coordinator"
        );
    }

    /// The sweep holds the lock while it deletes.
    ///
    /// A coordinator that starts between the test and the deletion would lose
    /// its directory. It must meet the lock instead.
    #[test]
    fn the_sweep_holds_the_lock_while_it_deletes() {
        let base = TestDir::make("qex-claimtest");
        std::fs::write(base.path().join(PID_FILE), b"").unwrap();

        let claim = claim_unused_dir(base.path(), std::time::Duration::from_millis(50))
            .expect("a directory with a free lock and no socket must give a claim");
        assert_eq!(
            pid_file_state(base.path()),
            PidFile::Held,
            "the claim must hold the lock while the caller deletes the directory"
        );
        drop(claim);
        assert_eq!(
            pid_file_state(base.path()),
            PidFile::Free,
            "the claim must give the lock back when it goes out of scope"
        );

        // A DIRECTORY WITH NO PID FILE MUST ALSO GIVE A LOCK.
        //
        // The sweep and a coordinator that starts in the directory must lock
        // one inode. A sweep that only reads takes no lock here, and the two
        // then have no common lock at all.
        let empty = TestDir::make("qex-claimempty");
        let claim = claim_unused_dir(empty.path(), std::time::Duration::from_millis(50))
            .expect("a directory with no pid file must give a claim");
        assert_eq!(
            pid_file_state(empty.path()),
            PidFile::Held,
            "the claim must make the pid file and hold its lock, so that a coordinator \
             that starts here meets the same lock"
        );
        drop(claim);
    }

    /// A kill that a process cannot catch gives the lock back.
    ///
    /// This property is the reason that the lock is the evidence and the number
    /// is not. The kernel gives an `flock` back when the process stops, whatever
    /// stops it, so a coordinator that a kill removes cannot leave evidence that
    /// outlives it.
    #[test]
    fn a_kill_of_the_process_gives_the_lock_back() {
        let base = TestDir::make("qex-killtest");
        let path = base.path().join(PID_FILE);
        std::fs::write(&path, b"").unwrap();
        let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();

        // The child takes the lock and then waits. It uses the calls of the
        // system only, which a process may use after a fork.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "the test cannot make a process");
        if child == 0 {
            unsafe {
                let fd = libc::open(name.as_ptr(), libc::O_RDWR);
                if fd < 0 || libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
                    libc::_exit(1);
                }
                loop {
                    libc::pause();
                }
            }
        }

        let held = wait_for_pid_file(base.path(), PidFile::Held);
        if held != PidFile::Held {
            // The child could not take the lock, so this test cannot measure
            // the product. A machine with no more file descriptors gives this.
            // Say so, and do not report a fault of qex that this test did not
            // see.
            let mut status = 0;
            let gone = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
            if gone != 0 {
                // The child stopped before it took the lock. A machine with no
                // more file descriptors gives this. The test cannot measure the
                // product, so it says so and stops. It must not report a fault
                // of qex that it did not see.
                eprintln!(
                    "this test did not run: the child could not take the lock, and the \
                     state of qex is not known from it"
                );
                return;
            }
            unsafe { libc::kill(child, libc::SIGKILL) };
            unsafe { libc::waitpid(child, &mut status, 0) };
            panic!("the child holds the lock, so the file must say that a process operates");
        }

        // SIGKILL. A process cannot catch it, and it cannot clean anything.
        unsafe {
            libc::kill(child, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(child, &mut status, 0);
        }

        assert_eq!(
            wait_for_pid_file(base.path(), PidFile::Free),
            PidFile::Free,
            "the kernel must give the lock back when the process stops, and a kill that \
             the process cannot catch is the test of that promise"
        );
    }

    #[test]
    fn ensure_dir_applies_mode_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = test_root().join(format!("qex-mode-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        ensure_dir(&dir, 0o700).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "the group and other users must not read job directories"
        );
        // On an existing directory, the function must set the mode again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_dir(&dir, 0o700).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }
}
