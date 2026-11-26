# OpenAPI HTTP Client для Zed

Запросы в `.http`-файлах (синтаксис REST Client / JetBrains) с подсказками и документацией из OpenAPI-спека.

- **Hover**, **Completion**, **Diagnostics** по спеку — [подробнее](docs/editor.md).
- **▶ Send** (code lens или code action): ответ сохраняется в `.openapi/responses/<name>.http` вместе с отправленным запросом — [подробнее](docs/sending.md).
- **Preview request** (code action): итоговый запрос без отправки.

## Быстрый старт

```sh
cargo install --path server          # ставит openapi-lsp в ~/.cargo/bin
```

В Zed: `zed: install dev extension` → папка репозитория. Затем `.zed/settings.json` в проекте:

```json
{
  "code_lens": "on",
  "lsp": {
    "openapi-lsp": {
      "settings": {
        "spec": "docs/API/openapi.json",
        "envs": { "dev": { "baseUrl": "http://localhost:8080/api" } }
      }
    }
  }
}
```

```http
### Profile
GET {{baseUrl}}/v1/profile
```

## Документация

- [Установка](docs/installation.md)
- [Настройки](docs/configuration.md)
- [Формат `.http`](docs/http-syntax.md)
- [Переменные](docs/variables.md)
- [Отправка запросов](docs/sending.md)
- [Возможности редактора](docs/editor.md)
- [Примеры](docs/examples.md)
- [Разработка](docs/development.md)
