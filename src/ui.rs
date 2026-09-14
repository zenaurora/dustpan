//! Terminal output helpers: colors, size formatting, confirm prompt.

use std::io::{self, BufRead, IsTerminal, Write};

pub struct Style {
    on: bool,
}

impl Style {
    pub fn auto() -> Style {
        Style {
            on: std::env::var_os("DPAN_NO_COLOR").is_none()
                && crate::settings::Settings::load().color
                && crate::term::enable_color(),
        }
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    /// Reverse video for cursor rows; plain text when colors are off.
    pub fn invert(&self, s: &str) -> String {
        self.wrap("7", s)
    }
}

pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Terminal display width of one char: CJK/fullwidth glyphs take 2 cells.
/// Approximation of Unicode East Asian Width, enough for app names.
fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = matches!(cp,
        0x1100..=0x115F        // Hangul Jamo
        | 0x2E80..=0xA4CF      // CJK radicals, kana, unified ideographs
        | 0xAC00..=0xD7A3      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0xFE30..=0xFE4F      // CJK compatibility forms
        | 0xFF00..=0xFF60      // fullwidth forms
        | 0xFFE0..=0xFFE6      // fullwidth signs
        | 0x20000..=0x3FFFD    // CJK extensions
    );
    if wide {
        2
    } else {
        1
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Truncate to at most `max` display cells, appending `…` when cut.
pub fn truncate_width(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Left-align to `width` display cells (space padded); wider input passes
/// through untouched, so pair with `truncate_width`.
pub fn pad_to_width(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

/// Thousands separators for large counts (e.g. "1,234,567").
pub fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `[y/N]` prompt; anything but y/yes is a no.
pub fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Keep result output visible before an argument-free workflow returns to its
/// full-screen menu. Non-interactive callers never block.
pub fn pause(prompt: &str) {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return;
    }
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_formatting() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(2048), "2.0 KB");
        assert_eq!(fmt_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn count_formatting() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(1234567), "1,234,567");
    }

    #[test]
    fn cjk_display_width() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("上传到百度网盘"), 14);
        assert_eq!(display_width("7-Zip 中文版"), 12);
    }

    #[test]
    fn width_aware_truncate_and_pad() {
        assert_eq!(truncate_width("short", 10), "short");
        // 6 cells of CJK + ellipsis fits in 8
        assert_eq!(truncate_width("上传到百度网盘", 8), "上传到…");
        assert_eq!(display_width(&truncate_width("上传到百度网盘", 8)), 7);
        assert_eq!(pad_to_width("中文", 6), "中文  ");
        assert_eq!(display_width(&pad_to_width("中文", 6)), 6);
        assert_eq!(pad_to_width("toolong", 3), "toolong");
    }
}
