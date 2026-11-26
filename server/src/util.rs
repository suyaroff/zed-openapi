use lsp_types::{Position, Range, Uri};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};

static UTF8_POSITIONS: AtomicBool = AtomicBool::new(false);

pub fn set_utf8_positions(v: bool) {
    UTF8_POSITIONS.store(v, Ordering::Relaxed);
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn byte_to_lsp(line: &str, byte: usize) -> u32 {
    let byte = floor_boundary(line, byte);
    if UTF8_POSITIONS.load(Ordering::Relaxed) {
        byte as u32
    } else {
        line[..byte].encode_utf16().count() as u32
    }
}

pub fn lsp_to_byte(line: &str, ch: u32) -> usize {
    if UTF8_POSITIONS.load(Ordering::Relaxed) {
        return floor_boundary(line, ch as usize);
    }
    let mut units = 0u32;
    for (i, c) in line.char_indices() {
        if units >= ch {
            return i;
        }
        units += c.len_utf16() as u32;
    }
    line.len()
}

pub fn range_on_line(lines: &[&str], line: usize, start: usize, end: usize) -> Range {
    let text = lines.get(line).copied().unwrap_or("");
    Range::new(
        Position::new(line as u32, byte_to_lsp(text, start)),
        Position::new(line as u32, byte_to_lsp(text, end)),
    )
}

pub fn full_line(lines: &[&str], line: usize) -> Range {
    let len = lines.get(line).map_or(0, |l| l.len());
    range_on_line(lines, line, 0, len)
}

pub fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}

pub fn path_to_uri(path: &Path) -> Uri {
    let mut s = String::from("file://");
    for b in path.to_string_lossy().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                s.push(*b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    Uri::from_str(&s).expect("valid file uri")
}

pub fn snippet_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '$' | '}') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn one_line(s: &str, max: usize) -> String {
    let flat: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::POS_LOCK;

    struct Mode {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Mode {
        fn utf8(v: bool) -> Mode {
            let guard = POS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            set_utf8_positions(v);
            Mode { _guard: guard }
        }
    }

    impl Drop for Mode {
        fn drop(&mut self) {
            set_utf8_positions(false);
        }
    }

    #[test]
    fn utf16_positions_count_code_units() {
        let _m = Mode::utf8(false);
        let line = "ab привет 😀x";
        let emoji = line.find('😀').unwrap();
        assert_eq!(byte_to_lsp(line, 3), 3);
        assert_eq!(byte_to_lsp(line, emoji), 10);
        assert_eq!(byte_to_lsp(line, emoji + 4), 12);
        assert_eq!(lsp_to_byte(line, 12), emoji + 4);
        assert_eq!(lsp_to_byte(line, 999), line.len());
        for (b, _) in line.char_indices() {
            assert_eq!(lsp_to_byte(line, byte_to_lsp(line, b)), b);
        }
    }

    #[test]
    fn utf8_positions_are_bytes() {
        let _m = Mode::utf8(true);
        let line = "ab привет";
        assert_eq!(byte_to_lsp(line, 5), 5);
        assert_eq!(lsp_to_byte(line, 5), 5);
        // Mid-character offsets snap back to the char start.
        assert_eq!(lsp_to_byte(line, 4), 3);
        assert_eq!(byte_to_lsp(line, 4), 3);
    }

    #[test]
    fn mid_char_byte_floors_in_utf16() {
        let _m = Mode::utf8(false);
        assert_eq!(byte_to_lsp("жx", 1), 0);
        assert_eq!(byte_to_lsp("жx", 100), 2);
    }

    #[test]
    fn ranges_on_lines() {
        let _m = Mode::utf8(false);
        let lines = ["GET /", "ж = 1"];
        assert_eq!(range_on_line(&lines, 0, 4, 5), Range::new(Position::new(0, 4), Position::new(0, 5)));
        assert_eq!(full_line(&lines, 1), Range::new(Position::new(1, 0), Position::new(1, 5)));
        assert_eq!(full_line(&lines, 7), Range::new(Position::new(7, 0), Position::new(7, 0)));
    }

    #[test]
    fn split_lines_strips_cr() {
        assert_eq!(split_lines("a\r\nb\n\nc"), vec!["a", "b", "", "c"]);
        assert_eq!(split_lines(""), vec![""]);
        assert_eq!(split_lines("a\n"), vec!["a", ""]);
    }

    #[test]
    fn uri_path_round_trip() {
        let p = Path::new("/home/u/my api/файл#1.http");
        let uri = path_to_uri(p);
        assert!(uri.as_str().starts_with("file:///home/u/my%20api/"));
        assert!(!uri.as_str().contains('#'));
        assert_eq!(uri_to_path(&uri).unwrap(), p);
        assert_eq!(uri_to_path(&Uri::from_str("untitled:x").unwrap()), None);
    }

    #[test]
    fn snippet_escape_escapes_specials() {
        assert_eq!(snippet_escape(r"a$b}c\d{e"), r"a\$b\}c\\d{e");
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("  a\n  b\tc ", 10), "a b c");
        assert_eq!(one_line("привет мир", 6), "привет…");
        assert_eq!(one_line("abc", 3), "abc");
    }
}
