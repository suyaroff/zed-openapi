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
        "env": "dev",
        "vars": { "locale": "ru" },
        "envs": {
          "dev":  { "baseUrl": "http://reg.napp.local/api" },
          "prod": { "baseUrl": "https://reg.napp.uz/api", "token": "{{$env.PROD_TOKEN}}" }
        }
      }
    }
  }
}
```

| Ключ | По умолчанию | |
|---|---|---|
| `spec` | автопоиск `openapi.{json,yaml}` | путь от корня проекта; в файле можно переопределить через `# @spec ../path.json` |
| `env` | `dev`, иначе первое окружение | окружение из `envs` |
| `vars`, `envs` | — | переменные и окружения (см. пример выше) |
| `responsesDir` | `.openapi/responses` | куда сохранять ответы: путь от корня проекта (или абсолютный) |
| `timeout` | `30` | секунды |
| `defaultAccept` | `application/json` | добавляется, если нет `Accept`; `null` — не добавлять |
| `focusResponse` | `false` | переводить фокус на ответ |
| `insecure` | `false` | не проверять TLS-сертификат (по умолчанию используется системное хранилище, так что mkcert и подобные работают) |

Редиректы на тот же хост (например, nginx `http → https`) сохраняют метод, тело и `Authorization`; в файле ответа появляется строка `# redirect: …`.

Если `openapi-lsp` не в `PATH`, путь задаётся в `lsp.openapi-lsp.binary.path`.

`envs` — окружения, `vars` — переменные, общие для всех окружений. Секреты берите через `{{$env.X}}` или кладите в пользовательский `~/.config/zed/settings.json`.

Если `baseUrl` не задан, берётся `servers[0].url` из спека.

## Формат `.http`

### Разделение запросов

Запросы разделяются строкой `###`; текст после неё — просто заголовок блока. Над каждым блоком появляется code lens **▶ Send**.

```http
### Login
POST {{baseUrl}}/login
Content-Type: application/json

{ "email": "admin@example.com", "password": "secret" }

### Profile
GET {{baseUrl}}/v1/profile
```

Метод можно опустить — тогда это `GET`. Длинный query можно переносить, продолжение начинается с отступа и `?` / `&`:

```http
GET {{baseUrl}}/v1/users
  ?page=1
  &limit=20
```

Комментарии — строки с `#` или `//`.

### Переменные

Переменные в файле объявляются строкой `@name = value` и действуют на весь файл. Остальные источники — `vars` / `envs` из настроек и значения, сохранённые через `@capture`.

```http
@email = admin@example.com
@baseUrl = http://localhost:8080/api

### Login
POST {{baseUrl}}/login
Content-Type: application/json

{ "email": "{{email}}", "password": "secret" }
```

Порядок: `@var` в файле → сохранённые `@capture` → `envs[env]` → `vars` → `baseUrl` из `servers[0]` спека.

Динамические переменные:

```http
POST {{baseUrl}}/v1/orders
X-Request-ID: {{$uuid}}
X-Sent-At: {{$timestamp}}
X-Nonce: {{$randomInt}}
Authorization: Bearer {{$env.API_TOKEN}}
```

### Имя запроса и захват значений

`# @name` задаёт имя файла ответа (иначе `METHOD_path.http`). `# @capture var = dot.path` после успешного (2xx) ответа сохраняет поле ответа в `.openapi/state.json` — отдельно для каждого окружения и для всех файлов проекта.

```http
### Login
# @name login
# @capture token = data.token
# @capture userId = data.user.id
POST {{baseUrl}}/login
Content-Type: application/json

{ "email": "admin@example.com", "password": "secret" }

### Мой профиль
GET {{baseUrl}}/v1/users/{{userId}}
```

Путь — через точки, элементы массива по индексу: `data.items.0.id`.

### Авторизация

Если у операции в спеке есть `security` (bearer / oauth) и заголовка `Authorization` нет, добавляется `Authorization: Bearer {{token}}`. Чтобы переопределить, напишите заголовок сами:

```http
GET {{baseUrl}}/v1/profile
Authorization: Bearer {{adminToken}}
```

### Тело запроса

JSON с подсказками по схеме из спека:

```http
POST {{baseUrl}}/v1/users
Content-Type: application/json

{
  "name": "Ivan",
  "role": "admin"
}
```

Форма:

```http
POST {{baseUrl}}/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=password&username=admin&password=secret
```

Файл целиком как тело:

```http
POST {{baseUrl}}/v1/import
Content-Type: application/json

< ./fixtures/users.json
```

Multipart с файлом:

```http
POST {{baseUrl}}/v1/uploads
Content-Type: multipart/form-data; boundary=boundary

--boundary
Content-Disposition: form-data; name="title"

Contract
--boundary
Content-Disposition: form-data; name="file"; filename="doc.pdf"

< ./doc.pdf
--boundary--
```

### Другой спек для файла

```http
# @spec ../other-service/openapi.yaml
```

Директива действует на весь файл и переопределяет настройку `spec`.

### Ответы

Ответ сохраняется в `<responsesDir>/<name>.http` и открывается в редакторе. Бинарные ответы (pdf, xlsx и т. п.) сохраняются рядом с файлом ответа. Файл, где первый блок начинается с `HTTP/…`, считается ответом: запросов в нём нет.

## Структура

- `src/lib.rs` — WASM-часть: находит и запускает `openapi-lsp`, передаёт настройки.
- `languages/http` — tree-sitter-http: подсветка, JSON-инъекция в теле, outline.
- `server/` — LSP на `lsp-server` + `ureq`.
