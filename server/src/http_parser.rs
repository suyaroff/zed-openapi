use std::time::Duration;

pub const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];

#[derive(Debug, Clone)]
pub struct Header {
    pub name: String,
    pub value: String,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Body {
    pub text: String,
    pub start_line: usize,
}

/// Per-request `# @…` directives (JetBrains-compatible names where they exist).
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub timeout: Option<Duration>,
    pub no_redirect: bool,
    pub insecure: bool,
    pub no_auth: bool,
    pub no_cookie_jar: bool,
    /// `# @auth login`: name of the request that refreshes the token.
    pub auth: Option<String>,
    /// `# @expect …` lines: (line, expression).
    pub expects: Vec<(usize, String)>,
}

pub const DIRECTIVES: &[(&str, &str)] = &[
    ("name", "request name: response file name and `@auth` target"),
    ("capture", "`var = json.path`: save a response field after a 2xx response"),
    ("spec", "OpenAPI spec for this file"),
    ("expect", "assertion: `status 201`, `data.id exists`, `data.items.length > 0`, `time < 500`"),
    ("auth", "request that logs in: run first if its captures are empty, and again on 401"),
    ("no-auth", "don't add `Authorization` and don't run `@auth`"),
    ("timeout", "timeout for this request: `120`, `500ms`, `2m`"),
    ("no-redirect", "don't follow redirects"),
    ("insecure", "don't verify the TLS certificate"),
    ("no-cookie-jar", "don't send or store cookies"),
];

