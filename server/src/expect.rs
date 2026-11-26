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

/// (expression, failure) for every `@expect`; unparsable or unresolved ones fail with that error.
pub fn run_all(exprs: &[Result<String, (String, String)>], o: &Observed) -> Vec<(String, Option<String>)> {
    exprs
        .iter()
        .map(|e| match e {
            Ok(e) => (e.clone(), parse(e).and_then(|x| x.check(o)).err()),
            Err((raw, err)) => (raw.clone(), Some(err.clone())),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obs<'a>(json: Option<&'a Value>, text: &'a str, headers: &'a [(String, String)]) -> Observed<'a> {
        Observed { status: 201, elapsed_ms: 120, headers, json, text }
    }

    fn check(expr: &str, o: &Observed) -> Result<(), String> {
        parse(expr)?.check(o)
    }

    #[test]
    fn parses_subjects_and_ops() {
        let p = |s: &str| {
            let e = parse(s).unwrap();
            (e.subject, e.op, e.value)
        };
        assert_eq!(p("status 200"), (Subject::Status, Op::Eq, "200".into()));
        assert_eq!(p("status != 500"), (Subject::Status, Op::Ne, "500".into()));
        assert_eq!(p("time <= 500"), (Subject::Time, Op::Le, "500".into()));
        assert_eq!(p("header Content-Type contains json"), (Subject::Header("Content-Type".into()), Op::Contains, "json".into()));
        assert_eq!(p("header X-Id"), (Subject::Header("X-Id".into()), Op::Exists, String::new()));
        assert_eq!(p("data.id"), (Subject::Body("data.id".into()), Op::Exists, String::new()));
        assert_eq!(p("data.id not exists"), (Subject::Body("data.id".into()), Op::NotExists, String::new()));
        assert_eq!(p("data.id !exists"), (Subject::Body("data.id".into()), Op::NotExists, String::new()));
        assert_eq!(p("data.role == \"admin\""), (Subject::Body("data.role".into()), Op::Eq, "\"admin\"".into()));
        assert_eq!(p("body contains ok"), (Subject::Body(String::new()), Op::Contains, "ok".into()));
    }

    #[test]
    fn parse_errors() {
        assert!(parse("header").unwrap_err().contains("needs a name"));
        assert!(parse("status").unwrap_err().contains("missing comparison"));
        assert!(parse("time").unwrap_err().contains("missing comparison"));
        assert!(parse("time ~ 5").unwrap_err().contains("unknown operator"));
        assert!(parse("data.id ==").unwrap_err().contains("missing value"));
        assert!(parse("data.id exists 1").unwrap_err().contains("unexpected `1`"));
        assert!(parse("data.items containsx 1").unwrap_err().contains("unknown operator"));
    }

    #[test]
    fn status_and_time() {
        let o = obs(None, "", &[]);
        assert!(check("status 201", &o).is_ok());
        assert!(check("status 2xx", &o).is_ok());
        assert!(check("status == 2XX", &o).is_ok());
        assert!(check("status != 4xx", &o).is_ok());
        assert_eq!(check("status 200", &o).unwrap_err(), "got 201");
        assert!(check("status >= 200", &o).is_ok());
        assert!(check("time < 500", &o).is_ok());
        assert_eq!(check("time < 100", &o).unwrap_err(), "got 120");
        assert!(check("time > fast", &o).unwrap_err().starts_with("not a number"));
    }

    #[test]
    fn headers_are_case_insensitive() {
        let h = [("content-type".to_string(), "application/json; charset=utf-8".to_string())];
        let o = obs(None, "", &h);
        assert!(check("header Content-Type contains json", &o).is_ok());
        assert!(check("header CONTENT-TYPE", &o).is_ok());
        assert_eq!(check("header X-Missing", &o).unwrap_err(), "missing");
        assert!(check("header X-Missing !exists", &o).is_ok());
        assert!(check("header Content-Type !exists", &o).is_err());
    }

    #[test]
    fn json_body_paths() {
        let v = json!({"data": {"id": 7, "role": "admin", "items": [1, 2, 3], "name": "Ann", "tags": ["a"], "meta": {"k": 1}, "s": "10"}});
        let o = obs(Some(&v), "", &[]);
        for ok in [
            "data.id exists",
            "data.nope !exists",
            "data.id == 7",
            "data.id == \"7\"",
            "data.role == admin",
            "data.role == \"admin\"",
            "data.role != user",
            "data.items.length > 0",
            "data.items.length == 3",
            "data.name.length == 3",
            "data.meta.length == 1",
            "data.items contains 2",
            "data.tags contains \"a\"",
            "data.meta contains k",
            "data.role contains adm",
            "data.s > 9",
            "data.items.1 == 2",
        ] {
            assert!(check(ok, &o).is_ok(), "{ok}: {:?}", check(ok, &o));
        }
        assert_eq!(check("data.id == 8", &o).unwrap_err(), "got 7");
        assert_eq!(check("data.nope exists", &o).unwrap_err(), "missing");
        assert_eq!(check("data.id.length > 0", &o).unwrap_err(), "missing");
        assert!(check("data.role > 1", &o).unwrap_err().starts_with("not a number"));
    }

    #[test]
    fn whole_body() {
        let v = json!({"ok": true});
        let o = obs(Some(&v), r#"{"ok": true}"#, &[]);
        assert!(check("body contains \"ok\": true", &o).is_ok());
        assert!(check("body exists", &o).is_ok());
        let o = obs(None, "plain text", &[]);
        assert!(check("body contains plain", &o).is_ok());
        assert!(check("body == plain text", &o).is_ok());
        assert_eq!(check("data.x exists", &o).unwrap_err(), "missing");
    }

    #[test]
    fn run_all_keeps_order() {
        let o = obs(None, "", &[]);
        let exprs = vec![
            Ok("status 201".to_string()),
            Ok("status 500".to_string()),
            Ok("bogus ~".to_string()),
            Err(("status {{$nope}}".to_string(), "undefined variable(s): $nope".to_string())),
        ];
        let r = run_all(&exprs, &o);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0], ("status 201".to_string(), None));
        assert_eq!(r[1].1.as_deref(), Some("got 201"));
        assert!(r[2].1.as_deref().unwrap().contains("unknown operator"));
        assert_eq!(r[3], ("status {{$nope}}".to_string(), Some("undefined variable(s): $nope".to_string())));
    }
}
