use crate::env::{var_names_in, Ctx};
use crate::http_parser::{HttpFile, Request};
use crate::json_scan::{self, KeyTok};
use crate::spec::{Match, PathElem, Spec};
use crate::util::{range_on_line, split_lines};
use lsp_types::{Diagnostic, DiagnosticSeverity, Range};
use serde_json::Value;

fn diag(range: Range, severity: DiagnosticSeverity, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        source: Some("openapi".into()),
        message,
        ..Default::default()
    }
}

pub fn compute(text: &str, file: &HttpFile, spec: Option<&Spec>, ctx: &Ctx) -> Vec<Diagnostic> {
    let lines = split_lines(text);
    let mut out = Vec::new();
    let captured_here: Vec<&str> = file
        .requests
        .iter()
        .flat_map(|r| r.captures.iter().map(|(k, _)| k.as_str()))
        .collect();

    for req in &file.requests {
        if let Some(spec) = spec {
            check_request(&lines, req, spec, &mut out);
        }
        let last = req.end_line.min(lines.len().saturating_sub(1));
        for (i, line) in lines.iter().enumerate().take(last + 1).skip(req.line) {
            for (name, s, e) in var_names_in(line) {
                if !ctx.is_defined(&name) && !captured_here.contains(&name.as_str()) {
                    out.push(diag(
                        range_on_line(&lines, i, s, e),
                        DiagnosticSeverity::WARNING,
                        format!("Variable `{name}` is not defined in env `{}`", ctx.env_name),
                    ));
                }
            }
        }
    }
    out
}

fn check_request(lines: &[&str], req: &Request, spec: &Spec, out: &mut Vec<Diagnostic>) {
    let url_range = range_on_line(lines, req.line, req.url_start, req.url_end);
    let op = match spec.find(&req.method, &req.url) {
        Match::Exact(op) => op,
        Match::WrongMethod(ops) => {
            let allowed: Vec<&str> = ops.iter().map(|o| o.method.as_str()).collect();
            out.push(diag(
                url_range,
                DiagnosticSeverity::WARNING,
                format!("{} is not defined for {}; allowed: {}", req.method, ops[0].path, allowed.join(", ")),
            ));
            return;
        }
        Match::None => {
            out.push(diag(url_range, DiagnosticSeverity::WARNING, "No matching path in the OpenAPI spec".into()));
            return;
        }
    };

    let query_keys: Vec<&str> = req
        .url
        .split_once('?')
        .map(|(_, q)| q.split('&').map(|kv| kv.split('=').next().unwrap_or("")).collect())
        .unwrap_or_default();
    for p in spec.params(op) {
        if p.get("required").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let name = p.get("name").and_then(Value::as_str).unwrap_or("");
        let missing = match p.get("in").and_then(Value::as_str) {
            Some("query") => !query_keys.contains(&name),
            Some("header") => req.header(name).is_none() && !name.eq_ignore_ascii_case("authorization"),
            _ => false,
        };
        if missing {
            out.push(diag(
                url_range,
                DiagnosticSeverity::WARNING,
                format!("Missing required {} parameter `{name}`", p["in"].as_str().unwrap_or("")),
            ));
        }
    }

    let body_required = spec
        .request_body(op)
        .and_then(|b| b.get("required"))
        .and_then(Value::as_bool)
        == Some(true);
    let Some(body) = &req.body else {
        if body_required {
            out.push(diag(url_range, DiagnosticSeverity::WARNING, "Request body is required".into()));
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

    let to_range = |start: usize, end: usize| {
        let (l, c) = json_scan::offset_to_line_col(&body.text, start);
        let (_, ce) = json_scan::offset_to_line_col(&body.text, end);
        range_on_line(lines, body.start_line + l, c, ce.max(c))
    };

    let value: Value = match serde_json::from_str(&json_scan::neutralize_vars(&body.text)) {
        Ok(v) => v,
        Err(e) => {
            let line = body.start_line + e.line().saturating_sub(1);
            out.push(diag(
                range_on_line(lines, line, 0, lines.get(line).map_or(0, |l| l.len())),
                DiagnosticSeverity::WARNING,
                format!("Invalid JSON: {e}"),
            ));
            return;
        }
    };
    let (keys, _) = json_scan::scan(&body.text, None);
    let body_start = range_on_line(lines, body.start_line, 0, lines.get(body.start_line).map_or(0, |l| l.len()));
    let mut path = Vec::new();
    validate(spec, schema, &value, &mut path, &keys, &to_range, body_start, out);
}

#[allow(clippy::too_many_arguments)]
fn validate(
    spec: &Spec,
    schema: &Value,
    value: &Value,
    path: &mut Vec<PathElem>,
    keys: &[KeyTok],
    to_range: &dyn Fn(usize, usize) -> Range,
    body_start: Range,
    out: &mut Vec<Diagnostic>,
) {
    if path.len() > 12 {
        return;
    }
    if !spec.variants(schema).is_empty() {
        return;
    }
    match value {
        Value::Object(obj) => {
            let view = spec.object_view(schema);
            if view.props.is_empty() {
                return;
            }
            let opener = path.split_last().and_then(|(last, parent)| match last {
                PathElem::Key(k) => keys.iter().find(|t| &t.key == k && t.parent == parent),
                PathElem::Index => None,
            });
            let here = opener.map_or(body_start, |t| to_range(t.start, t.end));
            for r in &view.required {
                if !obj.contains_key(r) {
                    out.push(diag(here, DiagnosticSeverity::WARNING, format!("Missing required field `{r}`")));
                }
            }
            for (k, v) in obj {
                match view.prop(k) {
                    Some(child) => {
                        path.push(PathElem::Key(k.clone()));
                        validate(spec, child, v, path, keys, to_range, body_start, out);
                        path.pop();
                    }
                    None if view.additional.is_none() => {
                        if let Some(t) = keys.iter().find(|t| &t.key == k && t.parent == *path) {
                            let name = Spec::ref_name(schema).unwrap_or("schema");
                            out.push(diag(
                                to_range(t.start, t.end),
                                DiagnosticSeverity::WARNING,
                                format!("Unknown field `{k}` for `{name}`"),
                            ));
                        }
                    }
                    None => {}
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = spec.child(schema, &PathElem::Index) {
                path.push(PathElem::Index);
                for it in items {
                    validate(spec, item_schema, it, path, keys, to_range, body_start, out);
                }
                path.pop();
            }
        }
        _ => {}
    }
}
