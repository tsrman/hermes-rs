# Проверка совместимости хранилища

Фикстура создаётся публичным API хранения зафиксированного Python-эталона. Требуются его checkout, Python 3.14 и зависимости `ruamel.yaml==0.18.16`, `ruamel.yaml.clib==0.2.15` из upstream `pyproject.toml` для Linux x86_64. Команды ниже используют отдельные каталоги, которые ещё не существуют.

```bash
/tmp/hermes-reference-venv/bin/python compat/create_reference_fixture.py /tmp/hermes-main /tmp/hermes-profile-lumio
cp compat/session_import.rs /tmp/hermes-lumio/crates/hermes-agent/tests/compat_session_import.rs
cd /tmp/hermes-lumio
HERMES_COMPAT_PROFILE=/tmp/hermes-profile-lumio cargo test -p hermes-agent --locked --no-default-features --test compat_session_import
```

Для Ultra сгенерировать новый профиль и повторить тест в `/tmp/hermes-ultra`. Тест может создавать файлы в указанном профиле, поэтому использовать только одноразовую копию. Результат проверяет путь `SessionPersistence::load_session`; другие пути чтения сессий требуют своих тестов.

## Чтение истории в Rust

Для проверки `state.db`, созданной Python, собрать Rust CLI и использовать отдельную фикстуру профиля.

```bash
cargo build --locked -p hermes-cli
/tmp/hermes-reference-venv/bin/python compat/create_reference_fixture.py /tmp/hermes-main /tmp/hermes-profile-rust
HERMES_HOME=/tmp/hermes-profile-rust target/debug/hermes sessions show compat-session
HERMES_HOME=/tmp/hermes-profile-rust target/debug/hermes sessions search 'привет' --limit 10
```

Команды возвращают JSON и открывают базу в режиме чтения. `show` выводит активные строки в порядке записи. `--all` включает архивные и отменённые строки, а `--follow-compression` показывает последний сегмент цепочки сжатия. `search` принимает выражение SQLite FTS5 и находит активные и архивные сообщения за исключением результатов инструментов. Поддерживаются схемы 32 и 33. Версия схемы проверяется отдельно от формата индекса.

Для проверки схемы 32 подготовьте отдельный checkout зафиксированного коммита и создайте временный профиль историческим Python API.

```bash
git -C /tmp/hermes-main fetch --no-tags --depth=1 origin 10d25f8ad65fdf71304e9272959421a0c22e5d23
git -C /tmp/hermes-main worktree add --detach /tmp/hermes-v32 10d25f8ad65fdf71304e9272959421a0c22e5d23
/tmp/hermes-reference-venv/bin/python compat/create_reference_fixture.py /tmp/hermes-v32 /tmp/hermes-profile-v32 --schema-version 32
HERMES_HOME=/tmp/hermes-profile-v32 target/debug/hermes sessions show compat-legacy-root --all
HERMES_HOME=/tmp/hermes-profile-v32 target/debug/hermes sessions show compat-legacy-root --follow-compression
HERMES_HOME=/tmp/hermes-profile-v32 target/debug/hermes sessions search 'архивный'
```

Исторический профиль содержит архивное сообщение, сводку, продолжение и отдельную сессию субагента. Rust читает его без миграции схемы. `chat --save-session` и `chat --resume` требуют схему 33.

## Миграция копии базы сессий

`sessions migrate` переносит `state.db` со схемы 32 и форматом FTS 3 на схему 33. Команда берёт согласованный снимок SQLite с учётом зафиксированных записей WAL, проверяет целостность базы и обоих индексов FTS и сохраняет результат новым файлом. Исходный профиль открывается для чтения. Выходной файл и его служебные файлы SQLite должны отсутствовать.

```bash
migrated_profile="$(mktemp -d)"
HERMES_HOME=/tmp/hermes-profile-v32 target/debug/hermes sessions migrate --output "$migrated_profile/state.db"
HERMES_HOME="$migrated_profile" target/debug/hermes sessions show compat-legacy-root --all
```

