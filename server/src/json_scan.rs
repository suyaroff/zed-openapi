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

/// Strict JSON tree with byte spans, for pointing diagnostics at exact keys and values.
#[derive(Debug)]
pub struct Node {
    pub start: usize,
    pub end: usize,
    pub kind: NodeKind,
}

#[derive(Debug)]
pub enum NodeKind {
    /// (key, key_start, key_end, value)
    Object(Vec<(String, usize, usize, Node)>),
    Array(Vec<Node>),
    Scalar(serde_json::Value),
}

pub fn parse_spanned(text: &str) -> Option<Node> {
    let mut p = SpanParser { s: text.as_bytes(), text, i: 0 };
    let node = p.value(0)?;
    p.ws();
    (p.i == p.s.len()).then_some(node)
}

struct SpanParser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl SpanParser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn string_end(&self, start: usize) -> Option<usize> {
        let mut j = start + 1;
        while j < self.s.len() {
            match self.s[j] {
                b'\\' => j += 2,
                b'"' => return Some(j + 1),
                _ => j += 1,
            }
        }
        None
    }

    fn value(&mut self, depth: usize) -> Option<Node> {
        if depth > 256 {
            return None;
        }
        self.ws();
        let start = self.i;
        let kind = match *self.s.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        if self.s.get(self.i) != Some(&b'"') {
                            return None;
                        }
                        let ks = self.i;
                        let ke = self.string_end(ks)?;
                        let key: String = serde_json::from_str(&self.text[ks..ke]).ok()?;
                        self.i = ke;
                        self.ws();
                        if self.s.get(self.i) != Some(&b':') {
                            return None;
                        }
                        self.i += 1;
                        let v = self.value(depth + 1)?;
                        fields.push((key, ks, ke, v));
                        self.ws();
                        match self.s.get(self.i)? {
                            b',' => self.i += 1,
                            b'}' => {
                                self.i += 1;
                                break;
                            }
                            _ => return None,
                        }
                    }
                }
                NodeKind::Object(fields)
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                } else {
                    loop {
                        items.push(self.value(depth + 1)?);
                        self.ws();
                        match self.s.get(self.i)? {
                            b',' => self.i += 1,
                            b']' => {
                                self.i += 1;
                                break;
                            }
                            _ => return None,
                        }
                    }
                }
                NodeKind::Array(items)
            }
            b'"' => {
                let end = self.string_end(start)?;
                self.i = end;
                NodeKind::Scalar(serde_json::from_str(&self.text[start..end]).ok()?)
            }
            _ => {
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.'))
                {
                    self.i += 1;
                }
                NodeKind::Scalar(serde_json::from_str(self.text.get(start..self.i)?).ok()?)
            }
        };
        Some(Node { start, end: self.i, kind })
    }
}
