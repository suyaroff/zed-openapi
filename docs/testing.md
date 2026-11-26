# Проверки и Run all

`# @expect` описывает, каким должен быть ответ, а **Run all** прогоняет весь файл. Вместе это smoke-тест API прямо в `.http`.

## `@expect`

```http
### Создать пользователя
# @expect status 201
# @expect data.id exists
# @expect data.role == "admin"
# @expect data.tags.length > 0
# @expect header Content-Type contains json
# @expect time < 500
POST {{baseUrl}}/v1/users
Content-Type: application/json

{ "name": "Ivan", "role": "admin", "tags": ["a"] }
```

Строка — `<что> <оператор> <значение>`:

| Что | |
|---|---|
| `status` | код ответа; `status 201` = `status == 201`, `status 2xx` — любой 2xx |
| `time` | время ответа, мс |
| `header <Name>` | заголовок ответа (без учёта регистра имени) |
| `data.items.0.id` | поле JSON-тела, путь как в `@capture`; `….length` — длина массива, строки или объекта |
| `body` | всё тело |

| Оператор | |
|---|---|
| `==`, `!=` | равенство; значение — JSON (`1`, `true`, `null`, `"admin"`) или просто текст: `data.role == admin` |
| `>`, `>=`, `<`, `<=` | числа (строка с числом тоже подходит) |
| `contains` | подстрока для строк, элемент для массивов, ключ для объектов; `body contains` ищет в тексте ответа |
| `exists`, `!exists` | поле есть / нет; `data.id` без оператора = `data.id exists` |

Результат:

- в code lens запроса: `✓ 201 Created · 84 ms · ✓ 6 expect` или `✗ 200 OK · 84 ms · ✗ 2/6 expect` (метка становится ✗, если хоть одна проверка не прошла);
- в файле ответа — строки `# expect ✓ …` / `# expect ✗ … · got 200`;
- непрошедшие проверки — diagnostics на строках `# @expect` в `.http`-файле; строка, которую не удалось разобрать, подсвечивается сразу, ещё до отправки.

## Run all

Code lens **▶ Run all (N)** над первым запросом (и code action **Run all N requests**) отправляет все запросы файла по порядку:

- значения из `@capture` сразу доступны следующим запросам — логин в начале файла, дальше запросы с токеном;
- `@auth` работает как обычно: логин выполнится один раз, когда понадобится;
- ответы сохраняются, но не открываются; у каждого запроса обновляется свой code lens;
- запрос считается успешным, если статус < 400 и все `@expect` прошли; ошибка одного запроса не останавливает остальные.

В конце — сообщение `Run all: 5/6 passed; failed: createUser (422)`, а в lens: `▶ Run all (6) · ✗ 1 · ✓ 5 · 1.2 s`.

## Пример: smoke-тест

```http
### Login
# @name login
# @capture token = data.token
# @expect status 200
# @expect data.token exists
POST {{baseUrl}}/login
Content-Type: application/json

{ "email": "{{email}}", "password": "{{$env.API_PASSWORD}}" }

### Профиль
# @expect status 200
# @expect data.email == {{email}}
GET {{baseUrl}}/v1/profile

### Список
# @expect status 200
# @expect data.items.length > 0
# @expect time < 1000
GET {{baseUrl}}/v1/users?page=1
```

`{{переменные}}` в `@expect` подставляются перед проверкой; если переменная не разрешилась, проверка считается проваленной (`undefined variable(s): …`), а не сравнивается с текстом `{{…}}`.
