# Подпись Windows-бинарников (`hsr.exe`, `hsr-gui.exe`) самоподписанным сертификатом

Релизные воркфлоу [release-cli.yml](../.github/workflows/release-cli.yml) и [release-gui.yml](../.github/workflows/release-gui.yml) автоматически подписывают `hsr.exe` и `hsr-gui.exe` через [.github/scripts/sign-windows.ps1](../.github/scripts/sign-windows.ps1), если в GitHub Secrets заданы `WINDOWS_CERT_PFX_B64` и `WINDOWS_CERT_PASSWORD`. Если секреты не настроены, шаг подписи пропускается.

---

## 1. Генерация сертификата на Linux

Создайте временную защищённую папку **вне репозитория**, чтобы приватные ключи случайно не попали в git:

```bash
mkdir -p ~/.hermes-signing
chmod 700 ~/.hermes-signing
cd ~/.hermes-signing
```

Сгенерируйте самоподписанный сертификат Code Signing (RSA 3072, SHA-256, срок действия 5 лет):

```bash
openssl req -x509 -newkey rsa:3072 -sha256 -days 1825 -nodes \
  -subj "/CN=Hermes Local Signing" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=codeSigning" \
  -addext "subjectKeyIdentifier=hash" \
  -keyout hermes-codesign.key \
  -out hermes-codesign.crt

chmod 600 hermes-codesign.key
```

Упакуйте ключ и сертификат в `.pfx` (OpenSSL запросит пароль — запомните его для `WINDOWS_CERT_PASSWORD`):

```bash
openssl pkcs12 -export \
  -out hermes-codesign.pfx \
  -inkey hermes-codesign.key \
  -in hermes-codesign.crt

chmod 600 hermes-codesign.pfx
```

Сделайте публичную копию `.cer` (удобное расширение для Windows) и закодируйте `.pfx` в base64 в одну строку для GitHub Secrets:

```bash
cp hermes-codesign.crt hermes-codesign.cer
base64 -w 0 hermes-codesign.pfx > hermes-codesign.pfx.b64
chmod 600 hermes-codesign.pfx.b64
```

В результате в `~/.hermes-signing/` будут файлы:
- `hermes-codesign.cer` — **публичный** сертификат (его устанавливают на Windows-машинах).
- `hermes-codesign.pfx` и `hermes-codesign.pfx.b64` — **приватный** архив для GitHub Actions (храните в секрете).
- `hermes-codesign.key` — **приватный** ключ (храните в секрете).

---

## 2. Добавление секретов на GitHub

### Вариант A — через веб-интерфейс GitHub

1. Откройте репозиторий на GitHub → **Settings** → **Secrets and variables** → **Actions**.
2. Нажмите **New repository secret** и добавьте два секрета:
   - **`WINDOWS_CERT_PFX_B64`** — вставьте содержимое файла `~/.hermes-signing/hermes-codesign.pfx.b64` целиком.
   - **`WINDOWS_CERT_PASSWORD`** — вставьте пароль, который вы задали при создании `hermes-codesign.pfx`.

### Вариант B — через GitHub CLI (`gh`)

```bash
gh secret set WINDOWS_CERT_PFX_B64 < ~/.hermes-signing/hermes-codesign.pfx.b64
gh secret set WINDOWS_CERT_PASSWORD
```

*(Команда `gh secret set WINDOWS_CERT_PASSWORD` безопасно запросит пароль интерактивно без сохранения в истории shell.)*

---

## 3. Проверка подписи в GitHub Actions

После добавления секретов можно проверить сборку без создания нового тега:

1. Откройте **Actions** → **Release GUI** (или **Release CLI**) → **Run workflow** → ветка `main`.
2. В логе шага **Build and package (Windows MSVC)** должна появиться строка:
   ```text
   Signed ...\hsr-gui.exe with CN=Hermes Local Signing (thumbprint ..., status=...)
   ```
3. Все последующие релизы по тегам `v*` и `gui-v*` будут подписываться автоматически.

---

## 4. Установка публичного сертификата на Windows (один раз)

Чтобы Windows доверяла подписи вашего самоподписанного сертификата, скопируйте **только** публичный `hermes-codesign.cer` на целевой ПК и установите его в PowerShell.

**Для всех пользователей (PowerShell от имени администратора):**
```powershell
Import-Certificate -FilePath .\hermes-codesign.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate -FilePath .\hermes-codesign.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
```

**Только для текущего пользователя (без прав администратора):**
```powershell
Import-Certificate -FilePath .\hermes-codesign.cer -CertStoreLocation Cert:\CurrentUser\Root
Import-Certificate -FilePath .\hermes-codesign.cer -CertStoreLocation Cert:\CurrentUser\TrustedPublisher
```

Проверить подпись скачанного `.exe` на Windows:
```powershell
Get-AuthenticodeSignature .\hsr-gui.exe | Format-List Status, StatusMessage, SignerCertificate, TimeStamperCertificate
```
После установки `.cer` поле `Status` будет `Valid`.

---

## 5. Безопасность и ротация

- Никогда не коммитьте `.key`, `.pfx` и `.pfx.b64` в git.
- После загрузки секретов в GitHub файл `hermes-codesign.pfx.b64` можно удалить (`rm ~/.hermes-signing/hermes-codesign.pfx.b64`), сохранив резервную копию `hermes-codesign.pfx` и `hermes-codesign.cer` в менеджере паролей.
- Подпись ставится с RFC 3161 меткой времени (`timestamp.digicert.com` / `timestamp.sectigo.com`), поэтому уже подписанные релизы останутся валидными и после истечения срока действия сертификата. Для ротации просто повторите шаги 1–4 с новым сертификатом.
