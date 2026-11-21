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

## Предварительная проверка и миграция профиля

В Linux команды работают с профилем, выбранным через `HERMES_HOME` или `-p`.

```bash
HERMES_HOME=/path/to/source-profile hermes profile check
pilot_parent="$(mktemp -d)"
HERMES_HOME=/path/to/source-profile hermes profile migrate --output "$pilot_parent/profile"
HERMES_HOME="$pilot_parent/profile" hermes sessions search 'привет'
```

`profile check` печатает JSON с инвентарём путей, правами доступа и признаком `ready`. `shared_access` отмечает права группы или других пользователей. Отчёт о секретах содержит только наличие и права доступа. При блокирующей ошибке проверка возвращает код 1. Проверка базы выполняется на временной копии.

`profile migrate` публикует новый каталог вне исходного профиля, когда все проверки пройдены. Копия включает `config.yaml`, `.env`, `SOUL.md`, `memories/`, `skills/` и `state.db` при их наличии. Хранилища авторизации отмечены в отчёте как `inspect_only`. Каталоги и файлы копии доступны владельцу. Перед копированием профиля для пилота остановите процессы, которые могут менять его файлы. Ссылки и специальные файлы внутри копируемых путей блокируют операцию.

Снимок SQLite включает зафиксированные данные из WAL. Поддерживаются схемы 32 и 33 с FTS 3. Схема 32 обновляется в копии, схема 33 сохраняется. В JSON-отчёте указаны обе версии схемы, число сессий и сообщений и результаты проверки истории и FTS. [Проверки на эталонных профилях](matrix.md#проверка-и-миграция-профиля)

## Пилот переключения на Rust CLI

1. На изолированной копии профиля остановите приём новых запросов Python. Дождитесь сохранения текущего хода и запишите ID контрольной сессии.
2. Остановите процесс Python и проверьте, что он завершился. Если переключение прервалось, перед повтором сверяйте состояние процесса и сохранённой истории.
3. Удерживайте блокировку переключения до готовности Rust. Например, запустите `flock -n /path/to/pilot.lock bash` и выполните проверку, миграцию и контрольный ход из этой оболочки. Все операторы должны использовать один и тот же файл блокировки.
4. Выполните `profile check` и `profile migrate` в новый каталог. После прерывания до публикации можно повторить команду с тем же выходным путём. Временный каталог `.hermes-migrate-*` может остаться в родительской папке. Удаляйте его после проверки, что процесс миграции завершился. Опубликованную копию проверьте через `profile check` и сравните контрольную сессию с источником.
5. Запустите `hermes owner` для копии в другом терминале. Выполните `hermes chat --oneshot --resume <id> -q 'Проверка пилота'`, затем проверьте `sessions show <id>` и `sessions search`. После аварии владельца до начала хода запустите его повторно. Для прерванного хода сначала сверяйте историю и эффекты инструментов. [Аренда сессии](#локальный-владелец-сессий)

Репетиция с принудительными прерываниями создаёт одноразовый профиль из зафиксированного Python-эталона и использует локальный имитатор.

```bash
python3 compat/rehearse_profile_pilot.py target/x86_64-unknown-linux-gnu/release/hermes \
  /tmp/hermes-reference-venv/bin/python /tmp/hermes-main
```

Для musl укажите бинарник из `target/x86_64-unknown-linux-musl/release/`. Результаты прерываний записаны в [матрице совместимости](matrix.md#пилот-переключения-на-rust-cli).

## Контрольный возврат на Python

1. Завершите текущий ход, остановите владельца Rust и возьмите ту же блокировку, что при переключении на Rust. Сохраните копию профиля после Rust командой `hermes profile migrate --output <архивный-каталог>`.
2. Для проверенного Python схемы 33 откройте рабочую копию после Rust. Сверьте контрольные сессии с архивом, возобновите их по ID и проверьте поиск.
3. Для возврата к Python схемы 32 восстановите отдельную копию снимка до переключения. Экспортируйте новые сессии из архива Rust через `maintenance.export_session` у Python схемы 33 и преобразуйте результат функцией `to_legacy_export_dict`. Импортируйте его через `SessionDB.import_sessions` у Python схемы 32. Импорт пропускает существующие ID. Поздние Rust-ходы в этих сессиях остаются доступны в архиве схемы 33.
4. Сверьте выполненные действия инструментов по истории и состоянию рабочих файлов. В репетиции созданный Rust файл сохранился при возврате на Python, а его вызов и результат передались в историю без повторного исполнения.

```bash
python3 compat/rehearse_profile_rollback.py target/x86_64-unknown-linux-gnu/release/hermes \
  /tmp/hermes-reference-venv/bin/python /tmp/hermes-main /tmp/hermes-v32.wAEl1m
```

Скрипт проверяет оба варианта на одноразовых профилях и локальном имитаторе. Для musl укажите бинарник из `target/x86_64-unknown-linux-musl/release/`. [Результаты](matrix.md#контрольный-возврат-на-python)

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

## Подтверждение записи файлов

В Linux команда `hermes chat -q 'Обнови notes.txt'` предлагает модели `write_file` при запуске из интерактивного терминала. На экране показываются полный путь и новое содержимое файла. Введите `y` или `yes`, чтобы разрешить одну запись. Пустой ответ означает отказ, а `Ctrl+C` отменяет ход. При `--oneshot` и при перенаправлении ввода запись скрыта. Таймаут задаётся в `approvals.timeout` в секундах и по умолчанию составляет 300 секунд.

Запись поддерживает UTF-8-файлы до 64 КиБ в существующих каталогах внутри рабочей папки. Перезапись требует полного чтения той же версии файла в текущем ходе. После подтверждения CLI повторно проверяет исходное содержимое и заменяет файл атомарно. Файлы профиля, секреты, метаданные Git и пути записи через символические ссылки защищены. Через локального владельца подтверждение отображается в терминале вызывающего клиента.

## Поиск, правки и запуск команд

- `search_files` ищет по регулярному выражению в UTF-8-файлах или по маске имени. Результаты содержат пути относительно рабочей папки. Поддерживаются `limit`, `offset`, контекст вокруг совпадений и подсчёт совпавших строк по файлам. При обходе пропускаются скрытые файлы, защищённые пути и символические ссылки.
- `patch` заменяет точный фрагмент `old_string` на `new_string` в существующем файле до 64 КиБ. Для нескольких совпадений требуется `replace_all: true`. Действуют те же подтверждение и проверка исходной версии, что и для `write_file`. Rust CLI-бета поддерживает режим точной замены. Формат V4A, нечёткое сопоставление и автоматический линтинг остаются возможностями Python.
- `terminal` доступен в интерактивном CLI на Linux и macOS при `terminal.backend: local`. При отсутствии настройки учитывается `TERMINAL_ENV` из окружения, затем из `.env` профиля. По умолчанию используется `local`. Перед однократным запуском CLI показывает команду и рабочую папку. Команда выполняется через `/bin/sh` с правами пользователя, в выбранной папке проекта и с минимальным окружением. Таймаут выполнения по умолчанию составляет 180 секунд, допустимый диапазон от 1 до 600 секунд. При завершении, таймауте или отмене CLI завершает оставшиеся процессы в группе команды.

После сборки `cargo build --locked -p hermes-cli` запустите `python3 -m unittest -v compat.test_file_tools` на Linux или macOS. Для проверки только запуска команд укажите `compat.test_file_tools.TerminalToolTest`. Чтобы проверить бинарник из архива, задайте его путь в `HERMES_TEST_BINARY`.

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

## Установка CLI из GitHub Releases

Выберите цель из артефактов опубликованного тега и скачайте архив вместе с файлом SHA-256. `v0.2.0` включает все цели из таблицы. `v0.1.0` доступен для Linux x86_64 GNU и musl.

| Система | Цель |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` для glibc, `x86_64-unknown-linux-musl` для Alpine |
| Linux arm64 | `aarch64-unknown-linux-gnu` для glibc, `aarch64-unknown-linux-musl` для Alpine |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Windows x64 | `x86_64-pc-windows-msvc` |

Для Linux и macOS используйте архив `.tar.gz`.

```bash
tag=v0.2.0
target=x86_64-unknown-linux-gnu
archive="hermes-${tag#v}-$target.tar.gz"
url="https://github.com/tsrman/hermes-rs/releases/download/$tag"
(
  set -e
  download_dir="$(mktemp -d)"
  cd "$download_dir"
  curl -fsSLO "$url/$archive"
  curl -fsSLO "$url/$archive.sha256"
  if command -v sha256sum >/dev/null; then
    sha256sum --check "$archive.sha256"
  else
    shasum -a 256 -c "$archive.sha256"
  fi
  tar -xzf "$archive" hermes
  mkdir -p "$HOME/.local/hermes-rs/$tag"
  install -m 755 hermes "$HOME/.local/hermes-rs/$tag/hermes"
) && "$HOME/.local/hermes-rs/$tag/hermes" --help
```

Для Windows используйте `.zip` и распакуйте его в PowerShell.

```powershell
$Tag = 'v0.2.0'
$Archive = "hermes-$($Tag.Substring(1))-x86_64-pc-windows-msvc.zip"
$Url = "https://github.com/tsrman/hermes-rs/releases/download/$Tag"
Invoke-WebRequest "$Url/$Archive" -OutFile $Archive
Invoke-WebRequest "$Url/$Archive.sha256" -OutFile "$Archive.sha256"
$Expected = (Get-Content "$Archive.sha256").Split(' ')[0]
if ((Get-FileHash $Archive -Algorithm SHA256).Hash -ne $Expected) { throw 'SHA-256 mismatch' }
$InstallDir = Join-Path $env:LOCALAPPDATA "hermes-rs\$Tag"
Expand-Archive -LiteralPath $Archive -DestinationPath $InstallDir
& (Join-Path $InstallDir 'hermes.exe') --help
```

Устанавливайте каждую версию в свой каталог. Для обновления или возврата запускайте бинарник нужной версии по его пути. Профиль выбирается отдельно через `HERMES_HOME` или `-p`. По умолчанию в Windows используется `%USERPROFILE%\.hermes`.

Используйте отдельный профиль для [миграции](#предварительная-проверка-и-миграция-профиля) и [контрольного возврата на Python](#контрольный-возврат-на-python).

## Релизная проверка Linux x86_64

```bash
cargo build --release --locked -p hermes-cli --target x86_64-unknown-linux-gnu
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked -p hermes-cli --target x86_64-unknown-linux-musl
```

Workflow [Release CLI](../.github/workflows/release-cli.yml) проверяет workspace и собирает архивы с SHA-256 на нативных раннерах. Linux проверяется в контейнерах, macOS и Windows проверяются распаковкой и запуском на раннере. Вручную workflow можно запустить для проверки артефактов в Actions. Публикация запускается отправкой тега `v<версия hermes-cli>`. После неё workflow скачивает архивы и суммы из GitHub Releases и сверяет их с проверенными файлами. [События GitHub Actions](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow)

Упаковку и проверку архивов можно воспроизвести локально.

```bash
output="$(mktemp -d)"
bash compat/package_cli_release.sh x86_64-unknown-linux-gnu "$output"
bash compat/package_cli_release.sh x86_64-unknown-linux-musl "$output"
python3 -m compat.smoke_cli_release "$output/hermes-0.2.0-x86_64-unknown-linux-gnu.tar.gz" debian:trixie-slim
python3 -m compat.smoke_cli_release "$output/hermes-0.2.0-x86_64-unknown-linux-musl.tar.gz" alpine:3.24.2
```

Проверка запускает распакованный бинарник в контейнере без Python. Она проверяет отклонение недоверенного сертификата и успешный запрос к локальному HTTPS-имитатору после подключения тестового корня как системного хранилища. Затем проверяет сохранение сессии, `--resume` и поиск по истории.

Для проверки запуска на чистой системе использовать `debian:trixie-slim` для GNU и `alpine:3.24.2` для musl. В контейнер CLI смонтировать только соответствующий бинарник. Завести временный `HERMES_HOME` и отдельную рабочую папку. Для проверки записи, истории и `--resume` запустить `model_server.py --scenario local_tools` в отдельном Python-контейнере и подключить CLI к его сетевому пространству через `--network container:<id>`. Запрос на запись отправить через `docker exec -it`, чтобы CLI получил управляющий терминал для подтверждения.

Для живого теста передать тестовый ключ через `HERMES_API_KEY` и указать адрес Chat Completions через `--base-url`. Использовать временный профиль и синтетический запрос. Для проверки системных корней на Alpine использовать штатное хранилище CA образа.

### Замеры производительности

```bash
python3 compat/benchmark_cli.py target/x86_64-unknown-linux-gnu/release/hermes \
  /tmp/hermes-reference-venv/bin/python /tmp/hermes-main --samples 15
```

Скрипт измеряет запуск `--help` у обоих CLI, время до первого запроса к имитатору и полное время текстового хода. Ход Rust запускается через `hermes chat --oneshot`, ход Python через `run_reference_turn.py` и `AIAgent.run_conversation` в том же сценарии. Измерения включают запуск процессов и используют отдельный временный профиль на каждый прогон. RSS измеряется в отдельном запуске, пока процесс ждёт ответ имитатора. Базовое время HTTP на loopback измеряется отдельным запросом. [Результаты проверки](matrix.md#релизная-проверка-и-замеры-cli)
