import json
import subprocess
import sys
import unittest
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen


class ModelServerTest(unittest.TestCase):
    def setUp(self):
        scenario = "tool" if self._testMethodName == "test_tool_call_followed_by_final_answer" else "text"
        self.server = subprocess.Popen(
            [sys.executable, str(Path(__file__).with_name("model_server.py")), "--port", "0", "--scenario", scenario],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.base = self.server.stdout.readline().strip()
        self.addCleanup(self.stop_server)

    def stop_server(self):
        self.server.terminate()
        self.server.communicate(timeout=5)

    def post(self, body):
        request = Request(
            self.base + "/v1/chat/completions",
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
        )
        return urlopen(request, timeout=5)

    def test_streaming_response_and_recorded_request(self):
        request = {"model": "fixture", "stream": True, "messages": [{"role": "user", "content": "Привет"}]}
        with self.post(request) as response:
            self.assertEqual(response.headers.get_content_type(), "text/event-stream")
            events = response.read().decode().split("\n\n")
        chunks = [json.loads(event[6:]) for event in events if event.startswith("data: {")]
        self.assertEqual(chunks[0]["choices"][0]["delta"]["content"], "Ответ модели")
        self.assertEqual(chunks[-1]["choices"][0]["finish_reason"], "stop")
        self.assertIn("data: [DONE]", events)
        with urlopen(self.base + "/requests", timeout=5) as response:
            self.assertEqual(json.load(response), [request])

    def test_non_streaming_response_and_bad_json(self):
        with self.post({"model": "fixture", "messages": [], "stream": False}) as response:
            result = json.load(response)
        self.assertEqual(result["choices"][0]["message"]["content"], "Ответ модели")
        request = Request(self.base + "/v1/chat/completions", data=b"{", headers={"Content-Type": "application/json"})
        with self.assertRaises(HTTPError) as error:
            urlopen(request, timeout=5)
        self.assertEqual(error.exception.code, 400)

    def test_tool_call_followed_by_final_answer(self):
        with self.post({"model": "fixture", "stream": True, "messages": []}) as response:
            events = response.read().decode().split("\n\n")
        chunks = [json.loads(event[6:]) for event in events if event.startswith("data: {")]
        call = chunks[0]["choices"][0]["delta"]["tool_calls"][0]
        self.assertEqual(call["function"], {"name": "read_file", "arguments": '{"path":"fixture.txt"}'})
        self.assertEqual(chunks[-1]["choices"][0]["finish_reason"], "tool_calls")
        messages = [{"role": "tool", "tool_call_id": call["id"], "content": "fixture content"}]
        with self.post({"model": "fixture", "stream": True, "messages": messages}) as response:
            final = response.read().decode()
        self.assertIn("Ответ модели", final)
        self.assertIn('"finish_reason": "stop"', final)


if __name__ == "__main__":
    unittest.main()
