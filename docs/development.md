# Разработка

## Структура

- `src/lib.rs` — WASM-часть: находит и запускает `openapi-lsp`, передаёт настройки.
- `languages/http` — tree-sitter-http: подсветка, JSON-инъекция в теле, outline.
- `server/` — LSP на `lsp-server` + `ureq`. Отдельный крейт, не член workspace.

## Сборка

```sh
# LSP (из server/)
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo install --path . --locked

# WASM-расширение (из корня); Zed собирает его сам при `zed: install dev extension`
cargo build --target wasm32-wasip2 --release
```

## Тесты

Юнит-тесты лежат в модулях `server/src` (`#[cfg(test)] mod tests`), запуск — `cargo test` из `server/`. Тестовые спеки — `server/tests/fixtures/`. Отправка запросов (`runner`) и LSP-цикл тестами не покрыты. CI (`.github/workflows/ci.yml`) гоняет `cargo test` и `clippy` на push в `main` и на PR.

Подробности архитектуры и неочевидные решения — в [`CLAUDE.md`](../CLAUDE.md).
