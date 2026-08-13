//! This module reads one key at a time from the terminal.
//!
//! `qex top` uses it to move the selection and to act on a job. A terminal
//! normally gives a line to a program when the user presses Enter, so this
//! module turns that behaviour off while the command operates.
//!
//! The terminal must go back to its usual behaviour when the command stops. A
//! signal stops a process without a call to `Drop`, so this module also puts the
//! terminal back from a signal handler.

use std::collections::VecDeque;
use std::sync::Mutex;

/// The settings of the terminal before this module changed them.
static SAVED: Mutex<Option<libc::termios>> = Mutex::new(None);

/// Keys that the reader thread has not yet given to the command.
static EVENTS: Mutex<VecDeque<Key>> = Mutex::new(VecDeque::new());

/// One key from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(u8),
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Esc,
}

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

/// Reads each key in its own thread.
///
/// Gives `false` when there is no terminal. The command then operates without
/// a key, and a user stops it with Ctrl-C.
pub fn watch() -> bool {
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
        let mut stdin = std::io::stdin();
        let mut byte = [0u8; 1];
        while stdin.read(&mut byte).unwrap_or(0) == 1 {
            let key = if byte[0] == 0x1b {
                read_escape(&mut stdin)
            } else {
                decode_plain(byte[0])
            };
            push(key);
        }
    });

    true
}

/// Gives every key that arrived since the last call, in the order of arrival.
pub fn take() -> Vec<Key> {
    match EVENTS.lock() {
        Ok(mut q) => q.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

fn push(key: Key) {
    if let Ok(mut q) = EVENTS.lock() {
        q.push_back(key);
    }
}

fn decode_plain(byte: u8) -> Key {
    match byte {
        b'\n' | b'\r' => Key::Enter,
        other => Key::Char(other),
    }
}

/// Reads the rest of an escape sequence after the ESC byte.
///
/// A lone ESC is a key of its own. An arrow key is ESC, then `[`, then a
/// letter, and those three bytes must become one key. A wait that is too long
/// makes the arrow feel late; a wait that is too short turns an arrow into
/// Esc. 50 ms is enough for a local terminal.
fn read_escape(stdin: &mut std::io::Stdin) -> Key {
    use std::io::Read;
    if !stdin_ready(50) {
        return Key::Esc;
    }
    let mut intro = [0u8; 1];
    if stdin.read(&mut intro).unwrap_or(0) != 1 {
        return Key::Esc;
    }
    if intro[0] != b'[' && intro[0] != b'O' {
        return Key::Esc;
    }
    if !stdin_ready(50) {
        return Key::Esc;
    }
    let mut next = [0u8; 1];
    if stdin.read(&mut next).unwrap_or(0) != 1 {
        return Key::Esc;
    }
    match (intro[0], next[0]) {
        (b'[', b'A') | (b'O', b'A') => Key::Up,
        (b'[', b'B') | (b'O', b'B') => Key::Down,
        (b'[', b'H') | (b'[', b'1') => {
            eat_tilde(stdin, next[0]);
            Key::Home
        }
        (b'[', b'F') | (b'[', b'4') => {
            eat_tilde(stdin, next[0]);
            Key::End
        }
        (b'[', b'5') => {
            eat_tilde(stdin, next[0]);
            Key::PageUp
        }
        (b'[', b'6') => {
            eat_tilde(stdin, next[0]);
            Key::PageDown
        }
        _ => Key::Esc,
    }
}

fn eat_tilde(stdin: &mut std::io::Stdin, already: u8) {
    use std::io::Read;
    if already == b'~' {
        return;
    }
    if !stdin_ready(20) {
        return;
    }
    let mut extra = [0u8; 1];
    let _ = stdin.read(&mut extra);
}

fn stdin_ready(timeout_ms: i32) -> bool {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut fd, 1, timeout_ms) > 0 }
}

/// Reads one complete key from a known sequence of bytes.
///
/// The reader thread cannot use this: it must wait for the rest of an escape
/// sequence. The tests can, because they hold the whole sequence.
pub fn decode(bytes: &[u8]) -> Option<(Key, usize)> {
    let first = *bytes.first()?;
    if first != 0x1b {
        return Some((decode_plain(first), 1));
    }
    match bytes {
        [0x1b, b'[', b'A', ..] | [0x1b, b'O', b'A', ..] => Some((Key::Up, 3)),
        [0x1b, b'[', b'B', ..] | [0x1b, b'O', b'B', ..] => Some((Key::Down, 3)),
        [0x1b, b'[', b'5', b'~', ..] => Some((Key::PageUp, 4)),
        [0x1b, b'[', b'6', b'~', ..] => Some((Key::PageDown, 4)),
        [0x1b, b'[', b'H', ..] | [0x1b, b'[', b'1', b'~', ..] => {
            Some((Key::Home, bytes_for_home(bytes)))
        }
        [0x1b, b'[', b'F', ..] | [0x1b, b'[', b'4', b'~', ..] => {
            Some((Key::End, bytes_for_end(bytes)))
        }
        _ => Some((Key::Esc, 1)),
    }
}

fn bytes_for_home(bytes: &[u8]) -> usize {
    if bytes.starts_with(&[0x1b, b'[', b'1', b'~']) {
        4
    } else {
        3
    }
}

fn bytes_for_end(bytes: &[u8]) -> usize {
    if bytes.starts_with(&[0x1b, b'[', b'4', b'~']) {
        4
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_letter_is_a_character() {
        assert_eq!(decode(b"q"), Some((Key::Char(b'q'), 1)));
        assert_eq!(decode(b"j"), Some((Key::Char(b'j'), 1)));
    }

    #[test]
    fn enter_and_return_are_the_same_key() {
        assert_eq!(decode(b"\n"), Some((Key::Enter, 1)));
        assert_eq!(decode(b"\r"), Some((Key::Enter, 1)));
    }

    #[test]
    fn an_arrow_is_one_key() {
        assert_eq!(decode(b"\x1b[A"), Some((Key::Up, 3)));
        assert_eq!(decode(b"\x1b[B"), Some((Key::Down, 3)));
        assert_eq!(decode(b"\x1bOA"), Some((Key::Up, 3)));
        assert_eq!(decode(b"\x1bOB"), Some((Key::Down, 3)));
    }

    #[test]
    fn page_and_home_keys_decode() {
        assert_eq!(decode(b"\x1b[5~"), Some((Key::PageUp, 4)));
        assert_eq!(decode(b"\x1b[6~"), Some((Key::PageDown, 4)));
        assert_eq!(decode(b"\x1b[H"), Some((Key::Home, 3)));
        assert_eq!(decode(b"\x1b[F"), Some((Key::End, 3)));
        assert_eq!(decode(b"\x1b[1~"), Some((Key::Home, 4)));
        assert_eq!(decode(b"\x1b[4~"), Some((Key::End, 4)));
    }

    #[test]
    fn a_lone_escape_is_escape() {
        assert_eq!(decode(b"\x1b"), Some((Key::Esc, 1)));
    }
}
