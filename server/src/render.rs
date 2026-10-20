use crate::spec::{Op, Spec};
use crate::util::one_line;
use serde_json::Value;
use std::fmt::Write;

const DESC_MAX: usize = 200;

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn constraints(spec: &Spec, schema: &Value) -> Vec<String> {
    let r = spec.resolve(schema);
    let mut out = Vec::new();
    if let Some(e) = r.get("enum").and_then(Value::as_array) {
        let vals: Vec<String> = e.iter().take(12).map(|v| format!("`{v}`")).collect();
        let more = if e.len() > 12 { ", …" } else { "" };
        out.push(format!("one of {}{more}", vals.join(", ")));
    }
    for (k, label) in [
        ("minLength", "min length"),
        ("maxLength", "max length"),
        ("minimum", "min"),
        ("maximum", "max"),
        ("minItems", "min items"),
        ("maxItems", "max items"),
        ("pattern", "pattern"),
        ("default", "default"),
    ] {
        if let Some(v) = r.get(k) {
            out.push(format!("{label} `{v}`"));
        }
    }
    if r.get("readOnly").and_then(Value::as_bool) == Some(true) {
        out.push("read-only".into());
    }
    if r.get("deprecated").and_then(Value::as_bool) == Some(true) {
        out.push("**deprecated**".into());
    }
    out
}

fn description(spec: &Spec, schema: &Value) -> Option<String> {
    s(schema, "description")
        .or_else(|| s(spec.resolve(schema), "description"))
        .map(|d| one_line(d, DESC_MAX))
}

pub fn field_line(spec: &Spec, name: &str, schema: &Value, required: bool) -> String {
    let mut line = if required {
        format!("**`{name}`**\\* `{}`", spec.type_label(schema))
    } else {
        format!("`{name}` `{}`", spec.type_label(schema))
    };
    let mut extra = constraints(spec, schema);
    if let Some(d) = description(spec, schema) {
        extra.insert(0, d);
    }
    if !extra.is_empty() {
        let _ = write!(line, " — {}", extra.join(" · "));
    }
    line
}

/// Markdown nested list of an object's fields, expanding refs up to `max_depth`.
pub fn schema_tree(spec: &Spec, schema: &Value, max_depth: usize) -> String {
    let mut out = String::new();
    let mut stack = Vec::new();
    tree(spec, schema, 0, max_depth, &mut stack, &mut out);
    out
}

fn tree(spec: &Spec, schema: &Value, depth: usize, max: usize, stack: &mut Vec<String>, out: &mut String) {
    let indent = "  ".repeat(depth);
    let mut target = schema;
    while spec.is_array(target) {
        match spec.resolve(target).get("items") {
            Some(items) => target = items,
            None => return,
        }
    }
    let ref_name = Spec::ref_name(target).map(str::to_string);
    if let Some(n) = &ref_name {
        if stack.contains(n) {
            return;
        }
        stack.push(n.clone());
    }
    let view = spec.object_view(target);
    if view.props.is_empty() {
        let variants = spec.variants(target);
        for v in variants {
            let _ = writeln!(out, "{indent}- *variant* `{}`", spec.type_label(v));
            if depth < max {
                tree(spec, v, depth + 1, max, stack, out);
            }
        }
    }
    for (name, prop) in &view.props {
        let required = view.required.contains(name);
        let _ = writeln!(out, "{indent}- {}", field_line(spec, name, prop, required));
        if depth < max {
            tree(spec, prop, depth + 1, max, stack, out);
        }
    }
    if ref_name.is_some() {
        stack.pop();
    }
}

fn param_line(spec: &Spec, p: &Value) -> String {
    let name = s(p, "name").unwrap_or("?");
    let loc = s(p, "in").unwrap_or("?");
    let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
    let schema = p.get("schema").cloned().unwrap_or(Value::Null);
    let mut line = if required {
        format!("**`{name}`**\\* *{loc}* `{}`", spec.type_label(&schema))
    } else {
        format!("`{name}` *{loc}* `{}`", spec.type_label(&schema))
    };
    let mut extra = constraints(spec, &schema);
    if let Some(d) = s(p, "description") {
        extra.insert(0, one_line(d, DESC_MAX));
    }
    if let Some(e) = p.get("example") {
        extra.push(format!("e.g. `{e}`"));
    }
    if !extra.is_empty() {
        let _ = write!(line, " — {}", extra.join(" · "));
    }
    line
}

