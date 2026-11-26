# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Zed extension: `.http` request files (REST Client / JetBrains syntax) backed by an OpenAPI spec — hover docs, completion, diagnostics, and sending requests. User-facing docs: `docs/` (index in `docs/README.md`), one file per topic; `README.md` is only a landing page. Update the matching `docs/*.md` when behavior changes.

## Two separate crates

- **Root crate** (`Cargo.toml`, `src/lib.rs`) — the WASM extension (`zed_extension_api`). It only locates the `openapi-lsp` binary (`lsp.openapi-lsp.binary.path`, else `worktree.which`) and forwards `lsp.openapi-lsp.settings` as both initialization options and workspace configuration. No download logic; the binary must be installed.
- **`server/`** — the native LSP binary `openapi-lsp`. Deliberately **not** a workspace member, so the WASM build doesn't pull in native-only deps. Build and run cargo commands from inside `server/`.

## Commands

```sh
# LSP server (run inside server/)
cargo build                      # debug build -> server/target/debug/openapi-lsp
cargo test
cargo clippy --all-targets -- -D warnings   # CI runs exactly this plus `cargo test --locked`
cargo install --path . --locked  # install to ~/.cargo/bin (what Zed actually runs)

# WASM extension (repo root); Zed builds it itself on "zed: install dev extension"
cargo build --target wasm32-wasip2 --release

# Validate tree-sitter queries against the pinned grammar (rest-nvim/tree-sitter-http, rev in extension.toml)
tree-sitter query languages/http/highlights.scm some.http   # run from a checkout of the grammar after `tree-sitter generate`
```

Tests are inline `#[cfg(test)] mod tests` at the bottom of each module in `server/src` (binary-only crate, so no `tests/*.rs`). Shared helpers are in `test_util.rs` (`spec()`, `ctx()`, `at("…|…")` cursor marker, `apply(text, edits)`); spec fixtures are in `server/tests/fixtures/`. Covered: the pure modules. `runner` (HTTP) and the `main` LSP loop have no tests; they were verified by hand with an ad-hoc Python stdio LSP client plus a mock HTTP server. `util::set_utf8_positions` is process-global: keep test text ASCII, except in the util tests, which hold `test_util::POS_LOCK`. CI: `.github/workflows/ci.yml`.

After reinstalling the binary, Zed needs `editor: restart language server`.

## Server architecture (`server/src`)

Synchronous `lsp-server` main loop in `main.rs`. The only concurrency: `openapi.send` builds a `Job` (request + `Ctx` + spec + options, all owned) and spawns a thread; `Bg` holds what that thread needs (cloned `Sender`, lens map, pending `showDocument` ids). The thread prepares the request itself, because `@auth` may run the login first and the request must then be re-prepared with the freshly captured vars (`Job::reload_captured`). Last-run results live in `Arc<Mutex<..>>` keyed by `(uri, Request::key())` and are rendered as a second code lens; Run all stores its summary under `RUN_ALL_KEY`. Failed `@expect`s are diagnostics computed in `publish` from that map, so the thread asks the main loop to republish through an internal channel (`Bg::republish`; `main` `select!`s over it and the LSP receiver).

The document text is re-parsed on every request; there is no incremental state. The pipeline:

- `http_parser.rs` — line-based `.http` parser (it does not use tree-sitter):
  - splits blocks on `###`;
  - collects `@var = value` lines and `# @…` directives: `name`, `capture`, `spec`, plus per-request `Options` (`auth`, `no-auth`, `timeout`, `no-redirect`, `insecure`, `no-cookie-jar`); `@auth` in a block without a request becomes the file default (`HttpFile::auth_for`);
  - a block whose first line is `HTTP/…` is a response, so saved response files yield no requests.
- `spec.rs` — the spec is kept as raw `serde_json::Value` (no typed OpenAPI model; YAML via `serde_yaml`):
  - `resolve` follows local `$ref` JSON pointers;
  - `find` matches a raw URL to an operation by **suffix-matching path segments**, so `{{baseUrl}}`, a host or a base path like `/api` in front don't matter. More segments win, then more literal segments. The result distinguishes `Exact` from `WrongMethod`;
  - `object_view` merges `allOf`, and `child` / `schema_at` walk a JSON path through the schema.
