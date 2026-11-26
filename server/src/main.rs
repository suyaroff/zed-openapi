mod completion;
mod cookies;
mod diagnostics;
mod env;
mod expect;
mod generate;
mod http_parser;
mod json_scan;
mod render;
mod response;
mod runner;
mod spec;
mod util;

use crossbeam_channel::Sender;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeConfiguration, DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument,
    DidSaveTextDocument, LogMessage, Notification as _, PublishDiagnostics, ShowMessage,
};
use lsp_types::request::{
    CodeActionRequest, CodeLensRefresh, CodeLensRequest, Completion, ExecuteCommand, GotoDefinition, HoverRequest,
    Request as _, ShowDocument, ShowMessageRequest, WorkspaceConfiguration,
};
use lsp_types::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::env::Ctx;
use crate::http_parser::HttpFile;
use crate::spec::{Match, PathElem, Spec};
use crate::util::{lsp_to_byte, path_to_uri, split_lines, uri_to_path};

const CMD_SEND: &str = "openapi.send";
const CMD_OPEN: &str = "openapi.openResponse";
const CMD_PREVIEW: &str = "openapi.preview";
const CMD_CURL: &str = "openapi.curl";
const CMD_ENV: &str = "openapi.selectEnv";
const CMD_CLEAR_COOKIES: &str = "openapi.clearCookies";
const CMD_RUN_ALL: &str = "openapi.runAll";
const CMD_DIFF: &str = "openapi.diffResponse";
/// `runs` key of the file-level Run all result.
const RUN_ALL_KEY: &str = "\0run-all";

/// Everything needed to prepare and send one request, so it can be (re)prepared off the main
/// thread, e.g. after `@auth` captured a fresh token.
#[derive(Clone)]
struct Job {
    uri: Uri,
    req: http_parser::Request,
    ctx: Ctx,
    spec: Option<Arc<Spec>>,
    spec_path: Option<PathBuf>,
    base_dir: PathBuf,
    root: PathBuf,
    opts: runner::RunOptions,
    cookies: bool,
}

impl Job {
    fn key(&self) -> (Uri, String) {
        (self.uri.clone(), self.req.key())
    }

    fn label(&self) -> String {
        self.req.name.clone().unwrap_or_else(|| format!("{} {}", self.req.method, self.req.url))
    }

    fn jar(&self) -> Option<cookies::Jar> {
        self.cookies.then(|| cookies::Jar::load(&self.root, &self.ctx.env_name))
    }

    fn prepare(&self, jar: Option<&cookies::Jar>) -> Result<runner::Prepared, String> {
        let mut p = runner::prepare(&self.req, &self.ctx, self.spec.as_deref(), &self.base_dir, &self.opts)?;
        p.spec_path = self.spec_path.clone();
        if let Some(jar) = jar {
            runner::apply_cookies(&mut p, jar);
        }
        Ok(p)
    }

    fn reload_captured(&mut self) {
        self.ctx.captured = env::load_captured(&self.root, &self.ctx.env_name);
    }

    fn missing_captures(&self, login: &Job) -> bool {
        login.req.captures.iter().any(|(var, _)| self.ctx.raw(var).is_none_or(str::is_empty))
    }
}

/// Main-thread state the send thread needs: lenses, opening responses.
#[derive(Clone)]
struct Bg {
    sender: Sender<Message>,
    runs: Arc<Mutex<HashMap<(Uri, String), LastRun>>>,
    pending: Arc<Mutex<HashMap<RequestId, PathBuf>>>,
    next_id: Arc<AtomicU64>,
    focus: bool,
    /// Open each response in the editor (off for Run all).
    open: bool,
    responses_dir: String,
    /// Asks the main loop to republish diagnostics for a document.
    republish: Sender<Uri>,
}

impl Bg {
    fn set_lens(&self, key: (Uri, String), run: LastRun) {
        self.runs.lock().unwrap().insert(key, run);
        request::<CodeLensRefresh>(&self.sender, &self.next_id, ());
    }

    fn exec(&self, job: &Job, note: Option<&str>) -> Result<runner::Outcome, String> {
        let mut jar = job.jar();
        let prepared = job.prepare(jar.as_ref()).map_err(|e| format!("Request not sent: {e}"))?;
        log(&self.sender, format!("{} {}", prepared.method, prepared.url));
        let dir = job.root.join(&self.responses_dir);
        let result = runner::run(&prepared, &job.opts, &dir, &job.root, &job.ctx.env_name, jar.as_mut());
        if let Some(jar) = jar.filter(|j| j.changed) {
            if let Err(e) = jar.save(&job.root, &job.ctx.env_name) {
                log(&self.sender, format!("cannot save cookies: {e}"));
            }
        }
        let o = match result {
            Ok(o) => o,
            Err(e) => {
                let e = format!("{} {}: {e}", prepared.method, prepared.url);
                self.set_lens(job.key(), LastRun::note(format!("✗ {}", util::one_line(&e, 60))));
                return Err(e);
            }
        };
        let mark = if o.status < 400 { "✓" } else { "✗" };
        let mut title = format!("{mark} {} {} · {} ms", o.status, o.reason, o.elapsed.as_millis());
        if let Some(n) = note {
            title.push_str(&format!(" · {n}"));
        }
        if !o.captured.is_empty() {
            let names: Vec<&str> = o.captured.iter().map(|(k, _)| k.as_str()).collect();
            title.push_str(&format!(" · captured {}", names.join(", ")));
        }
        if !o.checks.is_empty() {
            let failed = o.checks.iter().filter(|(_, f)| f.is_some()).count();
            if failed == 0 {
                title.push_str(&format!(" · ✓ {} expect", o.checks.len()));
            } else {
                title = title.replacen('✓', "✗", 1);
                title.push_str(&format!(" · ✗ {failed}/{} expect", o.checks.len()));
            }
        }
        let issues = job
            .spec
            .as_deref()
            .zip(std::fs::read_to_string(&o.response_path).ok())
            .map_or(0, |(spec, text)| response::warning_count(&text, spec));
        if issues > 0 {
            title.push_str(&format!(" · ⚠ {issues} spec"));
        }
        if self.open {
            let id = request::<ShowDocument>(
                &self.sender,
                &self.next_id,
                ShowDocumentParams {
                    uri: path_to_uri(&o.response_path),
                    external: Some(false),
                    take_focus: Some(self.focus),
                    selection: None,
                },
            );
            self.pending.lock().unwrap().insert(id, o.response_path.clone());
        }
        let run = LastRun { title, path: Some(o.response_path.clone()), checks: o.checks.clone() };
        self.set_lens(job.key(), run);
        let _ = self.republish.send(job.uri.clone());
        Ok(o)
    }

