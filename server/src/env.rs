use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_FILE: &str = ".openapi/state.json";
const SELECTED_ENV_FILE: &str = ".openapi/env";

#[derive(Default, Clone)]
pub struct Ctx {
    pub env_name: String,
    pub file_vars: HashMap<String, String>,
    pub captured: HashMap<String, String>,
    pub env: HashMap<String, String>,
    pub spec_base: Option<String>,
}

impl Ctx {
    pub fn raw(&self, name: &str) -> Option<&str> {
        self.file_vars
            .get(name)
            .or_else(|| self.captured.get(name))
            .or_else(|| self.env.get(name))
            .map(String::as_str)
            .or_else(|| (name == "baseUrl").then_some(self.spec_base.as_deref()).flatten())
    }

    /// Why `{{name}}` has no value; `None` when it resolves.
    pub fn missing(&self, name: &str) -> Option<Missing> {
        if !is_dynamic(name) {
            return self.raw(name).is_none().then_some(Missing::Var);
        }
        match parse_dynamic(name) {
            None => Some(Missing::UnknownDynamic),
            Some(Dynamic::Env(var)) if std::env::var_os(var).is_none() => Some(Missing::EnvUnset(var.to_string())),
            Some(_) => None,
        }
    }

    pub fn names(&self) -> Vec<(String, String)> {
        let mut seen = HashMap::new();
        for (src, map) in [("file", &self.file_vars), ("captured", &self.captured), ("env", &self.env)] {
            for k in map.keys() {
                seen.entry(k.clone()).or_insert(src.to_string());
            }
        }
        if self.spec_base.is_some() {
            seen.entry("baseUrl".into()).or_insert("spec servers[0]".into());
        }
        let mut v: Vec<_> = seen.into_iter().collect();
        v.sort();
        v
    }

    /// Substitutes `{{var}}` recursively; returns the names that could not be resolved.
    pub fn substitute(&self, text: &str) -> Result<String, Vec<String>> {
        let mut missing = Vec::new();
        let out = self.subst_depth(text, 0, &mut missing);
        if missing.is_empty() {
            Ok(out)
        } else {
            missing.dedup();
            Err(missing)
        }
    }

    fn subst_depth(&self, text: &str, depth: usize, missing: &mut Vec<String>) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(s) = rest.find("{{") {
            out.push_str(&rest[..s]);
            let Some(e) = rest[s..].find("}}") else {
                rest = &rest[s..];
                break;
            };
            let name = rest[s + 2..s + e].trim();
            match self.value(name) {
                Some(v) if depth < 8 => out.push_str(&self.subst_depth(&v, depth + 1, missing)),
                Some(v) => out.push_str(&v),
                None => {
                    missing.push(name.to_string());
                    out.push_str(&rest[s..s + e + 2]);
                }
            }
            rest = &rest[s + e + 2..];
        }
        out.push_str(rest);
        out
    }

    fn value(&self, name: &str) -> Option<String> {
        if let Some(d) = dynamic(name) {
            return d;
        }
        self.raw(name).map(str::to_string)
    }
}

pub fn var_names_in(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(s) = text[from..].find("{{") {
        let s = from + s;
        let Some(e) = text[s..].find("}}") else { break };
        out.push((text[s + 2..s + e].trim().to_string(), s, s + e + 2));
        from = s + e + 2;
    }
    out
}

/// Why a variable has no value, see `Ctx::missing`.
#[derive(Debug, Clone, PartialEq)]
pub enum Missing {
    Var,
    UnknownDynamic,
    EnvUnset(String),
}

impl Missing {
    pub fn message(&self, name: &str, env_name: &str) -> String {
        match self {
            Missing::Var => format!("Variable `{name}` is not defined in env `{env_name}`"),
            Missing::UnknownDynamic => {
                let known = DYNAMIC_NAMES.join(", ");
                format!("Unknown dynamic variable `{name}`; supported: {known}, $env.NAME")
            }
            Missing::EnvUnset(var) => format!("Environment variable `{var}` is not set"),
        }
    }
}

