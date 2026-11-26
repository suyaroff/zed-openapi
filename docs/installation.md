# Установка

```sh
cargo install --path server          # ставит openapi-lsp в ~/.cargo/bin
```

В Zed выполнить `zed: install dev extension` и выбрать папку репозитория.

Если `openapi-lsp` не в `PATH`, путь задаётся в `lsp.openapi-lsp.binary.path`.

После переустановки бинарника — `editor: restart language server`.

Дальше: [настройки проекта](configuration.md).
