//! Lightweight text-only mirror of a PTY's output, kept alongside the vt100
//! parser so search can scan history without thrashing the parser's scrollback
//! position.
//!
//! Tradeoffs vs. vt100's own scrollback:
//! - We only need plain text — ANSI styling and cursor moves are dropped.
//! - `\r` resets the current line (mimics overwrite) and `\n` commits it.
//! - UTF-8 multibyte sequences split across chunks degrade to U+FFFD; this is
//!   acceptable because search is best-effort and the visible PTY render is
//!   the source of truth.

use std::collections::VecDeque;

pub const MAX_LINES: usize = 10_000;

#[derive(Default)]
pub struct ScrollbackText {
    /// Completed lines, oldest first.
    lines: VecDeque<String>,
    /// In-progress line — no trailing newline yet.
    current: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    /// Index into `lines` for completed lines, or `lines.len()` for `current`.
    pub line_idx: usize,
    pub col: usize,
    pub len: usize,
}

impl ScrollbackText {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed PTY bytes. ANSI/control sequences are stripped before splitting
    /// into lines on `\n`; `\r` clears the in-progress line.
    pub fn feed(&mut self, bytes: &[u8]) {
        let stripped = strip_ansi(bytes);
        let s = String::from_utf8_lossy(&stripped);
        for ch in s.chars() {
            match ch {
                '\n' => {
                    let line = std::mem::take(&mut self.current);
                    self.lines.push_back(line);
                    while self.lines.len() > MAX_LINES {
                        self.lines.pop_front();
                    }
                }
                '\r' => self.current.clear(),
                c if c.is_control() => {}
                c => self.current.push(c),
            }
        }
    }

    /// Case-insensitive search for `query`. Returns matches in scan order.
    pub fn find_all(&self, query: &str) -> Vec<Match> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            collect_line_matches(&q, line, i, &mut out);
        }
        collect_line_matches(&q, &self.current, self.lines.len(), &mut out);
        out
    }

    pub fn line(&self, idx: usize) -> Option<&str> {
        match idx.cmp(&self.lines.len()) {
            std::cmp::Ordering::Less => Some(&self.lines[idx]),
            std::cmp::Ordering::Equal => Some(&self.current),
            std::cmp::Ordering::Greater => None,
        }
    }

    /// Total addressable line count (completed + in-progress).
    pub fn total_lines(&self) -> usize {
        self.lines.len() + 1
    }

    /// Distance of `line_idx` from the bottom of the buffer. Used to map a
    /// match line into a vt100 scrollback offset.
    pub fn lines_above_bottom(&self, line_idx: usize) -> usize {
        self.total_lines().saturating_sub(1).saturating_sub(line_idx)
    }
}

fn collect_line_matches(query_lc: &str, line: &str, line_idx: usize, out: &mut Vec<Match>) {
    let lc = line.to_lowercase();
    let mut start = 0;
    while let Some(pos) = lc[start..].find(query_lc) {
        let abs = start + pos;
        out.push(Match {
            line_idx,
            col: abs,
            len: query_lc.len(),
        });
        // Advance by 1 char to allow overlapping matches; standard search
        // semantics would advance by len, but for substring highlight users
        // expect "next" to step forward at least one char.
        start = abs + query_lc.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        if start > line.len() {
            break;
        }
    }
}

/// Strip ANSI escape sequences (CSI and OSC, plus simple two-char escapes).
/// Partial sequences split across `feed()` calls are not stitched; the small
/// resulting noise is acceptable for search purposes.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // ESC: peek the next byte to decide which sequence kind.
        match bytes.get(i + 1) {
            Some(b'[') => {
                // CSI: ESC [ <params> <final 0x40..=0x7e>
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                i = j.saturating_add(1);
            }
            Some(b']') => {
                // OSC: ESC ] <text> (BEL | ESC \)
                let mut j = i + 2;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if bytes[j] == 0x1b
                        && j + 1 < bytes.len()
                        && bytes[j + 1] == b'\\'
                    {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            Some(_) => {
                // Two-char escape (e.g. ESC =, ESC >).
                i += 2;
            }
            None => i += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_splits_on_newline() {
        let mut s = ScrollbackText::new();
        s.feed(b"hello\nworld\n");
        assert_eq!(s.lines.len(), 2);
        assert_eq!(s.lines[0], "hello");
        assert_eq!(s.lines[1], "world");
        assert_eq!(s.current, "");
    }

    #[test]
    fn cr_clears_current_line() {
        let mut s = ScrollbackText::new();
        s.feed(b"abc\rdef\n");
        assert_eq!(s.lines[0], "def");
    }

    #[test]
    fn ansi_csi_stripped() {
        let mut s = ScrollbackText::new();
        s.feed(b"\x1b[31mred\x1b[0m text\n");
        assert_eq!(s.lines[0], "red text");
    }

    #[test]
    fn ansi_osc_stripped() {
        let mut s = ScrollbackText::new();
        s.feed(b"\x1b]0;title\x07after\n");
        assert_eq!(s.lines[0], "after");
    }

    #[test]
    fn find_all_case_insensitive() {
        let mut s = ScrollbackText::new();
        s.feed(b"Hello World\nhello again\n");
        let m = s.find_all("hello");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].line_idx, 0);
        assert_eq!(m[0].col, 0);
        assert_eq!(m[1].line_idx, 1);
    }

    #[test]
    fn find_all_includes_current_line() {
        let mut s = ScrollbackText::new();
        s.feed(b"first\nstreaming part");
        let m = s.find_all("stream");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line_idx, 1);
    }

    #[test]
    fn multiple_matches_per_line() {
        let mut s = ScrollbackText::new();
        s.feed(b"ab ab ab\n");
        let m = s.find_all("ab");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn lines_above_bottom_basic() {
        let mut s = ScrollbackText::new();
        s.feed(b"a\nb\nc\nd\n");
        // total_lines = 4 completed + 1 current = 5; bottom-indexed at 4.
        // Line idx 0 ("a") is 4 above the bottom.
        assert_eq!(s.lines_above_bottom(0), 4);
        assert_eq!(s.lines_above_bottom(3), 1);
        assert_eq!(s.lines_above_bottom(4), 0);
    }

    #[test]
    fn buffer_caps_at_max_lines() {
        let mut s = ScrollbackText::new();
        for i in 0..(MAX_LINES + 50) {
            s.feed(format!("line {}\n", i).as_bytes());
        }
        assert_eq!(s.lines.len(), MAX_LINES);
        // Oldest 50 lines should have been dropped.
        assert_eq!(s.lines[0], format!("line {}", 50));
    }
}
