//! This module reads the machine capacity and the current machine load.
//! It also holds the process functions that qex needs.
//!
//! Each function has a Linux version and a macOS version. If a load measurement
//! is not available, the function gives a safe default value. It does not give
//! an error. A measurement that qex cannot read must not stop a job.

use std::time::{SystemTime, UNIX_EPOCH};

/// Gives the number of cores that this machine can use.
pub fn cpu_count() -> u64 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1)
}

/// Gives the quantity of physical memory in bytes.
#[cfg(target_os = "linux")]
pub fn total_memory() -> u64 {
    meminfo_field("MemTotal:").unwrap_or(0)
}

#[cfg(target_os = "macos")]
pub fn total_memory() -> u64 {
    sysctl_u64(b"hw.memsize\0").unwrap_or(0)
}

/// Gives the quantity of memory that a new process can use now.
///
/// The machine can supply this memory without swap.
///
/// On Linux this value is `MemAvailable`. That value includes the page cache
/// that the kernel can reclaim, so it is more accurate than `MemFree`.
/// On macOS this value is the total of the free pages and the inactive pages.
#[cfg(target_os = "linux")]
pub fn available_memory() -> u64 {
    meminfo_field("MemAvailable:").unwrap_or_else(total_memory)
}

#[cfg(target_os = "macos")]
pub fn available_memory() -> u64 {
    vm_available().unwrap_or_else(total_memory)
}

/// Gives the memory pressure as a value from 0 to 100.
///
/// The result is `None` if the platform does not supply this measurement.
///
/// On Linux the value is the PSI `some avg10` field of `/proc/pressure/memory`.
/// It is the percentage of the last 10 seconds in which one task or more
/// stopped and waited for memory. This value increases before the quantity of
/// free memory decreases, so it is an earlier warning.
///
/// macOS does not have an equivalent measurement. The result is `None` there,
/// and the caller uses the free memory test only.
#[cfg(target_os = "linux")]
pub fn memory_pressure() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    let some = text.lines().find(|l| l.starts_with("some "))?;
    let field = some.split_whitespace().find(|f| f.starts_with("avg10="))?;
    field.trim_start_matches("avg10=").parse().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn memory_pressure() -> Option<f64> {
    None
}

#[cfg(target_os = "linux")]
fn meminfo_field(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with(key))?;
    // Each line has this format: "MemTotal:       29316304 kB"
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr() as *const libc::c_char,
            &mut value as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value)
}

#[cfg(target_os = "macos")]
fn vm_available() -> Option<u64> {
    // The structure and the count come from `libc`. qex made its own structure
    // before, and that structure was WRONG: the real one mixes 32-bit and
    // 64-bit fields and it aligns to 8 bytes, so the size that qex sent to the
    // kernel did not agree with the size that the kernel writes.
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;

    // `libc` marks `mach_host_self` as deprecated and gives the `mach2` crate
    // as the answer. qex reads one value from it, and a dependency for one
    // value is a poor exchange. The function itself is not deprecated: it is
    // the interface of the kernel, and it does not go away.
    #[allow(deprecated)]
    let rc = unsafe {
        libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut stats as *mut _ as *mut libc::integer_t,
            &mut count,
        )
    };
    if rc != 0 {
        return None;
    }

    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;

    // WHICH PAGES A NEW JOB CAN USE.
    //
    // The free pages are not the answer on macOS. macOS keeps the memory of
    // the machine in use, and it gives the memory back when a program asks for
    // it. A count of the free pages alone thus says that a machine with 16GB
    // has 300MB, and qex would then keep each job in the queue for ever on a
    // machine that has no fault.
    //
    // These four kinds of page go to a new job with no operation to the disk:
    //
    //   free         nothing holds them
    //   inactive     a program had them, and the kernel can take them back
    //   purgeable    a program said that the kernel can discard them
    //   speculative  the kernel read them before a program asked
    //
    // This total is higher than the memory that a job receives in the worst
    // case, and that is the correct direction on macOS. macOS compresses memory
    // and writes it to the disk; it does not stop a program for memory in the
    // way that the Linux out-of-memory killer does. A number that is too low
    // stops each job for ever, which is a fault with no remedy. A number that is
    // a little high makes the machine slow, which the user can see and correct.
    let usable = stats.free_count as u64
        + stats.inactive_count as u64
        + stats.purgeable_count as u64
        + stats.speculative_count as u64;

    Some(usable * page_size)
}

/// Gives an identifier for the current start of the machine.
///
/// qex deletes a peer record that has a different identifier. The system uses
/// each pid again after a restart. Without this test, an old record can look
/// like a live process.
pub fn boot_id() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
            return id.trim().to_string();
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(boot) = sysctl_boottime() {
            return boot;
        }
    }
    // Without this identifier, qex loses the restart test only. It continues to
    // test each peer process for life.
    "unknown".to_string()
}

#[cfg(target_os = "macos")]
fn sysctl_boottime() -> Option<String> {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<libc::timeval>();
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            &mut tv as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then(|| format!("boot-{}", tv.tv_sec))
}