pub fn param_doc(spec: &Spec, p: &Value) -> String {
    let mut out = param_line(spec, p);
    if let Some(d) = s(p, "description") {
        let _ = write!(out, "\n\n{d}");
    }
    out
}

pub fn operation(spec: &Spec, op: &Op) -> String {
    let o = spec.operation(op);
    let mut out = String::new();
    let _ = writeln!(out, "### `{}` {}\n", op.method, op.path);
    if let Some(sum) = s(o, "summary") {
        let _ = writeln!(out, "**{sum}**\n");
    }
    if o.get("deprecated").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(out, "⚠️ **Deprecated**\n");
    }
    if let Some(d) = s(o, "description") {
        let _ = writeln!(out, "{d}\n");
    }
    let mut meta = Vec::new();
    if let Some(tags) = o.get("tags").and_then(Value::as_array) {
        let t: Vec<String> = tags.iter().filter_map(Value::as_str).map(|t| format!("`{t}`")).collect();
        if !t.is_empty() {
            meta.push(format!("Tags: {}", t.join(", ")));
        }
    }
    if let Some(id) = s(o, "operationId") {
        meta.push(format!("operationId: `{id}`"));
    }
    let sec = spec.security(op);
    meta.push(if sec.is_empty() {
        "🔓 public".to_string()
    } else {
        format!("🔒 {}", sec.join(", "))
    });
    let _ = writeln!(out, "{}\n", meta.join(" · "));

    let params = spec.params(op);
    if !params.is_empty() {
        let _ = writeln!(out, "#### Parameters\n");
        for p in &params {
            let _ = writeln!(out, "- {}", param_line(spec, p));
        }
        out.push('\n');
    }

    if let Some(body) = spec.request_body(op) {
        let required = body.get("required").and_then(Value::as_bool).unwrap_or(false);
        if let Some(content) = body.get("content").and_then(Value::as_object) {
            for (media, m) in content {
                let schema = m.get("schema").cloned().unwrap_or(Value::Null);
                let _ = writeln!(
                    out,
                    "#### Body{} `{media}` — `{}`\n",
                    if required { "\\*" } else { "" },
                    spec.type_label(&schema)
                );
                out.push_str(&schema_tree(spec, &schema, 3));
                out.push('\n');
            }
        }
    }

    if let Some(responses) = o.get("responses").and_then(Value::as_object) {
        let _ = writeln!(out, "#### Responses\n");
        let mut expanded = false;
        for (code, r) in responses {
            let r = spec.resolve(r);
            let desc = s(r, "description").map(|d| one_line(d, DESC_MAX)).unwrap_or_default();
            let content = r.get("content").and_then(Value::as_object);
            let media = content.and_then(|c| c.iter().find(|(k, _)| k.contains("json")).or_else(|| c.iter().next()));
            match media {
                Some((mt, m)) => {
                    let schema = m.get("schema").cloned().unwrap_or(Value::Null);
                    let _ = writeln!(out, "- **{code}** {desc} — `{}` *{mt}*", spec.type_label(&schema));
                    if !expanded && code.starts_with('2') {
                        expanded = true;
                        for l in schema_tree(spec, &schema, 3).lines() {
                            let _ = writeln!(out, "  {l}");
                        }
                    }
                }
                None => {
                    let _ = writeln!(out, "- **{code}** {desc}");
                }
            }
        }
    }
    out
}

pub fn field_doc(spec: &Spec, name: &str, schema: &Value, required: bool) -> String {
    let mut out = field_line(spec, name, schema, required);
    let r = spec.resolve(schema);
    if let Some(d) = s(schema, "description").or_else(|| s(r, "description")) {
        if d.chars().count() > DESC_MAX {
            let _ = write!(out, "\n\n{d}");
        }
    }
    let nested = schema_tree(spec, schema, 2);
    if !nested.is_empty() {
        let _ = write!(out, "\n\n{nested}");
    }
    out
}