/// Dynamic variables without arguments, for messages and "did you mean" fixes.
pub const DYNAMIC_NAMES: &[&str] = &["$uuid", "$guid", "$random.uuid", "$timestamp", "$randomInt"];

fn is_dynamic(name: &str) -> bool {
    name.starts_with('$')
}

enum Dynamic<'a> {
    Uuid,
    Timestamp,
    RandomInt,
    Env(&'a str),
}

/// `None` for names that are not supported dynamic variables.
fn parse_dynamic(name: &str) -> Option<Dynamic<'_>> {
    if name == "$random.uuid" {
        return Some(Dynamic::Uuid);
    }
    let (head, arg) = match name.split_once(|c: char| c.is_whitespace() || c == '.') {
        Some((h, a)) => (h, a.trim()),
        None => (name, ""),
    };
    match head {
        "$uuid" | "$guid" => Some(Dynamic::Uuid),
        "$timestamp" => Some(Dynamic::Timestamp),
        "$randomInt" => Some(Dynamic::RandomInt),
        "$env" | "$processEnv" if !arg.is_empty() => Some(Dynamic::Env(arg)),
        _ => None,
    }
}

fn random_u64() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish()
}

/// `None` for non-`$` names; `Some(None)` for `$` names without a value.
fn dynamic(name: &str) -> Option<Option<String>> {
    if !is_dynamic(name) {
        return None;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let Some(d) = parse_dynamic(name) else { return Some(None) };
    Some(match d {
        Dynamic::Uuid => {
            let (a, b) = (random_u64(), random_u64());
            Some(format!(
                "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
                a >> 32,
                (a >> 16) & 0xffff,
                a & 0xfff,
                (b >> 48) & 0x3fff | 0x8000,
                b & 0xffff_ffff_ffff
            ))
        }
        Dynamic::Timestamp => Some(now.as_secs().to_string()),
        Dynamic::RandomInt => Some((random_u64() % 1000).to_string()),
        Dynamic::Env(var) => std::env::var(var).ok(),
    })
}

fn read_json(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.as_object().cloned()
}

fn to_string_map(v: Option<&Value>, out: &mut HashMap<String, String>) {
    if let Some(obj) = v.and_then(Value::as_object) {
        for (k, v) in obj {
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.insert(k.clone(), s);
        }
    }
}

/// Picks the configured env, else `dev`, else the first one declared.
pub fn load_env(
    wanted: Option<&str>,
    vars: &Map<String, Value>,
    envs: &Map<String, Value>,
) -> (String, HashMap<String, String>) {
    let name = wanted
        .map(str::to_string)
        .or_else(|| envs.contains_key("dev").then(|| "dev".to_string()))
        .or_else(|| envs.keys().next().cloned())
        .unwrap_or_else(|| "default".into());
    let mut out = HashMap::new();
    to_string_map(Some(&Value::Object(vars.clone())), &mut out);
    to_string_map(envs.get(&name), &mut out);
    (name, out)
}

pub fn load_captured(root: &Path, env: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(state) = read_json(&root.join(STATE_FILE)) {
        to_string_map(state.get(env), &mut out);
    }
    out
}

pub fn save_captured(root: &Path, env: &str, values: &[(String, String)]) -> std::io::Result<()> {
    let path = root.join(STATE_FILE);
    let mut state = read_json(&path).unwrap_or_default();
    let entry = state
        .entry(env.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(obj) = entry.as_object_mut() {
        for (k, v) in values {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
    }
    ensure_state_dir(&path)?;
    std::fs::write(path, serde_json::to_string_pretty(&Value::Object(state))?)
}

fn ensure_state_dir(file: &Path) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(ignore, "*\n")?;
        }
    }
    Ok(())
}

