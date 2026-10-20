use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::path::Path;

pub const HTTP_METHODS: &[&str] = &["get", "put", "post", "delete", "options", "head", "patch", "trace"];

#[derive(Debug, Clone)]
enum Seg {
    Lit(String),
    Param,
}

#[derive(Debug)]
pub struct Op {
    pub method: String,
    pub path: String,
    segs: Vec<Seg>,
}

pub struct Spec {
    pub root: Value,
    pub ops: Vec<Op>,
}

pub enum Match<'a> {
    Exact(&'a Op),
    WrongMethod(Vec<&'a Op>),
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathElem {
    Key(String),
    Index,
}

#[derive(Default)]
pub struct ObjectView<'a> {
    pub props: Vec<(String, &'a Value)>,
    pub required: Vec<String>,
    pub closed: bool,
    pub additional: Option<&'a Value>,
}

impl<'a> ObjectView<'a> {
    pub fn prop(&self, name: &str) -> Option<&'a Value> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }
}

fn parse_segs(path: &str) -> Vec<Seg> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') {
                Seg::Param
            } else {
                Seg::Lit(s.to_string())
            }
        })
        .collect()
}

/// Path segments of a raw .http url: scheme/host and query are dropped, `{{vars}}` kept as-is.
pub fn url_segments(url: &str) -> Vec<String> {
    let mut u = url;
    if let Some(i) = u.find(['?', '#']) {
        u = &u[..i];
    }
    if let Some(i) = u.find("://") {
        u = match u[i + 3..].find('/') {
            Some(j) => &u[i + 3 + j..],
            None => "",
        };
    }
    u.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl Spec {
    pub fn load(path: &Path) -> Result<Spec> {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml" | "yml")
        );
        let root: Value = if is_yaml {
            serde_yaml::from_str(&text).context("parse yaml")?
        } else {
            serde_json::from_str(&text).context("parse json")?
        };
        let mut ops = Vec::new();
        if let Some(paths) = root.get("paths").and_then(Value::as_object) {
            for (p, item) in paths {
                for m in HTTP_METHODS {
                    if item.get(*m).is_some() {
                        ops.push(Op {
                            method: m.to_uppercase(),
                            path: p.clone(),
                            segs: parse_segs(p),
                        });
                    }
                }
            }
        }
        Ok(Spec { root, ops })
    }

    pub fn path_item(&self, op: &Op) -> &Value {
        &self.root["paths"][&op.path]
    }

    pub fn operation(&self, op: &Op) -> &Value {
        &self.path_item(op)[op.method.to_lowercase()]
    }

    pub fn resolve<'a>(&'a self, mut v: &'a Value) -> &'a Value {
        for _ in 0..32 {
            match v.get("$ref").and_then(Value::as_str) {
                Some(r) => match r.strip_prefix('#').and_then(|p| self.root.pointer(p)) {
                    Some(t) => v = t,
                    None => return v,
                },
                None => return v,
            }
        }
        v
    }

    pub fn ref_name(v: &Value) -> Option<&str> {
        v.get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.rsplit('/').next())
    }

    pub fn base_url(&self) -> Option<String> {
        self.root
            .pointer("/servers/0/url")
            .and_then(Value::as_str)
            .map(|s| s.trim_end_matches('/').to_string())
    }

    pub fn find(&self, method: &str, url: &str) -> Match<'_> {
        let segs = url_segments(url);
        let mut best: Option<(usize, usize)> = None;
        let mut hits: Vec<&Op> = Vec::new();
        for op in &self.ops {
            let n = op.segs.len();
            if segs.len() < n {
                continue;
            }
            let tail = &segs[segs.len() - n..];
            let mut lits = 0;
            let ok = op.segs.iter().zip(tail).all(|(s, t)| match s {
                Seg::Lit(l) => {
                    lits += 1;
                    l == t
                }
                Seg::Param => true,
            });
            if !ok || (n == 0 && !segs.is_empty()) {
                continue;
            }
            let score = (n, lits);
            match best {
                Some(b) if b > score => {}
                Some(b) if b == score => hits.push(op),
                _ => {
                    best = Some(score);
                    hits = vec![op];
                }
            }
        }
        if hits.is_empty() {
            return Match::None;
        }
        match hits.iter().find(|o| o.method.eq_ignore_ascii_case(method)) {
            Some(op) => Match::Exact(op),
            None => Match::WrongMethod(hits),
        }
    }

    pub fn find_exact(&self, method: &str, url: &str) -> Option<&Op> {
        match self.find(method, url) {
            Match::Exact(op) => Some(op),
            _ => None,
        }
    }

    /// Path-level params overridden by operation-level ones with the same (name, in).
    pub fn params(&self, op: &Op) -> Vec<&Value> {
        let mut out: Vec<&Value> = Vec::new();
        let sources = [self.path_item(op).get("parameters"), self.operation(op).get("parameters")];
        for list in sources.into_iter().flatten().filter_map(Value::as_array) {
            for p in list {
                let p = self.resolve(p);
                let key = (p.get("name"), p.get("in"));
                out.retain(|q| (q.get("name"), q.get("in")) != key);
                out.push(p);
            }
        }
        out
    }

    pub fn request_body(&self, op: &Op) -> Option<&Value> {
        self.operation(op).get("requestBody").map(|b| self.resolve(b))
    }

    /// Preferred media type entry of the request body: JSON first.
    pub fn body_media(&self, op: &Op) -> Option<(&str, &Value)> {
        let content = self.request_body(op)?.get("content")?.as_object()?;
        content
            .iter()
            .find(|(k, _)| k.contains("json"))
            .or_else(|| content.iter().next())
            .map(|(k, v)| (k.as_str(), v))
    }

    pub fn body_schema_for(&self, op: &Op, content_type: Option<&str>) -> Option<&Value> {
        let content = self.request_body(op)?.get("content")?.as_object()?;
        let media = content_type
            .map(|ct| ct.split(';').next().unwrap_or("").trim())
            .and_then(|ct| content.get(ct))
            .or_else(|| self.body_media(op).map(|(_, m)| m))?;
        media.get("schema")
    }

    pub fn security(&self, op: &Op) -> Vec<String> {
        let sec = self
            .operation(op)
            .get("security")
            .or_else(|| self.root.get("security"));
        sec.and_then(Value::as_array)
            .map(|reqs| {
                reqs.iter()
                    .filter_map(Value::as_object)
                    .flat_map(|o| o.keys().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn needs_bearer(&self, op: &Op) -> bool {
        self.security(op).iter().any(|name| {
            let s = &self.root["components"]["securitySchemes"][name];
            s.get("scheme").and_then(Value::as_str) == Some("bearer")
                || s.get("type").and_then(Value::as_str) == Some("oauth2")
                || s.get("type").and_then(Value::as_str) == Some("openIdConnect")
        })
    }

    pub fn object_view<'a>(&'a self, schema: &'a Value) -> ObjectView<'a> {
        let mut view = ObjectView::default();
        self.collect_object(schema, &mut view, 0);
        view
    }

    fn collect_object<'a>(&'a self, schema: &'a Value, view: &mut ObjectView<'a>, depth: usize) {
        if depth > 16 {
            return;
        }
        let s = self.resolve(schema);
        if let Some(props) = s.get("properties").and_then(Value::as_object) {
            for (k, v) in props {
                if view.prop(k).is_none() {
                    view.props.push((k.clone(), v));
                }
            }
        }
        if let Some(req) = s.get("required").and_then(Value::as_array) {
            view.required.extend(req.iter().filter_map(Value::as_str).map(str::to_string));
        }
        match s.get("additionalProperties") {
            Some(Value::Bool(false)) => view.closed = true,
            Some(v @ Value::Object(_)) => view.additional = Some(v),
            _ => {}
        }
        if let Some(all) = s.get("allOf").and_then(Value::as_array) {
            for sub in all {
                self.collect_object(sub, view, depth + 1);
            }
        }
    }

    pub fn variants<'a>(&'a self, schema: &'a Value) -> Vec<&'a Value> {
        let s = self.resolve(schema);
        ["oneOf", "anyOf"]
            .iter()
            .filter_map(|k| s.get(*k).and_then(Value::as_array))
            .flatten()
            .collect()
    }

    pub fn is_array(&self, schema: &Value) -> bool {
        let s = self.resolve(schema);
        s.get("type").and_then(Value::as_str) == Some("array") || s.get("items").is_some()
    }

    pub fn child<'a>(&'a self, schema: &'a Value, elem: &PathElem) -> Option<&'a Value> {
        let s = self.resolve(schema);
        match elem {
            PathElem::Index => s.get("items").or_else(|| {
                self.variants(s).into_iter().find_map(|v| self.resolve(v).get("items"))
            }),
            PathElem::Key(k) => {
                let view = self.object_view(s);
                if let Some(p) = view.prop(k) {
                    return Some(p);
                }
                if let Some(a) = view.additional {
                    return Some(a);
                }
                self.variants(s).into_iter().find_map(|v| self.child(v, elem))
            }
        }
    }

    pub fn schema_at<'a>(&'a self, schema: &'a Value, path: &[PathElem]) -> Option<&'a Value> {
        path.iter().try_fold(schema, |s, e| self.child(s, e))
    }

    pub fn type_label(&self, schema: &Value) -> String {
        self.type_label_depth(schema, 0)
    }

    fn type_label_depth(&self, schema: &Value, depth: usize) -> String {
        if depth > 4 {
            return "…".into();
        }
        if let Some(name) = Self::ref_name(schema) {
            let s = self.resolve(schema);
            let is_primitive = matches!(
                s.get("type").and_then(Value::as_str),
                Some("string" | "integer" | "number" | "boolean")
            );
            return if is_primitive {
                format!("{name}: {}", self.type_label_depth(s, depth + 1))
            } else {
                name.to_string()
            };
        }
        let vars = self.variants(schema);
        if !vars.is_empty() {
            return vars
                .iter()
                .map(|v| self.type_label_depth(v, depth + 1))
                .collect::<Vec<_>>()
                .join(" | ");
        }
        if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
            if all.len() == 1 {
                return self.type_label_depth(&all[0], depth + 1);
            }
        }
        let ty = match schema.get("type") {
            Some(Value::String(t)) => t.clone(),
            Some(Value::Array(ts)) => ts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | "),
            _ if schema.get("properties").is_some() || schema.get("allOf").is_some() => "object".into(),
            _ if schema.get("items").is_some() => "array".into(),
            _ => "any".into(),
        };
        let mut label = if ty == "array" {
            match schema.get("items") {
                Some(items) => format!("{}[]", self.type_label_depth(items, depth + 1)),
                None => "array".into(),
            }
        } else if let Some(f) = schema.get("format").and_then(Value::as_str) {
            format!("{ty}({f})")
        } else {
            ty
        };
        if schema.get("nullable").and_then(Value::as_bool) == Some(true) {
            label.push('?');
        }
        label
    }

    /// Example value for a schema: example > default > enum > synthesized from type.
    pub fn skeleton(&self, schema: &Value, depth: usize, all_props: bool) -> Value {
        let s = self.resolve(schema);
        if let Some(e) = s.get("example") {
            return e.clone();
        }
        if let Some(d) = s.get("default") {
            return d.clone();
        }
        if let Some(first) = s.get("enum").and_then(Value::as_array).and_then(|e| e.first()) {
            return first.clone();
        }
        if let Some(v) = self.variants(s).first() {
            return self.skeleton(v, depth, all_props);
        }
        if self.is_array(s) {
            return match s.get("items") {
                Some(items) if depth < 4 => Value::Array(vec![self.skeleton(items, depth + 1, false)]),
                _ => Value::Array(vec![]),
            };
        }
        let view = self.object_view(s);
        if !view.props.is_empty() || s.get("type").and_then(Value::as_str) == Some("object") {
            let mut map = Map::new();
            if depth < 4 {
                let use_all = all_props || view.required.is_empty();
                for (k, v) in &view.props {
                    if use_all || view.required.contains(k) {
                        let rv = self.resolve(v);
                        if rv.get("readOnly").and_then(Value::as_bool) == Some(true) {
                            continue;
                        }
                        map.insert(k.clone(), self.skeleton(v, depth + 1, false));
                    }
                }
            }
            return Value::Object(map);
        }
        let ty = s.get("type").and_then(Value::as_str).unwrap_or("string");
        match ty {
            "integer" | "number" => Value::from(0),
            "boolean" => Value::Bool(false),
            _ => Value::String(
                match s.get("format").and_then(Value::as_str) {
                    Some("date") => "2026-01-01",
                    Some("date-time") => "2026-01-01T00:00:00Z",
                    Some("email") => "user@example.com",
                    Some("uuid") => "00000000-0000-0000-0000-000000000000",
                    Some("uri" | "url") => "https://example.com",
                    _ => "",
                }
                .to_string(),
            ),
        }
    }

    pub fn media_example(media: &Value) -> Option<&Value> {
        media.get("example").or_else(|| {
            media
                .get("examples")
                .and_then(Value::as_object)
                .and_then(|e| e.values().next())
                .and_then(|e| e.get("value"))
        })
    }
}
