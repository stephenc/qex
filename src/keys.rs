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

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

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
    // `write` is safe in a signal handler. Show the cursor before the
    // process stops, or a Ctrl-C leaves the terminal with no cursor.
    unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            SHOW_CURSOR.as_ptr() as *const libc::c_void,
            SHOW_CURSOR.len(),
        );
    }
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

    // Hide the cursor for the life of the page. A cursor on the last row,
    // after a newline, scrolls the header off the screen.
    unsafe {
        libc::write(
            libc::STDOUT_FILENO,
            HIDE_CURSOR.as_ptr() as *const libc::c_void,
            HIDE_CURSOR.len(),
        );
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
        while let Some(key) = read_key() {
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

/// Reads one key from the terminal file descriptor.
///
/// Do not use `std::io::Stdin` here. That handle has a buffer. An arrow key
/// is three bytes (`ESC [ A`). A buffered read of the ESC byte also takes
/// `[` and `A` out of the kernel, `poll` then sees nothing, and the arrow
/// becomes a lone Esc. The selection does not move. Read the descriptor
/// itself, so `poll` and `read` see the same bytes.
fn read_key() -> Option<Key> {
    let first = read_fd()?;
    if first != 0x1b {
        return Some(decode_plain(first));
    }
    // A lone ESC is a key of its own. An arrow is ESC, then more bytes, in
    // the same instant. 50 ms is enough for a local terminal.
    if !stdin_ready(50) {
        return Some(Key::Esc);
    }
    let mut buf = vec![0x1b];
    while buf.len() < 16 {
        let Some(byte) = read_fd() else {
            break;
        };
        buf.push(byte);
        if sequence_done(&buf) {
            break;
        }
        if !stdin_ready(20) {
            break;
        }
    }
    Some(decode(&buf).map(|(key, _)| key).unwrap_or(Key::Esc))
}

fn sequence_done(buf: &[u8]) -> bool {
    if buf.len() < 3 || buf[0] != 0x1b {
        return false;
    }
    match buf[1] {
        b'[' => (0x40..=0x7e).contains(buf.last().unwrap()),
        b'O' => buf.len() >= 3,
        _ => true,
    }
}

fn read_fd() -> Option<u8> {
    let mut byte = [0u8; 1];
    loop {
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                byte.as_mut_ptr() as *mut libc::c_void,
                1,
            )
        };
        if n == 1 {
            return Some(byte[0]);
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
        }
        return None;
    }
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
/// The tests hold the whole sequence. The reader thread builds that
/// sequence with `read_key` and then calls this function.
pub fn decode(bytes: &[u8]) -> Option<(Key, usize)> {
    let first = *bytes.first()?;
    if first != 0x1b {
        return Some((decode_plain(first), 1));
    }
    if bytes.len() >= 3 && (bytes[1] == b'[' || bytes[1] == b'O') {
        if let Some(key) = decode_csi(bytes) {
            return Some((key, bytes.len()));
        }
    }
    Some((Key::Esc, 1))
}

fn decode_csi(bytes: &[u8]) -> Option<Key> {
    match *bytes.last()? {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'~' => match csi_number(bytes) {
            5 => Some(Key::PageUp),
            6 => Some(Key::PageDown),
            1 | 7 => Some(Key::Home),
            4 | 8 => Some(Key::End),
            _ => None,
        },
        _ => None,
    }
}

fn csi_number(bytes: &[u8]) -> u32 {
    let mid = bytes.get(2..bytes.len().saturating_sub(1)).unwrap_or(&[]);
    let digits: String = mid
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .map(|b| *b as char)
        .collect();
    digits.parse().unwrap_or(0)
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
        // A modifier (Shift, Ctrl) sits in the middle. The last letter is
        // still the arrow. A decoder that only accepts ESC [ A turns that
        // sequence into Esc, and the selection does not move.
        assert_eq!(decode(b"\x1b[1;5A"), Some((Key::Up, 6)));
        assert_eq!(decode(b"\x1b[1;2B"), Some((Key::Down, 6)));
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
