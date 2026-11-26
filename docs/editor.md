# Возможности редактора

Работают, когда найден OpenAPI-спек (см. `spec` в [настройках](configuration.md)). Запрос сопоставляется с операцией по концу пути, так что `{{baseUrl}}`, хост или префикс вроде `/api` перед путём не мешают.

## Hover

- На строке `METHOD url`: описание операции, параметры, тело, ответы с раскрытыми `$ref`.
- На query-параметре, заголовке или ключе JSON: описание поля из спека.
- На `{{var}}`: значение переменной.

## Completion

Операции с готовым шаблоном запроса (тело собирается из `example` или схемы), пути, query-параметры и их значения (`enum`, `true`/`false`), заголовки и их значения, ключи и enum-значения JSON-тела, переменные.

- Базовый путь перед путём из спека (`https://host/api/v3/…`, `{{baseUrl}}/api/v3/…`) при выборе пути сохраняется.
- Уже указанные query-параметры и заголовки повторно не предлагаются.

## Diagnostics

В запросах: неизвестный путь или метод, пропущенные обязательные параметры и поля, лишние поля, невалидный JSON, неопределённые переменные.

В файлах ответов: недокументированный статус или `Content-Type`, несовпадение типов, `enum`, обязательных полей — см. [Проверка ответа по спеку](sending.md#проверка-ответа-по-спеку).

## Quick fixes

На подсвеченной проблеме code action (в Zed — `Ctrl+.` / `Cmd+.`) предлагает исправление:

| Diagnostic | Исправление |
|---|---|
| `Missing required field `email`` | **Add field `email`** — вставляет поле со значением из `example` / схемы, сохраняя формат объекта (многострочный — с отступом, однострочный — в строку); при нескольких — **Add all N missing required fields** |
| `Missing required query parameter `locale`` | **Add query parameter** — дописывает `?locale=` / `&locale=` (или новую строку `  &locale=`, если query перенесён) |
| `Missing required header parameter `X-Tenant`` | **Add header parameter** — строка `X-Tenant: ` после заголовков |
| `Request body is required` | **Add body from schema** — `Content-Type` и JSON-тело из схемы |
| `Unknown field `junk`` | **Remove field** — удаляет поле вместе с запятой |
| `DELETE is not defined for /users/{id}; allowed: GET, PUT` | **Change method to GET / PUT** |
| `No matching path in the OpenAPI spec` | **Change path to GET /users/{id}/profile** — до трёх ближайших путей из спека (опечатки, лишний или пропущенный сегмент); префикс вроде `{{baseUrl}}/api` и query сохраняются, значения path-параметров переносятся |
| `Variable `uid` is not defined` | **Define `@uid` in this file** — `@uid = ` рядом с остальными `@var` в начале файла |

## Go to definition

- На `{{var}}`: строка `@var = …` в файле, иначе `# @capture var = …` (в этом или другом `.http` проекта), иначе ключ в `.openapi/state.json`, `.zed/settings.json` (или пользовательском `settings.json`), для `baseUrl` — `servers` в спеке. Порядок тот же, что при [разрешении переменных](variables.md#порядок-разрешения).
- На имени в `# @auth login`: запрос с `# @name login`.

## Генерация запросов из спека

Code actions **Generate N requests for tag `users`** и **Generate all N missing requests** дописывают в конец файла запросы для операций спека, которых в файле ещё нет. Действия появляются в пустом файле или на пустой строке.

```http
### Current user
# @name me
GET {{baseUrl}}/me

### GET /users/{id}
# @name getUser
GET {{baseUrl}}/users/{{id}}?locale=
```

- Заголовок блока — `summary` операции, `@name` — `operationId` (если имя ещё не занято).
- Path-параметры становятся переменными `{{id}}`, обязательные query- и header-параметры добавляются с пустыми значениями, тело собирается из `example` или схемы — как в completion.
- Операции без тегов группируются в `untagged`.
