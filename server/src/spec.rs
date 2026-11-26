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

    /// Up to 3 spec paths close to `url` (a request line URL): (new url, "METHOD /path").
    /// The URL prefix (`{{baseUrl}}/api`, host) and the query are kept.
    pub fn closest_paths(&self, method: &str, url: &str) -> Vec<(String, String)> {
        let split = url.find(['?', '#']).unwrap_or(url.len());
        let (path_part, rest) = url.split_at(split);
        let path_start = path_part
            .find("://")
            .map_or(0, |i| path_part[i + 3..].find('/').map_or(path_part.len(), |j| i + 3 + j));
        let mut segs: Vec<(usize, &str)> = Vec::new();
        let mut offset = path_start;
        for s in path_part[path_start..].split('/') {
            if !s.is_empty() {
                segs.push((offset, s));
            }
            offset += s.len() + 1;
        }
        let is_var = |s: &str| s.starts_with("{{");
        // Leading `{{baseUrl}}`-like segments are the prefix, never part of the path to fix.
        let base = segs.iter().take_while(|(_, s)| is_var(s)).count();
        let avail = segs.len() - base;
        let mut scored: Vec<(usize, bool, String, String)> = Vec::new();
        for op in &self.ops {
            let n = op.segs.len();
            for k in n.saturating_sub(1).max(1)..=(n + 1).min(avail) {
                let tail = &segs[segs.len() - k..];
                if tail.iter().all(|(_, s)| is_var(s)) {
                    continue;
                }
                let score = seg_distance(&op.segs, tail.iter().map(|(_, s)| *s).collect::<Vec<_>>().as_slice());
                let chars: usize = tail.iter().map(|(_, s)| s.chars().count()).sum();
                if score == 0 || score > (chars * 2 / 5).max(2) {
                    continue;
                }
                let new_path: Vec<String> = op
                    .segs
                    .iter()
                    .zip(op.path.split('/').filter(|s| !s.is_empty()))
                    .enumerate()
                    .map(|(i, (seg, raw))| match seg {
                        Seg::Lit(l) => l.clone(),
                        Seg::Param if k == n => tail[i].1.to_string(),
                        Seg::Param => format!("{{{{{}}}}}", raw.trim_matches(['{', '}'])),
                    })
                    .collect();
                let prefix = &path_part[..tail[0].0.saturating_sub(1).max(path_start)];
                let new_url = format!("{prefix}/{}{rest}", new_path.join("/"));
                let same_method = op.method.eq_ignore_ascii_case(method);
                scored.push((score, !same_method, new_url, format!("{} {}", op.method, op.path)));
            }
        }
        scored.sort_by_key(|a| (a.0, a.1));
        let mut out: Vec<(String, String)> = Vec::new();
        for (_, _, url, label) in scored {
            if !out.iter().any(|(u, l)| *u == url || *l == label) {
                out.push((url, label));
            }
            if out.len() == 3 {
                break;
            }
        }
        out
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

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Segment-level edit distance; a `{param}` matches any segment, `{{var}}` matches anything.
fn seg_distance(op: &[Seg], req: &[&str]) -> usize {
    let cost = |s: &Seg| match s {
        Seg::Lit(l) => l.chars().count().max(1),
        Seg::Param => 2,
    };
    let mut prev: Vec<usize> = std::iter::once(0)
        .chain(req.iter().scan(0, |acc, r| {
            *acc += r.chars().count().max(1);
            Some(*acc)
        }))
        .collect();
    for s in op {
        let mut cur = vec![prev[0] + cost(s)];
        for (j, r) in req.iter().enumerate() {
            let sub = match s {
                _ if r.starts_with("{{") => 0,
                Seg::Param => 0,
                Seg::Lit(l) => levenshtein(l, r),
            };
            let del = prev[j + 1] + cost(s);
            let ins = cur[j] + r.chars().count().max(1);
            cur.push((prev[j] + sub).min(del).min(ins));
        }
        prev = cur;
    }
    prev[req.len()]
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{spec, spec_named};
    use serde_json::json;

    fn exact(s: &Spec, method: &str, url: &str) -> String {
        match s.find(method, url) {
            Match::Exact(op) => format!("{} {}", op.method, op.path),
            Match::WrongMethod(ops) => panic!("{method} {url}: wrong method, have {:?}", ops),
            Match::None => panic!("{method} {url}: no match"),
        }
    }

    #[test]
    fn loads_yaml_and_json() {
        let s = spec();
        let ops: Vec<String> = s.ops.iter().map(|o| format!("{} {}", o.method, o.path)).collect();
        assert_eq!(
            ops,
            [
                "GET /pets",
                "POST /pets",
                "GET /pets/{id}",
                "DELETE /pets/{id}",
                "GET /pets/{id}/photos",
                "GET /pets/mine/photos",
                "GET /owners/{ownerId}",
                "POST /login",
                "GET /health",
            ]
        );
        assert_eq!(spec_named("minimal.json").ops.len(), 2);
        assert!(Spec::load(Path::new("/nonexistent.yaml")).is_err());
    }

    #[test]
    fn url_segments_drop_host_and_query() {
        assert_eq!(url_segments("https://h:1/api/pets?x=1#f"), ["api", "pets"]);
        assert_eq!(url_segments("{{baseUrl}}/pets/{{id}}"), ["{{baseUrl}}", "pets", "{{id}}"]);
        assert!(url_segments("http://h").is_empty());
    }

    #[test]
    fn find_suffix_matches_through_prefixes() {
        let s = spec();
        assert_eq!(exact(&s, "GET", "{{baseUrl}}/pets?limit=1"), "GET /pets");
        assert_eq!(exact(&s, "GET", "https://pets.example.com/api/pets/7"), "GET /pets/{id}");
        assert_eq!(exact(&s, "GET", "/api/v2/pets/{{id}}/photos"), "GET /pets/{id}/photos");
        assert_eq!(exact(&s, "delete", "/pets/1"), "DELETE /pets/{id}");
    }

    #[test]
    fn find_prefers_more_literal_segments() {
        let s = spec();
        assert_eq!(exact(&s, "GET", "/pets/mine/photos"), "GET /pets/mine/photos");
        assert_eq!(exact(&s, "GET", "/pets/other/photos"), "GET /pets/{id}/photos");
    }

    #[test]
    fn find_wrong_method_and_none() {
        let s = spec();
        match s.find("PUT", "/pets/1") {
            Match::WrongMethod(ops) => {
                let m: Vec<&str> = ops.iter().map(|o| o.method.as_str()).collect();
                assert_eq!(m, ["GET", "DELETE"]);
            }
            _ => panic!("expected WrongMethod"),
        }
        assert!(matches!(s.find("GET", "/cats"), Match::None));
        assert!(s.find_exact("PUT", "/pets/1").is_none());
    }

    #[test]
    fn root_path_matches_only_empty_urls() {
        let s = spec_named("minimal.json");
        assert_eq!(exact(&s, "GET", "http://localhost:8080"), "GET /");
        assert_eq!(exact(&s, "GET", "/"), "GET /");
        assert_eq!(exact(&s, "GET", "{{baseUrl}}/items"), "GET /items");
    }

    #[test]
    fn closest_paths_keep_prefix_and_query() {
        let s = spec();
        let c = s.closest_paths("GET", "{{baseUrl}}/pet/5?x=1");
        assert_eq!(c[0], ("{{baseUrl}}/pets/5?x=1".to_string(), "GET /pets/{id}".to_string()));
        assert!(c.len() <= 3);
        let c = s.closest_paths("GET", "https://h/api/helth");
        assert_eq!(c[0], ("https://h/api/health".to_string(), "GET /health".to_string()));
        assert!(s.closest_paths("GET", "/completely/unrelated/thing").is_empty());
    }

    #[test]
    fn resolve_and_ref_names() {
        let s = Spec {
            root: json!({"a": {"$ref": "#/b"}, "b": {"$ref": "#/c"}, "c": {"type": "string"}, "loop": {"$ref": "#/loop"}}),
            ops: vec![],
        };
        assert_eq!(s.resolve(&s.root["a"]), &json!({"type": "string"}));
        assert_eq!(s.resolve(&json!({"$ref": "#/missing"})), &json!({"$ref": "#/missing"}));
        assert_eq!(s.resolve(&s.root["loop"]), &json!({"$ref": "#/loop"}));
        assert_eq!(Spec::ref_name(&json!({"$ref": "#/components/schemas/Pet"})), Some("Pet"));
        assert_eq!(Spec::ref_name(&json!({})), None);
    }

    #[test]
    fn base_url_trims_slash() {
        assert_eq!(spec().base_url().as_deref(), Some("https://pets.example.com/api"));
        assert_eq!(spec_named("minimal.json").base_url().as_deref(), Some("http://localhost:8080"));
    }

    #[test]
    fn params_merge_path_item_and_operation() {
        let s = spec();
        let names = |m: &str, u: &str| -> Vec<String> {
            let op = s.find_exact(m, u).unwrap();
            s.params(op).iter().map(|p| p["name"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(names("GET", "/pets/1"), ["id"]);
        assert_eq!(names("GET", "/pets"), ["limit", "status", "tags", "X-Tenant"]);

        let s = Spec {
            root: json!({"paths": {"/a": {
                "parameters": [{"name": "x", "in": "query", "description": "path"}, {"name": "y", "in": "query"}],
                "get": {"parameters": [{"name": "x", "in": "query", "description": "op"}, {"name": "x", "in": "header"}]}
            }}}),
            ops: vec![Op { method: "GET".into(), path: "/a".into(), segs: parse_segs("/a") }],
        };
        let p = s.params(&s.ops[0]);
        let keys: Vec<(&str, &str)> = p.iter().map(|p| (p["name"].as_str().unwrap(), p["in"].as_str().unwrap())).collect();
        assert_eq!(keys, [("y", "query"), ("x", "query"), ("x", "header")]);
        assert_eq!(p[1]["description"], "op");
    }

    #[test]
    fn request_body_schema() {
        let s = spec();
        let op = s.find_exact("POST", "/pets").unwrap();
        let ref_ = json!({"$ref": "#/components/schemas/NewPet"});
        assert_eq!(s.body_media(op).unwrap().0, "application/json");
        assert_eq!(s.body_schema_for(op, Some("application/json; charset=utf-8")), Some(&ref_));
        assert_eq!(s.body_schema_for(op, Some("text/plain")), Some(&ref_));
        assert_eq!(s.body_schema_for(op, None), Some(&ref_));
        assert!(s.body_schema_for(s.find_exact("GET", "/pets").unwrap(), None).is_none());
    }

    #[test]
    fn security_and_bearer() {
        let s = spec();
        let list = s.find_exact("GET", "/pets").unwrap();
        let get = s.find_exact("GET", "/pets/1").unwrap();
        assert_eq!(s.security(list), ["bearerAuth"]);
        assert!(s.needs_bearer(list));
        assert!(s.security(get).is_empty());
        assert!(!s.needs_bearer(get));

        let s = Spec {
            root: json!({
                "components": {"securitySchemes": {"o": {"type": "oauth2"}, "k": {"type": "apiKey"}}},
                "paths": {"/a": {"get": {"security": [{"k": []}]}, "post": {"security": [{"k": []}, {"o": []}]}}}
            }),
            ops: vec![
                Op { method: "GET".into(), path: "/a".into(), segs: vec![] },
                Op { method: "POST".into(), path: "/a".into(), segs: vec![] },
            ],
        };
        assert!(!s.needs_bearer(&s.ops[0]));
        assert!(s.needs_bearer(&s.ops[1]));
    }

    #[test]
    fn object_view_merges_all_of() {
        let s = spec();
        let pet = &s.root["components"]["schemas"]["Pet"];
        let v = s.object_view(pet);
        let props: Vec<&str> = v.props.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(props, ["name", "kind", "vaccinated", "category", "tags", "id", "owner"]);
        assert_eq!(v.required, ["name", "kind", "id"]);
        assert!(v.closed);
        assert!(v.prop("owner").is_some());
        assert!(v.prop("nope").is_none());
        let cat = s.object_view(&s.root["components"]["schemas"]["Category"]);
        assert!(!cat.closed && cat.additional.is_none());
    }

    #[test]
    fn child_and_schema_at() {
        let s = spec();
        let owner = json!({"$ref": "#/components/schemas/Owner"});
        let path = [PathElem::Key("pets".into()), PathElem::Index, PathElem::Key("name".into())];
        assert_eq!(s.schema_at(&owner, &path).unwrap()["type"], "string");
        assert!(s.schema_at(&owner, &[PathElem::Key("nope".into())]).is_none());
        assert!(s.is_array(&s.root["components"]["schemas"]["Owner"]["properties"]["pets"]));
        assert!(!s.is_array(&owner));

        let s = Spec {
            root: json!({
                "map": {"type": "object", "additionalProperties": {"type": "integer"}},
                "one": {"oneOf": [{"type": "object", "properties": {"a": {"type": "string"}}}, {"type": "array", "items": {"type": "boolean"}}]}
            }),
            ops: vec![],
        };
        assert_eq!(s.child(&s.root["map"], &PathElem::Key("any".into())).unwrap()["type"], "integer");
        assert_eq!(s.child(&s.root["one"], &PathElem::Key("a".into())).unwrap()["type"], "string");
        assert_eq!(s.child(&s.root["one"], &PathElem::Index).unwrap()["type"], "boolean");
    }

    #[test]
    fn type_labels() {
        let s = Spec {
            root: json!({"components": {"schemas": {"Id": {"type": "string", "format": "uuid"}, "Obj": {"type": "object"}}}}),
            ops: vec![],
        };
        let l = |v: Value| s.type_label(&v);
        assert_eq!(l(json!({"$ref": "#/components/schemas/Id"})), "Id: string(uuid)");
        assert_eq!(l(json!({"$ref": "#/components/schemas/Obj"})), "Obj");
        assert_eq!(l(json!({"type": "array", "items": {"type": "integer"}})), "integer[]");
        assert_eq!(l(json!({"type": ["string", "null"]})), "string | null");
        assert_eq!(l(json!({"type": "string", "nullable": true})), "string?");
        assert_eq!(l(json!({"oneOf": [{"type": "string"}, {"type": "integer"}]})), "string | integer");
        assert_eq!(l(json!({"allOf": [{"$ref": "#/components/schemas/Obj"}]})), "Obj");
        assert_eq!(l(json!({"properties": {}})), "object");
        assert_eq!(l(json!({})), "any");
    }

    #[test]
    fn skeletons() {
        let s = spec();
        let new_pet = json!({"$ref": "#/components/schemas/NewPet"});
        assert_eq!(s.skeleton(&new_pet, 0, false), json!({"name": "", "kind": "cat"}));
        assert_eq!(
            s.skeleton(&new_pet, 0, true),
            json!({"name": "", "kind": "cat", "vaccinated": false, "category": {"id": 0, "name": ""}, "tags": [""]})
        );
        // readOnly `id` is required but never synthesized.
        assert_eq!(s.skeleton(&json!({"$ref": "#/components/schemas/Pet"}), 0, false), json!({"name": "", "kind": "cat"}));
        assert_eq!(s.skeleton(&json!({"type": "string", "format": "date-time"}), 0, false), "2026-01-01T00:00:00Z");
        assert_eq!(s.skeleton(&json!({"type": "integer", "example": 7, "default": 3}), 0, false), 7);
        assert_eq!(s.skeleton(&json!({"type": "integer", "default": 3}), 0, false), 3);
        assert_eq!(s.skeleton(&json!({"oneOf": [{"type": "boolean"}]}), 0, false), false);
        assert_eq!(s.skeleton(&json!({"type": "array", "items": {"type": "integer"}}), 4, false), json!([]));
    }

    #[test]
    fn media_examples() {
        assert_eq!(Spec::media_example(&json!({"example": 1})), Some(&json!(1)));
        assert_eq!(Spec::media_example(&json!({"examples": {"a": {"value": {"x": 1}}}})), Some(&json!({"x": 1})));
        assert_eq!(Spec::media_example(&json!({})), None);
    }

    #[test]
    fn distances() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        let op = parse_segs("/pets/{id}");
        assert_eq!(seg_distance(&op, &["pets", "5"]), 0);
        assert_eq!(seg_distance(&op, &["pet", "{{id}}"]), 1);
        assert_eq!(seg_distance(&op, &["pets"]), 2);
    }
}
