use crate::env::{var_names_in, Ctx};
use crate::http_parser::{parse_var, HttpFile, Request};
use crate::json_scan::{self, Node, NodeKind};
use crate::spec::{Match, PathElem, Spec};
use crate::util::{byte_to_lsp, full_line, range_on_line, split_lines};
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, TextEdit};
use serde_json::Value;

/// A quick fix for a diagnostic, already resolved to text edits.
pub struct Fix {
    pub title: String,
    pub edits: Vec<TextEdit>,
    pub preferred: bool,
}

pub struct Finding {
    pub diag: Diagnostic,
    pub fixes: Vec<Fix>,
}

fn finding(range: Range, message: String) -> Finding {
    Finding {
        diag: Diagnostic {
            range,
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("openapi".into()),
            message,
            ..Default::default()
        },
        fixes: Vec::new(),
    }
}

fn fix(title: String, edits: Vec<TextEdit>, preferred: bool) -> Fix {
    Fix { title, edits, preferred }
}

fn pos(lines: &[&str], line: usize, byte: usize) -> Position {
    Position::new(line as u32, byte_to_lsp(lines.get(line).copied().unwrap_or(""), byte))
}

fn insert(at: Position, text: String) -> TextEdit {
    TextEdit { range: Range::new(at, at), new_text: text }
}

fn line_end(lines: &[&str], line: usize) -> Position {
    pos(lines, line, lines.get(line).map_or(0, |l| l.len()))
}

pub fn compute(text: &str, file: &HttpFile, spec: Option<&Spec>, ctx: &Ctx) -> Vec<Finding> {
    let lines = split_lines(text);
    let mut out = Vec::new();
    let captured_here: Vec<&str> = file
        .requests
        .iter()
        .flat_map(|r| r.captures.iter().map(|(k, _)| k.as_str()))
        .collect();
    let var_anchor = define_var_position(&lines, file);

    for req in &file.requests {
        for (line, e) in &req.opts.expects {
            if let Err(msg) = crate::expect::parse(e) {
                out.push(finding(full_line(&lines, *line), format!("@expect: {msg}")));
            }
        }
        if let Some(spec) = spec {
            check_request(&lines, req, spec, &mut out);
        }
        let last = req.end_line.min(lines.len().saturating_sub(1));
        for (i, line) in lines.iter().enumerate().take(last + 1).skip(req.line) {
            for (name, s, e) in var_names_in(line) {
                if !ctx.is_defined(&name) && !captured_here.contains(&name.as_str()) {
                    let mut f = finding(
                        range_on_line(&lines, i, s, e),
                        format!("Variable `{name}` is not defined in env `{}`", ctx.env_name),
                    );
                    if !name.starts_with('$') {
                        let edit = insert(var_anchor, format!("@{name} = \n"));
                        f.fixes.push(fix(format!("Define `@{name}` in this file"), vec![edit], true));
                    }
                    out.push(f);
                }
            }
        }
    }
    out
}

/// After the last `@var` line above the first request, else the top of the file.
fn define_var_position(lines: &[&str], file: &HttpFile) -> Position {
    let first = file.requests.first().map_or(lines.len(), |r| r.line);
    lines[..first.min(lines.len())]
        .iter()
        .rposition(|l| parse_var(l.trim()).is_some())
        .map_or(Position::new(0, 0), |i| Position::new(i as u32 + 1, 0))
}

/// Last line of the request line plus its `  ?a=1` / `  &b=2` continuation lines.
fn last_url_line(lines: &[&str], req: &Request) -> usize {
    let mut i = req.line;
    while lines.get(i + 1).is_some_and(|l| {
        let t = l.trim_start();
        l.len() > t.len() && (t.starts_with('?') || t.starts_with('&'))
    }) {
        i += 1;
    }
    i
}

