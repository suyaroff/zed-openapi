# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Zed extension: `.http` request files (REST Client / JetBrains syntax) backed by an OpenAPI spec — hover docs, completion, diagnostics, and sending requests. User-facing docs and settings: `README.md`.

## Two separate crates

- **Root crate** (`Cargo.toml`, `src/lib.rs`) — the WASM extension (`zed_extension_api`). It only locates the `openapi-lsp` binary (`lsp.openapi-lsp.binary.path`, else `worktree.which`) and forwards `lsp.openapi-lsp.settings` as both initialization options and workspace configuration. No download logic; the binary must be installed.
- **`server/`** — the native LSP binary `openapi-lsp`. Deliberately **not** a workspace member, so the WASM build doesn't pull in native-only deps. Build and run cargo commands from inside `server/`.

## Commands

```sh
# LSP server (run inside server/)
cargo build                      # debug build -> server/target/debug/openapi-lsp
cargo clippy
cargo install --path . --locked  # install to ~/.cargo/bin (what Zed actually runs)

# WASM extension (repo root); Zed builds it itself on "zed: install dev extension"
cargo build --target wasm32-wasip2 --release

# Validate tree-sitter queries against the pinned grammar (rest-nvim/tree-sitter-http, rev in extension.toml)
tree-sitter query languages/http/highlights.scm some.http   # run from a checkout of the grammar after `tree-sitter generate`
```

There are no tests in the repo. The server was verified with an ad-hoc Python stdio LSP client plus a mock HTTP server. After reinstalling the binary, Zed needs `editor: restart language server`.

## Server architecture (`server/src`)

Synchronous `lsp-server` main loop in `main.rs`. The only concurrency: `openapi.send` spawns a thread that does the HTTP call and then sends `window/showDocument` and `workspace/codeLens/refresh` through a cloned `Sender`. Last-run results live in `Arc<Mutex<..>>` keyed by `(uri, Request::key())` and are rendered as a second code lens.

The document text is re-parsed on every request; there is no incremental state. The pipeline:

- `http_parser.rs` — line-based `.http` parser (it does not use tree-sitter):
  - splits blocks on `###`;
  - collects `@var = value` lines and `# @name` / `# @capture var = path` / `# @spec path` directives;
  - a block whose first line is `HTTP/…` is a response, so saved response files yield no requests.
- `spec.rs` — the spec is kept as raw `serde_json::Value` (no typed OpenAPI model; YAML via `serde_yaml`):
  - `resolve` follows local `$ref` JSON pointers;
  - `find` matches a raw URL to an operation by **suffix-matching path segments**, so `{{baseUrl}}`, a host or a base path like `/api` in front don't matter. More segments win, then more literal segments. The result distinguishes `Exact` from `WrongMethod`;
  - `object_view` merges `allOf`, and `child` / `schema_at` walk a JSON path through the schema.
- `json_scan.rs` — tolerant scanner for incomplete JSON bodies containing `{{vars}}`. It yields key positions (used by diagnostics) and the cursor context (used by hover and completion).
- `env.rs` — variable resolution. Order: file `@vars` > captured state (`.openapi/state.json`, per env) > `http-client.env.json` / `.private.env.json` (searched upward from the file's dir; `$shared` merged first) > `baseUrl` fallback from `servers[0]`. Dynamic `{{$uuid}}`, `{{$timestamp}}`, `{{$randomInt}}`, `{{$env.X}}`.
- `render.rs` (hover markdown), `completion.rs`, `diagnostics.rs` — consumers of the modules above.
- `runner.rs` — `prepare` substitutes vars, prefixes relative URLs with `baseUrl`, auto-adds `Authorization: Bearer {{token}}` when the matched operation has bearer/oauth security, adds a default `Accept`, and expands `< ./file` includes (multipart bodies are converted to CRLF). `run` writes `<responsesDir>/<name|METHOD_path>.http` and saves captures.

## Non-obvious decisions to preserve

- **Redirects are followed manually** (`max_redirects(0)`). For a same-host redirect (the dev nginx answers `301 http → https`), the method, body and `Authorization` are kept. ureq/curl defaults would turn the POST into a body-less GET.
- **TLS uses `RootCerts::PlatformVerifier`** (the system trust store), so locally issued dev certs work. `insecure` disables verification.
- **Positions:** the server negotiates UTF-8 positions when the client offers them, otherwise UTF-16. All byte↔LSP conversions go through `util.rs`.
- **Shutdown:** `main` must drop `server` (which holds a `Sender` clone) before `io_threads.join()`, otherwise the process never exits.
- `ureq` 3 rejects bodies on GET/DELETE unless you call `force_send_body()`. `send_once` handles this per method.
