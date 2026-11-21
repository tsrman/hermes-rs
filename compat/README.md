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

Проверка сервера выполняется командой `python3 -m unittest discover -s compat -p 'test_model_server.py' -v`. Для работы необходимы разрешённые loopback-соединения.

Для запуска Python-эталона в созданном отдельно окружении использовать `run_reference_turn.py CHECKOUT NEW_PROFILE http://127.0.0.1:PORT/v1`. Скрипт проверяет коммит и создаёт новый профиль, а затем выполняет один запрос через `AIAgent.run_conversation`. Для режима `tool` создать `fixture.txt` в текущем рабочем каталоге процесса. Сервер показывает все запросы, включая содержимое prompt и результатов инструментов, поэтому использовать для таких проверок только тестовые данные.