fn parse_duration(v: &str) -> Option<Duration> {
    let v = v.trim();
    let split = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let n: u64 = v[..split].parse().ok()?;
    match v[split..].trim() {
        "" | "s" => Some(Duration::from_secs(n)),
        "ms" => Some(Duration::from_millis(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub block_start: usize,
    pub end_line: usize,
    pub name: Option<String>,
    pub captures: Vec<(String, String)>,
    pub method: String,
    pub method_explicit: bool,
    pub line: usize,
    pub url: String,
    pub url_start: usize,
    pub url_end: usize,
    pub headers: Vec<Header>,
    /// First line after the header block (the blank separator), if any.
    pub headers_end: usize,
    pub body: Option<Body>,
    pub opts: Options,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    pub fn contains_line(&self, line: usize) -> bool {
        (self.block_start..=self.end_line).contains(&line)
    }

    pub fn key(&self) -> String {
        match &self.name {
            Some(n) => format!("@{n}"),
            None => format!("{} {}", self.method, self.url),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Var {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Default, Clone)]
pub struct HttpFile {
    pub vars: Vec<Var>,
    pub requests: Vec<Request>,
    pub spec: Option<String>,
    /// `# @auth` in a block without a request: default for every request in the file.
    pub auth: Option<String>,
}

impl HttpFile {
    pub fn request_at(&self, line: usize) -> Option<&Request> {
        self.requests.iter().find(|r| r.contains_line(line))
    }

    /// The `@auth` request for `req`, unless disabled or `req` is that request itself.
    pub fn auth_for<'a>(&'a self, req: &'a Request) -> Option<&'a str> {
        if req.opts.no_auth {
            return None;
        }
        let name = req.opts.auth.as_deref().or(self.auth.as_deref())?;
        (req.name.as_deref() != Some(name)).then_some(name)
    }

    pub fn named(&self, name: &str) -> Option<&Request> {
        self.requests.iter().find(|r| r.name.as_deref() == Some(name))
    }
}

fn comment_text(trimmed: &str) -> Option<&str> {
    trimmed
        .strip_prefix('#')
        .or_else(|| trimmed.strip_prefix("//"))
        .map(str::trim)
}

pub fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with('#') || trimmed.starts_with("//")
}

pub fn parse_var(trimmed: &str) -> Option<Var> {
    let rest = trimmed.strip_prefix('@')?;
    let (name, value) = rest.split_once('=')?;
    let name = name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    Some(Var {
        name: name.to_string(),
        value: value.trim().to_string(),
    })
}

/// Splits `METHOD url [HTTP/x]` into (method, explicit, url, url_start, url_end) byte offsets.
pub fn parse_request_line(line: &str) -> (String, bool, String, usize, usize) {
    let lead = line.len() - line.trim_start().len();
    let rest = &line[lead..];
    let (first, after) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (method, explicit, url_from) = if METHODS.contains(&first) {
        let ws = after.len() - after.trim_start().len();
        (first.to_string(), true, lead + first.len() + ws)
    } else {
        ("GET".to_string(), false, lead)
    };
    let mut url = line[url_from..].trim_end();
    if let Some(i) = url.rfind(" HTTP/") {
        url = url[..i].trim_end();
    }
    (method, explicit, url.to_string(), url_from, url_from + url.len())
}

pub fn parse(text: &str) -> HttpFile {
    let lines = crate::util::split_lines(text);
    let mut file = HttpFile::default();

    let mut starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with("###"))
        .map(|(i, _)| i)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }

    for (bi, &start) in starts.iter().enumerate() {
        let end = starts
            .get(bi + 1)
            .map_or(lines.len().saturating_sub(1), |n| n.saturating_sub(1));
        if let Some(req) = parse_block(&lines, start, end, &mut file) {
            file.requests.push(req);
        }
    }
    file
}

fn parse_block(lines: &[&str], start: usize, end: usize, file: &mut HttpFile) -> Option<Request> {
    let mut name = None;
    let mut captures = Vec::new();
    let mut opts = Options::default();
    let mut i = start;

    if lines.get(start).is_some_and(|l| l.trim_start().starts_with("###")) {
        i += 1;
    }

    let req_line = loop {
        if i > end || i >= lines.len() {
            if let Some(auth) = opts.auth {
                file.auth.get_or_insert(auth);
            }
            return None;
        }
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
        } else if let Some(c) = comment_text(trimmed) {
            if let Some(d) = c.strip_prefix('@') {
                let (key, val) = match d.find(|ch: char| ch.is_whitespace() || ch == '=') {
                    Some(p) => (&d[..p], d[p..].trim_start_matches([' ', '\t', '=']).trim()),
                    None => (d, ""),
                };
                match key {
                    "name" if !val.is_empty() => name = Some(val.to_string()),
                    "capture" => {
                        if let Some((var, path)) = val.split_once('=') {
                            captures.push((var.trim().to_string(), path.trim().to_string()));
                        }
                    }
                    "spec" if !val.is_empty() => file.spec = Some(val.to_string()),
                    "auth" if !val.is_empty() => opts.auth = Some(val.to_string()),
                    "timeout" => opts.timeout = parse_duration(val),
                    "no-redirect" => opts.no_redirect = true,
                    "insecure" => opts.insecure = true,
                    "no-auth" => opts.no_auth = true,
                    "no-cookie-jar" => opts.no_cookie_jar = true,
                    "expect" if !val.is_empty() => opts.expects.push((i, val.to_string())),
                    _ => {}
                }
            }
        } else if let Some(v) = parse_var(trimmed) {
            file.vars.push(v);
        } else if trimmed.starts_with("HTTP/") {
            return None;
        } else {
            break i;
        }
        i += 1;
    };

    let (method, method_explicit, mut url, url_start, url_end) = parse_request_line(lines[req_line]);
    i = req_line + 1;
    while i <= end && i < lines.len() {
        let l = lines[i];
        let t = l.trim_start();
        if l.len() > t.len() && (t.starts_with('?') || t.starts_with('&')) {
            url.push_str(t.trim_end());
            i += 1;
        } else {
            break;
        }
    }

    let mut headers = Vec::new();
    let mut headers_end = end + 1;
    while i <= end && i < lines.len() {
        let t = lines[i].trim();
        if t.is_empty() {
            headers_end = i;
            i += 1;
            break;
        }
        if is_comment(t) {
            i += 1;
            continue;
        }
        match t.split_once(':') {
            Some((n, v)) if !n.trim().is_empty() && !n.contains(' ') => {
                headers.push(Header {
                    name: n.trim().to_string(),
                    value: v.trim().to_string(),
                    line: i,
                });
                i += 1;
            }
            _ => {
                headers_end = i;
                break;
            }
        }
    }

    let mut body = None;
    if i <= end && i < lines.len() {
        let mut last = end.min(lines.len() - 1);
        while last >= i {
            let t = lines[last].trim();
            if t.is_empty() || t.starts_with("> ") || t.starts_with(">> ") || t.starts_with(">>! ") {
                if last == 0 {
                    break;
                }
                last -= 1;
            } else {
                break;
            }
        }
        if last >= i && !lines[i..=last].iter().all(|l| l.trim().is_empty()) {
            body = Some(Body {
                text: lines[i..=last].join("\n"),
                start_line: i,
            });
        }
    }

    Some(Request {
        block_start: start,
        end_line: end,
        name,
        captures,
        method,
        method_explicit,
        line: req_line,
        url,
        url_start,
        url_end,
        headers,
        headers_end,
        body,
        opts,
    })
}
