use crate::env::Ctx;
use crate::http_parser::{HttpFile, Request, METHODS};
use crate::json_scan;
use crate::render;
use crate::spec::{Op, Spec};
use crate::util::{range_on_line, snippet_escape};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, InsertTextFormat,
    MarkupContent, MarkupKind, TextEdit,
};
use serde_json::Value;
use std::collections::BTreeSet;

const COMMON_HEADERS: &[&str] = &[
    "Accept",
    "Accept-Charset",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "Cache-Control",
    "Connection",
    "Content-Disposition",
    "Content-Encoding",
    "Content-Language",
    "Content-Length",
    "Content-Type",
    "Cookie",
    "DNT",
    "Host",
    "If-Match",
    "If-Modified-Since",
    "If-None-Match",
    "Origin",
    "Pragma",
    "Range",
    "Referer",
    "User-Agent",
    "X-API-Key",
    "X-Correlation-ID",
    "X-Forwarded-For",
    "X-Forwarded-Host",
    "X-Forwarded-Proto",
    "X-Request-ID",
    "X-Requested-With",
];

const MIME_TYPES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/x-www-form-urlencoded",
    "application/octet-stream",
    "application/pdf",
    "multipart/form-data",
    "text/plain",
    "text/html",
    "text/csv",
    "image/png",
    "image/jpeg",
    "*/*",
];

const STATIC_VALUES: &[(&str, &[&str])] = &[
    (
        "accept-language",
        &["ru", "ru-RU", "en", "en-US", "en-GB", "uz", "uz-UZ", "uz-Cyrl", "uz-Latn", "tr", "de", "fr", "es", "zh-CN", "*"],
    ),
    ("content-language", &["ru", "ru-RU", "en", "en-US", "uz", "uz-UZ"]),
    ("accept-encoding", &["gzip", "deflate", "br", "gzip, deflate, br", "identity"]),
    ("content-encoding", &["gzip", "deflate", "br", "identity"]),
    ("accept-charset", &["utf-8", "iso-8859-1", "*"]),
    ("cache-control", &["no-cache", "no-store", "max-age=0", "max-age=3600", "must-revalidate", "public", "private"]),
    ("connection", &["keep-alive", "close"]),
    ("pragma", &["no-cache"]),
    ("x-requested-with", &["XMLHttpRequest"]),
    ("x-forwarded-proto", &["https", "http"]),
    ("dnt", &["1", "0"]),
    ("x-request-id", &["{{$uuid}}"]),
    ("x-correlation-id", &["{{$uuid}}"]),
];

struct Cx<'a> {
    lines: &'a [&'a str],
    line: usize,
    col: usize,
}

impl Cx<'_> {
    fn edit(&self, start: usize, end: usize, text: String) -> Option<CompletionTextEdit> {
        Some(CompletionTextEdit::Edit(TextEdit {
            range: range_on_line(self.lines, self.line, start, end),
            new_text: text,
        }))
    }
}

fn md(text: String) -> Option<Documentation> {
    Some(Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value: text }))
}

pub fn complete(
    file: &HttpFile,
    lines: &[&str],
    line: usize,
    col: usize,
    spec: Option<&Spec>,
    ctx: &Ctx,
) -> Vec<CompletionItem> {
    let cx = Cx { lines, line, col };
    let text = lines.get(line).copied().unwrap_or("");
    let before = &text[..col.min(text.len())];

    if let Some(open) = before.rfind("{{") {
        if before[open..].find("}}").is_none() {
            return variables(&cx, open + 2, ctx, text);
        }
    }

    let req = file.request_at(line);
    match req {
        Some(r) if r.line == line => request_line(&cx, r, before, spec),
        Some(r) if line > r.line && line < r.headers_end => header(&cx, r, before, spec),
        Some(r)
            if line == r.headers_end
                && line > r.line
                && !before.trim_start().starts_with(['{', '[', '"', '-', '<']) =>
        {
            header(&cx, r, before, spec)
        }
        Some(r) if r.body.as_ref().is_some_and(|b| line >= b.start_line) => body(&cx, r, spec),
        Some(r) if line > r.line && line >= r.headers_end => {
            let mut v = Vec::new();
            if before.trim().is_empty() || before.trim_start().starts_with(['{', '"']) {
                v = body(&cx, r, spec);
            }
            v
        }
        _ => {
            let t = before.trim_start();
            if t.starts_with('#') || t.starts_with("//") || t.starts_with('@') {
                return Vec::new();
            }
            operations(&cx, spec, true, before)
        }
    }
}

