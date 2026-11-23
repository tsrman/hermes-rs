# Интерфейс расширений Hermes CLI (Rust)

Rust-версия Hermes запускает штатные сценарии без установленного Python. Пользовательские расширения и интеграции с Python работают через **изолированные внешние процессы с версионированными контрактами**, а не через загрузку Python-модулей внутрь процесса агента.

Поддерживаются четыре точки расширения:

1. **Markdown-навыки (`skills/`)** — декларативные инструкции и справочные материалы.
2. **Пользовательские инструменты и ресурсы через MCP** — внешний процесс (Python или любой другой язык) по JSON-RPC 2.0 (`stdio` или `Streamable HTTP`).
3. **Программные вычисления на Python (`execute_code`)** — опциональное ядро Python с RPC-мостом `hermes_tools`.
4. **Программный запуск агента из Python (замена `AIAgent`)** — версионированный поток JSON Lines (`hermes chat --events-json`) и лёгкая Python SDK-обёртка.

---

## 1. Markdown-навыки (`skills/`)

Формат навыков эталона сохранён полностью: каталог навыка с файлом `SKILL.md` (YAML frontmatter `name`, `description`, `metadata.hermes.*`), а также вспомогательными подкаталогами (`references/`, `templates/`, `scripts/`, `assets/`).

- **Поиск и приоритет**: доверенный проект (`skills.project_discovery`, `skills.trusted_project_dirs`), `skills/` выбранного профиля, `skills.create_dir`, `skills.external_dirs`.
- **Настройки**: `skills.auto_load`, `skills.template_vars` (`${HERMES_SKILL_DIR}`, `${HERMES_SESSION_ID}` для уже сохранённой сессии), `skills.config.*`, `skills.disabled`, `skills.index_desc_limit`.
- **Инструменты**: `skills_list`, `skill_view`, `skill_manage` (в порте каждая запись через `skill_manage` запрашивает подтверждение).
- **Намеренное отличие**: `skills.inline_shell` (выполнение `` !`cmd` `` при чтении `SKILL.md`) **не исполняется** — CLI печатает предупреждение и оставляет текст как есть, чтобы файл навыка не мог запускать команды в обход политики одобрений.

---

## 2. Пользовательские инструменты и ресурсы на Python (MCP)

Основной способ добавить собственные инструменты на Python — подключить их как MCP-сервер в `config.yaml` выбранного профиля. Клиент реализует MCP JSON-RPC 2.0 (`protocolVersion: 2025-06-18`) поверх `stdio` и `Streamable HTTP`.

Пример конфигурации профиля (`<HERMES_HOME>/config.yaml`) с готовым примером [python_mcp_plugin.py](../examples/extensions/python_mcp_plugin.py):

```yaml
mcp_servers:
  pyext:
    command: python3
    args:
      - "-I"
      - "/path/to/examples/extensions/python_mcp_plugin.py"
    env:
      PLUGIN_TOKEN: "${PLUGIN_TOKEN}"
    timeout: 60
    connect_timeout: 10
```

### Контракт и гарантии

