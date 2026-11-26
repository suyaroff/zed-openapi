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
    let n: u64 = v[..split].parse().ok().filter(|&n| n > 0)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_blocks_and_collects_vars() {
        let f = parse(
            "@host = http://x\n\n\nGET {{host}}/a\n\n### second\nPOST {{host}}/b\nContent-Type: application/json\n\n{\"a\": 1}\n",
        );
        assert_eq!(f.vars.len(), 1);
        assert_eq!((f.vars[0].name.as_str(), f.vars[0].value.as_str()), ("host", "http://x"));
        assert_eq!(f.requests.len(), 2);
        let (a, b) = (&f.requests[0], &f.requests[1]);
        assert_eq!((a.method.as_str(), a.url.as_str(), a.line), ("GET", "{{host}}/a", 3));
        assert_eq!((a.block_start, a.end_line), (0, 4));
        assert_eq!((b.block_start, b.line, b.end_line), (5, 6, 10));
        assert_eq!(b.header("content-type"), Some("application/json"));
        assert_eq!(b.headers[0].line, 7);
        assert_eq!(b.headers_end, 8);
        let body = b.body.as_ref().unwrap();
        assert_eq!((body.text.as_str(), body.start_line), ("{\"a\": 1}", 9));
    }

    #[test]
    fn parse_var_rejects_bad_names() {
        assert!(parse_var("@a b = 1").is_none());
        assert!(parse_var("@ = 1").is_none());
        assert!(parse_var("@a").is_none());
        assert_eq!(parse_var("@a=x = y").unwrap().value, "x = y");
    }

    #[test]
    fn request_line_parts() {
        assert_eq!(parse_request_line("  POST /a?b=1 HTTP/1.1"), ("POST".into(), true, "/a?b=1".into(), 7, 13));
        assert_eq!(parse_request_line("/a"), ("GET".into(), false, "/a".into(), 0, 2));
        assert_eq!(parse_request_line("get /a").0, "GET");
        assert!(!parse_request_line("get /a").1);
    }

    #[test]
    fn directives() {
        let f = parse(
            "# @name login\n// @capture token = data.token\n# @capture  bad\n# @timeout 500ms\n# @no-redirect\n# @insecure\n# @no-auth\n# @no-cookie-jar\n# @expect status 200\n# @expect\n# @unknown x\nPOST /login\n",
        );
        let r = &f.requests[0];
        assert_eq!(r.name.as_deref(), Some("login"));
        assert_eq!(r.captures, vec![("token".to_string(), "data.token".to_string())]);
        let o = &r.opts;
        assert_eq!(o.timeout, Some(Duration::from_millis(500)));
        assert!(o.no_redirect && o.insecure && o.no_auth && o.no_cookie_jar);
        assert_eq!(o.expects, vec![(8, "status 200".to_string())]);
        assert_eq!(r.key(), "@login");
        assert!(f.named("login").is_some());
        assert!(f.named("other").is_none());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("3s"), Some(Duration::from_secs(3)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration(" 5 ms "), Some(Duration::from_millis(5)));
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("1h"), None);
        assert_eq!(parse_duration("fast"), None);
    }

    #[test]
    fn spec_directive_is_file_level() {
        let f = parse("# @spec ./api.yaml\nGET /a\n");
        assert_eq!(f.spec.as_deref(), Some("./api.yaml"));
    }

    #[test]
    fn auth_defaults_and_overrides() {
        let f = parse(
            "# @auth login\n\n###\n# @name login\nPOST /login\n\n###\nGET /me\n\n###\n# @no-auth\nGET /public\n\n###\n# @auth other\nGET /x\n",
        );
        assert_eq!(f.auth.as_deref(), Some("login"));
        let by_url = |u: &str| f.requests.iter().find(|r| r.url == u).unwrap();
        assert_eq!(f.auth_for(by_url("/login")), None);
        assert_eq!(f.auth_for(by_url("/me")), Some("login"));
        assert_eq!(f.auth_for(by_url("/public")), None);
        assert_eq!(f.auth_for(by_url("/x")), Some("other"));
    }

    #[test]
    fn response_files_have_no_requests() {
        let f = parse("# GET http://x/a\nHTTP/1.1 200 OK\nContent-Type: application/json\n\n{}\n");
        assert!(f.requests.is_empty());
    }

    #[test]
    fn multiline_url_continuation() {
        let f = parse("GET /pets\n    ?limit=1\n    &status=sold\nAccept: */*\n");
        let r = &f.requests[0];
        assert_eq!(r.url, "/pets?limit=1&status=sold");
        assert_eq!((r.url_start, r.url_end), (4, 9));
        assert_eq!(r.headers[0].line, 3);
        // An unindented `?` line is not a continuation.
        let f = parse("GET /pets\n?limit=1\n");
        assert_eq!(f.requests[0].url, "/pets");
    }

    #[test]
    fn headers_end_without_blank_line() {
        let f = parse("POST /a\nX-A: 1\n<a>text</a>\n");
        let r = &f.requests[0];
        assert_eq!(r.headers.len(), 1);
        assert_eq!(r.headers_end, 2);
        assert_eq!(r.body.as_ref().unwrap().start_line, 2);
    }

    #[test]
    fn body_trims_trailing_blank_and_handler_lines() {
        let f = parse("POST /a\n\nhello\n\n> {% client.test() %}\n\n");
        assert_eq!(f.requests[0].body.as_ref().unwrap().text, "hello");
        let f = parse("POST /a\n\n\n\n");
        assert!(f.requests[0].body.is_none());
    }

    #[test]
    fn comments_between_headers_are_skipped() {
        let f = parse("GET /a\n# note\nX-A: 1\n// note\nX-B: 2\n");
        let names: Vec<&str> = f.requests[0].headers.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["X-A", "X-B"]);
    }

    #[test]
    fn request_lookup_by_line() {
        let f = parse("GET /a\n\n###\nGET /b\n");
        assert_eq!(f.request_at(1).unwrap().url, "/a");
        assert_eq!(f.request_at(2).unwrap().url, "/b");
        assert_eq!(f.request_at(3).unwrap().key(), "GET /b");
        assert!(is_comment("// x") && is_comment("# x") && !is_comment("x"));
    }
}