fn variables(cx: &Cx, start: usize, ctx: &Ctx, text: &str) -> Vec<CompletionItem> {
    let closed = text[cx.col.min(text.len())..].starts_with("}}");
    let mut items: Vec<CompletionItem> = ctx
        .names()
        .into_iter()
        .map(|(name, src)| CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(format!("{src}: {}", ctx.raw(&name).unwrap_or(""))),
            text_edit: cx.edit(start, cx.col, if closed { name.clone() } else { format!("{name}}}}}") }),
            ..Default::default()
        })
        .collect();
    for d in ["$uuid", "$timestamp", "$randomInt", "$env."] {
        items.push(CompletionItem {
            label: d.into(),
            kind: Some(CompletionItemKind::CONSTANT),
            detail: Some("dynamic".into()),
            text_edit: cx.edit(start, cx.col, if closed || d.ends_with('.') { d.into() } else { format!("{d}}}}}") }),
            ..Default::default()
        });
    }
    items
}

fn op_summary(spec: &Spec, op: &Op) -> String {
    spec.operation(op)
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Snippet for a full request; `{param}` path segments become placeholders.
pub fn request_snippet(spec: &Spec, op: &Op, full: bool) -> String {
    let mut n = 1;
    let mut path = String::new();
    for seg in op.path.split('/').filter(|s| !s.is_empty()) {
        path.push('/');
        match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(p) => {
                path.push_str(&format!("${{{n}:{p}}}"));
                n += 1;
            }
            None => path.push_str(&snippet_escape(seg)),
        }
    }
    let params = spec.params(op);
    let required = |p: &&&Value| p.get("required").and_then(Value::as_bool) == Some(true);
    let query: Vec<String> = params
        .iter()
        .filter(|p| p.get("in").and_then(Value::as_str) == Some("query"))
        .filter(required)
        .map(|p| {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            let s = format!("{}=${{{n}}}", snippet_escape(name));
            n += 1;
            s
        })
        .collect();
    let mut out = format!("{} {{{{baseUrl}}}}{path}", op.method);
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    if !full {
        return out;
    }
    out.push('\n');
    for p in params
        .iter()
        .filter(|p| p.get("in").and_then(Value::as_str) == Some("header"))
        .filter(required)
    {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("{}: ${{{n}}}\n", snippet_escape(name)));
        n += 1;
    }
    if let Some((media, m)) = spec.body_media(op) {
        let schema = m.get("schema").cloned().unwrap_or(Value::Null);
        if media.starts_with("multipart/") {
            out.push_str("Content-Type: multipart/form-data; boundary=boundary\n\n");
            let view = spec.object_view(&schema);
            for (name, prop) in &view.props {
                let r = spec.resolve(prop);
                let is_file = r.get("format").and_then(Value::as_str) == Some("binary")
                    || r.pointer("/items/format").and_then(Value::as_str) == Some("binary");
                out.push_str("--boundary\n");
                if is_file {
                    out.push_str(&format!(
                        "Content-Disposition: form-data; name=\"{0}\"; filename=\"file\"\n\n< ./${{{n}:file}}\n",
                        snippet_escape(name)
                    ));
                } else {
                    out.push_str(&format!(
                        "Content-Disposition: form-data; name=\"{}\"\n\n${{{n}}}\n",
                        snippet_escape(name)
                    ));
                }
                n += 1;
            }
            out.push_str("--boundary--");
        } else if media.contains("x-www-form-urlencoded") {
            out.push_str(&format!("Content-Type: {media}\n\n"));
            let view = spec.object_view(&schema);
            let fields: Vec<String> = view.props.iter().map(|(k, _)| format!("{}=", snippet_escape(k))).collect();
            out.push_str(&fields.join("&"));
        } else {
            out.push_str(&format!("Content-Type: {media}\n\n"));
            let value = Spec::media_example(m)
                .cloned()
                .unwrap_or_else(|| spec.skeleton(&schema, 0, false));
            out.push_str(&snippet_escape(&serde_json::to_string_pretty(&value).unwrap_or_default()));
        }
    } else if out.ends_with('\n') {
        out.pop();
    }
    out.push_str("$0");
    out
}

