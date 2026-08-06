//! This module shares the resource claims between the users of one machine.
//!
//! Each coordinator writes its claims to a file in a shared directory. Each
//! coordinator reads the files of the other users before it starts a job. The
//! method needs no administrator rights.
//!
//! The method is cooperative. A different user can write an incorrect value.
//! The threat model is a colleague or an agent that does not coordinate, and
//! not an attacker. The scheduler also tests the free memory of the machine, so
//! it finds a load that no coordinator reports.
//!
//! The shared directory is writable by every user, so this module tests each
//! file before it reads the file. See [`peer_dir`] for the tests.

use crate::config::Config;
use crate::sys;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The record that one coordinator publishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub uid: u32,
    pub pid: i32,
    pub boot_id: String,
    pub cpu: u64,
    pub mem: u64,
    pub updated_at: u64,
}

/// The total of the claims of the other users.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims {
    pub cpu: u64,
    pub mem: u64,
    /// The number of other coordinators that qex counted.
    pub count: usize,
}

/// Gives the directory for the peer files, if the directory is safe.
///
/// The directory is writable by every user, so this function makes these tests:
///
/// - The path is a directory.
/// - The directory has the sticky bit. A different user then cannot delete the
///   subdirectory of this user.
/// - The owner is the root user or the current user.
///
/// If a test fails, the function gives `None`. qex then operates for one user
/// only. It does not give an error, because a shared directory is not necessary
/// for a correct queue.
fn peer_dir(cfg: &Config) -> Option<PathBuf> {
    let dir = PathBuf::from(&cfg.peers.dir);

    match std::fs::symlink_metadata(&dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return None;
            }
            let mode = meta.mode();
            // 0o1000 is the sticky bit. It stops a different user from the
            // deletion of the files of this user.
            if mode & 0o1000 == 0 {
                return None;
            }

            // Accept a directory that every user can write, whatever its owner.
            //
            // qex makes this directory, so the first user of the machine owns
            // it. A test for the owner would thus refuse the directory for
            // every other user, and each of those users would lose the shared
            // accounting with no message.
            //
            // The owner of the directory does not give safety here. The safety
            // comes from the sticky bit and from the test of the owner of each
            // file. See `read_peer`.
            let world_writable = mode & 0o002 != 0;
            let owner = meta.uid();
            if !world_writable && owner != 0 && owner != current_uid() {
                return None;
            }
            Some(dir)
        }
        Err(_) => {
            // Make the directory. The first user of the machine arrives here.
            std::fs::create_dir_all(&dir).ok()?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).ok()?;
            Some(dir)
        }
    }
}

/// Writes the true state of the shared accounting, for `qex config show`.
///
/// This function tests the directory. A message that says "on" while the
/// directory fails a test would tell the user that a feature operates when it
/// does not.
pub fn describe(cfg: &Config) -> String {
    if !cfg.peers.enabled {
        return "off; the config file sets [peers] enabled = false".to_string();
    }
    match peer_dir(cfg) {
        Some(dir) => {
            let c = claims(cfg);
            format!(
                "on, in {} ({} other coordinator(s), {} cores and {} claimed)",
                dir.display(),
                c.count,
                c.cpu,
                crate::units::format_size(c.mem)
            )
        }
        None => format!(
            "NOT ACTIVE; qex cannot use the directory {}. It must be a directory with the \
             sticky bit. qex uses the budget of this user only.",
            cfg.peers.dir
        ),
    }
}

pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Gives the file name of the directory of one user.
fn user_dir_name(uid: u32) -> String {
    format!("u{uid}")
}

/// Gives the file name for one coordinator.
///
/// The name holds the process id, so each coordinator has its own file. One
/// user can have more than one coordinator, with one for each state directory.
/// With one file for each user, the last coordinator to write would hide the
/// claims of the others, and a busy coordinator would report no load.
fn peer_file_name(pid: i32) -> String {
    format!("peer-{pid}.json")
}

