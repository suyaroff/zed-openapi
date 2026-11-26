//! Validates a saved response file (`# METHOD url` … `HTTP/x status` … body) against the spec.

use crate::http_parser::parse_request_line;
use crate::json_scan::{self, Node, NodeKind};
use crate::spec::{Match, Op, PathElem, Spec};
use crate::util::{full_line, range_on_line, split_lines};
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use serde_json::Value;

const MAX_DIAGNOSTICS: usize = 200;

struct Issue {
    start: usize,
    end: usize,
    severity: DiagnosticSeverity,
    message: String,
}

fn diag(range: Range, severity: DiagnosticSeverity, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        source: Some("openapi".into()),
        message,
        ..Default::default()
    }
}

pub fn diagnostics(text: &str, spec: &Spec) -> Vec<Diagnostic> {
    let mut out = analyze(text, spec);
    if out.len() > MAX_DIAGNOSTICS {
        let rest = out.len() - MAX_DIAGNOSTICS;
        out.truncate(MAX_DIAGNOSTICS);
        let at = out[0].range;
        out.push(diag(at, DiagnosticSeverity::WARNING, format!("… and {rest} more spec issues")));
    }
    out
}

pub fn warning_count(text: &str, spec: &Spec) -> usize {
    analyze(text, spec)
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::WARNING))
        .count()
}

fn analyze(text: &str, spec: &Spec) -> Vec<Diagnostic> {
    let lines = split_lines(text);
    let mut out = Vec::new();
    let Some(first) = lines.first().and_then(|l| l.strip_prefix("# ")) else { return out };
    let (method, explicit, url, _, _) = parse_request_line(first);
    if !explicit {
        return out;
    }
    let Match::Exact(op) = spec.find(&method, &url) else { return out };
    let Some(status_line) = lines.iter().position(|l| l.starts_with("HTTP/")) else { return out };
    let Some(status) = lines[status_line].split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()) else {
        return out;
    };
    let status_range = full_line(&lines, status_line);

    let mut content_type = None;
    let mut i = status_line + 1;
    while i < lines.len() && !lines[i].trim().is_empty() {
        if let Some((k, v)) = lines[i].split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-type") {
                content_type = Some((i, v.trim().to_ascii_lowercase()));
            }
        }
        i += 1;
    }
    let body_line = i + 1;

    let Some(resp) = response_for(spec, op, status) else {
        let documented = documented_statuses(spec, op);
        let msg = if documented.is_empty() {
            format!("No responses are documented for {} {}", op.method, op.path)
        } else {
            format!(
                "Status {status} is not documented for {} {}; documented: {}",
                op.method,
                op.path,
                documented.join(", ")
            )
        };
        out.push(diag(status_range, DiagnosticSeverity::WARNING, msg));
        return out;
    };

    let ct = content_type.as_ref().map(|(_, v)| v.split(';').next().unwrap_or("").trim().to_string());
    let schema = match resp.get("content").and_then(Value::as_object) {
        Some(content) if !content.is_empty() => {
            let media = match &ct {
                Some(ct) => content
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(ct))
                    .or_else(|| content.iter().find(|(k, _)| ct.contains("json") && k.contains("json")))
                    .or_else(|| content.iter().find(|(k, _)| k.as_str() == "*/*")),
                None => content.iter().find(|(k, _)| k.contains("json")),
            };
            let Some((_, media)) = media else {
                let range = content_type.as_ref().map_or(status_range, |(l, _)| full_line(&lines, *l));
                let expected: Vec<&str> = content.keys().map(String::as_str).collect();
                out.push(diag(
                    range,
                    DiagnosticSeverity::WARNING,
                    format!(
                        "Content-Type `{}` is not documented for status {status}; expected: {}",
                        ct.as_deref().unwrap_or(""),
                        expected.join(", ")
                    ),
                ));
                return out;
            };
            media.get("schema")
        }
        _ => resp.get("schema"),
    };
    let Some(schema) = schema else { return out };
    if ct.as_deref().is_some_and(|c| !c.contains("json")) {
        return out;
    }

    let body = lines.get(body_line..).unwrap_or_default().join("\n");
    let Some(node) = json_scan::parse_spanned(&body) else { return out };
    let mut issues = Vec::new();
    let anchor = (node.start, node.start + 1);
    check(spec, schema, &node, "", anchor, &mut issues, 0);

    for is in issues {
        let (l, c) = json_scan::offset_to_line_col(&body, is.start);
        let (le, ce) = json_scan::offset_to_line_col(&body, is.end);
        let line = body_line + l;
        let end = if le == l { ce } else { lines.get(line).map_or(c, |t| t.len()) };
        out.push(diag(range_on_line(&lines, line, c, end), is.severity, is.message));
    }
    out
}

fn response_for<'a>(spec: &'a Spec, op: &Op, status: u16) -> Option<&'a Value> {
    let responses = spec.operation(op).get("responses")?.as_object()?;
    let code = status.to_string();
    let range = format!("{}XX", status / 100);
    responses
        .get(&code)
        .or_else(|| responses.iter().find(|(k, _)| k.eq_ignore_ascii_case(&range)).map(|(_, v)| v))
        .or_else(|| responses.get("default"))
        .map(|r| spec.resolve(r))
}

fn documented_statuses(spec: &Spec, op: &Op) -> Vec<String> {
    spec.operation(op)
        .get("responses")
        .and_then(Value::as_object)
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default()
}

fn prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("`{path}`: ")
    }
}

fn child_path(path: &str, seg: &str) -> String {
    if path.is_empty() {
        seg.to_string()
    } else {
        format!("{path}.{seg}")
    }
}