fn operations(cx: &Cx, spec: Option<&Spec>, full: bool, before: &str) -> Vec<CompletionItem> {
    let Some(spec) = spec else {
        return METHODS
            .iter()
            .map(|m| CompletionItem {
                label: m.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            })
            .collect();
    };
    let start = before.len() - before.trim_start().len();
    let end = cx.lines.get(cx.line).map_or(cx.col, |l| l.len());
    spec.ops
        .iter()
        .map(|op| {
            let summary = op_summary(spec, op);
            CompletionItem {
                label: format!("{} {}", op.method, op.path),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(summary.clone()),
                documentation: md(render::operation(spec, op)),
                filter_text: Some(format!("{} {} {summary}", op.method, op.path)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                text_edit: cx.edit(start, end, request_snippet(spec, op, full)),
                ..Default::default()
            }
        })
        .collect()
}

fn request_line(cx: &Cx, req: &Request, before: &str, spec: Option<&Spec>) -> Vec<CompletionItem> {
    let in_method = before.trim_start().find(char::is_whitespace).is_none();
    if in_method {
        let has_more = !req.headers.is_empty() || req.body.is_some();
        return operations(cx, spec, !has_more, before);
    }
    let Some(spec) = spec else { return Vec::new() };
    if cx.col < req.url_start {
        return Vec::new();
    }
    let typed = &before[req.url_start..];

    if let Some(q) = typed.find('?') {
        let Some(op) = spec.find_exact(&req.method, &req.url) else { return Vec::new() };
        let start = req.url_start + typed.rfind(['?', '&']).unwrap_or(q) + 1;
        return spec
            .params(op)
            .into_iter()
            .filter(|p| p.get("in").and_then(Value::as_str) == Some("query"))
            .map(|p| {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: p.get("schema").map(|s| spec.type_label(s)),
                    documentation: md(render::param_doc(spec, p)),
                    text_edit: cx.edit(start, cx.col, format!("{name}=")),
                    ..Default::default()
                }
            })
            .collect();
    }

    let prefix_len = if typed.starts_with("{{") {
        typed.find("}}").map_or(typed.len(), |e| e + 2)
    } else if let Some(i) = typed.find("://") {
        typed[i + 3..].find('/').map_or(typed.len(), |j| i + 3 + j)
    } else {
        0
    };
    let start = req.url_start + prefix_len;
    let end = req.url_start + req.url.find('?').unwrap_or(req.url.len()).min(cx.lines[cx.line].len() - req.url_start);
    let end = end.max(cx.col);
    let mut seen = BTreeSet::new();
    spec.ops
        .iter()
        .filter(|op| !req.method_explicit || op.method == req.method)
        .filter(|op| seen.insert(op.path.clone()))
        .map(|op| {
            let mut snippet = request_snippet(spec, op, false);
            snippet = snippet
                .split_once("}}")
                .map(|(_, p)| p.to_string())
                .unwrap_or(snippet);
            CompletionItem {
                label: op.path.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some(op_summary(spec, op)),
                documentation: md(render::operation(spec, op)),
                filter_text: Some(op.path.clone()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                text_edit: cx.edit(start, end, snippet),
                ..Default::default()
            }
        })
        .collect()
}

fn header(cx: &Cx, req: &Request, before: &str, spec: Option<&Spec>) -> Vec<CompletionItem> {
    let op = spec.and_then(|s| s.find_exact(&req.method, &req.url).map(|o| (s, o)));
    let lead = before.len() - before.trim_start().len();
    match before.find(':') {
        None => {
            let mut names: Vec<(String, Option<String>)> =
                COMMON_HEADERS.iter().map(|h| (h.to_string(), None)).collect();
            if let Some((s, op)) = op {
                for p in s.params(op) {
                    if p.get("in").and_then(Value::as_str) == Some("header") {
                        let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        names.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
                        names.insert(0, (name, Some(render::param_doc(s, p))));
                    }
                }
            }
            names
                .into_iter()
                .map(|(name, doc)| CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    documentation: doc.and_then(md),
                    text_edit: cx.edit(lead, cx.col, format!("{name}: ")),
                    ..Default::default()
                })
                .collect()
        }
        Some(colon) => {
            let name = before[..colon].trim();
            let start = colon + 1 + (before[colon + 1..].len() - before[colon + 1..].trim_start().len());
            let mut values: Vec<String> = Vec::new();
            if name.eq_ignore_ascii_case("content-type") {
                if let Some(content) = op
                    .and_then(|(s, op)| s.request_body(op))
                    .and_then(|b| b.get("content"))
                    .and_then(Value::as_object)
                {
                    values.extend(content.keys().cloned());
                }
                values.extend(MIME_TYPES.iter().map(|m| m.to_string()));
            } else if name.eq_ignore_ascii_case("authorization") {
                values.push("Bearer {{token}}".into());
            } else if name.eq_ignore_ascii_case("accept") {
                values.extend(MIME_TYPES.iter().map(|m| m.to_string()));
            } else if let Some((_, vs)) = STATIC_VALUES.iter().find(|(h, _)| name.eq_ignore_ascii_case(h)) {
                values.extend(vs.iter().map(|v| v.to_string()));
            }
            if let Some((s, op)) = op {
                for p in s.params(op) {
                    let is_it = p.get("in").and_then(Value::as_str) == Some("header")
                        && p.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name));
                    if let Some(e) = is_it
                        .then(|| p.get("schema").map(|sc| s.resolve(sc)))
                        .flatten()
                        .and_then(|sc| sc.get("enum"))
                        .and_then(Value::as_array)
                    {
                        values.extend(e.iter().filter_map(Value::as_str).map(str::to_string));
                    }
                }
            }
            let mut seen = BTreeSet::new();
            values.retain(|v| seen.insert(v.clone()));
            values
                .into_iter()
                .map(|v| CompletionItem {
                    label: v.clone(),
                    kind: Some(CompletionItemKind::VALUE),
                    text_edit: cx.edit(start.min(cx.col), cx.col, v),
                    ..Default::default()
                })
                .collect()
        }
    }
}