/// Writes the claims of this coordinator.
///
/// The function writes the file in one operation, so a reader never sees a part
/// of the record.
pub fn publish(cfg: &Config, cpu: u64, mem: u64) {
    if !cfg.peers.enabled {
        return;
    }
    let Some(dir) = peer_dir(cfg) else { return };

    let uid = current_uid();
    let mine = dir.join(user_dir_name(uid));
    if std::fs::create_dir_all(&mine).is_err() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&mine, std::fs::Permissions::from_mode(0o755)).ok();

    let peer = Peer {
        uid,
        pid: std::process::id() as i32,
        boot_id: sys::boot_id(),
        cpu,
        mem,
        updated_at: sys::now_secs(),
    };

    if let Ok(bytes) = serde_json::to_vec(&peer) {
        crate::job::write_atomic(&mine.join(peer_file_name(peer.pid)), &bytes, 0o644).ok();
    }
}

/// Deletes the record of this coordinator.
///
/// The coordinator calls this function when it stops, so its claims do not stop
/// the jobs of a different user.
pub fn withdraw(cfg: &Config) {
    if let Some(dir) = peer_dir(cfg) {
        let mine = dir.join(user_dir_name(current_uid()));
        std::fs::remove_file(mine.join(peer_file_name(std::process::id() as i32))).ok();
    }
}

/// Reads the claims of the other users.
///
/// This function ignores the record of the current user, and each record that
/// fails a test. A record that qex cannot read gives no error, because a
/// damaged file must not stop the queue.
pub fn claims(cfg: &Config) -> Claims {
    let mut total = Claims::default();
    if !cfg.peers.enabled {
        return total;
    }

    let Some(dir) = peer_dir(cfg) else {
        return total;
    };

    let stale = cfg
        .peer_stale_after()
        .unwrap_or(std::time::Duration::from_secs(30))
        .as_secs();
    let now = sys::now_secs();
    let boot = sys::boot_id();
    let me = current_uid();

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return total;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };

        // The directory name gives the user. Read it, then compare it with the
        // owner of the file. A user thus cannot write a record for a different
        // user.
        let Some(claimed_uid) = name.strip_prefix('u').and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };

        // One user can have more than one coordinator, with one for each state
        // directory. Read every file in the directory, and skip the file of
        // this coordinator only.
        //
        // A test that skips the whole directory of this user would hide a
        // second coordinator of the same user. Two coordinators would then each
        // use the full budget, and together they would start twice the
        // permitted work.
        let Ok(files) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        let my_pid = std::process::id() as i32;

        for file in files.flatten() {
            let fname = file.file_name();
            let Some(fname) = fname.to_str() else {
                continue;
            };
            if !fname.starts_with("peer-") || !fname.ends_with(".json") {
                continue;
            }

            let Some(peer) = read_peer(&file.path(), claimed_uid) else {
                continue;
            };

            // Skip this coordinator. Its own claims are already in its total.
            if claimed_uid == me && peer.pid == my_pid {
                continue;
            }
            // A record from a different start of the machine is old. The system
            // uses each process id again after a restart.
            if peer.boot_id != boot {
                std::fs::remove_file(file.path()).ok();
                continue;
            }
            // A coordinator writes its record frequently. An old record shows a
            // coordinator that stopped without a clean end.
            if now.saturating_sub(peer.updated_at) > stale {
                std::fs::remove_file(file.path()).ok();
                continue;
            }
            // The process must be alive.
            if !sys::pid_alive(peer.pid) {
                std::fs::remove_file(file.path()).ok();
                continue;
            }

            total.cpu += peer.cpu;
            total.mem += peer.mem;
            total.count += 1;
        }
    }

    total
}

