//! Tolerant JSON scanner: works on incomplete bodies with `{{vars}}` in them.

use crate::spec::PathElem;

#[derive(Debug, Clone)]
pub struct KeyTok {
    pub parent: Vec<PathElem>,
    pub key: String,
    pub start: usize,
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

/// Replaces `{{...}}` placeholders so the body can be parsed as JSON. Length-preserving
/// (`0` padded with spaces), so byte offsets stay valid for the original text.
pub fn neutralize_vars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(s) = rest.find("{{") {
        out.push_str(&rest[..s]);
        match rest[s..].find("}}") {
            Some(e) => {
                let len = rest[s..s + e + 2].len();
                out.push('0');
                out.extend(std::iter::repeat_n(' ', len - 1));
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

/// Pretty-prints a JSON body; unquoted `{{vars}}` survive as-is.
/// JSON text without whitespace outside strings.
fn strip_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        if in_string {
            in_string = escaped || c != '"';
            escaped = !escaped && c == '\\';
        } else if c.is_whitespace() {
            continue;
        } else {
            in_string = c == '"';
        }
        out.push(c);
    }
    out
}

pub fn format_body(text: &str) -> Option<String> {
    let mut masked = String::with_capacity(text.len());
    let mut vars = Vec::new();
    let mut in_string = false;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if in_string {
            masked.push(c);
            match c {
                '\\' => {
                    if let Some((_, n)) = chars.next() {
                        masked.push(n);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        if c == '{' && text[i..].starts_with("{{") {
            if let Some(e) = text[i..].find("}}") {
                masked.push_str(&format!("\"__oapi_var_{}__\"", vars.len()));
                vars.push(&text[i..i + e + 2]);
                for _ in 0..text[i..i + e + 2].chars().count() - 1 {
                    chars.next();
                }
                continue;
            }
        }
        if c == '"' {
            in_string = true;
        }
        masked.push(c);
    }
    let value: serde_json::Value = serde_json::from_str(&masked).ok()?;
    // serde_json would round high-precision numbers and drop duplicate keys: format only
    // when re-serializing reproduces the same tokens.
    if serde_json::to_string(&value).ok()? != strip_ws(&masked) {
        return None;
    }
    let mut pretty = serde_json::to_string_pretty(&value).ok()?;
    for (n, v) in vars.iter().enumerate() {
        pretty = pretty.replace(&format!("\"__oapi_var_{n}__\""), v);
    }
    Some(pretty)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(k: &str) -> PathElem {
        PathElem::Key(k.into())
    }

    #[test]
    fn scan_collects_keys_with_parents() {
        let text = r#"{"a": 1, "b": {"c": {{v}}, "d": [{"e": 2}]}}"#;
        let (keys, ctx) = scan(text, None);
        assert!(ctx.is_none());
        let got: Vec<(Vec<PathElem>, &str, usize)> = keys.iter().map(|k| (k.parent.clone(), k.key.as_str(), k.start)).collect();
        assert_eq!(
            got,
            [
                (vec![], "a", 1),
                (vec![], "b", 9),
                (vec![key("b")], "c", 15),
                (vec![key("b")], "d", 27),
                (vec![key("b"), key("d"), PathElem::Index], "e", 34),
            ]
        );
    }

    #[test]
    fn cursor_in_value_position() {
        let text = r#"{"a": 1, "b": {"c": "#;
        let (_, ctx) = scan(text, Some(text.len()));
        let ctx = ctx.unwrap();
        assert_eq!(ctx.container, [key("b")]);
        assert!(!ctx.expect_key);
        assert_eq!(ctx.value_of.as_deref(), Some("c"));
    }

    #[test]
    fn cursor_expecting_key() {
        let text = r#"{"a": 1, "#;
        let ctx = scan(text, Some(text.len())).1.unwrap();
        assert!(ctx.expect_key && ctx.container.is_empty() && ctx.value_of.is_none());

        let text = r#"[{"x": 1}, {"#;
        let ctx = scan(text, Some(text.len())).1.unwrap();
        assert!(ctx.expect_key);
        assert_eq!(ctx.container, [PathElem::Index]);
    }

    #[test]
    fn cursor_inside_strings() {
        let ctx = scan(r#"{"na"#, Some(4)).1.unwrap();
        assert_eq!(ctx.on_key, Some(("na".to_string(), 1, 4)));

        let text = r#"{"kind": "ca"}"#;
        let ctx = scan(text, Some(11)).1.unwrap();
        assert_eq!(ctx.in_string, Some((9, 13)));
        assert_eq!(ctx.value_of.as_deref(), Some("kind"));
        assert!(ctx.on_key.is_none());
    }

    #[test]
    fn neutralize_vars_preserves_length() {
        let text = r#"{"a": {{id}}, "b": "{{ x }}", "c": {{open"#;
        let out = neutralize_vars(text);
        assert_eq!(out.len(), text.len());
        assert_eq!(out, r#"{"a": 0     , "b": "0      ", "c": {{open"#);
        let ok = neutralize_vars(r#"{"a": {{id}}, "b": [{{x}}]}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&ok).is_ok());
    }

    #[test]
    fn parse_spanned_spans() {
        let text = r#"{"a": [1, {"b": true}], "c": null}"#;
        let root = parse_spanned(text).unwrap();
        assert_eq!((root.start, root.end), (0, 34));
        let NodeKind::Object(fields) = &root.kind else { panic!() };
        let (k, ks, ke, arr) = &fields[0];
        assert_eq!((k.as_str(), *ks, *ke, arr.start, arr.end), ("a", 1, 4, 6, 22));
        let NodeKind::Array(items) = &arr.kind else { panic!() };
        assert_eq!((items[0].start, items[0].end), (7, 8));
        assert_eq!((items[1].start, items[1].end), (10, 21));
        let NodeKind::Object(inner) = &items[1].kind else { panic!() };
        assert_eq!((inner[0].1, inner[0].2, inner[0].3.start, inner[0].3.end), (11, 14, 16, 20));
        assert!(matches!(&inner[0].3.kind, NodeKind::Scalar(v) if *v == json!(true)));
        let (k, ks, ke, null) = &fields[1];
        assert_eq!((k.as_str(), *ks, *ke, null.start, null.end), ("c", 24, 27, 29, 33));
    }

    #[test]
    fn parse_spanned_whitespace_and_escapes() {
        let root = parse_spanned("  [\"a\\\"b\", -1.5e3] \n").unwrap();
        assert_eq!((root.start, root.end), (2, 18));
        let NodeKind::Array(items) = &root.kind else { panic!() };
        assert!(matches!(&items[0].kind, NodeKind::Scalar(v) if *v == json!("a\"b")));
        assert!(matches!(&items[1].kind, NodeKind::Scalar(v) if *v == json!(-1500.0)));
        let NodeKind::Object(f) = &parse_spanned(r#"{"A": {}}"#).unwrap().kind else { panic!() };
        assert_eq!(f[0].0, "A");
    }

    #[test]
    fn parse_spanned_rejects_invalid() {
        for bad in ["", "{} x", r#"{"a" 1}"#, "[1,]", "{,}", r#"{"a": }"#, "tru", r#""open"#, "[1 2]"] {
            assert!(parse_spanned(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn format_body_keeps_vars() {
        let out = format_body(r#"{"a":{{id}},"b":[1,2],"c":"{{x}}"}"#).unwrap();
        assert_eq!(out, "{\n  \"a\": {{id}},\n  \"b\": [\n    1,\n    2\n  ],\n  \"c\": \"{{x}}\"\n}");
        assert_eq!(format_body(r#"{"s":"a\"{{b"}"#).unwrap(), "{\n  \"s\": \"a\\\"{{b\"\n}");
    }

    #[test]
    fn format_body_refuses_lossy_input() {
        assert_eq!(format_body(r#"{"a":1,"a":2}"#), None);
        assert_eq!(format_body(r#"{"a":1.000000000000000001}"#), None);
        assert_eq!(format_body("{"), None);
        assert_eq!(format_body("not json"), None);
    }

    #[test]
    fn offsets_to_line_col() {
        assert_eq!(offset_to_line_col("ab\ncd", 0), (0, 0));
        assert_eq!(offset_to_line_col("ab\ncd", 2), (0, 2));
        assert_eq!(offset_to_line_col("ab\ncd", 4), (1, 1));
        assert_eq!(offset_to_line_col("ab\ncd", 99), (1, 2));
    }
}