/// Env picked via the `env` code lens; overrides the `env` setting until that setting changes.
pub fn load_selected(root: &Path) -> Option<String> {
    let s = std::fs::read_to_string(root.join(SELECTED_ENV_FILE)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn save_selected(root: &Path, env: Option<&str>) -> std::io::Result<()> {
    let path = root.join(SELECTED_ENV_FILE);
    match env {
        Some(e) => {
            ensure_state_dir(&path)?;
            std::fs::write(path, format!("{e}\n"))
        }
        None => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// `data.items.0.id` style lookup.
pub fn json_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim().trim_start_matches("$.").trim_start_matches('$');
    path.split('.')
        .filter(|s| !s.is_empty())
        .try_fold(v, |cur, seg| match cur {
            Value::Array(a) => seg.parse::<usize>().ok().and_then(|i| a.get(i)),
            Value::Object(o) => o.get(seg),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn layered() -> Ctx {
        Ctx {
            env_name: "dev".into(),
            file_vars: map(&[("a", "file")]),
            captured: map(&[("a", "captured"), ("b", "captured")]),
            env: map(&[("a", "env"), ("b", "env"), ("c", "env")]),
            spec_base: Some("https://spec".into()),
        }
    }

    #[test]
    fn precedence() {
        let c = layered();
        assert_eq!(c.raw("a"), Some("file"));
        assert_eq!(c.raw("b"), Some("captured"));
        assert_eq!(c.raw("c"), Some("env"));
        assert_eq!(c.raw("baseUrl"), Some("https://spec"));
        assert_eq!(c.raw("d"), None);
        let mut c = c;
        c.env.insert("baseUrl".into(), "https://env".into());
        assert_eq!(c.raw("baseUrl"), Some("https://env"));
        assert_eq!(c.missing("a"), None);
        assert_eq!(c.missing("zzz"), Some(Missing::Var));
    }

    #[test]
    fn names_report_the_winning_source() {
        let names = layered().names();
        assert_eq!(
            names,
            [
                ("a".to_string(), "file".to_string()),
                ("b".into(), "captured".into()),
                ("baseUrl".into(), "spec servers[0]".into()),
                ("c".into(), "env".into()),
            ]
        );
    }

    #[test]
    fn substitute_recursive_and_missing() {
        let c = Ctx { env: map(&[("host", "{{scheme}}://x"), ("scheme", "https"), ("self", "{{self}}")]), ..Ctx::default() };
        assert_eq!(c.substitute("{{ host }}/a").unwrap(), "https://x/a");
        assert_eq!(c.substitute("no vars {{").unwrap(), "no vars {{");
        assert_eq!(c.substitute("{{nope}} {{nope}} {{other}}").unwrap_err(), ["nope", "other"]);
        // Self-reference stops at the depth limit instead of recursing forever.
        assert_eq!(c.substitute("{{self}}").unwrap(), "{{self}}");
    }

    #[test]
    fn dynamic_vars() {
        let c = Ctx::default();
        let uuid = c.substitute("{{$uuid}}").unwrap();
        let parts: Vec<&str> = uuid.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')));
        assert!(uuid.chars().all(|ch| ch == '-' || ch.is_ascii_hexdigit()));
        assert!(c.substitute("{{$timestamp}}").unwrap().parse::<u64>().unwrap() > 1_700_000_000);
        assert!(c.substitute("{{$randomInt}}").unwrap().parse::<u64>().unwrap() < 1000);
        assert_eq!(c.substitute("{{$env.OPENAPI_LSP_TEST_SURELY_UNSET}}").unwrap_err(), ["$env.OPENAPI_LSP_TEST_SURELY_UNSET"]);
        let home = std::env::var("PATH").unwrap();
        assert_eq!(c.substitute("{{$env.PATH}}").unwrap(), home);
        assert_eq!(c.substitute("{{$processEnv PATH}}").unwrap(), home);
        assert!(c.substitute("{{$bogus}}").is_err());
    }

    #[test]
    fn random_uuid_alias() {
        let v = Ctx::default().substitute("{{$random.uuid}}").unwrap();
        assert_eq!(v.len(), 36);
    }

    #[test]
    fn missing_reasons() {
        let c = Ctx { env: map(&[("$custom", "x")]), ..Ctx::default() };
        assert_eq!(c.missing("$uuid"), None);
        assert_eq!(c.missing("$randomInt 1 10"), None);
        assert_eq!(c.missing("$env.PATH"), None);
        assert_eq!(c.missing("$nope"), Some(Missing::UnknownDynamic));
        assert_eq!(c.missing("$env"), Some(Missing::UnknownDynamic));
        assert_eq!(c.missing("$env.OPENAPI_LSP_TEST_SURELY_UNSET"), Some(Missing::EnvUnset("OPENAPI_LSP_TEST_SURELY_UNSET".into())));
        // A `$` name never falls back to plain variables, matching `substitute`.
        assert_eq!(c.missing("$custom"), Some(Missing::UnknownDynamic));
        assert!(c.substitute("{{$custom}}").is_err());
        assert_eq!(Missing::Var.message("x", "dev"), "Variable `x` is not defined in env `dev`");
    }

    #[test]
    fn var_names_with_spans() {
        assert_eq!(
            var_names_in("a {{ x }} b {{y}} {{open"),
            [("x".to_string(), 2, 9), ("y".to_string(), 12, 17)]
        );
    }

    #[test]
    fn json_paths() {
        let v = json!({"data": {"items": [{"id": 1}, {"id": 2}]}});
        assert_eq!(json_path(&v, "data.items.1.id"), Some(&json!(2)));
        assert_eq!(json_path(&v, "$.data.items.0.id"), Some(&json!(1)));
        assert_eq!(json_path(&v, "$"), Some(&v));
        assert_eq!(json_path(&v, "data.items.x"), None);
        assert_eq!(json_path(&v, "data.missing"), None);
    }

    #[test]
    fn load_env_picks_wanted_dev_or_first() {
        let vars = json!({"a": "global", "n": 1}).as_object().unwrap().clone();
        let envs = json!({"prod": {"a": "prod"}, "dev": {"a": "dev", "b": true}}).as_object().unwrap().clone();
        let (name, m) = load_env(None, &vars, &envs);
        assert_eq!(name, "dev");
        assert_eq!(m, map(&[("a", "dev"), ("b", "true"), ("n", "1")]));
        let (name, m) = load_env(Some("prod"), &vars, &envs);
        assert_eq!((name.as_str(), m["a"].as_str()), ("prod", "prod"));
        let only = json!({"stage": {}}).as_object().unwrap().clone();
        assert_eq!(load_env(None, &vars, &only).0, "stage");
        assert_eq!(load_env(None, &vars, &Map::new()).0, "default");
    }

    #[test]
    fn captured_state_is_per_env() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(load_captured(root, "dev").is_empty());
        save_captured(root, "dev", &[("token".into(), "1".into())]).unwrap();
        save_captured(root, "dev", &[("user".into(), "u".into())]).unwrap();
        save_captured(root, "prod", &[("token".into(), "2".into())]).unwrap();
        assert_eq!(load_captured(root, "dev"), map(&[("token", "1"), ("user", "u")]));
        assert_eq!(load_captured(root, "prod"), map(&[("token", "2")]));
        assert_eq!(std::fs::read_to_string(root.join(".openapi/.gitignore")).unwrap(), "*\n");
    }

    #[test]
    fn selected_env_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(load_selected(root), None);
        save_selected(root, Some("prod")).unwrap();
        assert_eq!(load_selected(root).as_deref(), Some("prod"));
        save_selected(root, None).unwrap();
        assert_eq!(load_selected(root), None);
        save_selected(root, None).unwrap();
        std::fs::write(root.join(SELECTED_ENV_FILE), "  \n").unwrap();
        assert_eq!(load_selected(root), None);
    }
}