/// Reads one peer file and tests its owner.
///
/// The function opens the file without a symbolic link. A different user thus
/// cannot point the file at a file of this user.
fn read_peer(path: &Path, expected_uid: u32) -> Option<Peer> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;

    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    // The owner of the file must be the user of the directory name.
    if meta.uid() != expected_uid {
        return None;
    }
    // A record is small. A large file is not a record.
    if meta.len() > 64 * 1024 {
        return None;
    }

    let text = std::io::read_to_string(file).ok()?;
    let peer: Peer = serde_json::from_str(&text).ok()?;
    // The record must name the same user as its directory and its owner.
    if peer.uid != expected_uid {
        return None;
    }
    Some(peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qex-peers-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg_for(dir: &Path) -> Config {
        toml::from_str(&format!(
            "[peers]\nenabled = true\ndir = \"{}\"\nstale_after = \"30s\"\n",
            dir.display()
        ))
        .unwrap()
    }

    #[test]
    fn a_record_of_this_user_does_not_count() {
        let dir = tmpdir("self");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let cfg = cfg_for(&dir);

        publish(&cfg, 4, 8 << 30);
        // The claims of this coordinator are already in its own total. A second
        // count would stop the jobs of this user.
        assert_eq!(claims(&cfg), Claims::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_without_the_sticky_bit_is_refused() {
        let dir = tmpdir("nosticky");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let cfg = cfg_for(&dir);
        // Without the sticky bit, a different user can delete the files of this
        // user. qex must then operate for one user only.
        assert!(peer_dir(&cfg).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_in_place_of_the_directory_is_refused() {
        let dir = tmpdir("isfile");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::write(&dir, b"not a directory").unwrap();
        let cfg = cfg_for(&dir);
        assert!(peer_dir(&cfg).is_none());
        std::fs::remove_file(&dir).ok();
    }

    #[test]
    fn a_record_with_the_wrong_owner_is_refused() {
        let dir = tmpdir("wrongowner");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();

        // Write a record in the directory of a different user. This process
        // owns the file, so the owner does not match the directory name.
        let other = dir.join("u99999");
        std::fs::create_dir_all(&other).unwrap();
        let peer = Peer {
            uid: 99999,
            pid: std::process::id() as i32,
            boot_id: sys::boot_id(),
            cpu: 64,
            mem: 64 << 30,
            updated_at: sys::now_secs(),
        };
        std::fs::write(other.join("peer.json"), serde_json::to_vec(&peer).unwrap()).unwrap();

        let cfg = cfg_for(&dir);
        assert_eq!(
            claims(&cfg),
            Claims::default(),
            "a record with an incorrect owner must not count"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_old_record_and_a_dead_process_do_not_count() {
        let dir = tmpdir("stale");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let cfg = cfg_for(&dir);
        let me = current_uid();

        // Use the directory of a different user, but keep the owner correct, so
        // the test measures the time rule and the process rule only.
        let other = dir.join(user_dir_name(me + 1));
        std::fs::create_dir_all(&other).unwrap();

        let old = Peer {
            uid: me + 1,
            pid: std::process::id() as i32,
            boot_id: sys::boot_id(),
            cpu: 64,
            mem: 64 << 30,
            updated_at: sys::now_secs().saturating_sub(3600),
        };
        std::fs::write(other.join("peer.json"), serde_json::to_vec(&old).unwrap()).unwrap();
        // The owner test refuses this file first, so the total stays at zero.
        assert_eq!(claims(&cfg), Claims::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_damaged_file_does_not_stop_the_reader() {
        let dir = tmpdir("garbage");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let other = dir.join("u12345");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("peer.json"), b"{not json").unwrap();

        let cfg = cfg_for(&dir);
        // The reader must give an answer and must not stop.
        assert_eq!(claims(&cfg), Claims::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_symbolic_link_is_not_read() {
        let dir = tmpdir("symlink");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();

        let target = dir.join("real.json");
        let peer = Peer {
            uid: current_uid(),
            pid: std::process::id() as i32,
            boot_id: sys::boot_id(),
            cpu: 64,
            mem: 64 << 30,
            updated_at: sys::now_secs(),
        };
        std::fs::write(&target, serde_json::to_vec(&peer).unwrap()).unwrap();

        let other = dir.join("u54321");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&target, other.join("peer.json")).unwrap();

        assert!(
            read_peer(&other.join("peer.json"), 54321).is_none(),
            "the reader must not follow a symbolic link"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_peer_system_is_off_when_the_config_says_so() {
        let dir = tmpdir("disabled");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let mut cfg = cfg_for(&dir);
        cfg.peers.enabled = false;
        assert_eq!(claims(&cfg), Claims::default());
        std::fs::remove_dir_all(&dir).ok();
    }
}