/// Tests if a process is alive.
///
/// qex uses this function to delete the records of dead peers. It also uses the
/// function to find a coordinator that stopped and left its files.
///
/// For a live process of a different user, `kill(pid, 0)` gives `EPERM`. That
/// result also shows that the process is alive.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Gives the number of seconds after the Unix epoch.
///
/// qex writes each time value as an integer. A reader can then compare the
/// times in a status file without a date library.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_capacity_is_plausible() {
        assert!(cpu_count() >= 1);
        let total = total_memory();
        assert!(total > 0, "total memory probe returned zero");
        assert!(
            available_memory() <= total,
            "available memory exceeds total"
        );
    }

    #[test]
    fn pressure_is_a_percentage_when_reported() {
        if let Some(p) = memory_pressure() {
            assert!((0.0..=100.0).contains(&p), "pressure {p} out of range");
        }
    }

    #[test]
    fn liveness_check_agrees_about_this_process() {
        assert!(pid_alive(std::process::id() as i32));
        assert!(!pid_alive(-1));
        // For kill(2), the pid 0 means the current process group. qex must not
        // accept 0 as the pid of a live job.
        assert!(!pid_alive(0));
    }
}

/// The resources that one process group uses now.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GroupUsage {
    /// The memory of every process of the group, in bytes.
    pub rss: u64,
    /// The CPU time of every process of the group, in seconds.
    pub cpu_secs: f64,
    /// The number of processes in the group.
    pub processes: usize,
}

/// Measures the processes of one process group.
///
/// This function compares the process group id, which is a number. It does not
/// read a command line, so it cannot match a command that holds the word `qex`.
/// That fault is the reason for this program.
#[cfg(target_os = "linux")]
pub fn group_usage(pgid: i32) -> GroupUsage {
    let mut out = GroupUsage::default();
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.parse::<i32>().is_err() {
            continue;
        }

        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The command of a process can hold a space or a bracket, and it is
        // inside brackets. Read the fields after the last bracket.
        let Some(rest) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<&str> = rest.1.split_whitespace().collect();
        // After the command, field 1 is the state and field 3 is the group.
        if fields.len() < 22 {
            continue;
        }
        let Ok(group) = fields[2].parse::<i32>() else {
            continue;
        };
        if group != pgid {
            continue;
        }

        let utime: f64 = fields[11].parse().unwrap_or(0.0);
        let stime: f64 = fields[12].parse().unwrap_or(0.0);
        let rss_pages: u64 = fields[21].parse().unwrap_or(0);

        out.cpu_secs += (utime + stime) / ticks;
        out.rss += rss_pages * page;
        out.processes += 1;
    }
    out
}

/// Measures the processes of one process group.
///
/// macOS has no `/proc`, so this version reads the output of `ps`.
#[cfg(not(target_os = "linux"))]
pub fn group_usage(pgid: i32) -> GroupUsage {
    let mut out = GroupUsage::default();
    let Ok(result) = std::process::Command::new("ps")
        .args(["-A", "-o", "pgid=,rss=,time="])
        .output()
    else {
        return out;
    };

    for line in String::from_utf8_lossy(&result.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        if fields[0].parse::<i32>() != Ok(pgid) {
            continue;
        }
        // `ps` gives the memory in kilobytes.
        out.rss += fields[1].parse::<u64>().unwrap_or(0) * 1024;
        out.cpu_secs += parse_ps_time(fields[2]);
        out.processes += 1;
    }
    out
}

/// Reads a time from `ps`, in the form `MM:SS.ss` or `HH:MM:SS`.
#[cfg(not(target_os = "linux"))]
fn parse_ps_time(text: &str) -> f64 {
    let parts: Vec<&str> = text.split(':').collect();
    let mut seconds = 0.0;
    for part in &parts {
        seconds = seconds * 60.0 + part.parse::<f64>().unwrap_or(0.0);
    }
    seconds
}

/// Gives the time of day as `HH:MM:SS`, in the time zone of the machine.
pub fn clock_text(epoch_secs: u64) -> String {
    // The type comes from `localtime_r`. Do not name it: on musl the name
    // `libc::time_t` is deprecated, because that type becomes 64 bits.
    let t = epoch_secs as _;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut parts);
    }
    format!(
        "{:02}:{:02}:{:02}",
        parts.tm_hour, parts.tm_min, parts.tm_sec
    )
}

/// Tests if the standard input is a terminal.
///
/// A command that reads a key needs a terminal. In a pipe or a script there is
/// no key to read.
pub fn stdin_is_terminal() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

/// The size of the terminal that this command writes to, as `(rows, columns)`.
///
/// `None` when the output is not a terminal, or when the system does not
/// give a size. A page that has no size writes every job and draws no frame.
pub fn terminal_size() -> Option<(usize, usize)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if ok == 0 && size.ws_row > 0 {
        let cols = if size.ws_col > 0 {
            size.ws_col as usize
        } else {
            80
        };
        Some((size.ws_row as usize, cols))
    } else {
        None
    }
}
