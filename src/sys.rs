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
    meminfo_field("MemAvailable:").unwrap_or_else(|| total_memory())
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
    // Call host_statistics64 with HOST_VM_INFO64. A new allocation can use the
    // free pages and the inactive pages without a page operation to disk.
    const HOST_VM_INFO64: libc::c_int = 4;
    const HOST_VM_INFO64_COUNT: libc::mach_msg_type_number_t = 38;

    #[repr(C)]
    #[derive(Default)]
    struct VmStatistics64 {
        free_count: u32,
        active_count: u32,
        inactive_count: u32,
        wire_count: u32,
        // The kernel structure has more fields, but qex reads the first fields
        // only. The count that qex sends to the kernel covers all the fields.
        rest: [u32; 34],
    }

    let mut stats = VmStatistics64::default();
    let mut count = HOST_VM_INFO64_COUNT;
    let rc = unsafe {
        libc::host_statistics64(
            libc::mach_host_self(),
            HOST_VM_INFO64,
            &mut stats as *mut _ as *mut libc::integer_t,
            &mut count,
        )
    };
    if rc != 0 {
        return None;
    }
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some((stats.free_count as u64 + stats.inactive_count as u64) * page_size)
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
            b"kern.boottime\0".as_ptr() as *const libc::c_char,
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
