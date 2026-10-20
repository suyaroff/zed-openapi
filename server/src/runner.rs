use crate::env::{self, Ctx};
use crate::http_parser::Request;
use crate::spec::Spec;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Prepared {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub captures: Vec<(String, String)>,
    pub stem: String,
}

pub struct Outcome {
    pub status: u16,
    pub reason: String,
    pub elapsed: Duration,
    pub response_path: PathBuf,
    pub captured: Vec<(String, String)>,
}

pub struct RunOptions {
    pub timeout: Duration,
    pub default_accept: Option<String>,
    pub insecure: bool,
}

fn stem_for(req: &Request) -> String {
    let raw = match &req.name {
        Some(n) => n.clone(),
        None => {
            let segs = crate::spec::url_segments(&req.url);
            let path: Vec<&str> = segs
                .iter()
                .filter(|s| !s.starts_with("{{"))
                .map(String::as_str)
                .collect();
            format!("{}_{}", req.method, path.join("_"))
        }
    };
    let mut s: String = raw
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    s.truncate(80);
    s
}

fn read_include(base_dir: &Path, rel: &str) -> Result<Vec<u8>, String> {
    let p = base_dir.join(rel.trim());
    std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))
}

fn build_body(text: &str, content_type: Option<&str>, base_dir: &Path) -> Result<Vec<u8>, String> {
    let multipart = content_type.is_some_and(|c| c.starts_with("multipart/"));
    let trimmed = text.trim();
    if !multipart {
        if let Some(rel) = trimmed.strip_prefix("< ") {
            if !rel.contains('\n') {
                return read_include(base_dir, rel);
            }
        }
        return Ok(text.as_bytes().to_vec());
    }
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        match line.strip_prefix("< ") {
            Some(rel) => out.extend(read_include(base_dir, rel)?),
            None => out.extend_from_slice(line.as_bytes()),
        }
        out.extend_from_slice(b"\r\n");
    }
    Ok(out)
}

pub fn prepare(
    req: &Request,
    ctx: &Ctx,
    spec: Option<&Spec>,
    base_dir: &Path,
    opts: &RunOptions,
) -> Result<Prepared, String> {
    let undefined = |names: Vec<String>| format!("undefined variable(s): {}", names.join(", "));
    let mut url = ctx.substitute(&req.url).map_err(undefined)?;
    if !url.contains("://") {
        let base = ctx
            .raw("baseUrl")
            .map(|b| ctx.substitute(b).unwrap_or_else(|_| b.to_string()))
            .ok_or("relative url and no `baseUrl` in env or spec servers")?;
        url = format!("{}/{}", base.trim_end_matches('/'), url.trim_start_matches('/'));
    }

    let mut headers = Vec::new();
    for h in &req.headers {
        headers.push((
            ctx.substitute(&h.name).map_err(undefined)?,
            ctx.substitute(&h.value).map_err(undefined)?,
        ));
    }
    let has = |hs: &[(String, String)], n: &str| hs.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));

    let op = spec.and_then(|s| s.find_exact(&req.method, &req.url).map(|op| (s, op)));
    if !has(&headers, "authorization") {
        let wants_auth = op.is_none_or(|(s, op)| s.needs_bearer(op));
        if let Some(token) = ctx.raw("token").filter(|t| !t.is_empty() && wants_auth) {
            let token = ctx.substitute(token).map_err(undefined)?;
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
    }
    if let Some(accept) = &opts.default_accept {
        if !has(&headers, "accept") {
            headers.push(("Accept".into(), accept.clone()));
        }
    }

    let body = match &req.body {
        Some(b) => {
            let text = ctx.substitute(&b.text).map_err(undefined)?;
            let ct = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.clone());
            if ct.is_none() {
                let t = text.trim_start();
                if t.starts_with('{') || t.starts_with('[') {
                    headers.push(("Content-Type".into(), "application/json".into()));
                }
            }
            Some(build_body(&text, ct.as_deref(), base_dir)?)
        }
        None => None,
    };

    Ok(Prepared {
        method: req.method.clone(),
        url,
        headers,
        body,
        captures: req.captures.clone(),
        stem: stem_for(req),
    })
}

struct RawResponse {
    status: u16,
    reason: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    redirects: Vec<String>,
}

fn agent(opts: &RunOptions) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .timeout_global(Some(opts.timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .disable_verification(opts.insecure)
                .build(),
        )
        .build()
        .into()
}

fn send_once(
    agent: &ureq::Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    macro_rules! with_headers {
        ($b:expr) => {{
            let mut b = $b;
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b
        }};
    }
    let result = match method {
        "POST" => with_headers!(agent.post(url)).send(body.unwrap_or_default()),
        "PUT" => with_headers!(agent.put(url)).send(body.unwrap_or_default()),
        "PATCH" => with_headers!(agent.patch(url)).send(body.unwrap_or_default()),
        m => {
            let b = match m {
                "GET" => agent.get(url),
                "DELETE" => agent.delete(url),
                "HEAD" => agent.head(url),
                "OPTIONS" => agent.options(url),
                "TRACE" => agent.trace(url),
                "CONNECT" => agent.connect(url),
                other => return Err(format!("unsupported method {other}")),
            };
            let b = with_headers!(b);
            match body {
                Some(bytes) => b.force_send_body().send(bytes),
                None => b.call(),
            }
        }
    };
    result.map_err(|e| e.to_string())
}

fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

fn resolve_location(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(path) = location.strip_prefix('/') {
        return format!("{scheme}://{host}/{path}");
    }
    let dir = base.rsplit_once('/').map_or(base, |(d, _)| d);
    format!("{dir}/{location}")
}

