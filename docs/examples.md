# Примеры

Готовые сценарии. Примеры с `httpbin.org` работают без своего сервера и без спека: httpbin возвращает в ответе то, что получил, поэтому удобно смотреть, что реально ушло.

Настройка для них (`.zed/settings.json`):

```json
{
  "code_lens": "on",
  "lsp": {
    "openapi-lsp": {
      "settings": {
        "envs": { "httpbin": { "baseUrl": "https://httpbin.org" } },
        "env": "httpbin"
      }
    }
  }
}
```

## Первый запрос

```http
### Echo
GET {{baseUrl}}/anything?page=1&limit=20
X-Request-ID: {{$uuid}}
```

`{{baseUrl}}` можно не писать: относительный URL дополняется им сам.

```http
GET /anything
```

## Логин → токен → запросы с авторизацией

`@capture` сохраняет поле ответа, а сохранённый `token` дальше подставляется в `Authorization` [автоматически](sending.md#автоматические-заголовки).

```http
### Login
# @name login
# @capture token = json.token
POST /anything
Content-Type: application/json

{ "token": "secret-123" }

### Проверка: httpbin отвечает 200, только если пришёл Bearer-токен
GET /bearer
```

Ответ на второй запрос — `{"authenticated": true, "token": "secret-123"}`, хотя `Authorization` в нём не написан. Токен хранится в `.openapi/state.json` отдельно для каждого окружения и виден во всех `.http`-файлах проекта.

На реальном API путь обычно глубже: `# @capture token = data.access_token`, элемент массива — `data.items.0.id`.

## Несколько окружений

```json
{
  "lsp": {
    "openapi-lsp": {
      "settings": {
        "env": "dev",
        "vars": { "locale": "ru" },
        "envs": {
          "dev":   { "baseUrl": "http://localhost:8080/api", "token": "dev-token" },
          "stage": { "baseUrl": "https://stage.example.com/api", "token": "{{$env.STAGE_TOKEN}}" },
          "prod":  { "baseUrl": "https://api.example.com", "token": "{{$env.PROD_TOKEN}}" }
        }
      }
    }
  }
}
```

Окружение переключается ключом `env`. `vars` общие для всех окружений, значения из `envs[env]` их перекрывают. Перед запросом в prod удобно сделать **Preview request** и проверить URL и токен.

## Числа и `true`/`false` в JSON-теле

Переменная подставляется как текст, поэтому для числа кавычки не нужны:

```http
@tin = 307990172
@active = true

POST {{baseUrl}}/api/insurant-policies/list

{"tin": {{tin}}, "active": {{active}}}
```

В настройках числа и булевы значения можно писать как есть: `"vars": { "tin": 307990172, "active": true }`. `Content-Type: application/json` добавится сам, раз тело начинается с `{`.

## Переменные из переменных

Подстановка рекурсивная:

```http
@api = {{baseUrl}}/v2
@userId = 42
@userUrl = {{api}}/users/{{userId}}

GET {{userUrl}}
```

## Свой токен или без авторизации

Другой токен для одного запроса — просто напишите заголовок:

```http
GET /bearer
Authorization: Bearer {{adminToken}}
```

Отключить автоматический `Authorization` для всего файла — объявить пустой `token`: переменные файла важнее окружения, а пустой токен не подставляется.

```http
@token =

### Публичный эндпоинт
GET /anything
```

`Accept` переопределяется так же, своим заголовком; отключается глобально через `"defaultAccept": null`.

```http
GET /xml
Accept: application/xml
```

## Форма и multipart

```http
### Форма
POST /post
Content-Type: application/x-www-form-urlencoded

grant_type=password&username=admin&password={{$env.ADMIN_PASSWORD}}

### Файл
POST /post
Content-Type: multipart/form-data; boundary=boundary

--boundary
Content-Disposition: form-data; name="title"

Contract
--boundary
Content-Disposition: form-data; name="file"; filename="doc.pdf"
Content-Type: application/pdf

< ./doc.pdf
--boundary--
```

Путь в `< ./doc.pdf` — относительно `.http`-файла. В отправленном запросе бинарное тело показывается только размером.

## Несколько файлов

В каждой части multipart — своя строка `< ./file`. Одно имя поля можно повторять (массив `files`):

```http
POST /post
Content-Type: multipart/form-data; boundary=b

--b
Content-Disposition: form-data; name="title"

Docs
--b
Content-Disposition: form-data; name="files"; filename="a.bin"
Content-Type: application/octet-stream

< ./a.bin
--b
Content-Disposition: form-data; name="files"; filename="b.txt"
Content-Type: text/plain

< ./b.txt
--b
Content-Disposition: form-data; name="image"; filename="c.png"
Content-Type: image/png

< ./c.png
--b--
```

httpbin вернёт `title` в `form`, два файла в `files` и картинку в `image`. Байты файлов вставляются как есть; перевод строки перед следующим `--b` в содержимое файла не попадает.

## Тело из файла

Текстовый:

```http
POST /anything
Content-Type: application/json

< ./fixtures/users.json
```

Бинарный:

```http
POST /anything
Content-Type: application/octet-stream

< ./archive.zip
```

- `< ./file` должна быть единственной строкой тела. Несколько файлов в одном теле — только через [multipart](#несколько-файлов).
- `Content-Type` указывайте сами: для тела из файла он не подставляется.
- В пути можно использовать переменные (`< ./{{fixture}}.json`), внутри файла — нет: содержимое уходит как есть.
- Файлы читаются в память целиком.

## Бинарный ответ

```http
# @name logo
GET /image/png
```

Картинка сохраняется в `.openapi/responses/logo.png`, а в `logo.http` — строка `# binary body saved to ./logo.png`.

## Редирект с сохранением метода

```http
POST /redirect-to?url=/anything&status_code=301
Content-Type: application/json

{ "id": 1 }
```

Редирект ведёт на тот же хост, поэтому запрос остаётся `POST` с телом; в файле ответа есть строка `# redirect: 301 …`. Обычные HTTP-клиенты в этом случае превращают POST в GET без тела.

## Другой спек для части файлов

```http
# @spec ../billing-service/openapi.yaml

GET {{billingUrl}}/v1/invoices
```

Hover, completion и diagnostics в этом файле берутся из указанного спека.
