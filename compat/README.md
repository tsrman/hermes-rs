# Проверка совместимости хранилища

Фикстура создаётся публичным API хранения зафиксированного Python-эталона. Требуются его checkout, Python 3.14 и зависимости `ruamel.yaml==0.18.16`, `ruamel.yaml.clib==0.2.15` из upstream `pyproject.toml` для Linux x86_64. Команды ниже используют отдельные каталоги, которые ещё не существуют.

```bash
/tmp/hermes-reference-venv/bin/python compat/create_reference_fixture.py /tmp/hermes-main /tmp/hermes-profile-lumio
cp compat/session_import.rs /tmp/hermes-lumio/crates/hermes-agent/tests/compat_session_import.rs
cd /tmp/hermes-lumio
HERMES_COMPAT_PROFILE=/tmp/hermes-profile-lumio cargo test -p hermes-agent --locked --no-default-features --test compat_session_import
```

Для Ultra сгенерировать новый профиль и повторить тест в `/tmp/hermes-ultra`. Тест может создавать файлы в указанном профиле, поэтому использовать только одноразовую копию. Результат проверяет путь `SessionPersistence::load_session`; другие пути чтения сессий требуют своих тестов.

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

Для запуска Python-эталона в созданном отдельно окружении использовать `run_reference_turn.py CHECKOUT NEW_PROFILE http://127.0.0.1:PORT/v1`. Скрипт проверяет коммит и создаёт новый профиль, а затем выполняет один запрос через `AIAgent.run_conversation`. Для режима `tool` создать `fixture.txt` в текущем рабочем каталоге процесса. Сервер показывает все запросы, включая содержимое prompt и результатов инструментов, поэтому использовать для таких проверок только тестовые данные.

Для сравнения исчерпания бюджета запустить сервер со сценарием `iteration_limit` и передать `--max-turns 2` в `run_reference_turn.py` и `hermes chat`. Для каждого клиента запустить свежий экземпляр сервера. В `/requests` первые два запроса содержат обычную историю, а третий добавляет запрос на сводку после второго результата инструмента.
