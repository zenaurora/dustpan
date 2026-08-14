//! Raw-mode terminal input, zero dependencies (mole hand-rolls ANSI in bash;
//! we do the same in Rust). Unix delegates raw mode to `stty`; Windows flips
//! console modes via kernel32 FFI and enables VT input so arrow keys arrive
//! as CSI sequences on both platforms, letting one parser handle everything.

use std::io::{Read, Write};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    /// Multi-select toggle in pickers; never an action trigger.
    Space,
    Quit,
    Top,
    Bottom,
    PageUp,
    PageDown,
    Number(u8),
    Other,
}

/// Parse one keypress from a byte stream (blocking). Vim keys first-class:
/// j/k/h/l move, g/G jump, q quits; arrows come in as `ESC [ A..D`.
pub fn read_key(input: &mut impl Read) -> Key {
    let mut b = [0u8; 1];
    if input.read(&mut b).unwrap_or(0) == 0 {
        return Key::Quit; // EOF: treat as quit, never spin
    }
    match b[0] {
        b'q' | 0x03 | 0x04 => Key::Quit, // q, Ctrl-C, Ctrl-D
        b'j' => Key::Down,
        b'k' => Key::Up,
        b'h' | b'u' => Key::Left,
        b'l' => Key::Right,
        b'g' => Key::Top,
        b'G' => Key::Bottom,
        0x15 => Key::PageUp,         // Ctrl-U
        0x06 => Key::PageDown,       // Ctrl-F
        b'\r' | b'\n' => Key::Enter, // Space is select-toggle, NOT Enter:
        // in the apps picker Enter starts an uninstall, and pager muscle
        // memory (space = scroll) must never land there
        b' ' => Key::Space,
        b'1'..=b'9' => Key::Number(b[0] - b'0'),
        0x1b => {
            // CSI sequence: the follow-up bytes are already buffered for
            // real arrow keys, so blocking reads are fine here.
            let mut seq = [0u8; 1];
            if input.read(&mut seq).unwrap_or(0) == 0 || seq[0] != b'[' {
                return Key::Quit; // bare ESC quits, like vim closing a menu
            }
            if input.read(&mut seq).unwrap_or(0) == 0 {
                return Key::Other;
            }
            match seq[0] {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'H' => Key::Top,
                b'F' => Key::Bottom,
                b'5' => {
                    let _ = input.read(&mut seq); // consume '~'
                    Key::PageUp
                }
                b'6' => {
                    let _ = input.read(&mut seq);
                    Key::PageDown
                }
                _ => Key::Other,
            }
        }
        _ => Key::Other,
    }
}

/// Terminal rows, best effort (fallback 24).
pub fn term_rows() -> usize {
    imp::term_rows().unwrap_or(24)
}

/// Enable ANSI styling on stdout. Redirected output and unsupported consoles
/// return false, so callers automatically fall back to plain text.
pub fn enable_color() -> bool {
    imp::enable_color()
}

/// RAII raw-mode guard: restores the terminal on drop, even on panic.
pub struct RawMode {
    _inner: imp::RawGuard,
}

impl RawMode {
    /// Returns `None` when raw mode can't be established (not a tty, etc.);
    /// callers should fall back to line-based input.
    pub fn enter() -> Option<RawMode> {
        imp::enter().map(|g| RawMode { _inner: g })
    }
}

/// RAII alternate-screen guard. Keeping it beside `RawMode` ensures every
/// full-screen view uses the same enter/restore sequence.
pub struct AltScreen;

impl AltScreen {
    pub fn enter() -> AltScreen {
        print!("\x1b[?1049h\x1b[?25l");
        let _ = std::io::stdout().flush();
        AltScreen
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
    }
}

#[cfg(unix)]
mod imp {
    use std::io::IsTerminal;
    use std::process::{Command, Stdio};