fn value_snippet(spec: &Spec, schema: &Value) -> String {
    let r = spec.resolve(schema);
    if spec.is_array(r) {
        return "[$1]".into();
    }
    if !spec.object_view(r).props.is_empty() {
        return "{$1}".into();
    }
    match r.get("type").and_then(Value::as_str) {
        Some("integer" | "number") => "${1:0}".into(),
        Some("boolean") => "${1:false}".into(),
        Some("object") => "{$1}".into(),
        _ => "\"$1\"".into(),
    }
}

fn body(cx: &Cx, req: &Request, spec: Option<&Spec>) -> Vec<CompletionItem> {
    let (Some(spec), Some(b)) = (spec, req.body.as_ref()) else { return Vec::new() };
    let Some(op) = spec.find_exact(&req.method, &req.url) else { return Vec::new() };
    let Some(root) = spec.body_schema_for(op, req.header("content-type")) else { return Vec::new() };

    let body_lines = &cx.lines[b.start_line..cx.line];
    let offset = body_lines.iter().map(|l| l.len() + 1).sum::<usize>() + cx.col;
    let (keys, ctx) = json_scan::scan(&b.text, Some(offset));
    let Some(ctx) = ctx else { return Vec::new() };
    let Some(container) = spec.schema_at(root, &ctx.container) else { return Vec::new() };

    let line_start = b.start_line;
    let to_col = |off: usize| {
        let (l, c) = json_scan::offset_to_line_col(&b.text, off);
        (l + line_start, c)
    };

    if ctx.expect_key {
        let (start, end) = match &ctx.on_key {
            Some((_, s, e)) => {
                let (sl, sc) = to_col(*s);
                let (el, ec) = to_col(*e);
                if sl != cx.line {
                    return Vec::new();
                }
                (sc, if el == cx.line { ec } else { cx.col })
            }
            None => {
                let line = cx.lines[cx.line];
                let word = line[..cx.col].rfind(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
                (word, cx.col)
            }
        };
        let present: BTreeSet<&str> = keys
            .iter()
            .filter(|k| k.parent == ctx.container && ctx.on_key.as_ref().is_none_or(|(_, s, _)| *s != k.start))
            .map(|k| k.key.as_str())
            .collect();
        let view = spec.object_view(container);
        return view
            .props
            .iter()
            .filter(|(name, _)| !present.contains(name.as_str()))
            .map(|(name, prop)| {
                let required = view.required.contains(name);
                CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(format!("{}{}", spec.type_label(prop), if required { " · required" } else { "" })),
                    documentation: md(render::field_doc(spec, name, prop, required)),
                    sort_text: Some(format!("{}{name}", if required { 0 } else { 1 })),
                    filter_text: Some(name.clone()),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    text_edit: cx.edit(
                        start,
                        end,
                        format!("\"{}\": {}", snippet_escape(name), value_snippet(spec, prop)),
                    ),
                    ..Default::default()
                }
            })
            .collect();
    }

    let Some(key) = ctx.value_of.as_ref() else { return Vec::new() };
    let Some(prop) = spec.child(container, &crate::spec::PathElem::Key(key.clone())) else {
        return Vec::new();
    };
    let r = spec.resolve(prop);
    let mut values: Vec<Value> = r.get("enum").and_then(Value::as_array).cloned().unwrap_or_default();
    if r.get("type").and_then(Value::as_str) == Some("boolean") {
        values = vec![Value::Bool(true), Value::Bool(false)];
    }
    let (start, end) = match ctx.in_string {
        Some((s, e)) => {
            let (_, sc) = to_col(s);
            let (el, ec) = to_col(e);
            (sc, if el == cx.line { ec } else { cx.col })
        }
        None => {
            let line = cx.lines[cx.line];
            let word = line[..cx.col].rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')).map_or(0, |i| i + 1);
            (word, cx.col)
        }
    };
    values
        .into_iter()
        .map(|v| {
            let text = v.to_string();
            CompletionItem {
                label: text.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                filter_text: Some(text.trim_matches('"').to_string()),
                text_edit: cx.edit(start, end, text),
                ..Default::default()
            }
        })
        .collect()
}

