mod completion;
mod diagnostics;
mod env;
mod http_parser;
mod json_scan;
mod render;
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
    CodeActionRequest, CodeLensRefresh, CodeLensRequest, Completion, ExecuteCommand, HoverRequest,
    Request as _, ShowDocument, WorkspaceConfiguration,
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

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    spec: Option<String>,
    env: Option<String>,
    responses_dir: String,
    timeout: u64,
    default_accept: Option<String>,
    focus_response: bool,
    insecure: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            spec: None,
            env: None,
            responses_dir: ".openapi/responses".into(),
            timeout: 30,
            default_accept: Some("application/json".into()),
            focus_response: false,
            insecure: false,
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

#[derive(Clone)]
struct LastRun {
    title: String,
    path: Option<PathBuf>,
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
    next_id: Arc<AtomicU64>,
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

fn discover_spec(root: &Path) -> Option<PathBuf> {
    const NAMES: &[&str] = &["openapi.json", "openapi.yaml", "openapi.yml", "swagger.json", "swagger.yaml"];
    const SKIP: &[&str] = &["node_modules", "vendor", "target", "storage", "dist", "build", "bootstrap"];
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
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                !n.starts_with('.') && !SKIP.contains(&n.as_ref())
            })
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
        let dir = uri_to_path(uri)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| root.clone());
        let (env_name, env) = env::load_env(&dir, &root, self.settings.env.as_deref());
        Ctx {
            captured: env::load_captured(&root, &env_name),
            env_name,
            env,
            file_vars: file.vars.iter().map(|v| (v.name.clone(), v.value.clone())).collect(),
            spec_base: spec.and_then(Spec::base_url),
        }
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
        let diagnostics = diagnostics::compute(&text, &file, spec.as_deref(), &ctx);
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
        let runs = self.runs.lock().unwrap();
        let mut out = Vec::new();
        for req in &file.requests {
            let range = util::full_line(&lines, req.line);
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
        let Some(text) = self.docs.get(&uri) else { return Vec::new() };
        let file = http_parser::parse(text);
        let Some(req) = file.request_at(p.range.start.line as usize) else { return Vec::new() };
        let mut out = vec![CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Send request: {} {}", req.method, req.url),
            command: Some(Command {
                title: "Send request".into(),
                command: CMD_SEND.into(),
                arguments: Some(vec![json!(uri.as_str()), json!(req.line)]),
            }),
            ..Default::default()
        })];
        if let Some(path) = self
            .runs
            .lock()
            .unwrap()
            .get(&(uri.clone(), req.key()))
            .and_then(|r| r.path.clone())
        {
            out.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Open last response".into(),
                command: Some(Command {
                    title: "Open last response".into(),
                    command: CMD_OPEN.into(),
                    arguments: Some(vec![json!(path.to_string_lossy())]),
                }),
                ..Default::default()
            }));
        }
        out
    }

    fn open_document(&self, path: &Path) {
        let id = request::<ShowDocument>(
            &self.sender,
            &self.next_id,
            ShowDocumentParams {
                uri: path_to_uri(path),
                external: Some(false),
                take_focus: Some(self.settings.focus_response),
                selection: None,
            },
        );
        self.pending_show.lock().unwrap().insert(id, path.to_path_buf());
    }

    fn execute(&mut self, p: ExecuteCommandParams) {
        match p.command.as_str() {
            CMD_OPEN => {
                if let Some(path) = p.arguments.first().and_then(Value::as_str) {
                    self.open_document(Path::new(path));
                }
            }
            CMD_SEND => {
                let uri = p.arguments.first().and_then(Value::as_str).and_then(|s| s.parse::<Uri>().ok());
                let line = p.arguments.get(1).and_then(Value::as_u64).map(|l| l as usize);
                if let (Some(uri), Some(line)) = (uri, line) {
                    self.send(uri, line);
                }
            }
            other => log(&self.sender, format!("unknown command {other}")),
        }
    }

    fn send(&mut self, uri: Uri, line: usize) {
        let Some((_, file, spec)) = self.load(&uri) else { return };
        let Some(req) = file.requests.iter().find(|r| r.line == line).or_else(|| file.request_at(line)) else {
            return;
        };
        let ctx = self.ctx_for(&uri, &file, spec.as_deref());
        let root = self.root_for(&uri);
        let base_dir = uri_to_path(&uri)
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| root.clone());
        let opts = runner::RunOptions {
            timeout: Duration::from_secs(self.settings.timeout.max(1)),
            default_accept: self.settings.default_accept.clone(),
            insecure: self.settings.insecure,
        };
        let key = (uri.clone(), req.key());
        let prepared = match runner::prepare(req, &ctx, spec.as_deref(), &base_dir, &opts) {
            Ok(p) => p,
            Err(e) => {
                show(&self.sender, MessageType::ERROR, format!("Request not sent: {e}"));
                return;
            }
        };

        self.runs.lock().unwrap().insert(key.clone(), LastRun { title: "⏳ sending…".into(), path: None });
        request::<CodeLensRefresh>(&self.sender, &self.next_id, ());

        let responses_dir = root.join(&self.settings.responses_dir);
        let sender = self.sender.clone();
        let runs = self.runs.clone();
        let pending = self.pending_show.clone();
        let next_id = self.next_id.clone();
        let focus = self.settings.focus_response;
        let env_name = ctx.env_name.clone();
        std::thread::spawn(move || {
            log(&sender, format!("{} {}", prepared.method, prepared.url));
            let run = match runner::run(&prepared, &opts, &responses_dir, &root, &env_name) {
                Ok(o) => {
                    let mark = if o.status < 400 { "✓" } else { "✗" };
                    let mut title = format!("{mark} {} {} · {} ms", o.status, o.reason, o.elapsed.as_millis());
                    if !o.captured.is_empty() {
                        let names: Vec<&str> = o.captured.iter().map(|(k, _)| k.as_str()).collect();
                        title.push_str(&format!(" · captured {}", names.join(", ")));
                    }
                    let id = request::<ShowDocument>(
                        &sender,
                        &next_id,
                        ShowDocumentParams {
                            uri: path_to_uri(&o.response_path),
                            external: Some(false),
                            take_focus: Some(focus),
                            selection: None,
                        },
                    );
                    pending.lock().unwrap().insert(id, o.response_path.clone());
                    LastRun { title, path: Some(o.response_path) }
                }
                Err(e) => {
                    show(&sender, MessageType::ERROR, format!("{} {}: {e}", prepared.method, prepared.url));
                    LastRun { title: format!("✗ {}", util::one_line(&e, 60)), path: None }
                }
            };
            runs.lock().unwrap().insert(key, run);
            request::<CodeLensRefresh>(&sender, &next_id, ());
        });
    }

    fn handle_response(&mut self, resp: Response) {
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
        self.settings = s;
        self.discovered = None;
        let uris: Vec<Uri> = self.docs.keys().cloned().collect();
        for uri in uris {
            self.publish(&uri);
        }
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
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(["/", "{", "\"", "?", "&", ":"].map(String::from).to_vec()),
            ..Default::default()
        }),
        code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) }),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![CMD_SEND.into(), CMD_OPEN.into()],
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
        next_id: Arc::new(AtomicU64::new(1)),
    };

    for msg in &connection.receiver {
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
    drop(server);
    drop(connection);
    io_threads.join()?;
    Ok(())
}