fn check_request(lines: &[&str], req: &Request, spec: &Spec, out: &mut Vec<Finding>) {
    let url_range = range_on_line(lines, req.line, req.url_start, req.url_end);
    let op = match spec.find(&req.method, &req.url) {
        Match::Exact(op) => op,
        Match::WrongMethod(ops) => {
            let allowed: Vec<&str> = ops.iter().map(|o| o.method.as_str()).collect();
            let mut f = finding(
                url_range,
                format!("{} is not defined for {}; allowed: {}", req.method, ops[0].path, allowed.join(", ")),
            );
            let lead = lines[req.line].len() - lines[req.line].trim_start().len();
            for m in &allowed {
                let edit = if req.method_explicit {
                    let range = range_on_line(lines, req.line, lead, lead + req.method.len());
                    TextEdit { range, new_text: m.to_string() }
                } else {
                    insert(pos(lines, req.line, req.url_start), format!("{m} "))
                };
                f.fixes.push(fix(format!("Change method to {m}"), vec![edit], allowed.len() == 1));
            }
            out.push(f);
            return;
        }
        Match::None => {
            let mut f = finding(url_range, "No matching path in the OpenAPI spec".into());
            let url_on_line = &lines[req.line][req.url_start..req.url_end];
            for (new_url, op_label) in spec.closest_paths(&req.method, url_on_line) {
                let edit = TextEdit { range: url_range, new_text: new_url };
                f.fixes.push(fix(format!("Change path to {op_label}"), vec![edit], false));
            }
            out.push(f);
            return;
        }
    };

    let query_keys: Vec<&str> = req
        .url
        .split_once('?')
        .map(|(_, q)| q.split('&').map(|kv| kv.split('=').next().unwrap_or("")).collect())
        .unwrap_or_default();
    let url_last = last_url_line(lines, req);
    let headers_last = req.headers.last().map_or(url_last, |h| h.line);
    for p in spec.params(op) {
        if p.get("required").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
        let edit = match p.get("in").and_then(Value::as_str) {
            Some("query") if !query_keys.contains(&name) => Some(if url_last == req.line {
                let sep = if req.url.contains('?') { '&' } else { '?' };
                insert(pos(lines, req.line, req.url_end), format!("{sep}{name}="))
            } else {
                let indent = &lines[url_last][..lines[url_last].len() - lines[url_last].trim_start().len()];
                insert(line_end(lines, url_last), format!("\n{indent}&{name}="))
            }),
            Some("header") if req.header(name).is_none() && !name.eq_ignore_ascii_case("authorization") => {
                Some(insert(line_end(lines, headers_last), format!("\n{name}: ")))
            }
            _ => None,
        };
        if let Some(edit) = edit {
            let kind = p["in"].as_str().unwrap_or("");
            let mut f = finding(url_range, format!("Missing required {kind} parameter `{name}`"));
            f.fixes.push(fix(format!("Add {kind} parameter `{name}`"), vec![edit], true));
            out.push(f);
        }
    }

    let body_required = spec
        .request_body(op)
        .and_then(|b| b.get("required"))
        .and_then(Value::as_bool)
        == Some(true);
    let Some(body) = &req.body else {
        if body_required {
            let mut f = finding(url_range, "Request body is required".into());
            if let Some((media, m)) = spec.body_media(op).filter(|(media, _)| media.contains("json")) {
                let value = Spec::media_example(m)
                    .cloned()
                    .unwrap_or_else(|| spec.skeleton(m.get("schema").unwrap_or(&Value::Null), 0, false));
                let pretty = serde_json::to_string_pretty(&value).unwrap_or_default();
                let ct = if req.header("content-type").is_some() { String::new() } else { format!("\nContent-Type: {media}") };
                let edit = insert(line_end(lines, headers_last), format!("{ct}\n\n{pretty}"));
                f.fixes.push(fix("Add body from schema".into(), vec![edit], true));
            }
            out.push(f);
        }
        return;
    };
    let is_json = req.header("content-type").map_or_else(
        || body.text.trim_start().starts_with(['{', '[']),
        |ct| ct.contains("json"),
    );
    if !is_json {
        return;
    }
    let Some(schema) = spec.body_schema_for(op, req.header("content-type")) else { return };

    let neutral = json_scan::neutralize_vars(&body.text);
    if let Err(e) = serde_json::from_str::<Value>(&neutral) {
        let line = body.start_line + e.line().saturating_sub(1);
        out.push(finding(full_line(lines, line), format!("Invalid JSON: {e}")));
        return;
    }
    let Some(node) = json_scan::parse_spanned(&neutral) else { return };
    let cx = BodyCx { text: &body.text, lines, start_line: body.start_line };
    let root_anchor = (node.start, node.start + 1);
    validate(spec, schema, &node, &mut Vec::new(), root_anchor, &cx, out);
}

/// Maps byte offsets in a request body to document positions.
struct BodyCx<'a> {
    text: &'a str,
    lines: &'a [&'a str],
    start_line: usize,
}

