#!/usr/bin/env python3
"""Record the Python reference's redaction of a corpus of synthetic strings.

Usage: create_redaction_corpus.py <reference checkout> <output.json>
Run it with the reference interpreter. Every secret below is made up. The output maps each
category to `{"input": ..., "force": ..., "output": ...}` rows that the Rust redactor must
reproduce; categories are the passes of `redact_credential_shapes`.
"""

import json
import os
import sys
import tempfile
from pathlib import Path

A = "abcdefghijklmnop1234567890"
CORPUS = {
    "prefix": [
        f"key sk-{A}", f"sk-{A} at the start", f"token=ghp_{A}", f"github_pat_{A}_xyz",
        f"gho_{A} ghu_{A} ghs_{A} ghr_{A}", f"slack xoxb-1234567890-{A}", f"xapp-1-{A}",
        f"AIza{A}ABCDEFGHIJ", f"GOCSPX-{A}", f"ya29.a.{A}ABCDEFGHIJ", f"1//0{A}ABCDEF",
        f"pplx-{A}", f"fal_{A}", f"fc-{A}", f"bb_live_{A}", f"gAAAA{A}ABCDEFGH==",
        "AKIAABCDEFGHIJKLMNOP", f"sk_live_{A}", f"sk_test_{A}", f"rk_live_{A}",
        f"SG.{A}.{A}", f"hf_{A}", f"r8_{A}", f"npm_{A}", f"pypi-{A}", f"dop_v1_{A}", f"am_org_{A}",
        f"tvly-dev-{A}", f"exa_{A}", f"gsk_{A}", f"xai-{A}{A}", f"glpat-{A}", f"glrt-{A}.{A}",
        f"glsa_{A}_abcdef12", f"pk-lf-{A}", f"ctx7sk-{A}",
        f"backup-sk-{A}", "scikit-learn is installed", "sk-short", "task-abcdefghijklmnopqrstu",
        f"(sk-{A}) and [ghp_{A}].", f"sk-{A}. Next sentence", f"two sk-{A} and sk-{A[::-1]}",
        f"sk-abc\x1bdef{A}", f"ghp_{A[:8]}​{A[8:]}", f"prefix_ghp_{A}_suffix",
        "sk-prod-deployment-01 is a name", "ghp_ with nothing", "см. ключ sk-" + A + " дальше",
    ],
    "private_key": [
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\nxyz987\n-----END RSA PRIVATE KEY-----",
        "before\n-----BEGIN PRIVATE KEY-----\nQUJDREVGR0hJSktMTU5PUA==\n-----END PRIVATE KEY-----\nafter",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----",
        "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkq\n-----END PUBLIC KEY-----",
        "AGE-SECRET-KEY-1" + "QPZRY9X8GF2TVDW0S3JN54KHCE6MUA7L" * 2,
    ],
    "jwt": [
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        "token eyJhbGciOiJIUzI1NiJ9 only header",
    ],
    "auth_header": [
        "Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456", "authorization: basic dXNlcjpwYXNzd29yZDEyMzQ1Ng==",
        "curl -H 'Authorization: Bearer " + A + "' https://x.test", "Authorization: Token " + A,
        "Proxy-Authorization: Bearer " + A, "authorization=Bearer " + A,
        "Authorization: Bearer short",
    ],
    "env_assignment": [
        f"OPENAI_API_KEY={A}", f"export GITHUB_TOKEN='{A}'", f"DB_PASSWORD=hunter2hunter2",
        f"MY_SECRET = \"{A}\"", "KEYBOARD=us", "PASSAGE=one", "TOKEN_LIMIT=4096", "API_KEY=",
        f"openai_key={A}", "export PATH=/usr/bin", "AWS_SECRET_ACCESS_KEY=" + A,
        "MAX_TOKENS=1000", "SECRET=${SECRET_FROM_ENV}", f"PW={A}",
        "SSH_AUTH_SOCK=$HOME/.ssh/agent.sock", "DOCKER_AUTH_CONFIG=/home/u/.docker", "API_KEY=true",
        "SESSION_TOKEN=os.getenv('SESSION')", "PGPASSWORD=s3cr3tpass", "DB_PASSWORD=/etc/db/pass",
        "AUTHOR=Smith", "CLIENT_SECRET=<your secret>", "SECRET_KEY=Zq8x4kLm2PvN7wRt", "ID_TOKEN=short",
        "CACHE_KEY=abcdef", "export SIGNING_KEY=\"Zq8x4kLm2PvN7wRt9s\"", "API_KEY=***",
        "A=1 B_TOKEN=Zq8x4kLm2PvN7wRt C=2", "app_secret=Zq8x4kLm2PvN7wRt9s", "tokenizer=cl100k_base",
        "AUTH_MODE=oauth2", "ENCRYPTION_KEY=/opt/keys/Zq8x4kLm2PvN7wRt/enc",
    ],
    "json_field": [
        json.dumps({"api_key": A}), json.dumps({"password": "hunter2hunter2", "user": "bob"}),
        json.dumps({"token": A, "nested": {"client_secret": A}}), '{"apiKey": "test"}',
        json.dumps({"safe": "value", "authorization": "Bearer " + A}),
        json.dumps(json.dumps({"access_token": A})),
    ],
    "cli_flag": [
        f"tool --password {A}", f"tool --token={A}", f"mysql -p{A} -u root", "run --api-key $KEY",
        f"docker login -u bob -p {A}", "tool --token 60", f"--secret '{A}'", "tool --password=${PW}",
        f"curl -u user:{A} https://x.test",
    ],
    "url": [
        f"https://user:{A}@example.com/path", f"https://example.com/cb?code={A}&state=ok",
        f"https://example.com/?access_token={A}", "https://example.com/plain/path?page=2",
        f"postgres://admin:{A}@db.internal:5432/app", f"redis://:{A}@cache:6379/0",
        f"mongodb+srv://u:{A}@cluster.example.net/db?retryWrites=true",
        f"https://example.com/hook?signature={A}", "ftp://anon@example.com/file",
    ],
    "connection_string": [
        f"Server=db;User Id=sa;Password={A};", f"host=localhost user=app password={A} dbname=x",
        f"DefaultEndpointsProtocol=https;AccountName=a;AccountKey={A}==;EndpointSuffix=core.windows.net",
        "Password=changeme", "password is a word in prose",
    ],
    "header_credential": [
        f"X-API-Key: {A}", f"Cookie: session={A}; theme=dark", f"X-Auth-Token: {A}",
        "Content-Type: application/json", f"Set-Cookie: sid={A}; HttpOnly",
    ],
    "telegram": [f"bot123456789:AA{A[:20]}ABCDEFGHIJKLMNO", "123456789:AAH" + "x" * 32],
    "python_repr": [
        "{'API_KEY': '" + A + "', 'name': 'x'}", "{'password': 'hunter2hunter2'}",
        "E   AssertionError: {'token': '" + A + "'}",
    ],
    "webhook": [
        "https://hooks.slack.com/services/T00000000/B00000000/" + "X" * 24,
        "https://discord.com/api/webhooks/123456789012345678/" + A + "ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    ],
    "phone": ["call +15551234567 now", "+4915112345678", "version +1.2.3", "id 12345678"],
    "plain": [
        "", "nothing secret here", "A normal sentence with = and : and // signs.",
        "path /usr/local/bin and https://example.com/docs", "***already masked***",
        "тест кириллицы и 日本語", "line one\nline two\r\nline three",
    ],
}


def main():
    reference, output = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    os.environ["HERMES_HOME"] = tempfile.mkdtemp()
    sys.path.insert(0, str(reference))
    os.chdir(reference)
    from agent.redact import redact_sensitive_text

    rows = {}
    for category, inputs in CORPUS.items():
        rows[category] = [{"input": text, "force": True, "output": redact_sensitive_text(text, force=True)}
                          for text in inputs]
    output.write_text(json.dumps(rows, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    print({category: len(items) for category, items in rows.items()})


if __name__ == "__main__":
    main()
