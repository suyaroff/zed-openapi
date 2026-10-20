//! Tolerant JSON scanner: works on incomplete bodies with `{{vars}}` in them.

use crate::spec::PathElem;

#[derive(Debug, Clone)]
pub struct KeyTok {
    pub parent: Vec<PathElem>,
    pub key: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default)]
pub struct CursorCtx {
    /// Path to the innermost container around the cursor.
    pub container: Vec<PathElem>,
    pub expect_key: bool,
    /// Key whose value the cursor is in (object value position).
    pub value_of: Option<String>,
    /// Cursor is inside a key string: (key, start, end) byte range incl. quotes.
    pub on_key: Option<(String, usize, usize)>,
    /// Cursor is inside a value string: (start, end) incl. quotes.
    pub in_string: Option<(usize, usize)>,
}

#[derive(Debug)]
struct Frame {
    obj: bool,
    key: Option<String>,
    expect_key: bool,
}

fn path_of(frames: &[Frame]) -> Vec<PathElem> {
    frames
        .iter()
        .map(|f| {
            if f.obj {
                PathElem::Key(f.key.clone().unwrap_or_default())
            } else {
                PathElem::Index
            }
        })
        .collect()
}

fn snapshot(frames: &[Frame]) -> CursorCtx {
    let Some(top) = frames.last() else {
        return CursorCtx::default();
    };
    CursorCtx {
        container: path_of(&frames[..frames.len() - 1]),
        expect_key: top.obj && top.expect_key,
        value_of: if top.obj && !top.expect_key { top.key.clone() } else { None },
        on_key: None,
        in_string: None,
    }
}

pub fn scan(text: &str, cursor: Option<usize>) -> (Vec<KeyTok>, Option<CursorCtx>) {
    let b = text.as_bytes();
    let mut frames: Vec<Frame> = Vec::new();
    let mut keys = Vec::new();
    let mut ctx: Option<CursorCtx> = None;
    let mut i = 0;
    while i < b.len() {
        if ctx.is_none() && cursor == Some(i) {
            ctx = Some(snapshot(&frames));
        }
        match b[i] {
            b'"' => {
                let start = i;
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' && b[j] != b'\n' {
                    if b[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                let closed = j < b.len() && b[j] == b'"';
                let end = if closed { j + 1 } else { j.min(b.len()) };
                let content = text.get(start + 1..j.min(b.len())).unwrap_or("").to_string();
                let is_key = frames.last().is_some_and(|f| f.obj && f.expect_key);
                if let Some(c) = cursor {
                    if ctx.is_none() && c > start && c <= end {
                        let mut snap = snapshot(&frames);
                        if is_key {
                            snap.on_key = Some((content.clone(), start, end));
                        } else {
                            snap.in_string = Some((start, end));
                        }
                        ctx = Some(snap);
                    }
                }
                if is_key {
                    keys.push(KeyTok {
                        parent: path_of(&frames[..frames.len() - 1]),
                        key: content.clone(),
                        start,
                        end,
                    });
                    if let Some(top) = frames.last_mut() {
                        top.key = Some(content);
                    }
                }
                i = end;
                continue;
            }
            b'{' => {
                if b.get(i + 1) == Some(&b'{') {
                    // `{{var}}` placeholder: treat as an opaque value
                    if let Some(p) = text[i..].find("}}") {
                        i += p + 2;
                        continue;
                    }
                }
                frames.push(Frame { obj: true, key: None, expect_key: true });
            }
            b'[' => frames.push(Frame { obj: false, key: None, expect_key: false }),
            b'}' | b']' => {
                frames.pop();
            }
            b':' => {
                if let Some(top) = frames.last_mut() {
                    top.expect_key = false;
                }
            }
            b',' => {
                if let Some(top) = frames.last_mut() {
                    if top.obj {
                        top.expect_key = true;
                        top.key = None;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    if ctx.is_none() && cursor.is_some_and(|c| c >= b.len()) {
        ctx = Some(snapshot(&frames));
    }
    (keys, ctx)
}

pub fn offset_to_line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count();
    let col = before.rfind('\n').map_or(before.len(), |p| before.len() - p - 1);
    (line, col)
}

/// Replaces `{{...}}` placeholders so the body can be parsed as JSON.
pub fn neutralize_vars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(s) = rest.find("{{") {
        out.push_str(&rest[..s]);
        match rest[s..].find("}}") {
            Some(e) => {
                out.push('0');
                rest = &rest[s + e + 2..];
            }
            None => {
                rest = &rest[s..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}
