//! Test-only helpers.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Gives a lock for the tests that change the process environment.
///
/// The function `std::env::set_var` changes data that all the threads share.
/// The test program starts many tests together. Without this lock, one test can
/// read a variable that a different test set. That failure occurs only when the
/// machine is busy, and it looks like a fault in the program.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // A poisoned lock shows that a different test stopped with a panic. The
    // environment is still usable. A failure of each subsequent test hides the
    // first failure.
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Sets an environment variable for one scope.
///
/// This type puts back the initial value at the end of the scope. It also puts
/// back the initial value if the test stops with a panic.
pub struct EnvVar {
    key: String,
    previous: Option<std::ffi::OsString>,
}

impl EnvVar {
    pub fn set(key: &str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self {
            key: key.to_string(),
            previous,
        }
    }

    pub fn unset(key: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self {
            key: key.to_string(),
            previous,
        }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => std::env::set_var(&self.key, v),
            None => std::env::remove_var(&self.key),
        }
    }
}
