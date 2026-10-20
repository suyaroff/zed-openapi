use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_FILE: &str = ".openapi/state.json";

#[derive(Default)]
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

    pub fn is_defined(&self, name: &str) -> bool {
        is_dynamic(name) || self.raw(name).is_some()
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

fn is_dynamic(name: &str) -> bool {
    name.starts_with('$')
}

fn random_u64() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish()
}

fn dynamic(name: &str) -> Option<Option<String>> {
    if !is_dynamic(name) {
        return None;
    }
    let (head, arg) = match name.split_once(|c: char| c.is_whitespace() || c == '.') {
        Some((h, a)) => (h, a.trim()),
        None => (name, ""),
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    Some(match head {
        "$uuid" | "$random.uuid" | "$guid" => {
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
        "$timestamp" => Some(now.as_secs().to_string()),
        "$randomInt" => Some((random_u64() % 1000).to_string()),
        "$env" | "$processEnv" => std::env::var(arg).ok(),
        _ => None,
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(ignore, "*\n")?;
        }
    }
    std::fs::write(path, serde_json::to_string_pretty(&Value::Object(state))?)
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