    fn login(&self, login: &Job) -> Result<(), String> {
        let o = self.exec(login, None)?;
        if !(200..300).contains(&o.status) {
            return Err(format!("@auth `{}` failed: {} {}", login.label(), o.status, o.reason));
        }
        if o.captured.is_empty() && !login.req.captures.is_empty() {
            return Err(format!("@auth `{}` captured nothing", login.label()));
        }
        Ok(())
    }

    /// `@auth`: log in first when the login's captures are empty, and once more on 401.
    fn send(&self, mut job: Job, login: Option<Job>) -> Result<runner::Outcome, String> {
        let name = login.as_ref().map(Job::label);
        let mut relogged = false;
        if let Some(login) = login.as_ref().filter(|l| job.missing_captures(l)) {
            self.set_lens(job.key(), LastRun::note(format!("⏳ {}…", login.label())));
            self.login(login)?;
            job.reload_captured();
            relogged = true;
        }
        let note = name.as_deref().filter(|_| relogged).map(|n| format!("↻ {n}"));
        let o = self.exec(&job, note.as_deref())?;
        if let (401, false, Some(login)) = (o.status, relogged, login.as_ref()) {
            self.login(login)?;
            job.reload_captured();
            let note = format!("↻ {} after 401", login.label());
            return self.exec(&job, Some(&note));
        }
        Ok(o)
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    spec: Option<String>,
    env: Option<String>,
    vars: serde_json::Map<String, Value>,
    envs: serde_json::Map<String, Value>,
    responses_dir: String,
    timeout: u64,
    default_accept: Option<String>,
    focus_response: bool,
    insecure: bool,
    cookie_jar: bool,
    history_size: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            spec: None,
            env: None,
            vars: Default::default(),
            envs: Default::default(),
            responses_dir: ".openapi/responses".into(),
            timeout: 30,
            default_accept: Some("application/json".into()),
            focus_response: false,
            insecure: false,
            cookie_jar: true,
            history_size: 10,
        }
    }
}

impl Settings {
    fn from_value(v: &Value) -> Option<Self> {
        let v = v.get("openapi-lsp").unwrap_or(v);
        if v.is_null() {
            return None;
        }
        serde_json::from_value(v.clone()).ok()
    }
}

#[derive(Clone, Default)]
struct LastRun {
    title: String,
    path: Option<PathBuf>,
    /// (expression, failure) per `@expect`, shown as diagnostics on the request.
    checks: Vec<(String, Option<String>)>,
}

impl LastRun {
    fn note(title: String) -> Self {
        LastRun { title, ..Default::default() }
    }
}

struct SpecEntry {
    mtime: SystemTime,
    spec: Arc<Spec>,
}

struct Server {
    sender: Sender<Message>,
    root: Option<PathBuf>,
    settings: Settings,
    docs: HashMap<Uri, String>,
    specs: HashMap<PathBuf, SpecEntry>,
    spec_errors: HashMap<PathBuf, String>,
    discovered: Option<Option<PathBuf>>,
    runs: Arc<Mutex<HashMap<(Uri, String), LastRun>>>,
    pending_show: Arc<Mutex<HashMap<RequestId, PathBuf>>>,
    /// `window/showMessageRequest` env pickers awaiting an answer, keyed to the project root.
    pending_env: HashMap<RequestId, PathBuf>,
    next_id: Arc<AtomicU64>,
    republish: Sender<Uri>,
}

fn notify<N: lsp_types::notification::Notification>(sender: &Sender<Message>, params: N::Params) {
    let _ = sender.send(Message::Notification(Notification::new(N::METHOD.to_string(), params)));
}

fn request<R: lsp_types::request::Request>(
    sender: &Sender<Message>,
    next_id: &AtomicU64,
    params: R::Params,
) -> RequestId {
    let id = RequestId::from(format!("openapi-{}", next_id.fetch_add(1, Ordering::Relaxed)));
    let _ = sender.send(Message::Request(Request::new(id.clone(), R::METHOD.to_string(), params)));
    id
}

fn log(sender: &Sender<Message>, msg: impl Into<String>) {
    notify::<LogMessage>(sender, LogMessageParams { typ: MessageType::LOG, message: msg.into() });
}

fn show(sender: &Sender<Message>, typ: MessageType, msg: impl Into<String>) {
    notify::<ShowMessage>(sender, ShowMessageParams { typ, message: msg.into() });
}

const SKIP_DIRS: &[&str] = &["node_modules", "vendor", "target", "storage", "dist", "build", "bootstrap"];

fn walkable(e: &std::fs::DirEntry) -> bool {
    let n = e.file_name();
    let n = n.to_string_lossy();
    !n.starts_with('.') && !SKIP_DIRS.contains(&n.as_ref())
}

/// `.http` / `.rest` files under `root`, breadth-first, at most 6 levels deep.
fn http_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut queue = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = queue.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() && depth < 6 && walkable(&e) => queue.insert(0, (path, depth + 1)),
                Ok(t) if t.is_file() && matches!(path.extension().and_then(|x| x.to_str()), Some("http" | "rest")) => {
                    out.push(path)
                }
                _ => {}
            }
        }
    }
    out
}

