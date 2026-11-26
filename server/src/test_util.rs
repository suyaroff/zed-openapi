use crate::env::Ctx;
use crate::spec::Spec;
use lsp_types::{Position, TextEdit};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `util::set_utf8_positions` flips a process-wide flag: tests that change it hold this lock.
pub static POS_LOCK: Mutex<()> = Mutex::new(());

pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

pub fn spec() -> Spec {
    spec_named("petstore.yaml")
}

pub fn spec_named(name: &str) -> Spec {
    Spec::load(&fixture(name)).unwrap_or_else(|e| panic!("load {name}: {e:#}"))
}

pub fn ctx(env: &[(&str, &str)]) -> Ctx {
    Ctx {
        env_name: "dev".into(),
        env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Ctx::default()
    }
}

/// Splits `src` at the `|` cursor marker: (text, line, byte column).
pub fn at(src: &str) -> (String, usize, usize) {
    let i = src.find('|').expect("cursor marker");
    let text = format!("{}{}", &src[..i], &src[i + 1..]);
    let line = src[..i].matches('\n').count();
    let col = i - src[..i].rfind('\n').map_or(0, |p| p + 1);
    (text, line, col)
}

/// Byte offset of an LSP position; tests use ASCII text, so columns are bytes.
pub fn offset(text: &str, p: Position) -> usize {
    let start: usize = text.split('\n').take(p.line as usize).map(|l| l.len() + 1).sum();
    start + p.character as usize
}

pub fn apply(text: &str, edits: &[TextEdit]) -> String {
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| (offset(text, e.range.start), offset(text, e.range.end), e.new_text.as_str()))
        .collect();
    spans.sort_by_key(|s| std::cmp::Reverse(s.0));
    let mut out = text.to_string();
    for (s, e, t) in spans {
        out.replace_range(s..e, t);
    }
    out
}