/// Follows redirects manually: unlike ureq/curl defaults, a same-host redirect
/// (typically nginx http -> https) keeps the method, body and Authorization.
fn execute(p: &Prepared, opts: &RunOptions) -> Result<RawResponse, String> {
    let agent = agent(opts);
    let mut method = p.method.clone();
    let mut url = p.url.clone();
    let mut headers = p.headers.clone();
    let mut body = p.body.clone();
    let mut redirects = Vec::new();
    let mut resp = loop {
        let resp = send_once(&agent, &method, &url, &headers, body.as_deref())?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(location) = location.filter(|_| matches!(status, 301 | 302 | 303 | 307 | 308)) else {
            break resp;
        };
        if redirects.len() >= 10 {
            return Err("too many redirects".into());
        }
        let next = resolve_location(&url, &location);
        let same_host = host_of(&next).eq_ignore_ascii_case(host_of(&url));
        let keep_method = matches!(status, 307 | 308) || (status != 303 && same_host);
        if !keep_method && method != "HEAD" {
            method = "GET".into();
            body = None;
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
        }
        if !same_host {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("cookie"));
        }
        redirects.push(format!("{status} {url} -> {next}"));
        url = next;
    };
    let status = resp.status();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let version = format!("{:?}", resp.version());
    let body = resp
        .body_mut()
        .with_config()
        .limit(200 * 1024 * 1024)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok(RawResponse {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or("").to_string(),
        version,
        headers,
        body,
        redirects,
    })
}

fn is_textual(content_type: &str, body: &[u8]) -> bool {
    let ct = content_type.to_ascii_lowercase();
    if ct.is_empty() {
        return std::str::from_utf8(body).is_ok();
    }
    ["json", "text/", "xml", "javascript", "html", "x-www-form-urlencoded", "yaml"]
        .iter()
        .any(|t| ct.contains(t))
}

fn extension_for(content_type: &str) -> &'static str {
    let ct = content_type.to_ascii_lowercase();
    match () {
        _ if ct.contains("pdf") => "pdf",
        _ if ct.contains("spreadsheetml") => "xlsx",
        _ if ct.contains("wordprocessingml") => "docx",
        _ if ct.contains("zip") => "zip",
        _ if ct.contains("png") => "png",
        _ if ct.contains("jpeg") => "jpg",
        _ => "bin",
    }
}

fn now_string() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = secs / 86400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // civil-from-days (Howard Hinnant)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

pub fn run(
    p: &Prepared,
    opts: &RunOptions,
    responses_dir: &Path,
    root: &Path,
    env_name: &str,
) -> Result<Outcome, String> {
    let started = Instant::now();
    let resp = execute(p, opts)?;
    let elapsed = started.elapsed();

    std::fs::create_dir_all(responses_dir).map_err(|e| e.to_string())?;
    let ignore = responses_dir.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(ignore, "*\n");
    }
    let ct = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");

    let mut out = String::new();
    let _ = writeln!(out, "# {} {}", p.method, p.url);
    let _ = writeln!(
        out,
        "# {} {} · {} ms · {} · {}",
        resp.status,
        resp.reason,
        elapsed.as_millis(),
        human_size(resp.body.len()),
        now_string()
    );
    for r in &resp.redirects {
        let _ = writeln!(out, "# redirect: {r}");
    }

    let parsed: Option<Value> = if ct.contains("json") || ct.is_empty() {
        serde_json::from_slice(&resp.body).ok()
    } else {
        None
    };

    let mut captured = Vec::new();
    if (200..300).contains(&resp.status) {
        if let Some(json) = &parsed {
            for (var, path) in &p.captures {
                if let Some(v) = env::json_path(json, path) {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    captured.push((var.clone(), s));
                }
            }
        }
    }
    if !captured.is_empty() {
        env::save_captured(root, env_name, &captured).map_err(|e| e.to_string())?;
        let names: Vec<&str> = captured.iter().map(|(k, _)| k.as_str()).collect();
        let _ = writeln!(out, "# captured: {}", names.join(", "));
    }
    let missed: Vec<&str> = p
        .captures
        .iter()
        .filter(|(k, _)| !captured.iter().any(|(c, _)| c == k))
        .map(|(k, _)| k.as_str())
        .collect();
    if !missed.is_empty() {
        let _ = writeln!(out, "# not captured: {}", missed.join(", "));
    }

    let version = resp.version.replace("HTTP_", "HTTP/").replace('_', ".");
    let _ = writeln!(out, "{version} {} {}", resp.status, resp.reason);
    for (k, v) in &resp.headers {
        let _ = writeln!(out, "{k}: {v}");
    }
    out.push('\n');

    if let Some(json) = &parsed {
        out.push_str(&serde_json::to_string_pretty(json).unwrap_or_default());
        out.push('\n');
    } else if is_textual(ct, &resp.body) {
        out.push_str(&String::from_utf8_lossy(&resp.body));
        out.push('\n');
    } else if !resp.body.is_empty() {
        let file = format!("{}.{}", p.stem, extension_for(ct));
        std::fs::write(responses_dir.join(&file), &resp.body).map_err(|e| e.to_string())?;
        let _ = writeln!(out, "# binary body saved to ./{file}");
    }

    let response_path = responses_dir.join(format!("{}.http", p.stem));
    std::fs::write(&response_path, out).map_err(|e| e.to_string())?;
    Ok(Outcome {
        status: resp.status,
        reason: resp.reason,
        elapsed,
        response_path,
        captured,
    })
}

fn human_size(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / 1024.0 / 1024.0),
    }
}
