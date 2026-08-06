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

    #[test]
    fn ensure_dir_applies_mode_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("qex-mode-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        ensure_dir(&dir, 0o700).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the group and other users must not read job directories");
        // On an existing directory, the function must set the mode again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_dir(&dir, 0o700).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }
}
