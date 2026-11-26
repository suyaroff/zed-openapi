# Формат `.http`

Синтаксис REST Client / JetBrains HTTP Client.

## Разделение запросов

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

Переменные (`@var`, `{{var}}`) и директивы `# @name` / `# @capture` — в [Переменных](variables.md).

## Тело запроса

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

Пути в `< ./file` — относительно `.http`-файла.

## Другой спек для файла

```http
# @spec ../other-service/openapi.yaml
```

Директива действует на весь файл и переопределяет настройку `spec`.