fn discover_spec(root: &Path) -> Option<PathBuf> {
    const NAMES: &[&str] = &["openapi.json", "openapi.yaml", "openapi.yml", "swagger.json", "swagger.yaml"];
    let mut queue = vec![(root.to_path_buf(), 0)];
    let mut i = 0;
    while i < queue.len() {
        let (dir, depth) = queue[i].clone();
        i += 1;
        for name in NAMES {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        if depth >= 5 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut subdirs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(walkable)
            .map(|e| e.path())
            .collect();
        subdirs.sort();
        queue.extend(subdirs.into_iter().map(|d| (d, depth + 1)));
    }
    None
}

impl Server {
    fn root_for(&self, uri: &Uri) -> PathBuf {
        if let Some(r) = &self.root {
            return r.clone();
        }
        uri_to_path(uri)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn spec_path(&mut self, uri: &Uri, file: &HttpFile) -> Option<PathBuf> {
        if let Some(rel) = &file.spec {
            let dir = uri_to_path(uri)?.parent()?.to_path_buf();
            return Some(dir.join(rel));
        }
        let root = self.root_for(uri);
        if let Some(rel) = &self.settings.spec {
            return Some(root.join(rel));
        }
        if self.discovered.is_none() {
            let found = discover_spec(&root);
            if let Some(p) = &found {
                log(&self.sender, format!("using spec {}", p.display()));
            }
            self.discovered = Some(found);
        }
        self.discovered.clone().flatten()
    }

    fn spec_for(&mut self, uri: &Uri, file: &HttpFile) -> Option<Arc<Spec>> {
        let path = self.spec_path(uri, file)?;
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        if let Some(e) = self.specs.get(&path) {
            if e.mtime == mtime {
                return Some(e.spec.clone());
            }
        }
        match Spec::load(&path) {
            Ok(spec) => {
                self.spec_errors.remove(&path);
                let spec = Arc::new(spec);
                log(&self.sender, format!("loaded {} ({} operations)", path.display(), spec.ops.len()));
                self.specs.insert(path, SpecEntry { mtime, spec: spec.clone() });
                Some(spec)
            }
            Err(e) => {
                let msg = format!("OpenAPI: failed to load {}: {e:#}", path.display());
                if self.spec_errors.get(&path) != Some(&msg) {
                    show(&self.sender, MessageType::ERROR, &msg);
                    self.spec_errors.insert(path, msg);
                }
                None
            }
        }
    }

    fn ctx_for(&self, uri: &Uri, file: &HttpFile, spec: Option<&Spec>) -> Ctx {
        let root = self.root_for(uri);
        let (env_name, env) = env::load_env(
            self.wanted_env(&root).as_deref(),
            &self.settings.vars,
            &self.settings.envs,
        );
        Ctx {
            captured: env::load_captured(&root, &env_name),
            env_name,
            env,
            file_vars: file.vars.iter().map(|v| (v.name.clone(), v.value.clone())).collect(),
            spec_base: spec.and_then(Spec::base_url),
        }
    }

    fn wanted_env(&self, root: &Path) -> Option<String> {
        env::load_selected(root)
            .filter(|e| self.settings.envs.contains_key(e))
            .or_else(|| self.settings.env.clone())
    }

    fn env_name(&self, root: &Path) -> String {
        env::load_env(self.wanted_env(root).as_deref(), &Default::default(), &self.settings.envs).0
    }

    fn load(&mut self, uri: &Uri) -> Option<(String, HttpFile, Option<Arc<Spec>>)> {
        let text = self.docs.get(uri)?.clone();
        let file = http_parser::parse(&text);
        let spec = self.spec_for(uri, &file);
        Some((text, file, spec))
    }

    fn publish(&mut self, uri: &Uri) {
        let Some((text, file, spec)) = self.load(uri) else { return };
        let ctx = self.ctx_for(uri, &file, spec.as_deref());
        let mut diagnostics = diagnostics::compute(&text, &file, spec.as_deref(), &ctx);
        let lines = split_lines(&text);
        let runs = self.runs.lock().unwrap();
        for req in &file.requests {
            let Some(run) = runs.get(&(uri.clone(), req.key())) else { continue };
            // Checks follow `@expect` order; expressions may differ after `{{var}}` substitution.
            if run.checks.len() != req.opts.expects.len() {
                continue;
            }
            for ((line, e), (shown, failure)) in req.opts.expects.iter().zip(&run.checks) {
                if let Some(failure) = failure.as_ref().filter(|_| expect::parse(e).is_ok()) {
                    diagnostics.push(Diagnostic {
                        range: util::full_line(&lines, *line),
                        severity: Some(DiagnosticSeverity::WARNING),
                        source: Some("openapi".into()),
                        message: format!("expect failed: {shown} · {failure}"),
                        ..Default::default()
                    });
                }
            }
        }
        drop(runs);
        if file.requests.is_empty() {
            if let Some(spec) = spec.as_deref() {
                diagnostics.extend(response::diagnostics(&text, spec));
            }
        }
        notify::<PublishDiagnostics>(
            &self.sender,
            PublishDiagnosticsParams { uri: uri.clone(), diagnostics, version: None },
        );
    }

    fn hover(&mut self, p: HoverParams) -> Option<Hover> {
        let uri = p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let (text, file, spec) = self.load(&uri)?;
        let lines = split_lines(&text);
        let line_no = pos.line as usize;
        let line = *lines.get(line_no)?;
        let col = lsp_to_byte(line, pos.character);

        for (name, s, e) in env::var_names_in(line) {
            if (s..e).contains(&col) {
                let ctx = self.ctx_for(&uri, &file, spec.as_deref());
                let value = match ctx.raw(&name) {
                    Some(v) => format!("`{name}` = `{}`", ctx.substitute(v).unwrap_or_else(|_| v.to_string())),
                    None if name.starts_with('$') => format!("`{name}` — dynamic value"),
                    None => format!("`{name}` is not defined in env `{}`", ctx.env_name),
                };
                return Some(markdown_hover(value));
            }
        }

        let spec = spec?;
        let req = file.request_at(line_no)?;
        if req.line == line_no {
            if let Some(q) = req.url.find('?') {
                let qstart = req.url_start + q;
                if col > qstart && col <= req.url_end {
                    let seg_start = line[..col].rfind(['?', '&']).map_or(qstart, |i| i + 1);
                    let seg_end = line[col..].find(['&', '=', ' ']).map_or(req.url_end, |i| col + i);
                    let name = &line[seg_start..seg_end.max(seg_start)];
                    let op = spec.find_exact(&req.method, &req.url)?;
                    let p = spec.params(op).into_iter().find(|p| {
                        p.get("in").and_then(Value::as_str) == Some("query")
                            && p.get("name").and_then(Value::as_str) == Some(name)
                    })?;
                    return Some(markdown_hover(render::param_doc(&spec, p)));
                }
            }
            return match spec.find(&req.method, &req.url) {
                Match::Exact(op) => Some(markdown_hover(render::operation(&spec, op))),
                Match::WrongMethod(ops) => {
                    let docs: Vec<String> = ops.iter().map(|op| render::operation(&spec, op)).collect();
                    Some(markdown_hover(docs.join("\n---\n")))
                }
                Match::None => None,
            };
        }

        let op = spec.find_exact(&req.method, &req.url)?;
        if let Some(h) = req.headers.iter().find(|h| h.line == line_no) {
            let p = spec.params(op).into_iter().find(|p| {
                p.get("in").and_then(Value::as_str) == Some("header")
                    && p.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(&h.name))
            })?;
            return Some(markdown_hover(render::param_doc(&spec, p)));
        }

        let body = req.body.as_ref().filter(|b| line_no >= b.start_line)?;
        let root = spec.body_schema_for(op, req.header("content-type"))?;
        let offset = lines[body.start_line..line_no].iter().map(|l| l.len() + 1).sum::<usize>() + col + 1;
        let (_, ctx) = json_scan::scan(&body.text, Some(offset));
        let (key, _, _) = ctx.as_ref()?.on_key.clone()?;
        let container = spec.schema_at(root, &ctx.as_ref()?.container)?;
        let view = spec.object_view(container);
        let prop = spec.child(container, &PathElem::Key(key.clone()))?;
        Some(markdown_hover(render::field_doc(&spec, &key, prop, view.required.contains(&key))))
    }

    fn completion(&mut self, p: CompletionParams) -> Vec<CompletionItem> {
        let uri = p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        let Some((text, file, spec)) = self.load(&uri) else { return Vec::new() };
        let lines = split_lines(&text);
        let Some(line) = lines.get(pos.line as usize) else { return Vec::new() };
        let col = lsp_to_byte(line, pos.character);
        let ctx = self.ctx_for(&uri, &file, spec.as_deref());
        completion::complete(&file, &lines, pos.line as usize, col, spec.as_deref(), &ctx)
    }

    fn code_lens(&mut self, p: CodeLensParams) -> Vec<CodeLens> {
        let uri = p.text_document.uri;
        let Some(text) = self.docs.get(&uri) else { return Vec::new() };
        let lines = split_lines(text);
        let file = http_parser::parse(text);
        let env_lens = (!self.settings.envs.is_empty()).then(|| self.env_name(&self.root_for(&uri)));
        let runs = self.runs.lock().unwrap();
        let mut out = Vec::new();
        for (i, req) in file.requests.iter().enumerate() {
            let range = util::full_line(&lines, req.line);
            if let Some(env) = env_lens.as_ref().filter(|_| i == 0) {
                out.push(CodeLens {
                    range,
                    command: Some(Command {
                        title: format!("env: {env}"),
                        command: CMD_ENV.into(),
                        arguments: Some(vec![json!(uri.as_str())]),
                    }),
                    data: None,
                });
            }
            if i == 0 && file.requests.len() > 1 {
                let mut title = format!("▶ Run all ({})", file.requests.len());
                if let Some(last) = runs.get(&(uri.clone(), RUN_ALL_KEY.to_string())) {
                    title.push_str(&format!(" · {}", last.title));
                }
                out.push(CodeLens {
                    range,
                    command: Some(Command {
                        title,
                        command: CMD_RUN_ALL.into(),
                        arguments: Some(vec![json!(uri.as_str())]),
                    }),
                    data: None,
                });
            }
            out.push(CodeLens {
                range,
                command: Some(Command {
                    title: "▶ Send".into(),
                    command: CMD_SEND.into(),
                    arguments: Some(vec![json!(uri.as_str()), json!(req.line)]),
                }),
                data: None,
            });
            if let Some(run) = runs.get(&(uri.clone(), req.key())) {
                out.push(CodeLens {
                    range,
                    command: Some(Command {
                        title: run.title.clone(),
                        command: CMD_OPEN.into(),
                        arguments: run.path.as_ref().map(|p| vec![json!(p.to_string_lossy())]),
                    }),
                    data: None,
                });
            }
        }
        out
    }

    fn code_actions(&mut self, p: CodeActionParams) -> Vec<CodeActionOrCommand> {
        let uri = p.text_document.uri;
        let Some((text, file, spec)) = self.load(&uri) else { return Vec::new() };
        let line = p.range.start.line as usize;
        let cmd = |title: String, command: &str, arguments: Vec<Value>| {
            CodeActionOrCommand::CodeAction(CodeAction {
                title: title.clone(),
                command: Some(Command { title, command: command.into(), arguments: Some(arguments) }),
                ..Default::default()
            })
        };
        let mut out = Vec::new();
        if let Some(req) = file.request_at(line) {
            let at = vec![json!(uri.as_str()), json!(req.line)];
            out.push(cmd(format!("Send request: {} {}", req.method, req.url), CMD_SEND, at.clone()));
            out.push(cmd("Preview request".into(), CMD_PREVIEW, at.clone()));
            out.push(cmd("Show as curl".into(), CMD_CURL, at));
            let last = self.runs.lock().unwrap().get(&(uri.clone(), req.key())).and_then(|r| r.path.clone());
            if let Some(path) = last {
                out.push(cmd("Open last response".into(), CMD_OPEN, vec![json!(path.to_string_lossy())]));
            }
            let dir = self.root_for(&uri).join(&self.settings.responses_dir);
            if runner::history(&dir, &runner::stem_for(req)).len() >= 2 {
                out.push(cmd("Diff with previous response".into(), CMD_DIFF, vec![json!(uri.as_str()), json!(req.line)]));
            }
        }
        if file.requests.len() > 1 {
            let title = format!("Run all {} requests", file.requests.len());
            out.push(cmd(title, CMD_RUN_ALL, vec![json!(uri.as_str())]));
        }
        let root = self.root_for(&uri);
        let current = self.env_name(&root);
        if !file.requests.is_empty() {
            for name in self.settings.envs.keys().filter(|n| **n != current) {
                out.push(cmd(format!("Switch env to {name}"), CMD_ENV, vec![json!(uri.as_str()), json!(name)]));
            }
            let jar = cookies::Jar::load(&root, &current);
            if jar.len() > 0 {
                let title = format!("Clear cookies ({}, env {current})", jar.len());
                out.push(cmd(title, CMD_CLEAR_COOKIES, vec![json!(uri.as_str())]));
            }
        }
        let blank = text.lines().nth(line).is_none_or(|l| l.trim().is_empty());
        if let Some(spec) = spec.as_deref().filter(|_| file.requests.is_empty() || blank) {
            out.extend(self.generate_actions(&uri, &text, &file, spec));
        }
        out
    }

    fn generate_actions(&self, uri: &Uri, text: &str, file: &HttpFile, spec: &Spec) -> Vec<CodeActionOrCommand> {
        let groups = generate::missing_by_tag(spec, file);
        let lines = split_lines(text);
        let last = lines.len().saturating_sub(1);
        let end = util::range_on_line(&lines, last, lines[last].len(), lines[last].len()).end;
        let sep = if text.trim().is_empty() || text.ends_with("\n\n") {
            ""
        } else if text.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let action = |title: String, ops: &[&spec::Op]| {
            let new_text = format!("{sep}{}\n", generate::blocks(spec, ops, file));
            let edit = TextEdit { range: Range::new(end, end), new_text };
            CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::SOURCE),
                edit: Some(WorkspaceEdit {
                    changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        let plural = |n: usize| if n == 1 { "request" } else { "requests" };
        let mut out: Vec<CodeActionOrCommand> = groups
            .iter()
            .map(|(tag, ops)| action(format!("Generate {} {} for tag `{tag}`", ops.len(), plural(ops.len())), ops))
            .collect();
        if groups.len() > 1 {
            let all: Vec<&spec::Op> = groups.iter().flat_map(|(_, ops)| ops.iter().copied()).collect();
            out.push(action(format!("Generate all {} missing {}", all.len(), plural(all.len())), &all));
        }
        out
    }

    fn open_document(&self, path: &Path, take_focus: bool) {
        let id = request::<ShowDocument>(
            &self.sender,
            &self.next_id,
            ShowDocumentParams {
                uri: path_to_uri(path),
                external: Some(false),
                take_focus: Some(take_focus),
                selection: None,
            },
        );
        self.pending_show.lock().unwrap().insert(id, path.to_path_buf());
    }

    fn execute(&mut self, p: ExecuteCommandParams) {
        match p.command.as_str() {
            CMD_OPEN => {
                if let Some(path) = p.arguments.first().and_then(Value::as_str) {
                    self.open_document(Path::new(path), self.settings.focus_response);
                }
            }
            CMD_RUN_ALL => {
                if let Some(uri) = p.arguments.first().and_then(Value::as_str).and_then(|s| s.parse::<Uri>().ok()) {
                    self.run_all(uri);
                }
            }
            CMD_SEND | CMD_PREVIEW | CMD_CURL | CMD_DIFF => {
                let uri = p.arguments.first().and_then(Value::as_str).and_then(|s| s.parse::<Uri>().ok());
                let line = p.arguments.get(1).and_then(Value::as_u64).map(|l| l as usize);
                if let (Some(uri), Some(line)) = (uri, line) {
                    match p.command.as_str() {
                        CMD_SEND => self.send(uri, line),
                        CMD_PREVIEW => self.preview(uri, line),
                        CMD_DIFF => self.diff(uri, line),
                        _ => self.curl(uri, line),
                    }
                }
            }
            CMD_CLEAR_COOKIES => {
                if let Some(uri) = p.arguments.first().and_then(Value::as_str).and_then(|s| s.parse::<Uri>().ok()) {
                    let root = self.root_for(&uri);
                    let env = self.env_name(&root);
                    match cookies::Jar::clear(&root, &env) {
                        Ok(()) => show(&self.sender, MessageType::INFO, format!("Cookies cleared for env `{env}`")),
                        Err(e) => show(&self.sender, MessageType::ERROR, format!("Cannot clear cookies: {e}")),
                    }
                }
            }
            CMD_ENV => {
                let Some(uri) = p.arguments.first().and_then(Value::as_str).and_then(|s| s.parse::<Uri>().ok())
                else {
                    return;
                };
                let root = self.root_for(&uri);
                match p.arguments.get(1).and_then(Value::as_str) {
                    Some(name) => self.select_env(&root, name),
                    None => self.prompt_env(root),
                }
            }
            other => log(&self.sender, format!("unknown command {other}")),
        }
    }

    fn job(&mut self, uri: &Uri, file: &HttpFile, req: &http_parser::Request, spec: Option<Arc<Spec>>) -> Job {
        let ctx = self.ctx_for(uri, file, spec.as_deref());
        let root = self.root_for(uri);
        let base_dir = uri_to_path(uri)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| root.clone());
        let opts = runner::RunOptions {
            timeout: req.opts.timeout.unwrap_or(Duration::from_secs(self.settings.timeout.max(1))),
            default_accept: self.settings.default_accept.clone(),
            insecure: self.settings.insecure || req.opts.insecure,
            follow_redirects: !req.opts.no_redirect,
            history: self.settings.history_size,
        };
        let spec_path = spec.is_some().then(|| self.spec_path(uri, file)).flatten();
        Job {
            uri: uri.clone(),
            req: req.clone(),
            ctx,
            spec,
            spec_path,
            base_dir,
            root,
            opts,
            cookies: self.settings.cookie_jar && !req.opts.no_cookie_jar,
        }
    }

    fn job_at(&mut self, uri: &Uri, line: usize) -> Option<(Job, HttpFile)> {
        let (_, file, spec) = self.load(uri)?;
        let req = file.requests.iter().find(|r| r.line == line).or_else(|| file.request_at(line))?.clone();
        Some((self.job(uri, &file, &req, spec), file))
    }

    /// `@auth` target: this file, then other open documents, then `.http` files in the project.
    fn find_named(&mut self, uri: &Uri, file: &HttpFile, name: &str) -> Option<Job> {
        if let Some(r) = file.named(name).cloned() {
            let spec = self.spec_for(uri, file);
            return Some(self.job(uri, file, &r, spec));
        }
        for (u, text) in self.project_docs(uri) {
            let other = http_parser::parse(&text);
            if let Some(r) = other.named(name).cloned() {
                let spec = self.spec_for(&u, &other);
                return Some(self.job(&u, &other, &r, spec));
            }
        }
        None
    }

    /// Other `.http` documents of the project: open buffers first, then files on disk.
    fn project_docs(&self, uri: &Uri) -> Vec<(Uri, String)> {
        let mut out: Vec<(Uri, String)> = self
            .docs
            .iter()
            .filter(|(u, _)| *u != uri)
            .map(|(u, t)| (u.clone(), t.clone()))
            .collect();
        let responses = self.root_for(uri).join(&self.settings.responses_dir);
        for path in http_files(&self.root_for(uri)) {
            let u = path_to_uri(&path);
            if path.starts_with(&responses) || u == *uri || out.iter().any(|(c, _)| *c == u) {
                continue;
            }
            if let Ok(t) = std::fs::read_to_string(&path) {
                out.push((u, t));
            }
        }
        out
    }

    fn definition(&mut self, p: GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let uri = p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let (text, file, spec) = self.load(&uri)?;
        let lines = split_lines(&text);
        let line = *lines.get(pos.line as usize)?;
        let col = lsp_to_byte(line, pos.character);
        let at = |u: &Uri, l: usize, line_text: &str, needle: &str| {
            let (s, e) = line_text.find(needle).map_or((0, line_text.len()), |s| (s, s + needle.len()));
            let ls = [line_text];
            let mut range = util::range_on_line(&ls, 0, s, e);
            range.start.line = l as u32;
            range.end.line = l as u32;
            GotoDefinitionResponse::Scalar(Location { uri: u.clone(), range })
        };

        if let Some(name) = directive_arg(line, "auth").filter(|(_, s, e)| (*s..=*e).contains(&col)).map(|(n, ..)| n) {
            let job = self.find_named(&uri, &file, &name)?;
            let target = if job.uri == uri { text.clone() } else { std::fs::read_to_string(uri_to_path(&job.uri)?).ok()? };
            let tl = split_lines(&target);
            let l = (job.req.block_start..=job.req.line)
                .find(|&i| directive_arg(tl[i], "name").is_some_and(|(n, ..)| n == name))
                .unwrap_or(job.req.line);
            return Some(at(&job.uri, l, tl[l], &name));
        }

        let (name, ..) = env::var_names_in(line).into_iter().find(|(_, s, e)| (*s..*e).contains(&col))?;
        if name.starts_with('$') {
            return None;
        }
        for (i, l) in lines.iter().enumerate() {
            if http_parser::parse_var(l.trim()).is_some_and(|v| v.name == name) {
                return Some(at(&uri, i, l, &name));
            }
        }
        let mut docs = vec![(uri.clone(), text.clone())];
        docs.extend(self.project_docs(&uri));
        for (u, t) in &docs {
            for (i, l) in split_lines(t).iter().enumerate() {
                if directive_arg(l, "capture").is_some_and(|(v, ..)| v.split('=').next().unwrap_or("").trim() == name) {
                    return Some(at(u, i, l, &name));
                }
            }
        }
        let root = self.root_for(&uri);
        let ctx = self.ctx_for(&uri, &file, spec.as_deref());
        let key = format!("\"{name}\"");
        let mut sources = Vec::new();
        if ctx.captured.contains_key(&name) {
            sources.push((root.join(".openapi/state.json"), Some(ctx.env_name.clone())));
        }
        if ctx.env.contains_key(&name) {
            let user = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/zed/settings.json"));
            for path in [Some(root.join(".zed/settings.json")), user].into_iter().flatten() {
                sources.push((path.clone(), Some(ctx.env_name.clone())));
                sources.push((path, None));
            }
        }
        for (path, after) in sources {
            let Ok(t) = std::fs::read_to_string(&path) else { continue };
            let tl = split_lines(&t);
            let from = after
                .and_then(|a| tl.iter().position(|l| l.contains(&format!("\"{a}\""))))
                .unwrap_or(0);
            let is_key = |l: &str| l.find(&key).is_some_and(|k| l[k + key.len()..].trim_start().starts_with(':'));
            if let Some(i) = (from..tl.len()).find(|&i| is_key(tl[i])) {
                return Some(at(&path_to_uri(&path), i, tl[i], &key));
            }
        }
        if name == "baseUrl" {
            let path = self.spec_path(&uri, &file)?;
            let t = std::fs::read_to_string(&path).ok()?;
            let tl = split_lines(&t);
            let i = tl.iter().position(|l| l.contains("\"servers\"") || l.trim_start().starts_with("servers:"))?;
            return Some(at(&path_to_uri(&path), i, tl[i], "servers"));
        }
        None
    }

    fn preview(&mut self, uri: Uri, line: usize) {
        let Some((job, _)) = self.job_at(&uri, line) else { return };
        let dir = job.root.join(&self.settings.responses_dir);
        let result = job
            .prepare(job.jar().as_ref())
            .and_then(|p| runner::write_preview(&p, &dir, &job.ctx.env_name));
        match result {
            Ok(path) => self.open_document(&path, true),
            Err(e) => show(&self.sender, MessageType::ERROR, format!("Preview failed: {e}")),
        }
    }

    fn curl(&mut self, uri: Uri, line: usize) {
        let Some((job, _)) = self.job_at(&uri, line) else { return };
        let dir = job.root.join(&self.settings.responses_dir);
        let result = job
            .prepare(job.jar().as_ref())
            .and_then(|p| runner::write_curl(&p, &job.opts, &dir));
        match result {
            Ok(path) => self.open_document(&path, true),
            Err(e) => show(&self.sender, MessageType::ERROR, format!("curl export failed: {e}")),
        }
    }

    fn prompt_env(&mut self, root: PathBuf) {
        let current = self.env_name(&root);
        let actions = self
            .settings
            .envs
            .keys()
            .map(|k| MessageActionItem {
                title: if *k == current { format!("{k} ✓") } else { k.clone() },
                properties: Default::default(),
            })
            .collect();
        let id = request::<ShowMessageRequest>(
            &self.sender,
            &self.next_id,
            ShowMessageRequestParams {
                typ: MessageType::INFO,
                message: format!("OpenAPI env (current: {current})"),
                actions: Some(actions),
            },
        );
        self.pending_env.insert(id, root);
    }

    fn select_env(&mut self, root: &Path, name: &str) {
        let name = name.trim_end_matches(" ✓");
        if !self.settings.envs.contains_key(name) {
            show(&self.sender, MessageType::ERROR, format!("Unknown env `{name}`"));
            return;
        }
        if let Err(e) = env::save_selected(root, Some(name)) {
            show(&self.sender, MessageType::ERROR, format!("Cannot save env: {e}"));
            return;
        }
        log(&self.sender, format!("env: {name}"));
        self.refresh_all();
    }

    fn refresh_all(&mut self) {
        let uris: Vec<Uri> = self.docs.keys().cloned().collect();
        for uri in uris {
            self.publish(&uri);
        }
        request::<CodeLensRefresh>(&self.sender, &self.next_id, ());
    }

    fn bg(&self, open: bool) -> Bg {
        Bg {
            sender: self.sender.clone(),
            runs: self.runs.clone(),
            pending: self.pending_show.clone(),
            next_id: self.next_id.clone(),
            focus: self.settings.focus_response,
            open,
            responses_dir: self.settings.responses_dir.clone(),
            republish: self.republish.clone(),
        }
    }

    /// `@auth` login job for `req`; `Err` when the named request doesn't exist.
    fn login_for(&mut self, uri: &Uri, file: &HttpFile, req: &http_parser::Request) -> Result<Option<Job>, String> {
        match file.auth_for(req) {
            Some(name) => self.find_named(uri, file, name).map(Some).ok_or(format!("@auth: no request named `{name}`")),
            None => Ok(None),
        }
    }

    fn run_all(&mut self, uri: Uri) {
        let Some((_, file, spec)) = self.load(&uri) else { return };
        let mut jobs = Vec::new();
        for req in &file.requests {
            let job = self.job(&uri, &file, req, spec.clone());
            let login = self.login_for(&uri, &file, req);
            jobs.push((job, login));
        }
        let total = jobs.len();
        let bg = self.bg(false);
        let key = (uri.clone(), RUN_ALL_KEY.to_string());
        bg.set_lens(key.clone(), LastRun::note(format!("⏳ 0/{total}")));
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let (mut ok, mut failed) = (0, Vec::new());
            for (i, (mut job, login)) in jobs.into_iter().enumerate() {
                bg.set_lens(key.clone(), LastRun::note(format!("⏳ {}/{total}", i + 1)));
                let label = job.label();
                job.reload_captured();
                let result = login.and_then(|mut login| {
                    if let Some(l) = login.as_mut() {
                        l.reload_captured();
                    }
                    bg.send(job.clone(), login)
                });
                match result {
                    Ok(o) if o.status < 400 && o.checks.iter().all(|(_, f)| f.is_none()) => ok += 1,
                    Ok(o) => failed.push(format!("{label} ({})", o.status)),
                    Err(e) => {
                        bg.set_lens(job.key(), LastRun::note(format!("✗ {}", util::one_line(&e, 60))));
                        failed.push(label);
                    }
                }
            }
            let secs = started.elapsed().as_secs_f32();
            let mut title = format!("✓ {ok}");
            if !failed.is_empty() {
                title = format!("✗ {} · {title}", failed.len());
            }
            title.push_str(&format!(" · {secs:.1} s"));
            let msg = if failed.is_empty() {
                format!("Run all: {ok}/{total} passed in {secs:.1} s")
            } else {
                format!("Run all: {ok}/{total} passed; failed: {}", failed.join(", "))
            };
            let typ = if failed.is_empty() { MessageType::INFO } else { MessageType::WARNING };
            show(&bg.sender, typ, msg);
            bg.set_lens(key, LastRun::note(title));
        });
    }

    fn diff(&mut self, uri: Uri, line: usize) {
        let Some((_, file, _)) = self.load(&uri) else { return };
        let Some(req) = file.requests.iter().find(|r| r.line == line).or_else(|| file.request_at(line)) else {
            return;
        };
        let dir = self.root_for(&uri).join(&self.settings.responses_dir);
        match runner::write_diff(&dir, &runner::stem_for(req)) {
            Ok(path) => self.open_document(&path, true),
            Err(e) => show(&self.sender, MessageType::ERROR, format!("Diff: {e}")),
        }
    }

    fn send(&mut self, uri: Uri, line: usize) {
        let Some((job, file)) = self.job_at(&uri, line) else { return };
        let login = match self.login_for(&uri, &file, &job.req) {
            Ok(l) => l,
            Err(e) => {
                show(&self.sender, MessageType::ERROR, e);
                return;
            }
        };
        // Without @auth nothing can fill missing variables later, so fail right away.
        if login.as_ref().is_none_or(|l| !job.missing_captures(l)) {
            if let Err(e) = job.prepare(None) {
                show(&self.sender, MessageType::ERROR, format!("Request not sent: {e}"));
                return;
            }
        }
        let bg = self.bg(true);
        bg.set_lens(job.key(), LastRun::note("⏳ sending…".into()));
        std::thread::spawn(move || {
            let key = job.key();
            if let Err(e) = bg.send(job, login).map(drop) {
                show(&bg.sender, MessageType::ERROR, &e);
                bg.set_lens(key, LastRun::note(format!("✗ {}", util::one_line(&e, 60))));
            }
        });
    }

    fn handle_response(&mut self, resp: Response) {
        if let Some(root) = self.pending_env.remove(&resp.id) {
            let picked = resp
                .response_result
                .ok()
                .and_then(|r| serde_json::from_value::<Option<MessageActionItem>>(r).ok().flatten());
            if let Some(item) = picked {
                self.select_env(&root, &item.title);
            }
            return;
        }
        let path = self.pending_show.lock().unwrap().remove(&resp.id);
        let Some(path) = path else {
            if resp.id.to_string().contains("cfg") {
                if let Ok(result) = &resp.response_result {
                    self.apply_config(result);
                }
            }
            return;
        };
        let ok = match &resp.response_result {
            Ok(r) => r.get("success").and_then(Value::as_bool).unwrap_or(true),
            Err(_) => false,
        };
        if !ok {
            show(&self.sender, MessageType::INFO, format!("Response saved to {}", path.display()));
        }
    }

    fn apply_config(&mut self, result: &Value) {
        let v = result.as_array().and_then(|a| a.first()).unwrap_or(result);
        if let Some(s) = Settings::from_value(v) {
            self.update_settings(s);
        }
    }

    fn update_settings(&mut self, s: Settings) {
        log(&self.sender, format!("settings: {s:?}"));
        if s.env != self.settings.env {
            if let Some(root) = &self.root {
                let _ = env::save_selected(root, None);
            }
        }
        self.settings = s;
        self.discovered = None;
        self.refresh_all();
    }

    fn handle_notification(&mut self, n: Notification) {
        match n.method.as_str() {
            DidOpenTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) {
                    self.docs.insert(p.text_document.uri.clone(), p.text_document.text);
                    self.publish(&p.text_document.uri);
                }
            }
            DidChangeTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params) {
                    if let Some(change) = p.content_changes.into_iter().last() {
                        self.docs.insert(p.text_document.uri.clone(), change.text);
                        self.publish(&p.text_document.uri);
                    }
                }
            }
            DidSaveTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(n.params) {
                    self.publish(&p.text_document.uri);
                }
            }
            DidCloseTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) {
                    self.docs.remove(&p.text_document.uri);
                }
            }
            DidChangeConfiguration::METHOD => {
                let settings = n.params.get("settings").cloned().unwrap_or(Value::Null);
                match Settings::from_value(&settings) {
                    Some(s) if settings.as_object().is_some_and(|o| !o.is_empty()) => self.update_settings(s),
                    _ => {
                        let id = RequestId::from(format!("cfg-{}", self.next_id.fetch_add(1, Ordering::Relaxed)));
                        let params = ConfigurationParams {
                            items: vec![ConfigurationItem { scope_uri: None, section: Some("openapi-lsp".into()) }],
                        };
                        let _ = self.sender.send(Message::Request(Request::new(
                            id,
                            WorkspaceConfiguration::METHOD.into(),
                            params,
                        )));
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        let result = match req.method.as_str() {
            HoverRequest::METHOD => parse::<HoverParams>(req).map(|p| json!(self.hover(p))),
            GotoDefinition::METHOD => parse::<GotoDefinitionParams>(req).map(|p| json!(self.definition(p))),
            Completion::METHOD => parse::<CompletionParams>(req).map(|p| {
                json!(CompletionResponse::List(CompletionList { is_incomplete: false, items: self.completion(p) }))
            }),
            CodeLensRequest::METHOD => parse::<CodeLensParams>(req).map(|p| json!(self.code_lens(p))),
            CodeActionRequest::METHOD => parse::<CodeActionParams>(req).map(|p| json!(self.code_actions(p))),
            ExecuteCommand::METHOD => parse::<ExecuteCommandParams>(req).map(|p| {
                self.execute(p);
                Value::Null
            }),
            _ => {
                return Response::new_err(id, lsp_server::ErrorCode::MethodNotFound as i32, "unsupported".into())
            }
        };
        match result {
            Ok(v) => Response::new_ok(id, v),
            Err(e) => Response::new_err(id, lsp_server::ErrorCode::InvalidParams as i32, e),
        }
    }
}

/// `# @<directive> <arg>` on `line`: (arg, arg_start, arg_end) byte offsets.
fn directive_arg(line: &str, directive: &str) -> Option<(String, usize, usize)> {
    let t = line.trim_start();
    let c = t.strip_prefix('#').or_else(|| t.strip_prefix("//"))?.trim_start();
    let rest = c.strip_prefix('@')?.strip_prefix(directive)?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let arg = rest.trim();
    let start = line.find(arg)?;
    (!arg.is_empty()).then(|| (arg.to_string(), start, start + arg.len()))
}

fn parse<P: serde::de::DeserializeOwned>(req: Request) -> Result<P, String> {
    serde_json::from_value(req.params).map_err(|e| e.to_string())
}

fn markdown_hover(value: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
        range: None,
    }
}