Команда переносит только базу сессий. Для запуска `chat --resume` на копии укажите параметры модели и ключ в окружении или настройте отдельный профиль. Базы со старым встроенным FTS по-прежнему доступны через `sessions show` и `sessions search`.

## Запись нового хода

Для сохранения `hermes chat --oneshot -q 'Прочитай fixture.txt'` передайте `--save-session` и укажите отдельный временный профиль через `HERMES_HOME` или `-p`. После успешного хода команда печатает ID новой сессии в `stderr`. Запрос, вызовы и результаты инструментов, финальный ответ и обновление FTS входят в одну транзакцию. Отмена, обрыв потока и исчерпание лимита оставляют ход без записи.

Для существующей базы запись поддерживается при `schema_version=33` и готовом FTS-индексе версии 3. Новый `state.db` создаётся по DDL эталона. Чтобы повторить выгрузку, передайте `--schema-out /tmp/hermes-schema-v33.sql` в `create_reference_fixture.py` при создании нового профиля и сравните её с `crates/hermes-runtime/src/storage/schema_v33.sql`.

## Возобновление сессии

`hermes chat --resume <id> --oneshot -q 'Следующий вопрос'` принимает точный ID в выбранном профиле схемы 33 с FTS 3. Для цепочек сжатия команда выбирает последний сегмент. При обычном возобновлении модель получает сохранённый системный prompt и активную историю. Явный `--model` или `HERMES_MODEL` имеет приоритет перед сохранённой моделью. Адрес и ключ берутся из текущих настроек профиля, инструменты используют текущий рабочий каталог.

CLI использует аренду SQLite на время хода. Она обновляется каждые 60 секунд, освобождается после завершения или управляемой отмены и истекает за пять минут без обновления. Перед добавлением хода CLI проверяет, что сегмент, аренда и прочитанная история остаются актуальными. Для проверок конкурентной записи используйте `stall_on_resume` и автоматические тесты из `compat/test_cli.py`.

## Сжатие перед возобновлением

Порог вычисляется по `model.context_length` и `compression.threshold`. `compression.threshold_tokens` задаёт верхнюю границу порога или включает проверку, когда размер окна неизвестен. Для тестового профиля с длинной сохранённой историей подойдёт такая настройка.

```yaml
compression:
  threshold_tokens: 450
  protect_first_n: 2
  protect_last_n: 2
```

CLI оценивает размер запроса по байтам и отправляет завершённые ходы из середины истории на суммаризацию выбранной модели. Границы выравниваются по пользовательским ходам, включая связанные вызовы и результаты инструментов. Если сводка сокращает контекст, CLI перечитывает файлы prompt и памяти. SQLite сохраняет сводку, архив и новый завершённый ход одной транзакцией под прежним ID сессии. При сбое суммаризации ответ запрашивается с исходной историей. Настройки `compression.enabled: false`, `compression.in_place: false` и `compression.checkpoint_required: true` отключают этот режим.

## Локальный владелец сессий

На Unix запустите `HERMES_HOME=/tmp/hermes-profile-rust target/debug/hermes owner` в отдельном терминале. `chat`, `chat --save-session` и `chat --resume` с тем же профилем автоматически подключаются к сокету `rust-owner/owner.sock`. `stdout`, `stderr` и код выхода доступны в вызывающем CLI. Рабочий каталог и настройки модели берутся из вызова `chat`. Системные сертификаты и сетевое окружение доступны процессу владельца.

Второй владелец того же профиля отклоняется. При `Ctrl+C` или отключении клиента владелец отменяет незавершённый ход. После перезапуска новый клиент может возобновить сохранённую сессию по её ID. При принудительном завершении самого владельца во время хода аренда действует до истечения TTL. Команды для запущенного владельца проверяются через `python3 -m unittest -v compat.test_owner`.

## Локальный провайдер

