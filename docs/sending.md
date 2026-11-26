# Отправка запросов

**▶ Send** — code lens над запросом или code action. Ответ сохраняется в файл и открывается в редакторе; рядом с **▶ Send** появляется статус последнего запуска, например `✓ 200 OK · 84 ms · captured token · ⚠ 2 spec` (последнее — число расхождений ответа со спеком, см. [Проверка ответа по спеку](#проверка-ответа-по-спеку)).

Code actions на запросе:

| Действие | |
|---|---|
| **Send request** | то же, что **▶ Send** |
| **Preview request** | собрать запрос без отправки — см. [Preview request](#preview-request) |
| **Show as curl** | открыть запрос как curl-команду — см. [curl](#curl) |
| **Open last response** | открыть последний ответ |
| **Diff with previous response** | diff двух последних ответов — см. [История](#история-ответов) |
| **Run all N requests** | отправить все запросы файла — см. [Run all](testing.md#run-all) |
| **Switch env to …** | сменить окружение — см. [настройки](configuration.md#переключение-окружения) |

## Автоматические заголовки

Если какого-то из этих заголовков нет в запросе, он добавляется при отправке:

| Заголовок | Когда добавляется | Как отключить |
|---|---|---|
| `Authorization: Bearer {{token}}` | переменная `token` определена и не пустая, а у операции в спеке есть `security` (bearer / oauth) **или операция не найдена в спеке** | написать свой `Authorization` или убрать `token` из окружения |
| `Accept: application/json` | если `defaultAccept` не `null` (значение берётся оттуда) | свой `Accept` или `"defaultAccept": null` |
| `Content-Type: application/json` | тело начинается с `{` или `[` | свой `Content-Type` |

Например, другой токен для одного запроса:

```http
GET {{baseUrl}}/v1/profile
Authorization: Bearer {{adminToken}}
```

Относительный URL дополняется `baseUrl`.

Что ушло на самом деле, видно в начале [файла ответа](#отправленный-запрос) или через [Preview request](#preview-request).

## Редиректы

Редиректы на тот же хост (например, nginx `http → https`) сохраняют метод, тело и `Authorization`; в файле ответа появляется строка `# redirect: …`. При редиректе на другой хост `Authorization` и `Cookie` не передаются.

## Ответы

Ответ сохраняется в `<responsesDir>/<name>.http` (по умолчанию `.openapi/responses`, см. [настройки](configuration.md)) и открывается в редакторе. Имя — из `# @name`, иначе `METHOD_path`. Бинарные ответы (pdf, xlsx и т. п.) сохраняются рядом с файлом ответа.

Файл, где первый блок начинается с `HTTP/…`, считается ответом: запросов в нём нет.

## Отправленный запрос

В начале файла ответа — запрос в том виде, в каком он ушёл: полный URL, все заголовки (включая [автоматические](#автоматические-заголовки)) и тело после подстановки переменных и `< ./file`. Строки закомментированы (`# > `), бинарное тело показывается только размером, текстовое обрезается после 64 KB.

```http
# > POST https://api.example.com/v1/users
# > Authorization: Bearer eyJ…
# > Accept: application/json
# > Content-Type: application/json
# >
# > { "name": "Ivan" }
HTTP/1.1 201 Created
```

Заголовки, которые HTTP-клиент добавляет сам (`User-Agent`, `Host`, `Content-Length`, `Accept-Encoding`), не показываются.

## Preview request

Code action **Preview request** собирает запрос так же, но не отправляет: результат пишется в `<responsesDir>/<name>.preview.http`. Удобно проверить переменные и токен перед запросом в prod.

## curl

Code action **Show as curl** собирает запрос так же, как при отправке (переменные, `baseUrl`, автоматические заголовки), и открывает `<responsesDir>/<name>.curl.sh`. Скопируйте команду оттуда — LSP не может писать в буфер обмена.

```sh
curl \
  -L \
  --post301 \
  --post302 \
  'https://api.example.com/v1/users' \
  -H 'Authorization: Bearer eyJ…' \
  -H 'Accept: application/json' \
  -H 'Content-Type: application/json' \
  --data-raw '{"name": "Ivan"}'
```

- `-L --post301 --post302` — чтобы редиректы вели себя как при отправке из редактора (POST с телом сохраняется).
- `-k` добавляется при `"insecure": true`.
- Бинарное тело (файл, multipart с файлами) сохраняется рядом в `<name>.body` и передаётся через `--data-binary @…`.

## Проверка ответа по спеку

Файл ответа проверяется по описанию операции в спеке; расхождения показываются как diagnostics прямо в файле ответа, а их число — в code lens над запросом.

| Проверка | Уровень |
|---|---|
| статус не описан в `responses` (учитываются `2XX` и `default`) | warning |
| `Content-Type` ответа не описан для этого статуса | warning |
| тип значения не совпадает со схемой (`integer`, `string`, …) | warning |
| `null` в поле без `nullable` | warning |
| значение не из `enum` | warning |
| нет обязательного поля (`required`, кроме `writeOnly`) | warning |
| поле не описано в схеме | info |

Учитываются `$ref`, `allOf`, `oneOf` / `anyOf` (ответ подходит, если подходит хотя бы один вариант; иначе показываются ошибки ближайшего), `additionalProperties`, массивы типов из OpenAPI 3.1. Не проверяются `format`, `pattern`, `minimum` / `maximum`, длины.

```text
"id": "2",        ⚠ `data.1.id`: expected `integer`, got string
"role": "boss",   ⚠ `data.1.role`: "boss" is not one of "admin", "user"
"extra": 1        ⓘ `data.1.extra` is not documented in `User`
```

Операция определяется по строке `# METHOD url` в начале файла ответа, спек — по строке `# @spec`, которую сервер записывает туда же. Diagnostics пересчитываются при каждом открытии файла, так что после правки спека достаточно переоткрыть ответ.

## Автологин (`@auth`)

`# @auth <name>` связывает запрос с запросом логина (у того должны быть `# @name` и `# @capture`):

1. если какая-то из переменных, которые логин захватывает (`@capture token = …`), пуста, сначала выполняется логин;
2. выполняется сам запрос;
3. если ответ — `401`, логин выполняется ещё раз и запрос повторяется (один раз).

```http
# @auth login

### Login
# @name login
# @capture token = data.token
POST {{baseUrl}}/login
Content-Type: application/json

{ "email": "{{email}}", "password": "{{$env.API_PASSWORD}}" }

### Профиль
GET {{baseUrl}}/v1/profile
```

`# @auth` в начале файла (до первого `###`) действует на все запросы файла, в блоке запроса — только на него. Сам запрос логина `@auth` не вызывает. Запрос с этим именем ищется в текущем файле, затем в открытых файлах, затем во всех `.http` / `.rest` проекта — логин можно держать в отдельном `auth.http`.

В code lens запроса видно, что логин был: `✓ 200 OK · 84 ms · ↻ login` или `↻ login after 401`. Если логин не удался (не 2xx или ничего не захвачено), запрос не отправляется. `# @no-auth` отключает `@auth` для отдельного запроса.

## Cookies

Cookies из `Set-Cookie` сохраняются в `.openapi/cookies.json` отдельно для каждого окружения и отправляются в следующих запросах — как в браузере: по домену, пути, флагу `Secure` и сроку (`Max-Age`, `Expires`). Это нужно для API на сессиях (Laravel Sanctum SPA, CSRF-токены в cookie и т. п.).

- Отправленный `Cookie` виден в [отправленном запросе](#отправленный-запрос), в Preview и в curl.
- Свой заголовок `Cookie` в запросе отключает хранилище для этого запроса (ответные cookies всё равно сохраняются).
- `# @no-cookie-jar` — не отправлять и не сохранять cookies для запроса; `"cookieJar": false` в настройках — для всех.
- Code action **Clear cookies (N, env dev)** очищает хранилище текущего окружения.

## История ответов

Каждый ответ, кроме основного `<responsesDir>/<name>.http`, сохраняется в `<responsesDir>/history/<name>/<дата-время>.http`; хранятся последние `historySize` (по умолчанию 10, `0` — не хранить).

Code action **Diff with previous response** (когда есть хотя бы два ответа) открывает `<name>.diff` — unified diff двух последних. Сравнивается файл целиком, так что в diff попадают и время ответа, и заголовки вроде `Date`.

Если у двух запросов одинаковые метод и путь (например, отличаются только query), они пишут в один файл и одну историю — задайте им разные `# @name`.
