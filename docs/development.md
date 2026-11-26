# Разработка

## Структура

- `src/lib.rs` — WASM-часть: находит и запускает `openapi-lsp`, передаёт настройки.
- `languages/http` — tree-sitter-http: подсветка, JSON-инъекция в теле, outline.
- `server/` — LSP на `lsp-server` + `ureq`. Отдельный крейт, не член workspace.

## Сборка

```sh
# LSP (из server/)
cargo build
cargo clippy
cargo install --path . --locked

# WASM-расширение (из корня); Zed собирает его сам при `zed: install dev extension`
cargo build --target wasm32-wasip2 --release
```

Подробности архитектуры и неочевидные решения — в [`CLAUDE.md`](../CLAUDE.md).