impl BodyCx<'_> {
    fn pos(&self, offset: usize) -> Position {
        let (l, c) = json_scan::offset_to_line_col(self.text, offset);
        pos(self.lines, self.start_line + l, c)
    }

    fn range(&self, start: usize, end: usize) -> Range {
        let (ls, _) = json_scan::offset_to_line_col(self.text, start);
        let (le, _) = json_scan::offset_to_line_col(self.text, end);
        let start_pos = self.pos(start);
        let end_pos = if le == ls { self.pos(end) } else { line_end(self.lines, self.start_line + ls) };
        Range::new(start_pos, end_pos)
    }

    fn edit(&self, start: usize, end: usize, text: String) -> TextEdit {
        TextEdit { range: Range::new(self.pos(start), self.pos(end)), new_text: text }
    }

    /// Leading whitespace of the body line containing `offset`.
    fn indent_at(&self, offset: usize) -> &str {
        let line_start = self.text[..offset].rfind('\n').map_or(0, |i| i + 1);
        let line = &self.text[line_start..];
        &line[..line.len() - line.trim_start().len()]
    }

    fn same_line(&self, a: usize, b: usize) -> bool {
        !self.text[a.min(b)..a.max(b)].contains('\n')
    }
}

/// Text that inserts `"key": value` pairs into `obj`, keeping its layout.
fn insert_fields(cx: &BodyCx, obj: &Node, fields: &[(String, usize, usize, Node)], pairs: &[(String, String)]) -> TextEdit {
    let kv = |(k, v): &(String, String)| format!("{}: {v}", serde_json::to_string(k).unwrap_or_default());
    match fields.last() {
        None => {
            let joined: Vec<String> = pairs.iter().map(kv).collect();
            cx.edit(obj.start + 1, obj.start + 1, joined.join(", "))
        }
        Some((_, ks, _, last)) => {
            let sep = if cx.same_line(obj.start, *ks) { ", ".to_string() } else { format!(",\n{}", cx.indent_at(*ks)) };
            let text: String = pairs.iter().map(|p| format!("{sep}{}", kv(p))).collect();
            cx.edit(last.end, last.end, text)
        }
    }
}

fn remove_field(cx: &BodyCx, obj: &Node, fields: &[(String, usize, usize, Node)], i: usize) -> TextEdit {
    if fields.len() == 1 {
        cx.edit(obj.start + 1, obj.end - 1, String::new())
    } else if i + 1 < fields.len() {
        cx.edit(fields[i].1, fields[i + 1].1, String::new())
    } else {
        cx.edit(fields[i - 1].3.end, fields[i].3.end, String::new())
    }
}

/// Checks required / unknown fields (no type checks). `anchor` is the key of this value,
/// or its opening bracket.
fn validate(
    spec: &Spec,
    schema: &Value,
    node: &Node,
    path: &mut Vec<PathElem>,
    anchor: (usize, usize),
    cx: &BodyCx,
    out: &mut Vec<Finding>,
) {
    if path.len() > 12 || !spec.variants(schema).is_empty() {
        return;
    }
    match &node.kind {
        NodeKind::Object(fields) => {
            let view = spec.object_view(schema);
            if view.props.is_empty() {
                return;
            }
            let missing: Vec<(String, String)> = view
                .required
                .iter()
                .filter(|r| !fields.iter().any(|(k, ..)| k == *r))
                .map(|r| {
                    let prop = view.prop(r).unwrap_or(&Value::Null);
                    (r.clone(), serde_json::to_string(&spec.skeleton(prop, 1, false)).unwrap_or_default())
                })
                .collect();
            for pair in &missing {
                let mut f = finding(cx.range(anchor.0, anchor.1), format!("Missing required field `{}`", pair.0));
                let edit = insert_fields(cx, node, fields, std::slice::from_ref(pair));
                f.fixes.push(fix(format!("Add field `{}`", pair.0), vec![edit], true));
                if missing.len() > 1 {
                    let all = insert_fields(cx, node, fields, &missing);
                    f.fixes.push(fix(format!("Add all {} missing required fields", missing.len()), vec![all], false));
                }
                out.push(f);
            }
            let name = Spec::ref_name(schema).unwrap_or("schema");
            for (i, (k, ks, ke, v)) in fields.iter().enumerate() {
                match view.prop(k) {
                    Some(child) => {
                        path.push(PathElem::Key(k.clone()));
                        validate(spec, child, v, path, (*ks, *ke), cx, out);
                        path.pop();
                    }
                    None if view.additional.is_none() => {
                        let mut f = finding(cx.range(*ks, *ke), format!("Unknown field `{k}` for `{name}`"));
                        f.fixes.push(fix(format!("Remove field `{k}`"), vec![remove_field(cx, node, fields, i)], true));
                        out.push(f);
                    }
                    None => {}
                }
            }
        }
        NodeKind::Array(items) => {
            if let Some(item_schema) = spec.child(schema, &PathElem::Index) {
                path.push(PathElem::Index);
                for it in items {
                    validate(spec, item_schema, it, path, (it.start, it.start + 1), cx, out);
                }
                path.pop();
            }
        }
        NodeKind::Scalar(_) => {}
    }
}