- **Имена инструментов**: `mcp__<сервер>__<инструмент>` (очистка `[^A-Za-z0-9_]` → `_`, обрезка свыше 64 символов с 8-значным хешем SHA-256). При объявлении `capabilities.resources` и `tools.resources != false` автоматически добавляются `mcp__<сервер>__list_resources` и `mcp__<сервер>__read_resource`.
- **Ограничение доступа к профилю и окружению**: дочерний процесс получает только минимальный список базовых переменных (`PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `TERM`, `SHELL`, `TMPDIR`/`TEMP`/`TMP` и Windows-эквиваленты) плюс явный словарь `mcp_servers.<имя>.env`. Ключи модели (`HERMES_API_KEY`, `OPENAI_API_KEY` и т. п.), `HERMES_HOME` и посторонние переменные родителя процессу плагина не передаются.
- **Отмена и очистка процессов**: при `Ctrl+C`, отмене через локального владельца или таймауте вызова CLI отправляет серверу `notifications/cancelled` (`{"requestId": <id>, "reason": "caller cancelled"}`) и после завершения хода останавливает всю группу процессов сервера (включая порождённых потомков).
- **Отсутствие интерпретатора**: если `command` (например `python3` или указанный виртуальный интерпретатор) не найден, `hermes` не падает — он выводит предупреждение `MCP server "<имя>" is unavailable: ...` (в `--events-json` это событие `warning`), пропускает этот сервер и продолжает ход со встроенными инструментами и остальными серверами.
- **Ограничения**: не перенесены `prompts`, `sampling`, `elicitation`, OAuth, транспорт SSE, кэш двоичных/медиа-ресурсов (`MEDIA:`) и перезапуск упавшего stdio-сервера внутри хода.

---

## 3. Программные вычисления на Python (`execute_code`)

Инструмент `execute_code` поднимает опциональный процесс `python3` (из `VIRTUAL_ENV` / `CONDA_PREFIX` или `PATH`) и передаёт ячейки кода по сокетной паре (дескриптор 3). Модуль `hermes_tools` внутри скрипта возвращает вызовы обратно в Rust через обычную таблицу инструментов (с теми же одобрениями, `approvals.deny`, редактированием секретов и проверками путей).

- **Границы доступа**: окружение фильтруется как в `terminal` — ключи моделей (`HERMES_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY` и др.) не передаются. Каждая ячейка требует отдельного подтверждения `RunCode` (кроме `--yolo` / `approvals.mode: off`).
- **Отсутствие интерпретатора**: при отсутствии `python3` в `PATH` и виртуальных окружениях инструмент не роняет процесс, а возвращает структурированный ответ:
  `{"status": "error", "error": "No python3 was found on PATH (or in the active VIRTUAL_ENV / CONDA_PREFIX).", "tool_calls_made": 0, "duration_seconds": 0}`.
- **Платформы**: только Unix (Linux и macOS).

---

## 4. Программная интеграция из Python (вместо `AIAgent`)

Вместо прямого импорта `from run_agent import AIAgent` внешние Python-приложения запускают бинарник `hermes` в режиме версионированного потока событий:

```bash
hermes chat --events-json -q "Прочитай fixture.txt" --save-session
```

Флаг `--events-json` требует `-q`/`--query`, автоматически включает `--oneshot` (без интерактивных вопросов в терминале) и пишет в `stdout` строго по одному JSON-объекту на строку (NDJSON) с полем `"version": 1`. Работает как напрямую, так и через локального владельца `hermes owner`.

### Протокол событий (`version: 1`)

| `event` | Поля | Когда отправляется |
| --- | --- | --- |
| `text` | `text: string` | Потоковый фрагмент ответа модели |
| `tool_started` | `name: string` | Начало вызова встроенного или MCP-инструмента |
| `tool_finished` | `name: string`, `succeeded: bool` | Завершение вызова инструмента |
| `fallback` | `model: string` | Переключение на резервный маршрут |
| `context_compressed` | `reason: "overflow" \| "midturn" \| "skipped"` | Сжатие истории до хода или внутри хода |
| `iteration_limit` | `limit: int` | Исчерпан бюджет итераций, запрошена сводка |
| `warning` | `message: string` | Предупреждение (например, недоступный MCP-сервер или отключённое редактирование секретов) |
| `usage` | `input_tokens: int`, `output_tokens: int` | Суммарные токены завершённого хода |
| `session` | `id: string` | Идентификатор сохранённой или продолженной сессии |
| `cancelled` | `exit_code: 130` | Ход прерван по `SIGINT` |
| `error` | `message: string` | Ошибка подготовки запроса или исполнения хода |
| `done` | `exit_code: int`, `status: "completed" \| "iteration_limit" \| "cancelled" \| "error"` | Заключительное событие каждого запуска |

### Python SDK-обёртка

Готовая обёртка без сторонних зависимостей находится в [examples/extensions/hermes_sdk.py](../examples/extensions/hermes_sdk.py):

```python
from examples.extensions.hermes_sdk import HermesClient

client = HermesClient(
    binary="/path/to/hermes",
    profile_home="/path/to/isolated-profile",
    workdir="/path/to/project",
)

first = client.run_turn(
    "Прочитай fixture.txt",
    save_session=True,
    on_event=lambda ev: print(ev.event, ev.data),
)
second = client.run_turn(
    "Продолжи разбор",
    resume=first.session_id,
)
```

- **Изоляция профиля**: `HermesClient` формирует минимальное окружение дочернего процесса (не пробрасывает случайные ключи и чужой `HERMES_HOME` из окружения вызывающего процесса) и привязывает вызов к явному `profile_home` или `profile` (`-p`).
- **Отмена**: `TurnHandle.cancel()` или `cancel_event: threading.Event` отправляют `SIGINT` процессу `hermes` (при подключении через `hermes owner` сигнал передаётся владельцу, освобождается аренда SQLite и отменяются активные MCP-вызовы), возвращается событие `cancelled` и код `130`.
- **Отсутствие бинарника**: при недоступном пути к `hermes` выбрасывается `HermesBinaryNotFoundError`.

---

## 5. Плагины эталона с зависимостью от внутренних Python-объектов

Плагины Python-эталона, которые импортируют внутренние классы или регистрируют колбэки в памяти процесса Python, не загружаются в Rust CLI напрямую. Ниже перечислены категории таких расширений и требуемые изменения:

| Категория плагина в эталоне | Зависимость от внутренних Python-объектов | Статус в Rust CLI | Требуемые изменения |
| --- | --- | --- | --- |
| Markdown-навыки (`skills/*/SKILL.md`) | Нет (кроме `inline_shell`) | Поддержано | Без изменений; вынести команды из `inline_shell` в явный инструмент или MCP-сервер |
| Пользовательские инструменты (`tools/registry.py`, `register_tool`) | Внутренний реестр инструментов и контекст `AIAgent` | Замена через MCP | Обернуть инструмент в MCP-сервер (`stdio` или `Streamable HTTP`) и подключить в `mcp_servers` |
| Скрипты, вызывающие `AIAgent.run_conversation` | Класс `AIAgent`, синхронные Python-колбэки событий | Замена через `--events-json` / SDK | Перейти на `hermes chat --events-json` или [hermes_sdk.py](../examples/extensions/hermes_sdk.py); при необходимости хранить историю в профиле через `--save-session` / `--resume` |
| Хуки жизненного цикла (`plugins/`, `hooks/`: `pre_tool_call`, `post_turn` и др.) | Прямая мутация сообщений, аргументов и состояния `AIAgent` в памяти | Не поддержано | Перенести проверку/логику во внешний MCP-инструмент, политику `approvals.*` или обёртку поверх `--events-json` (хуки мутации внутри цикла хода в CLI не предусмотрены) |
| Плагины памяти (`memory_provider`, `holographic`, `fact_store`) | Python-интерфейс `MemoryProvider`, прямое чтение контекста хода | Не поддержано | Оставаться на встроенной памяти `MEMORY.md`/`USER.md` или вынести внешнее хранилище фактов в MCP-сервер (с инструментами поиска/записи) |
| Платформенные адаптеры (Telegram, Discord, Slack и др.) | `plugins/platforms/`, `gateway/` | Не поддержано (этап 6) | Перенос в Rust gateway на этапе 6 или подключение внешнего бота-моста через `hermes chat --events-json` / `hermes owner` |
| Пользовательские провайдеры LLM (`providers/`) | Классы адаптеров и хуки модификации тела/сообщений | Частично (`providers:` / `custom_providers:`) | Для OpenAI/Anthropic/Responses-совместимых API описать маршрут в `providers:` (`base_url`, `extra_body`, `extra_headers`); провайдеры с переписыванием сообщений (NVIDIA, кастомный OAuth) требуют реализации в `hermes-runtime` или локального HTTP-прокси |
| Расширения CLI/TUI (`quick_commands`, кастомные slash-команды, `display.*`) | Внутренний цикл `cli.py` / Ink TUI | Не поддержано | Использовать внешние shell-алиасы или обёртку поверх CLI; для `quick_commands` и `inline_shell` требуется отдельное решение по политике подтверждений |

---

## Проверка

Сквозной набор тестов для всех пунктов контракта находится в [compat/test_extensions.py](../compat/test_extensions.py):

```bash
cargo build --locked -p hermes-cli
python3 -m unittest -v compat.test_extensions
```