Локальный сервер модели запускается командой `python3 compat/model_server.py --scenario text`. Он выводит свой адрес и отвечает на `/v1/chat/completions`. Режим `--scenario tool` сначала возвращает вызов `read_file`, а после получения сообщения с ролью `tool` — финальный ответ. Все принятые тела запросов доступны через `GET /requests`. Сервер слушает только `127.0.0.1` и хранит запросы в памяти до остановки.

Режим `partial_tool` обрывает поток после вызова инструмента, `stall` удерживает ответ для проверки отмены, а `tool_retry` возвращает ошибку 503 после результата инструмента и затем успешный ответ. Для проверки Rust CLI сначала выполнить `cargo build --offline --locked -p hermes-cli`, затем запустить `python3 -m unittest discover -s compat -p 'test_*.py' -v`.

CLI читает `fallback_providers` из `config.yaml` выбранного профиля. Поддерживаются `custom` с явным `base_url` и `openai` с ключом для указанного адреса. Ключ резервного маршрута можно задать через `key_env` в окружении или в `.env` профиля. `agent.api_max_retries` задаёт число попыток на маршрут, включая первую. По умолчанию их три. Значение `1` позволяет перейти к резервному маршруту после первой ошибки, допускающей переключение.

```yaml
agent:
  api_max_retries: 1
fallback_providers:
  - provider: custom
    model: backup
    base_url: http://127.0.0.1:8001/v1
    key_env: BACKUP_API_KEY
```

Сценарии `fallback_after_tool`, `fallback_auth`, `partial_text` и `disconnect_once` проверяют передачу результата инструмента, разделение ключей, реакцию на обрыв после вывода текста и повтор при разрыве соединения до ответа.

`hermes -p coder config set model.default fixture` меняет `config.yaml` выбранного профиля. `config set model ''` сохраняет пустую строку и оставляет остальные поля `model` на месте. Вложенные ключи задаются через точку, индексы существующих списков через `ключ.0.поле`, а точку в новом имени можно экранировать как `ключ\.с\.точками`. Значения остальных полей сохраняются. YAML форматируется заново, комментарии и оформление теряются. Секреты задаются в `.env` профиля или в окружении.

В Rust CLI события инструмента идут в `stderr`, ответ модели идёт в `stdout`. Лимит по умолчанию составляет 16 итераций. Положительное целое `agent.max_turns` в профиле меняет лимит, а `--max-turns N` имеет приоритет. Сценарий `iteration_limit` запрашивает сводку после последнего результата инструмента. CLI печатает сводку и возвращает код `1` для незавершённого хода. При `Ctrl+C` CLI возвращает `130`.

Проверка сервера выполняется командой `python3 -m unittest discover -s compat -p 'test_model_server.py' -v`. Для работы необходимы разрешённые loopback-соединения.

Проверки TLS на Linux запускаются командой `python3 -m unittest -v compat.test_tls` и генерируют одноразовые сертификаты через `openssl`. Они проверяют системные корни для основного и резервного маршрутов, а также отклонение сертификата с неверным именем хоста. При доступном `bwrap` отдельный тест подменяет `/etc/ssl/certs` только внутри тестового процесса и проверяет системный путь загрузки сертификатов по умолчанию.

Для запуска Python-эталона в созданном отдельно окружении использовать `run_reference_turn.py CHECKOUT NEW_PROFILE http://127.0.0.1:PORT/v1`. Скрипт проверяет коммит и создаёт новый профиль, а затем выполняет один запрос через `AIAgent.run_conversation`. Для режима `tool` создать `fixture.txt` в текущем рабочем каталоге процесса. Сервер показывает все запросы, включая содержимое prompt и результатов инструментов, поэтому использовать для таких проверок только тестовые данные.

Для сравнения исчерпания бюджета запустить сервер со сценарием `iteration_limit` и передать `--max-turns 2` в `run_reference_turn.py` и `hermes chat`. Для каждого клиента запустить свежий экземпляр сервера. В `/requests` первые два запроса содержат обычную историю, а третий добавляет запрос на сводку после второго результата инструмента.