    pub struct RawGuard {
        saved: String,
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = Command::new("stty")
                .arg(&self.saved)
                .stdin(Stdio::inherit())
                .status();
        }
    }

    pub fn enter() -> Option<RawGuard> {
        // `stty -g` emits a restorable settings string for the current tty.
        let out = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let saved = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let ok = Command::new("stty")
            .args(["raw", "-echo"])
            .stdin(Stdio::inherit())
            .status()
            .ok()?
            .success();
        ok.then_some(RawGuard { saved })
    }

    pub fn term_rows() -> Option<usize> {
        let out = Command::new("stty")
            .arg("size")
            .stdin(Stdio::inherit())
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    pub fn enable_color() -> bool {
        std::io::stdout().is_terminal()
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    type Handle = *mut c_void;
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ScreenBufferInfo {
        size: Coord,
        cursor_pos: Coord,
        attributes: u16,
        window: SmallRect,
        max_window: Coord,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
        fn GetConsoleScreenBufferInfo(handle: Handle, info: *mut ScreenBufferInfo) -> i32;
    }

    pub struct RawGuard {
        stdin: Handle,
        stdout: Handle,
        saved_in: u32,
        saved_out: u32,
    }

    unsafe fn enable_vt_output(stdout: Handle) -> Option<u32> {
        let mut saved = 0u32;
        if GetConsoleMode(stdout, &mut saved) == 0
            || SetConsoleMode(stdout, saved | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0
        {
            return None;
        }
        Some(saved)
    }

    pub fn enable_color() -> bool {
        unsafe { enable_vt_output(GetStdHandle(STD_OUTPUT_HANDLE)).is_some() }
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.stdin, self.saved_in);
                SetConsoleMode(self.stdout, self.saved_out);
            }
        }
    }

    pub fn enter() -> Option<RawGuard> {
        unsafe {
            let stdin = GetStdHandle(STD_INPUT_HANDLE);
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut saved_in = 0u32;
            if GetConsoleMode(stdin, &mut saved_in) == 0 {
                return None;
            }
            let raw_in = (saved_in
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            if SetConsoleMode(stdin, raw_in) == 0 {
                return None;
            }
            let Some(saved_out) = enable_vt_output(stdout) else {
                SetConsoleMode(stdin, saved_in);
                return None;
            };
            Some(RawGuard {
                stdin,
                stdout,
                saved_in,
                saved_out,
            })
        }
    }

    pub fn term_rows() -> Option<usize> {
        unsafe {
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info = ScreenBufferInfo::default();
            if GetConsoleScreenBufferInfo(stdout, &mut info) == 0 {
                return None;
            }
            Some((info.window.bottom - info.window.top + 1) as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn key(bytes: &[u8]) -> Key {
        read_key(&mut Cursor::new(bytes.to_vec()))
    }

    #[test]
    fn vim_keys() {
        assert_eq!(key(b"j"), Key::Down);
        assert_eq!(key(b"k"), Key::Up);
        assert_eq!(key(b"h"), Key::Left);
        assert_eq!(key(b"l"), Key::Right);
        assert_eq!(key(b"g"), Key::Top);
        assert_eq!(key(b"G"), Key::Bottom);
        assert_eq!(key(b"q"), Key::Quit);
        assert_eq!(key(b"\r"), Key::Enter);
        assert_eq!(key(b" "), Key::Space); // select toggle, never an action
        assert_eq!(key(b"4"), Key::Number(4));
        assert_eq!(key(b"u"), Key::Left);
    }

    #[test]
    fn arrow_keys_and_escape() {
        assert_eq!(key(b"\x1b[A"), Key::Up);
        assert_eq!(key(b"\x1b[B"), Key::Down);
        assert_eq!(key(b"\x1b[C"), Key::Right);
        assert_eq!(key(b"\x1b[D"), Key::Left);
        assert_eq!(key(b"\x1b[5~"), Key::PageUp);
        assert_eq!(key(b"\x1b[6~"), Key::PageDown);
        assert_eq!(key(b"\x1b"), Key::Quit); // bare ESC
        assert_eq!(key(b""), Key::Quit); // EOF
    }

    #[test]
    fn control_keys() {
        assert_eq!(key(b"\x03"), Key::Quit); // Ctrl-C
        assert_eq!(key(b"\x15"), Key::PageUp); // Ctrl-U
        assert_eq!(key(b"\x06"), Key::PageDown); // Ctrl-F
        assert_eq!(key(b"x"), Key::Other);
    }
}
