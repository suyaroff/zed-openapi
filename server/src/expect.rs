//! `# @expect` assertions:
//! `status 201`, `status 2xx`, `time < 500`, `header Content-Type contains json`,
//! `data.id exists`, `data.items.length > 0`, `data.role == "admin"`, `body contains ok`
//! (`body contains` searches the raw response text).

use crate::env::json_path;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
enum Subject {
    Status,
    Time,
    Header(String),
    /// Dot path into the JSON body; empty = the whole body.
    Body(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Contains,
    Exists,
    NotExists,
}

#[derive(Debug, Clone)]
pub struct Expect {
    subject: Subject,
    op: Op,
    value: String,
}

pub struct Observed<'a> {
    pub status: u16,
    pub elapsed_ms: u128,
    pub headers: &'a [(String, String)],
    pub json: Option<&'a Value>,
    pub text: &'a str,
}

const OPS: &[(&str, Op)] = &[
    ("==", Op::Eq),
    ("!=", Op::Ne),
    (">=", Op::Ge),
    ("<=", Op::Le),
    (">", Op::Gt),
    ("<", Op::Lt),
    ("contains", Op::Contains),
    ("!exists", Op::NotExists),
    ("not exists", Op::NotExists),
    ("exists", Op::Exists),
];

pub fn parse(s: &str) -> Result<Expect, String> {
    let s = s.trim();
    let (head, rest) = s.split_once(char::is_whitespace).unwrap_or((s, ""));
    let (subject, rest) = match head {
        "status" => (Subject::Status, rest),
        "time" => (Subject::Time, rest),
        "header" => {
            let rest = rest.trim_start();
            let (name, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if name.is_empty() {
                return Err("`header` needs a name: `header Content-Type contains json`".into());
            }
            (Subject::Header(name.to_string()), rest)
        }
        "body" => (Subject::Body(String::new()), rest),
        path => (Subject::Body(path.to_string()), rest),
    };
    let rest = rest.trim();
    let found = OPS
        .iter()
        .find(|(t, _)| rest.starts_with(t) && (t.starts_with(['=', '!', '<', '>']) || rest[t.len()..].is_empty() || rest[t.len()..].starts_with(' ')));
    let (op, value) = match found {
        Some((t, op)) => (*op, rest[t.len()..].trim().to_string()),
        None if rest.is_empty() && matches!(subject, Subject::Body(_) | Subject::Header(_)) => (Op::Exists, String::new()),
        None if subject == Subject::Status && !rest.is_empty() => (Op::Eq, rest.to_string()),
        None if rest.is_empty() => return Err("missing comparison, e.g. `status 200` or `time < 500`".into()),
        None => return Err(format!("unknown operator in `{rest}`; use == != > >= < <= contains exists !exists")),
    };
    let needs_value = !matches!(op, Op::Exists | Op::NotExists);
    if needs_value && value.is_empty() {
        return Err("missing value after the operator".into());
    }
    if !needs_value && !value.is_empty() {
        return Err(format!("unexpected `{value}` after `exists`"));
    }
    Ok(Expect { subject, op, value })
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn short(v: &Value) -> String {
    crate::util::one_line(&v.to_string(), 60)
}

/// The expected value as JSON when it parses (`1`, `true`, `"x"`), else as a bare string.
fn expected(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn equal(actual: &Value, raw: &str) -> bool {
    let exp = expected(raw);
    *actual == exp || text_of(actual) == text_of(&exp)
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

impl Expect {
    fn actual(&self, o: &Observed) -> Option<Value> {
        match &self.subject {
            Subject::Status => Some(Value::from(o.status)),
            Subject::Time => Some(Value::from(o.elapsed_ms as u64)),
            Subject::Header(name) => o
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| Value::String(v.clone())),
            Subject::Body(path) if path.is_empty() => {
                Some(o.json.cloned().unwrap_or_else(|| Value::String(o.text.to_string())))
            }
            Subject::Body(path) => {
                let json = o.json?;
                if let Some(v) = json_path(json, path) {
                    return Some(v.clone());
                }
                let parent = path.strip_suffix("length")?.trim_end_matches('.');
                let target = if parent.is_empty() { Some(json) } else { json_path(json, parent) };
                match target? {
                    Value::Array(a) => Some(Value::from(a.len())),
                    Value::String(s) => Some(Value::from(s.chars().count())),
                    Value::Object(m) => Some(Value::from(m.len())),
                    _ => None,
                }
            }
        }
    }

    /// `Ok(())` when it holds, else what was actually observed.
    pub fn check(&self, o: &Observed) -> Result<(), String> {
        let actual = self.actual(o);
        let Some(actual) = actual else {
            return match self.op {
                Op::NotExists => Ok(()),
                _ => Err("missing".into()),
            };
        };
        let ok = match self.op {
            Op::Exists => true,
            Op::NotExists => false,
            Op::Eq | Op::Ne => {
                let class = self.subject == Subject::Status
                    && self.value.len() == 3
                    && self.value.get(1..).is_some_and(|x| x.eq_ignore_ascii_case("xx"));
                let eq = if class {
                    self.value[..1] == (o.status / 100).to_string()
                } else {
                    equal(&actual, &self.value)
                };
                eq == (self.op == Op::Eq)
            }
            Op::Gt | Op::Ge | Op::Lt | Op::Le => {
                let (Some(a), Some(e)) = (number(&actual), self.value.parse::<f64>().ok()) else {
                    return Err(format!("not a number: {}", short(&actual)));
                };
                match self.op {
                    Op::Gt => a > e,
                    Op::Ge => a >= e,
                    Op::Lt => a < e,
                    _ => a <= e,
                }
            }
            Op::Contains if self.subject == Subject::Body(String::new()) => {
                o.text.contains(text_of(&expected(&self.value)).as_str())
            }
            Op::Contains => match &actual {
                Value::String(s) => s.contains(text_of(&expected(&self.value)).as_str()),
                Value::Array(items) => items.iter().any(|it| equal(it, &self.value)),
                Value::Object(m) => m.contains_key(&text_of(&expected(&self.value))),
                other => text_of(other).contains(&self.value),
            },
        };
        if ok {
            Ok(())
        } else {
            Err(format!("got {}", short(&actual)))
        }
    }
}

/// (expression, failure) for every `@expect`; unparsable ones fail with the parse error.
pub fn run_all(exprs: &[String], o: &Observed) -> Vec<(String, Option<String>)> {
    exprs
        .iter()
        .map(|e| {
            let r = parse(e).and_then(|x| x.check(o));
            (e.clone(), r.err())
        })
        .collect()
}
