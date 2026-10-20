# OpenAPI HTTP Client для Zed

Запросы в `.http`-файлах (синтаксис REST Client / JetBrains) с подсказками и документацией из OpenAPI-спека.

- **Hover** на строке `METHOD url`: описание операции, параметры, тело, ответы с раскрытыми `$ref`. Hover на ключе JSON или заголовке показывает описание поля, на `{{var}}` — его значение.
- **Completion**: операции с готовым шаблоном запроса (тело собирается из `example` или схемы), пути, query-параметры, заголовки, ключи и enum-значения JSON-тела, переменные.
- **Diagnostics**: неизвестный путь или метод, пропущенные обязательные параметры и поля, лишние поля, невалидный JSON, неопределённые переменные.
- **▶ Send** (code lens или code action): ответ сохраняется в `.openapi/responses/<name>.http` и открывается в редакторе.

## Установка

```sh
cargo install --path server          # ставит openapi-lsp в ~/.cargo/bin
```

В Zed выполнить `zed: install dev extension` и выбрать эту папку.

## Настройка проекта

`.zed/settings.json`:

```json
{
  "code_lens": "on",
  "lsp": {
    "openapi-lsp": {
      "settings": {
        "spec": "docs/API/openapi.json",
        "env": "dev"
      }
    }
  }
}
```

| Ключ | По умолчанию | |
|---|---|---|
| `spec` | автопоиск `openapi.{json,yaml}` | путь от корня проекта; в файле можно переопределить через `# @spec ../path.json` |
| `env` | `dev`, иначе первое окружение | окружение из `http-client.env.json` |
| `responsesDir` | `.openapi/responses` | |
| `timeout` | `30` | секунды |
| `defaultAccept` | `application/json` | добавляется, если нет `Accept`; `null` — не добавлять |
| `focusResponse` | `false` | переводить фокус на ответ |
| `insecure` | `false` | не проверять TLS-сертификат (по умолчанию используется системное хранилище, так что mkcert и подобные работают) |

Редиректы на тот же хост (например, nginx `http → https`) сохраняют метод, тело и `Authorization`; в файле ответа появляется строка `# redirect: …`.

Если `openapi-lsp` не в `PATH`, путь задаётся в `lsp.openapi-lsp.binary.path`.

Окружения — `http-client.env.json` (секреты кладите в `http-client.private.env.json`). Файл ищется от папки `.http`-файла вверх до корня проекта:

```json
{
  "$shared": { "locale": "ru" },
  "dev":  { "baseUrl": "http://reg.napp.local/api" },
  "prod": { "baseUrl": "https://reg.napp.uz/api" }
}
```

Если `baseUrl` не задан, берётся `servers[0].url` из спека.

## Формат `.http`

```http
@email = admin@example.com

### Login
# @name login
# @capture token = data.token
POST {{baseUrl}}/login
Content-Type: application/json

{ "email": "{{email}}", "password": "secret" }

### Profile — Authorization: Bearer {{token}} подставится сам
GET {{baseUrl}}/v1/profile
Accept-Language: {{locale}}

### Upload
POST {{baseUrl}}/v1/uploads
Content-Type: multipart/form-data; boundary=boundary

--boundary
Content-Disposition: form-data; name="file"; filename="doc.pdf"

< ./doc.pdf
--boundary--
```

- `# @capture var = dot.path` после успешного (2xx) ответа сохраняет значение в `.openapi/state.json`, отдельно для каждого окружения. Сохранённые значения доступны во всех файлах.
- Bearer-токен из `{{token}}` подставляется автоматически, если у операции есть `security` и явного `Authorization` нет.
- Динамические переменные: `{{$uuid}}`, `{{$timestamp}}`, `{{$randomInt}}`, `{{$env.NAME}}`.
- `< ./file` подставляет содержимое файла: целиком как тело или внутри multipart.
- Бинарные ответы (pdf, xlsx и т.п.) сохраняются рядом с файлом ответа.

## Структура

- `src/lib.rs` — WASM-часть: находит и запускает `openapi-lsp`, передаёт настройки.
- `languages/http` — tree-sitter-http: подсветка, JSON-инъекция в теле, outline.
- `server/` — LSP на `lsp-server` + `ureq`.
