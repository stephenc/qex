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

    Ok(dir)
}

/// The time that qex waits for one socket to answer.
///
/// A socket on this machine answers in less than one millisecond. A longer wait
/// gives no more information. Without this limit, a connect to a socket that a
/// live process holds, but never accepts, waits for ever.
const SOCKET_ANSWER_LIMIT: std::time::Duration = std::time::Duration::from_millis(100);

/// The time that one sweep of the temporary directory can take.
///
/// The sweep stops at this limit and leaves the rest of the directories. The
/// next coordinator continues the work. The limit is a time and not a count of
/// directories: a count leaves the same directories at each start, and they
/// then stay for ever.
const SWEEP_LIMIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Deletes the short socket directories that no coordinator uses.
///
/// Each test run and each unusual state directory can make one of these
/// directories in `/tmp`. Without this step they stay for ever.
///
/// This function deletes a directory of this user only, and only when it holds
/// no socket that answers.
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

        // Keep a directory with a socket that answers. A coordinator uses it.
        if socket_answers(&entry.path().join("s"), SOCKET_ANSWER_LIMIT) {
            continue;
        }
        std::fs::remove_dir_all(entry.path()).ok();
    }
}

/// Tests if a socket answers inside `limit`.
///
/// The standard library gives no connect with a time limit for a Unix socket.
/// This function thus uses a socket that does not block, and it tries again
/// until the limit.
///
/// The limit is necessary. A process that holds the socket, but never accepts a
/// connection, makes a connect without a limit wait for ever. A socket that
/// does not answer inside the limit is a socket that no coordinator uses.
pub fn socket_answers(socket: &std::path::Path, limit: std::time::Duration) -> bool {
    let Some(address) = unix_address(socket) else {
        return false;
    };

    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(answer) = connect_once(&address) {
            return answer;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
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

/// Makes one attempt to connect, and never blocks.
///
/// The result is `Some(true)` when the socket accepts the connection, and
/// `Some(false)` when it refuses. The result is `None` when the socket is full
/// at this moment: the caller can try again, or stop at its own limit.
fn connect_once(address: &libc::sockaddr_un) -> Option<bool> {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Some(false);
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        let result = libc::connect(
            fd,
            address as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        let error = std::io::Error::last_os_error().raw_os_error();
        libc::close(fd);
        if result == 0 {
            return Some(true);
        }
        match error {
            Some(libc::EAGAIN) | Some(libc::EINPROGRESS) => None,
            _ => Some(false),
        }
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

        // A different state directory must give a different socket.
        let _s2 = EnvVar::set("XDG_STATE_HOME", &format!("{long}other/"));
        assert_ne!(p, socket_path().unwrap());

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    /// Opens a socket that accepts no connection, and fills its backlog.
    ///
    /// A connect to this socket blocks. The standard `UnixListener` asks for a
    /// backlog of 128, so this test uses `libc` and asks for a backlog of one.
    ///
    /// The result holds the listener and the connections. The caller must keep
    /// them, because a closed socket answers at once with a refusal.
    fn a_socket_that_never_answers(path: &std::path::Path) -> Vec<libc::c_int> {
        let address = unix_address(path).expect("the path of the test socket is short");
        let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let mut open = Vec::new();

        unsafe {
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
                    libc::close(client);
                    break;
                }
                open.push(client);
            }
        }

        assert!(
            connect_once(&address).is_none(),
            "the test needs a socket with a full backlog"
        );
        open
    }

    /// The sweep must give a result, and a socket cannot stop it.
    ///
    /// A live process can hold a socket and accept no connection. A connect to
    /// that socket without a time limit waits for ever, and the coordinator
    /// then never opens its own socket. Every qex command on the machine then
    /// fails with "the coordinator did not start".
    #[test]
    fn a_socket_that_never_answers_does_not_stop_the_sweep() {
        let uid = unsafe { libc::getuid() };
        // The name of this directory does not start with `qex-<uid>-`, so a
        // coordinator on this machine does not delete it.
        let base = std::env::temp_dir().join(format!("qex-reaptest-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();

        let stale = base.join(format!("qex-{uid}-stale"));
        let dead = base.join(format!("qex-{uid}-dead"));
        let own = base.join(format!("qex-{uid}-own"));
        let stuck = base.join(format!("qex-{uid}-stuck"));
        for dir in [&stale, &dead, &own, &stuck] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A socket file that no process holds. A connect gives a refusal.
        std::fs::write(dead.join("s"), b"").unwrap();
        let open = a_socket_that_never_answers(&stuck.join("s"));

        // Run the sweep on a thread. A sweep that waits for ever then gives a
        // failure, and it does not stop the whole test suite.
        let (sender, receiver) = std::sync::mpsc::channel();
        let directory = base.clone();
        let keep = own.clone();
        std::thread::spawn(move || {
            sweep_socket_dirs(&directory, &[keep], std::time::Duration::from_secs(3));
            sender.send(()).ok();
        });
        let finished = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok();

        let results = [stale.exists(), dead.exists(), own.exists(), stuck.exists()];

        for fd in open {
            unsafe { libc::close(fd) };
        }
        std::fs::remove_dir_all(&base).ok();

        assert!(
            finished,
            "the sweep waits for a socket that never answers; every qex command then waits"
        );
        assert!(
            !results[0],
            "the sweep must delete a directory with no socket"
        );
        assert!(
            !results[1],
            "the sweep must delete a directory with a dead socket"
        );
        assert!(
            results[2],
            "the sweep must keep the directory of this process"
        );
        assert!(
            !results[3],
            "a socket that does not answer inside the limit is stale"
        );
    }

    /// The sweep stops at its time limit and leaves the rest for the next
    /// coordinator. A limit on the count of directories keeps the same
    /// directories for ever, so the limit is a time.
    #[test]
    fn the_sweep_stops_at_its_time_limit() {
        let uid = unsafe { libc::getuid() };
        let base = std::env::temp_dir().join(format!("qex-limittest-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).unwrap();
        for n in 0..20 {
            std::fs::create_dir_all(base.join(format!("qex-{uid}-{n}"))).unwrap();
        }

        let start = std::time::Instant::now();
        sweep_socket_dirs(&base, &[], std::time::Duration::ZERO);
        let elapsed = start.elapsed();
        let left = std::fs::read_dir(&base).unwrap().count();
        std::fs::remove_dir_all(&base).ok();

        assert!(elapsed < std::time::Duration::from_secs(1));
        assert_eq!(left, 20, "a sweep with no time must delete no directory");
    }

    #[test]
    fn ensure_dir_applies_mode_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("qex-mode-{}", std::process::id()));
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
