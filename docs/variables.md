# Переменные

## Переменные в файле

Объявляются строкой `@name = value` и действуют на весь файл. Остальные источники — `vars` / `envs` из [настроек](configuration.md) и значения, сохранённые через `@capture`.

```http
@email = admin@example.com
@baseUrl = http://localhost:8080/api

### Login
POST {{baseUrl}}/login
Content-Type: application/json

{ "email": "{{email}}", "password": "secret" }
```

## Порядок разрешения

`@var` в файле → сохранённые `@capture` → `envs[env]` → `vars` → `baseUrl` из `servers[0]` спека.

Hover на `{{var}}` показывает итоговое значение, а если в нём остались неразрешённые переменные — их список.

Неопределённые переменные подсвечиваются в запросах, в `# @expect` и в значениях `@var`.

## Динамические переменные

```http
POST {{baseUrl}}/v1/orders
X-Request-ID: {{$uuid}}
X-Sent-At: {{$timestamp}}
X-Nonce: {{$randomInt}}
Authorization: Bearer {{$env.API_TOKEN}}
```

| Переменная | Значение |
|---|---|
| `$uuid`, `$guid`, `$random.uuid` | случайный UUID v4 |
| `$timestamp` | Unix-время в секундах |
| `$randomInt` | случайное число 0–999 (аргументы вроде `{{$randomInt 1 100}}` игнорируются) |
| `$env.NAME`, `$processEnv NAME` | переменная окружения процесса Zed |

Других динамических переменных нет: `{{$datetime …}}`, `{{$isoTimestamp}}`, `{{$dotenv …}}` из REST Client / JetBrains подсвечиваются как неизвестные, а при опечатке (`{{$uuidd}}`) quick fix предлагает ближайшее имя. Незаданная `{{$env.NAME}}` тоже подсвечивается.

## Имя запроса и захват значений

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

Захваченный `token` сразу начинает подставляться в `Authorization` — см. [Автоматические заголовки](sending.md#автоматические-заголовки).
