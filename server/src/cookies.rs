//! Per-env cookie jar persisted in `.openapi/cookies.json`.
//! Own implementation instead of ureq's store so the `Cookie` header is visible in the
//! saved request, Preview and curl export.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const FILE: &str = ".openapi/cookies.json";

/// Serializes read-modify-write of the file across send threads.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    /// Unix seconds; `None` is a session cookie (kept until cleared).
    expires: Option<u64>,
}

#[derive(Default, Debug)]
pub struct Jar {
    cookies: Vec<Cookie>,
    pub changed: bool,
    /// (url, Set-Cookie) received since `load`, replayed onto the current file in `save`
    /// so concurrent requests don't overwrite each other's cookies.
    received: Vec<(String, String)>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn read_all(root: &Path) -> Map<String, Value> {
    std::fs::read_to_string(root.join(FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// (https, host, path) of an absolute url.
fn parts(url: &str) -> (bool, String, String) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host.starts_with('[') {
        host.split_inclusive(']').next().unwrap_or(host)
    } else {
        host.split(':').next().unwrap_or(host)
    };
    let path = rest[end..].split(['?', '#']).next().unwrap_or("");
    let path = if path.is_empty() { "/" } else { path };
    (scheme.eq_ignore_ascii_case("https"), host.to_ascii_lowercase(), path.to_string())
}

fn domain_match(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
}

fn path_match(req: &str, cookie: &str) -> bool {
    req == cookie
        || (req.starts_with(cookie) && (cookie.ends_with('/') || req[cookie.len()..].starts_with('/')))
}

impl Jar {
    pub fn load(root: &Path, env: &str) -> Jar {
        let cookies: Vec<Cookie> = read_all(root)
            .get(env)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let t = now();
        Jar {
            cookies: cookies.into_iter().filter(|c| c.expires.is_none_or(|e| e > t)).collect(),
            ..Jar::default()
        }
    }

    pub fn save(&self, root: &Path, env: &str) -> std::io::Result<()> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut current = Jar::load(root, env);
        for (url, set_cookie) in &self.received {
            current.store(url, set_cookie);
        }
        write(root, env, &current.cookies)
    }

    pub fn clear(root: &Path, env: &str) -> std::io::Result<()> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        write(root, env, &[])
    }


    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn header_for(&self, url: &str) -> Option<String> {
        let (https, host, path) = parts(url);
        let t = now();
        let mut matching: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| {
                (if c.host_only { host == c.domain } else { domain_match(&host, &c.domain) })
                    && path_match(&path, &c.path)
                    && (https || !c.secure)
                    && c.expires.is_none_or(|e| e > t)
            })
            .collect();
        // RFC 6265: longer paths first.
        matching.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        let pairs: Vec<String> = matching.iter().map(|c| format!("{}={}", c.name, c.value)).collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    pub fn store(&mut self, url: &str, set_cookie: &str) {
        self.received.push((url.to_string(), set_cookie.to_string()));
        let (_, host, req_path) = parts(url);
        let mut attrs = set_cookie.split(';');
        let Some((name, value)) = attrs.next().and_then(|nv| nv.split_once('=')) else { return };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() {
            return;
        }
        let mut domain = None;
        let mut path = None;
        let mut secure = false;
        let mut max_age: Option<i64> = None;
        let mut expires = None;
        for a in attrs {
            let (k, v) = a.split_once('=').map_or((a.trim(), ""), |(k, v)| (k.trim(), v.trim()));
            match k.to_ascii_lowercase().as_str() {
                "domain" if !v.is_empty() => domain = Some(v.trim_start_matches('.').to_ascii_lowercase()),
                "path" if v.starts_with('/') => path = Some(v.to_string()),
                "secure" => secure = true,
                "max-age" => max_age = v.parse().ok(),
                "expires" => expires = parse_http_date(v),
                _ => {}
            }
        }
        if domain.as_deref().is_some_and(|d| !domain_match(&host, d)) {
            return;
        }
        let path = path.unwrap_or_else(|| match req_path.rfind('/') {
            Some(0) | None => "/".into(),
            Some(i) => req_path[..i].to_string(),
        });
        let expires = match max_age {
            Some(s) if s <= 0 => Some(0),
            Some(s) => Some(now() + s as u64),
            None => expires,
        };
        let cookie = Cookie {
            name: name.to_string(),
            value: value.to_string(),
            host_only: domain.is_none(),
            domain: domain.unwrap_or(host),
            path,
            secure,
            expires,
        };
        self.cookies
            .retain(|c| !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path));
        if cookie.expires.is_none_or(|e| e > now()) {
            self.cookies.push(cookie);
        }
        self.changed = true;
    }
}

fn write(root: &Path, env: &str, cookies: &[Cookie]) -> std::io::Result<()> {
    let mut all = read_all(root);
    if cookies.is_empty() {
        all.remove(env);
    } else {
        all.insert(env.to_string(), serde_json::to_value(cookies).unwrap_or_default());
    }
    let path = root.join(FILE);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&Value::Object(all))?)
}

