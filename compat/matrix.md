# Матрица совместимости Hermes Agent и Rust-портов

Эталон для работы — `NousResearch/hermes-agent@2f8c1e7ed051c311af4f3b3ca0f8976b9c40839b`. Кандидаты — `Lumio-Research/hermes-agent-rs@9a145877cacfde43efa07115536b71c51344de75` и `sheawinkler/hermes-agent-ultra@4f83eedf9cfafa82d81b3f2ea8ada0027536c99a` (`v0.22.0`). Статусы ниже основаны на чтении исходников. Для подтверждения поведения нужны тесты на одном и том же эталоне.

| Приоритет | Возможность | Исходник эталона | Проверка | Статус |
| --- | --- | --- | --- | --- |
| P0 | CLI, один ход и потоковый ответ | `hermes_cli/`, `agent/conversation_loop.py` | Локальный провайдер, сравнение запросов и событий | Не проверено |
| P0 | Вызовы инструментов и подтверждения | `model_tools.py`, `tools/registry.py`, `tools/approval.py` | Чтение файла, запрет, отмена, повтор после эффекта | Не проверено |
| P0 | Настройки и секреты профиля | `hermes_cli/config.py`, `hermes_constants.py` | Два временных профиля, пустые значения, секреты и приоритет настроек | Не проверено |
| P0 | SQLite и FTS5, история и сжатие | `persistence/sqlite/` | Копия старой и текущей `state.db`, поиск, архивные поколения, `--resume` | Формат кандидатов отличается по имени файла и схеме, см. ниже |
| P0 | Память и prompt | `agent/prompt_builder.py`, `agent/memory_manager.py` | Снимки prompt, порядок инструкций, кеширование и сжатие | Не проверено |
| P1 | Канонический gateway и владельцы сессий | `gateway/session_authority.py`, `tui_gateway/contracts/` | Канонический wire, reconnect, approvals, `runtime_epoch`, отказ после захвата | Не проверено |
| P1 | TUI и Desktop | `ui-tui/`, `apps/desktop/`, `apps/shared/` | Запустить существующие клиенты против Rust gateway | Не проверено |
| P1 | Telegram и routing | `plugins/platforms/telegram/`, `gateway/` | Имитация платформы, авторизация, повторы и треды | Не проверено |
| P1 | OpenAI-compatible | `hermes_cli/runtime_provider.py`, `agent/` | SSE, разрывы UTF-8, tool calls, таймаут, retry и fallback | Не проверено |
| P2 | Anthropic, Codex и остальные провайдеры | `providers/`, `agent/anthropic_adapter.py` | Фикстуры запросов, streaming, OAuth и live smoke | Не проверено |
| P2 | MCP, browser, web, voice и среды исполнения | `tools/mcp_tool.py`, `tools/environments/`, `tools/` | Контракты протоколов, отмена, очистка процессов | Не проверено |
| P2 | Навыки и плагины | `skills/`, `plugins/`, `hermes_cli/plugins.py` | Discovery, lifecycle hooks, Python API и изоляция | Не проверено |
| P2 | Cron, Kanban и делегирование | `cron/`, `tools/delegate_tool.py` | Миграция, таймзоны, рестарт и внешние эффекты | Не проверено |
| P2 | Остальные каналы, ACP, dashboard, API и batch | `plugins/platforms/`, `acp_adapter/`, `web/`, `batch_runner.py` | Сквозные сценарии каждого заявленного клиента | Не проверено |

## Результаты чтения исходников

- Эталон использует `state.db`, SQLite schema version 33, независимую версию FTS и таблицы `runtime_epoch` и `session_admissions`. [Схема upstream](https://github.com/NousResearch/hermes-agent/blob/2f8c1e7ed051c311af4f3b3ca0f8976b9c40839b/persistence/sqlite/_schema_ddl.py)
- Lumio `SessionPersistence::new` открывает `sessions.db` и создаёт отдельную схему с `created_at`, `updated_at` и `platform`. Следовательно, автоматическую совместимость с профилями эталона считать неподтверждённой. [Исходник Lumio](https://github.com/Lumio-Research/hermes-agent-rs/blob/9a145877cacfde43efa07115536b71c51344de75/crates/hermes-agent/src/session_persistence.rs)
- Ultra `SessionPersistence::new` также открывает `sessions.db`. Совместимость остальных путей доступа и миграции требует отдельной проверки. [Исходник Ultra](https://github.com/sheawinkler/hermes-agent-ultra/blob/v0.22.0/crates/hermes-agent/src/session_persistence.rs)
- Lumio указывает эталон `v2026.4.13`, Ultra в своей опубликованной матрице указывает upstream `5e51b123f32b7f6a51fbd5759e89ba5146ce4003`. Эти проверки относятся к другим версиям upstream. [Lumio README](https://github.com/Lumio-Research/hermes-agent-rs/blob/9a145877cacfde43efa07115536b71c51344de75/README.md), [матрица Ultra](https://github.com/sheawinkler/hermes-agent-ultra/blob/v0.22.0/docs/parity/parity-matrix.md)

Сравнительный тест `session_import.rs` на свежем профиле Python для Lumio завершился ошибкой `no such table: messages`. `load_session` открыл `sessions.db`, отсутствовавший в исходном профиле. Пять собственных модульных тестов `session_persistence` Lumio прошли, однако они создают профиль Lumio и не проверяют импорт профиля Python. Общий запуск с фильтром `session_persistence` был прерван во время последующего property-теста, поэтому результат относится только к этим пяти модульным тестам. Команды воспроизведения приведены в `compat/README.md`.

Тот же тест для Ultra завершился несовпадением результатов. `load_session` вернул пустой список вместо двух сообщений Python. Это подтверждает расхождение конкретного API хранения, а проверка штатной команды `--resume` остаётся отдельной задачей.

Проверка собранных CLI показала, что оба порта отклоняют `hermes chat --resume compat-session` как неизвестный параметр. У Ultra есть отдельная команда `resume`. При её запуске в терминале на чистом профиле Python она завершилась ошибкой отсутствующего каталога `sessions/`, который предназначен для файлов сессий Ultra. Поддержка других способов импорта в этих наблюдениях не проверялась.

Один и тот же локальный сценарий `read_file` выполнен через `AIAgent.run_conversation` эталона и `hermes chat -q` обоих Rust-портов. Все три получили финальный ответ после передачи результата инструмента модели. Python отправил результат с полями `content`, `total_lines`, `file_size`, `truncated`, `is_binary`, `is_image`, `not_found`. Lumio и Ultra передали объект с единственным полем `result`. Эталон вызывался с отключённой памятью и локальными инструкциями, порты запускались со своими настройками по умолчанию. Поэтому число объявленных инструментов и prompt пока не являются сопоставимым измерением паритета. Lumio использовал модель `fixture`, Ultra заменил её на `gpt-5.5-pro`.

Решение о выборе базы остаётся открытым до выполнения сравнительных сценариев. Файлы профиля для экспериментов должны находиться во временных каталогах.