/// Declared types (OpenAPI 3.1 type arrays included) and whether `null` is allowed.
fn types_of(spec: &Spec, s: &Value) -> (Vec<String>, bool) {
    let mut nullable = s.get("nullable").and_then(Value::as_bool) == Some(true);
    let mut types: Vec<String> = match s.get("type") {
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => s
            .get("allOf")
            .and_then(Value::as_array)
            .and_then(|all| {
                all.iter()
                    .find_map(|sub| spec.resolve(sub).get("type").and_then(Value::as_str))
            })
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
    };
    if types.iter().any(|t| t == "null") {
        nullable = true;
        if types.len() > 1 {
            types.retain(|t| t != "null");
        }
    }
    (types, nullable)
}

fn matches_type(t: &str, node: &Node) -> bool {
    match (t, &node.kind) {
        ("object", NodeKind::Object(_)) | ("array", NodeKind::Array(_)) => true,
        ("string", NodeKind::Scalar(Value::String(_))) => true,
        ("boolean", NodeKind::Scalar(Value::Bool(_))) => true,
        ("number", NodeKind::Scalar(Value::Number(_))) => true,
        ("integer", NodeKind::Scalar(Value::Number(n))) => {
            n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        ("null", NodeKind::Scalar(Value::Null)) => true,
        ("object" | "array" | "string" | "boolean" | "number" | "integer" | "null", _) => false,
        _ => true,
    }
}

fn json_type(node: &Node) -> &'static str {
    match &node.kind {
        NodeKind::Object(_) => "object",
        NodeKind::Array(_) => "array",
        NodeKind::Scalar(Value::String(_)) => "string",
        NodeKind::Scalar(Value::Bool(_)) => "boolean",
        NodeKind::Scalar(Value::Number(_)) => "number",
        NodeKind::Scalar(Value::Null) => "null",
        NodeKind::Scalar(_) => "value",
    }
}

/// `anchor` is where object-level issues point: the key of this value, or its opening bracket.
fn check(
    spec: &Spec,
    schema: &Value,
    node: &Node,
    path: &str,
    anchor: (usize, usize),
    out: &mut Vec<Issue>,
    depth: usize,
) {
    if depth > 64 {
        return;
    }
    let s = spec.resolve(schema);
    let here = if matches!(node.kind, NodeKind::Scalar(_)) { (node.start, node.end) } else { anchor };
    let push = |out: &mut Vec<Issue>, at: (usize, usize), severity, message: String| {
        out.push(Issue { start: at.0, end: at.1, severity, message });
    };

    let variants = spec.variants(s);
    if !variants.is_empty() {
        let mut best: Option<Vec<Issue>> = None;
        for v in variants {
            let mut tmp = Vec::new();
            check(spec, v, node, path, anchor, &mut tmp, depth + 1);
            if tmp.is_empty() {
                return;
            }
            if best.as_ref().is_none_or(|b| tmp.len() < b.len()) {
                best = Some(tmp);
            }
        }
        out.extend(best.unwrap_or_default());
        return;
    }

    let (types, nullable) = types_of(spec, s);
    if matches!(node.kind, NodeKind::Scalar(Value::Null)) {
        if !nullable && !types.is_empty() {
            push(
                out,
                here,
                DiagnosticSeverity::WARNING,
                format!("{}null, but `{}` is not nullable", prefix(path), spec.type_label(schema)),
            );
        }
        return;
    }
    if !types.is_empty() && !types.iter().any(|t| matches_type(t, node)) {
        push(
            out,
            here,
            DiagnosticSeverity::WARNING,
            format!("{}expected `{}`, got {}", prefix(path), spec.type_label(schema), json_type(node)),
        );
        return;
    }
    if let (Some(allowed), NodeKind::Scalar(v)) = (s.get("enum").and_then(Value::as_array), &node.kind) {
        if !allowed.contains(v) {
            let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
            push(
                out,
                here,
                DiagnosticSeverity::WARNING,
                format!("{}{v} is not one of {}", prefix(path), list.join(", ")),
            );
        }
    }

    match &node.kind {
        NodeKind::Object(fields) => {
            let view = spec.object_view(s);
            for r in &view.required {
                let write_only = view
                    .prop(r)
                    .is_some_and(|p| spec.resolve(p).get("writeOnly").and_then(Value::as_bool) == Some(true));
                if !write_only && !fields.iter().any(|(k, ..)| k == r) {
                    push(
                        out,
                        anchor,
                        DiagnosticSeverity::WARNING,
                        format!("{}missing required field `{r}`", prefix(path)),
                    );
                }
            }
            if view.props.is_empty() && view.additional.is_none() {
                return;
            }
            let schema_name = Spec::ref_name(schema).unwrap_or("schema");
            for (k, ks, ke, v) in fields {
                let p = child_path(path, k);
                match view.prop(k).or(view.additional) {
                    Some(child) => check(spec, child, v, &p, (*ks, *ke), out, depth + 1),
                    None => push(
                        out,
                        (*ks, *ke),
                        DiagnosticSeverity::INFORMATION,
                        format!("`{p}` is not documented in `{schema_name}`"),
                    ),
                }
            }
        }
        NodeKind::Array(items) => {
            if let Some(item_schema) = spec.child(s, &PathElem::Index) {
                for (i, item) in items.iter().enumerate() {
                    let at = (item.start, item.start + 1);
                    check(spec, item_schema, item, &child_path(path, &i.to_string()), at, out, depth + 1);
                }
            }
        }
        NodeKind::Scalar(_) => {}
    }
}