/// `Wdy, DD Mon YYYY HH:MM:SS GMT` and the `DD-Mon-YY` variant, as Unix seconds.
fn parse_http_date(s: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let (mut day, mut month, mut year, mut time) = (None, None, None, None);
    for tok in s.split([' ', '-', ',']).filter(|t| !t.is_empty()) {
        let lower = tok.to_ascii_lowercase();
        if tok.contains(':') {
            let mut it = tok.split(':').map(|x| x.parse::<u64>().ok());
            time = Some((it.next()??, it.next()??, it.next().flatten().unwrap_or(0)));
        } else if let Some(m) = MONTHS.iter().position(|m| lower.starts_with(m)) {
            month = Some(m as i64 + 1);
        } else if let Ok(n) = tok.parse::<i64>() {
            if day.is_none() && tok.len() <= 2 {
                day = Some(n);
            } else if year.is_none() && !tok.starts_with('+') {
                year = Some(match n {
                    0..=69 => n + 2000,
                    70..=99 => n + 1900,
                    _ => n,
                });
            }
        }
    }
    let (d, m, y, (hh, mm, ss)) = (day?, month?, year?, time?);
    // days-from-civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok().map(|days| days * 86400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jar(url: &str, set: &[&str]) -> Jar {
        let mut j = Jar::default();
        for s in set {
            j.store(url, s);
        }
        j
    }

    #[test]
    fn host_only_and_default_path() {
        let j = jar("http://a.example.com/x/y", &["sid=1"]);
        assert!(j.changed);
        assert_eq!(j.header_for("http://a.example.com/x/z").as_deref(), Some("sid=1"));
        assert_eq!(j.header_for("http://a.example.com/x").as_deref(), Some("sid=1"));
        assert_eq!(j.header_for("http://a.example.com/xy"), None);
        assert_eq!(j.header_for("http://a.example.com/"), None);
        assert_eq!(j.header_for("http://b.a.example.com/x/z"), None);
    }

    #[test]
    fn domain_cookies_cover_subdomains() {
        let j = jar("http://a.example.com/", &["d=1; Domain=.Example.com; Path=/"]);
        assert_eq!(j.header_for("http://example.com/").as_deref(), Some("d=1"));
        assert_eq!(j.header_for("http://b.example.com/q").as_deref(), Some("d=1"));
        assert_eq!(j.header_for("http://badexample.com/"), None);
        let j = jar("http://a.example.com/", &["d=1; Domain=other.com"]);
        assert_eq!(j.len(), 0);
    }

    #[test]
    fn secure_only_over_https() {
        let j = jar("https://h/", &["s=1; Secure; Path=/"]);
        assert_eq!(j.header_for("https://h/a").as_deref(), Some("s=1"));
        assert_eq!(j.header_for("http://h/a"), None);
    }

    #[test]
    fn longer_paths_first() {
        let j = jar("http://h/", &["a=1; Path=/", "b=2; Path=/x/"]);
        assert_eq!(j.header_for("http://h/x/y").as_deref(), Some("b=2; a=1"));
        assert_eq!(j.header_for("http://h/z").as_deref(), Some("a=1"));
    }

    #[test]
    fn replace_and_expire() {
        let mut j = jar("http://h/", &["a=1; Path=/", "a=2; Path=/"]);
        assert_eq!(j.header_for("http://h/").as_deref(), Some("a=2"));
        j.store("http://h/", "a=; Max-Age=0; Path=/");
        assert_eq!(j.len(), 0);
        j.store("http://h/", "b=1; Path=/");
        j.store("http://h/", "b=1; Path=/; Expires=Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(j.len(), 0);
        j.store("http://h/", "c=1; Path=/; Max-Age=3600");
        assert_eq!(j.header_for("http://h/").as_deref(), Some("c=1"));
        j.store("http://h/", "=x");
        j.store("http://h/", "novalue");
        assert_eq!(j.len(), 1);
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480));
        assert_eq!(parse_http_date("Sun, 06-Nov-94 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("garbage"), None);
        assert_eq!(parse_http_date("21 Oct 2015"), None);
    }

    #[test]
    fn url_parts() {
        assert_eq!(parts("https://user:pw@Host.COM:8443/p?q#f"), (true, "host.com".into(), "/p".into()));
        assert_eq!(parts("http://[::1]:80"), (false, "[::1]".into(), "/".into()));
        assert_eq!(parts("x.com/a"), (false, "x.com".into(), "/a".into()));
    }

    #[test]
    fn save_replays_onto_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut a = Jar::load(root, "dev");
        let mut b = Jar::load(root, "dev");
        a.store("http://h/", "a=1; Path=/");
        b.store("http://h/", "b=2; Path=/");
        a.save(root, "dev").unwrap();
        b.save(root, "dev").unwrap();
        let j = Jar::load(root, "dev");
        assert_eq!(j.len(), 2);
        assert!(!j.changed);
        assert_eq!(Jar::load(root, "prod").len(), 0);

        Jar::clear(root, "dev").unwrap();
        assert_eq!(Jar::load(root, "dev").len(), 0);
    }

    #[test]
    fn load_drops_expired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let old = Cookie {
            name: "old".into(),
            value: "1".into(),
            domain: "h".into(),
            host_only: true,
            path: "/".into(),
            secure: false,
            expires: Some(1),
        };
        let fresh = Cookie { name: "fresh".into(), expires: None, ..old.clone() };
        write(root, "dev", &[old, fresh]).unwrap();
        let j = Jar::load(root, "dev");
        assert_eq!(j.header_for("http://h/").as_deref(), Some("fresh=1"));
    }
}