fn main() -> anyhow::Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!("openapi-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (connection, io_threads) = Connection::stdio();
    let (init_id, init_params) = connection.initialize_start()?;
    let init: InitializeParams = serde_json::from_value(init_params)?;

    let utf8 = init
        .capabilities
        .general
        .as_ref()
        .and_then(|g| g.position_encodings.as_ref())
        .is_some_and(|e| e.contains(&PositionEncodingKind::UTF8));
    util::set_utf8_positions(utf8);

    #[allow(deprecated)]
    let root = init
        .workspace_folders
        .as_ref()
        .and_then(|f| f.first())
        .map(|f| f.uri.clone())
        .or(init.root_uri.clone())
        .and_then(|u| uri_to_path(&u));

    let settings = init
        .initialization_options
        .as_ref()
        .and_then(Settings::from_value)
        .unwrap_or_default();

    let capabilities = ServerCapabilities {
        position_encoding: Some(if utf8 { PositionEncodingKind::UTF8 } else { PositionEncodingKind::UTF16 }),
        text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(TextDocumentSyncKind::FULL),
            save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            ..Default::default()
        })),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(["/", "{", "\"", "?", "&", ":", "@"].map(String::from).to_vec()),
            ..Default::default()
        }),
        code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) }),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: [CMD_SEND, CMD_OPEN, CMD_PREVIEW, CMD_CURL, CMD_ENV, CMD_CLEAR_COOKIES, CMD_RUN_ALL, CMD_DIFF].map(String::from).to_vec(),
            ..Default::default()
        }),
        ..Default::default()
    };
    connection.initialize_finish(
        init_id,
        json!({
            "capabilities": capabilities,
            "serverInfo": { "name": "openapi-lsp", "version": env!("CARGO_PKG_VERSION") },
        }),
    )?;

    let (republish_tx, republish_rx) = crossbeam_channel::unbounded::<Uri>();
    let mut server = Server {
        sender: connection.sender.clone(),
        root,
        settings,
        docs: HashMap::new(),
        specs: HashMap::new(),
        spec_errors: HashMap::new(),
        discovered: None,
        runs: Arc::new(Mutex::new(HashMap::new())),
        pending_show: Arc::new(Mutex::new(HashMap::new())),
        pending_env: HashMap::new(),
        republish: republish_tx,
        next_id: Arc::new(AtomicU64::new(1)),
    };

    loop {
        crossbeam_channel::select! {
            recv(connection.receiver) -> msg => {
                let Ok(msg) = msg else { break };
                match msg {
                    Message::Request(req) => {
                        if connection.handle_shutdown(&req)? {
                            break;
                        }
                        let resp = server.handle_request(req);
                        connection.sender.send(Message::Response(resp))?;
                    }
                    Message::Notification(n) => server.handle_notification(n),
                    Message::Response(r) => server.handle_response(r),
                }
            }
            recv(republish_rx) -> uri => {
                if let Ok(uri) = uri {
                    if server.docs.contains_key(&uri) {
                        server.publish(&uri);
                    }
                }
            }
        }
    }
    drop(server);
    drop(connection);
    io_threads.join()?;
    Ok(())
}
