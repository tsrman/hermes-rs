import os
import shutil
import ssl
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path

from compat.model_server import Handler


ROOT = Path(__file__).resolve().parent.parent
BINARY = ROOT / "target/debug/hermes"
OPENSSL = shutil.which("openssl")
BWRAP = shutil.which("bwrap")


def create_test_certificates(certs):
    def openssl(*args):
        subprocess.run([OPENSSL, *args], cwd=certs, check=True, capture_output=True, timeout=10)

    for name in ("trusted", "unrelated"):
        openssl(
            "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
            "-nodes", "-subj", f"/CN=Hermes test {name}", "-days", "2",
            "-keyout", f"{name}.key", "-out", f"{name}.pem",
            "-addext", "basicConstraints=critical,CA:TRUE",
            "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        )
    openssl(
        "req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
        "-nodes", "-subj", "/CN=Hermes test server", "-keyout", "server.key", "-out", "server.csr",
    )
    (certs / "server.ext").write_text(
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
        "extendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1\n",
        encoding="utf-8",
    )
    openssl(
        "x509", "-req", "-in", "server.csr", "-CA", "trusted.pem", "-CAkey", "trusted.key",
        "-CAcreateserial", "-days", "2", "-extfile", "server.ext", "-out", "server.pem",
    )


class UnavailableModel(Handler):
    def do_POST(self):
        self.send_json(503, {"error": "fixture primary unavailable"})


@unittest.skipUnless(sys.platform.startswith("linux") and OPENSSL, "requires Linux and openssl")
class SystemRootsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(directory.cleanup)
        cls.certs = Path(directory.name)
        (cls.certs / "empty-cert-dir").mkdir()
        create_test_certificates(cls.certs)

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name)
        self.tls = self.start_server(Handler, tls=True)

    def start_server(self, handler, *, tls=False):
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.addCleanup(server.server_close)
        server.scenario = "text"
        server.requests = []
        if tls:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(self.certs / "server.pem", self.certs / "server.key")
            server.socket = context.wrap_socket(server.socket, server_side=True)
        worker = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
        worker.start()
        self.addCleanup(worker.join, 2)
        self.addCleanup(server.shutdown)
        return server

    def chat(self, address, roots=None, fallback="", *, prefix=()):
        (self.home / "config.yaml").write_text(f"agent:\n  api_max_retries: 1\n{fallback}", encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "HERMES_HOME": str(self.home), "HERMES_API_KEY": "fixture"}
        if roots is not None:
            env["SSL_CERT_FILE"] = str(self.certs / roots)
            env["SSL_CERT_DIR"] = str(self.certs / "empty-cert-dir")
        return subprocess.run(
            [*prefix, str(BINARY), "chat", "--oneshot", "-q", "TLS fixture", "--model", "primary",
             "--base-url", address + "/v1"],
            cwd=self.home, capture_output=True, text=True, timeout=10, env=env,
        )

    def test_primary_accepts_root_from_os_certificate_bundle(self):
        result = self.chat(f"https://127.0.0.1:{self.tls.server_port}", "trusted.pem")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "Ответ модели\n")
        self.assertEqual(len(self.tls.requests), 1)

    def test_primary_rejects_untrusted_root_and_wrong_hostname(self):
        for host, bundle, error in (("127.0.0.1", "unrelated.pem", "UnknownIssuer"),
                                    ("localhost", "trusted.pem", "NotValidForName")):
            with self.subTest(host=host, bundle=bundle):
                result = self.chat(f"https://{host}:{self.tls.server_port}", bundle)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(error, result.stderr)
        self.assertEqual(self.tls.requests, [])

    def test_fallback_also_validates_against_os_certificate_bundle(self):
        primary = self.start_server(UnavailableModel)
        address = f"http://127.0.0.1:{primary.server_port}"
        fallback = ("fallback_providers:\n  - provider: custom\n    model: backup\n"
                    f"    base_url: https://127.0.0.1:{self.tls.server_port}/v1\n")
        rejected = self.chat(address, "unrelated.pem", fallback)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("UnknownIssuer", rejected.stderr)
        self.assertEqual(self.tls.requests, [])
        accepted = self.chat(address, "trusted.pem", fallback)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        self.assertEqual([request["model"] for request in self.tls.requests], ["backup"])

    @unittest.skipUnless(BWRAP, "requires bubblewrap for an isolated system trust store")
    def test_default_linux_system_store_is_used(self):
        namespace = [BWRAP, "--unshare-user", "--ro-bind", "/", "/"]
        probe = subprocess.run([*namespace, "/bin/true"], capture_output=True, timeout=5)
        if probe.returncode != 0:
            self.skipTest("user namespaces unavailable")

        store = self.home / "system-certificates"
        store.mkdir()
        bundle = store / "ca-certificates.crt"
        command = [*namespace, "--ro-bind", str(store), "/etc/ssl/certs"]
        address = f"https://127.0.0.1:{self.tls.server_port}"

        shutil.copyfile(self.certs / "unrelated.pem", bundle)
        rejected = self.chat(address, prefix=command)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("UnknownIssuer", rejected.stderr)
        self.assertEqual(self.tls.requests, [])

        shutil.copyfile(self.certs / "trusted.pem", bundle)
        accepted = self.chat(address, prefix=command)
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        self.assertEqual(len(self.tls.requests), 1)