- `json_scan.rs` — tolerant scanner for incomplete JSON bodies containing `{{vars}}`. It yields key positions (used by diagnostics) and the cursor context (used by hover and completion). `parse_spanned` is a separate strict parser that keeps byte spans for every key/value (response validation needs exact positions inside arrays, which the tolerant scanner can't give).
- `env.rs` — variable resolution. The active env is `.openapi/env` (picked via the `env` code lens / `openapi.selectEnv`) if it names a known env, else settings `env`; a change of settings `env` deletes `.openapi/env`. Order: file `@vars` > captured state (`.openapi/state.json`, per env) > settings `envs[env]` > settings `vars` > `baseUrl` fallback from `servers[0]`. Dynamic `{{$uuid}}`, `{{$timestamp}}`, `{{$randomInt}}`, `{{$env.X}}`.
- `render.rs` (hover markdown), `completion.rs`, `diagnostics.rs` — consumers of the modules above. `diagnostics::compute` returns `Finding { diag, fixes }` with fixes already resolved to `TextEdit`s; `publish` keeps only `diag`, `code_actions` recomputes and offers the fixes overlapping the range (no reliance on the client echoing `Diagnostic.data`). Body validation runs `json_scan::parse_spanned` on `neutralize_vars` output, which is length-preserving (`{{x}}` → `0` + spaces), so spans are valid offsets into the original body.
- `expect.rs` — `# @expect` parser and evaluator (status / time / header / JSON path / body, with `== != > >= < <= contains exists !exists`). `run` evaluates them (after `{{var}}` substitution in `prepare`) and writes `# expect ✓/✗` lines; results are matched back to the `@expect` lines by order.
- `cookies.rs` — own cookie jar per env in `.openapi/cookies.json` (not ureq's store, so the `Cookie` header shows up in the saved request, Preview and curl). `runner::apply_cookies` marks the header as jar-owned (`Prepared::jar_cookie`); `execute` then recomputes it on every redirect hop and stores every `Set-Cookie`.
- `generate.rs` — code actions that append request blocks for spec operations missing from the file, grouped by tag; the text comes from `completion::request_text` (same template as the completion snippet, without placeholders).
- `response.rs` — diagnostics for saved response files (files with no requests). It finds the operation from the first line `# METHOD url` and the spec from the `# @spec <abs path>` line that `run` writes, then checks status, `Content-Type` and the JSON body against the response schema. `warning_count` feeds the `⚠ N spec` part of the last-run lens.
- `runner.rs` — `prepare` substitutes vars, prefixes relative URLs with `baseUrl`, auto-adds `Authorization: Bearer {{token}}` when the matched operation has bearer/oauth security, adds a default `Accept`, and expands `< ./file` includes (multipart bodies are converted to CRLF). `run` writes `<responsesDir>/<name|METHOD_path>.http` (summary, `# @spec`, the sent request as `# > ` lines, then the raw response) and saves captures. `write_preview` / `write_curl` reuse `Prepared` without sending. Every response is also copied to `history/<stem>/<timestamp>.http` (last `historySize`); `write_diff` diffs the two latest with `similar`.

## Non-obvious decisions to preserve

- **Redirects are followed manually** (`max_redirects(0)`). For a same-host redirect (the dev nginx answers `301 http → https`), the method, body and `Authorization` are kept. ureq/curl defaults would turn the POST into a body-less GET.
- **TLS uses `RootCerts::PlatformVerifier`** (the system trust store), so locally issued dev certs work. `insecure` disables verification.
- **Positions:** the server negotiates UTF-8 positions when the client offers them, otherwise UTF-16. All byte↔LSP conversions go through `util.rs`.
- **Shutdown:** `main` must drop `server` (which holds a `Sender` clone) before `io_threads.join()`, otherwise the process never exits.
- `ureq` 3 rejects bodies on GET/DELETE unless you call `force_send_body()`. `send_once` handles this per method.
