//! This module reads one key at a time from the terminal.
//!
//! `qex top` uses it for the `q` key. A terminal normally gives a line to a
//! program when the user presses Enter, so this module turns that behaviour off
//! while the command operates.
//!
//! The terminal must go back to its usual behaviour when the command stops. A
//! signal stops a process without a call to `Drop`, so this module also puts the
//! terminal back from a signal handler.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The settings of the terminal before this module changed them.
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// True when the user pressed a key that stops the command.
static QUIT: AtomicBool = AtomicBool::new(false);

/// Puts the terminal back to its usual behaviour.
pub fn restore() {
    if let Ok(mut guard) = SAVED.lock() {
        if let Some(settings) = guard.take() {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &settings);
            }
        }
    }
}

extern "C" fn on_signal(_signal: libc::c_int) {
    restore();
    // Use `_exit`. A signal handler must call a few functions only, and the
    // usual exit path runs code that is not safe here.
    unsafe { libc::_exit(0) }
}

/// Reads each key in its own thread, and records the key that stops the command.
///
/// Gives `false` when there is no terminal. The command then operates without
/// a key, and a user stops it with Ctrl-C.
pub fn watch_for_quit() -> bool {
    if !crate::sys::stdin_is_terminal() {
        return false;
    }

    let mut settings: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut settings) } != 0 {
        return false;
    }
    if let Ok(mut guard) = SAVED.lock() {
        *guard = Some(settings);
    }

    let mut raw = settings;
    // ICANON gives a line at a time. ECHO writes each key to the screen.
    // Keep ISIG, so Ctrl-C still stops the command.
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        return false;
    }

    // Put the terminal back when a signal stops this process. Without this, a
    // Ctrl-C would leave the terminal without an echo of the keys.
    unsafe {
        // Cast through a pointer. A direct cast of a function to an integer
        // is not correct on every platform.
        let handler = on_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGHUP, handler);
    }

    std::thread::spawn(|| {
        use std::io::Read;
        let mut byte = [0u8; 1];
        while std::io::stdin().read(&mut byte).unwrap_or(0) == 1 {
            if byte[0] == b'q' || byte[0] == b'Q' {
                QUIT.store(true, Ordering::SeqCst);
                return;
            }
        }
    });

    true
}

/// Tests if the user asked to stop.
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}
