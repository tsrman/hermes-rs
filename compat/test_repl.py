"""The interactive conversation: several turns in one process, on a terminal and on a pipe."""
import fcntl
import json
import os
import pty
import select
import sqlite3
import subprocess
import sys
import termios
import time
import unittest
from urllib.request import urlopen

from test_file_tools import BINARY, ToolTestCase

PROMPT = "› ".encode()


class ReplBase(ToolTestCase):
    def start(self, *args, tty=True):
        command = [str(BINARY), "chat", *args]
        if not tty:
            process = subprocess.Popen(
                command, cwd=self.root, env=self.env, stdin=subprocess.PIPE,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.addCleanup(self.stop, process)
            return process, None

        master, slave = pty.openpty()
        self.addCleanup(os.close, master)

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        try:
            process = subprocess.Popen(
                command, cwd=self.root, env=self.env, stdin=slave, stdout=slave, stderr=slave,
                preexec_fn=controlling_terminal)
        finally:
            os.close(slave)
        self.addCleanup(self.stop_tty, process)
        self.transcript = bytearray()
        return process, master

    def stop_tty(self, process):
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)

    def wait_for(self, master, marker, count=1, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.transcript.count(marker) >= count:
                return
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    self.transcript.extend(os.read(master, 16384))
                except OSError:
                    break
        self.fail(f"{marker!r} x{count} not seen in {bytes(self.transcript)!r}")

    def finish(self, process, master, timeout=20):
        deadline = time.monotonic() + timeout
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.05)
            if ready:
                try:
                    self.transcript.extend(os.read(master, 16384))
                except OSError:
                    break
        self.assertIsNotNone(process.poll(), bytes(self.transcript))
        return process.returncode

    def requests(self):
        with urlopen(self.url + "/requests", timeout=4) as response:
            return json.load(response)

    def sessions(self):
        with sqlite3.connect(self.profile / "state.db") as db:
            return db.execute("SELECT id, message_count FROM sessions ORDER BY started_at").fetchall()



@unittest.skipUnless(sys.platform == "linux", "requires Linux terminals")
class ReplTest(ReplBase):
    scenario = "text"

    def test_three_turns_in_one_process_share_one_saved_session(self):
        process, master = self.start()
        self.wait_for(master, PROMPT)
        for index, line in enumerate(["first question", "second question", "third question"], 1):
            os.write(master, line.encode() + b"\n")
            self.wait_for(master, PROMPT, count=index + 1)
        os.write(master, b"\x04")
        self.assertEqual(self.finish(process, master), 0)
        requests = self.requests()
        self.assertEqual(len(requests), 3)
        users = [m["content"] for m in requests[2]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["first question", "second question", "third question"])
        sessions = self.sessions()
        self.assertEqual(len(sessions), 1, sessions)
        self.assertIn(b"hsr chat --resume " + sessions[0][0].encode(), bytes(self.transcript))
        self.assertEqual(self.transcript.count("Ответ модели".encode()), 3)

    def test_piped_input_runs_each_line_and_help_and_unknown_commands_do_not_call_the_model(self):
        process, _ = self.start(tty=False)
        out, err = process.communicate("one\n/help\n/nope\ntwo\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        self.assertIn("/new", out)
        self.assertIn("Unknown command /nope", err)
        self.assertRegex(err, r"\[\d+\.\ds[\] ·]")
        self.assertEqual(len(self.requests()), 2)
        self.assertEqual(len(self.sessions()), 1)

    def test_new_starts_another_conversation(self):
        process, _ = self.start(tty=False)
        _, err = process.communicate("a\n/new\nb\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        users = [[m["content"] for m in r["messages"] if m["role"] == "user"] for r in self.requests()]
        self.assertEqual(users, [["a"], ["b"]])
        self.assertEqual(len(self.sessions()), 2)

    def test_a_trailing_backslash_continues_the_input(self):
        process, _ = self.start(tty=False)
        _, err = process.communicate("line one\\\nline two\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        user = [m["content"] for m in self.requests()[0]["messages"] if m["role"] == "user"][-1]
        self.assertEqual(user, "line one\nline two")

    def test_the_session_can_be_resumed_by_a_later_conversation(self):
        first, _ = self.start(tty=False)
        first.communicate("remember apples\n", timeout=30)
        session = self.sessions()[0][0]
        second, _ = self.start("--resume", session, tty=False)
        _, err = second.communicate("and pears\n", timeout=30)
        self.assertEqual(second.returncode, 0, err)
        users = [m["content"] for m in self.requests()[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["remember apples", "and pears"])
        self.assertEqual(len(self.sessions()), 1)


    def test_model_sessions_and_yolo_commands(self):
        process, _ = self.start(tty=False)
        out, err = process.communicate(
            "/model\none\n/model other-model\ntwo\n/sessions\n/yolo\n/yolo\n/resume\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        self.assertEqual([r["model"] for r in self.requests()], ["fixture", "other-model"])
        self.assertIn("fixture\n", out)
        self.assertIn("Model for the next turns: other-model", out)
        self.assertIn(self.sessions()[0][0], out)
        self.assertIn("yolo on", out)
        self.assertIn("yolo off", out)
        self.assertIn("Usage: /resume", err)

    def test_continue_picks_up_the_latest_session_and_sessions_list_shows_it(self):
        first, _ = self.start(tty=False)
        first.communicate("about apples\n", timeout=30)
        second, _ = self.start("--continue", tty=False)
        _, err = second.communicate("and pears\n", timeout=30)
        self.assertEqual(second.returncode, 0, err)
        users = [m["content"] for m in self.requests()[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users, ["about apples", "and pears"])
        listing = subprocess.run([str(BINARY), "sessions", "list"], env=self.env, capture_output=True,
                                 text=True, timeout=10)
        self.assertEqual(listing.returncode, 0, listing.stderr)
        self.assertIn("about apples", listing.stdout)
        self.assertEqual(len(listing.stdout.strip().splitlines()), 1)

    def test_continue_without_a_saved_session_says_so(self):
        run = subprocess.run([str(BINARY), "chat", "--continue"], cwd=self.root, env=self.env,
                             input="", capture_output=True, text=True, timeout=10)
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("no saved session to continue", run.stderr)

    def test_a_missing_provider_explains_how_to_set_one_up(self):
        env = {k: v for k, v in self.env.items() if k != "HERMES_API_KEY"}
        (self.profile / "config.yaml").write_text("", encoding="utf-8")
        run = subprocess.run([str(BINARY), "chat", "-q", "hi"], cwd=self.root, env=env,
                             capture_output=True, text=True, timeout=10)
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("hsr config set model.base_url", run.stderr)


@unittest.skipUnless(sys.platform == "linux", "requires Linux terminals")
class ReplToolTest(ReplBase):
    scenario = "local_tools"

    def test_the_working_directory_survives_between_turns(self):
        (self.root / "sub").mkdir()
        process, _ = self.start("--yolo", tty=False)
        lines = [json.dumps({"path": "x", "content": "", "tool": "terminal", "arguments": {"command": command}})
                 for command in ("cd sub", "pwd")]
        _, err = process.communicate("\n".join(lines) + "\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        result, _ = self.last_tool_result()
        self.assertEqual(result["output"].strip(), str(self.root / "sub"))

    def test_exported_env_persists_between_turns_and_resets_on_new(self):
        process, _ = self.start("--yolo", tty=False)
        cmd = lambda c: json.dumps({"path": "x", "content": "", "tool": "terminal", "arguments": {"command": c}})
        script = "\n".join([
            cmd("export REPL_PERSIST_VAR=hello-world"),
            cmd('echo "seen=$REPL_PERSIST_VAR"'),
            "/new",
            cmd('echo "after_new=${REPL_PERSIST_VAR:-gone}"'),
        ]) + "\n"
        _, err = process.communicate(script, timeout=30)
        self.assertEqual(process.returncode, 0, err)
        tool_outputs = [
            json.loads(m["content"])["output"].strip()
            for r in self.requests()
            for m in r["messages"]
            if m.get("role") == "tool"
        ]
        # Each turn sends the whole session history; take the newest tool output from turn 2 and turn 3.
        self.assertIn("seen=hello-world", tool_outputs)
        self.assertEqual(tool_outputs[-1], "after_new=gone")

    def test_terminal_local_persistent_false_drops_exports_but_keeps_cwd(self):
        (self.root / "sub").mkdir()
        self.env = {**self.env, "TERMINAL_LOCAL_PERSISTENT": "false"}
        process, _ = self.start("--yolo", tty=False)
        cmd = lambda c: json.dumps({"path": "x", "content": "", "tool": "terminal", "arguments": {"command": c}})
        script = "\n".join([
            cmd("cd sub && export EPHEMERAL_VAR=nope"),
            cmd('echo "var=${EPHEMERAL_VAR:-gone}|$(pwd)"'),
        ]) + "\n"
        _, err = process.communicate(script, timeout=30)
        self.assertEqual(process.returncode, 0, err)
        result, _ = self.last_tool_result()
        self.assertEqual(result["output"].strip(), f"var=gone|{self.root / 'sub'}")


@unittest.skipUnless(sys.platform == "linux", "requires Linux terminals")
class ReplInterruptTest(ReplBase):
    scenario = "stall"

    def test_ctrl_c_stops_the_turn_and_returns_to_the_prompt(self):
        process, master = self.start()
        self.wait_for(master, PROMPT)
        os.write(master, b"this will hang\n")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not self.requests():
            time.sleep(0.05)
        self.assertTrue(self.requests(), "the model never saw the request")
        os.write(master, b"\x03")
        self.wait_for(master, b"interrupted")
        self.wait_for(master, PROMPT, count=2)
        self.assertIsNone(process.poll())
        os.write(master, b"/retry\n")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and len(self.requests()) < 2:
            time.sleep(0.05)
        self.assertEqual(len(self.requests()), 2, "/retry did not send the turn again")
        self.assertEqual(self.requests()[1]["messages"][-1]["content"], "this will hang")
        os.write(master, b"\x03")
        self.wait_for(master, PROMPT, count=3)
        os.write(master, b"/exit\n")
        self.assertEqual(self.finish(process, master), 0)


class PinnedSkillsTest(ReplBase):
    """`skills.auto_load` and the skills guidance of a conversation versus a `-q` run."""

    scenario = "text"

    def pin(self, extra="", names="[pinned-demo, ' pinned-demo ', missing-one, off-one]"):
        skill = self.profile / "skills" / "devtools" / "pinned-demo"
        (skill / "references").mkdir(parents=True)
        (skill / "references" / "a.md").write_text("a\n")
        (skill / "SKILL.md").write_text(
            "---\nname: pinned-demo\ndescription: Demo skill\n---\n\nUse ${HERMES_SKILL_DIR}/x.\n")
        other = self.profile / "skills" / "devtools" / "off-one"
        other.mkdir(parents=True)
        (other / "SKILL.md").write_text("---\nname: off-one\ndescription: Off\n---\n\nOff body\n")
        (self.profile / "config.yaml").write_text(
            self.config + f"skills:\n  auto_load: {names}\n  disabled: [off-one]\n{extra}")

    def system(self):
        return self.requests()[0]["messages"][0]["content"]

    def converse(self, env=None):
        if env:
            self.env = {**self.env, **env}
        process, _ = self.start(tty=False)
        _, err = process.communicate("hi\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        return err

    def test_a_conversation_pins_the_skill_once_and_warns_about_the_others(self):
        self.pin()
        err = self.converse()
        system = self.system()
        self.assertEqual(system.count('The "pinned-demo" skill is auto-loaded via config'), 1)
        self.assertIn(f"Use {self.profile}/skills/devtools/pinned-demo/x.", system)
        self.assertIn("- references/a.md", system)
        self.assertNotIn("Off body", system)
        self.assertIn("not found or disabled, skipped: missing-one, off-one", err)
        self.assertIn("Before replying, scan the skills below.", system)
        self.assertLess(system.index("is auto-loaded via config"), system.index("## Skills"))

    def test_a_single_query_gets_the_short_skills_guidance(self):
        self.pin()
        result = subprocess.run(
            [str(BINARY), "chat", "-q", "hi", "--oneshot"], cwd=self.root, env=self.env,
            capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        system = self.system()
        self.assertIn("Scan the skills below and load one with skill_view(name) only when", system)
        self.assertNotIn("Before replying, scan the skills below.", system)
        self.assertIn("is auto-loaded via config", system)

    def test_template_vars_off_keeps_the_token(self):
        self.pin(extra="  template_vars: false\n")
        self.converse()
        self.assertIn("Use ${HERMES_SKILL_DIR}/x.", self.system())

    def test_ignore_rules_and_a_disabled_skills_toolset_pin_nothing(self):
        self.pin()
        self.converse({"HERMES_IGNORE_RULES": "1"})
        self.assertNotIn("is auto-loaded via config", self.system())

    def test_without_the_skills_toolset_nothing_is_pinned(self):
        self.pin()
        (self.profile / "config.yaml").write_text(
            (self.profile / "config.yaml").read_text() + "agent:\n  disabled_toolsets: [skills]\n")
        self.converse()
        self.assertNotIn("is auto-loaded via config", self.system())
        self.assertNotIn("<available_skills>", self.system())

    def test_the_hermes_agent_skill_selects_the_skill_pointer_guidance(self):
        self.pin(names="[]")
        skill = self.profile / "skills" / "hermes-agent"
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text("---\nname: hermes-agent\ndescription: Help\n---\n\nBody\n")
        self.converse()
        self.assertIn("The `hermes-agent` skill has the actual commands", self.system())

    def test_inline_shell_is_not_run_and_is_reported(self):
        self.pin(extra="  inline_shell: true\n")
        skill = self.profile / "skills" / "devtools" / "pinned-demo" / "SKILL.md"
        skill.write_text(skill.read_text() + "Now: !`echo SHELL-RAN`.\n")
        err = self.converse()
        self.assertIn("!`echo SHELL-RAN`", self.system())
        self.assertNotIn("Now: SHELL-RAN", self.system())
        self.assertIn("skills.inline_shell is set", err)


@unittest.skipUnless(sys.platform == "linux", "requires Linux terminals")
class ReplCompressTest(ReplBase):
    scenario = "compact"

    def test_compress_handles_empty_and_short_sessions(self):
        process, _ = self.start(tty=False)
        out, err = process.communicate("/compress\nhello\n/compress\n", timeout=30)
        self.assertEqual(process.returncode, 0, err)
        self.assertIn("Nothing to compress yet: start a conversation first.", out)
        self.assertIn("Nothing to compress: conversation is short enough already.", out)
        # Only the single user turn hit the model; no summary request was sent.
        self.assertEqual(len(self.requests()), 1)

    def test_compress_and_compact_alias_summarize_with_focus_and_persist(self):
        (self.profile / "config.yaml").write_text(
            self.config + "compression:\n  protect_first_n: 1\n  protect_last_n: 2\n",
            encoding="utf-8",
        )
        process, _ = self.start(tty=False)
        turns = [
            "First turn establishing the project layout " + ("a" * 240),
            "Second turn discussing database migration details " + ("b" * 240),
            "Third turn checking schema indexes and locks " + ("c" * 240),
            "Fourth turn reviewing deployment steps " + ("d" * 240),
        ]
        script = "\n".join(turns + ["/compress database migration", "Fifth turn after compress"]) + "\n"
        out, err = process.communicate(script, timeout=30)
        self.assertEqual(process.returncode, 0, err)
        self.assertIn("archived ", out)
        requests = self.requests()
        summaries = [r for r in requests if r["messages"][0]["content"].startswith("Summarize completed")]
        self.assertEqual(len(summaries), 1, summaries)
        self.assertIn('FOCUS TOPIC: "database migration"', summaries[0]["messages"][0]["content"])
        # The fifth turn sees the persisted summary instead of the archived middle.
        last_messages = json.dumps(requests[-1]["messages"])
        self.assertIn("[CONTEXT SUMMARY]:", last_messages)
        self.assertNotIn("Second turn discussing database migration details", last_messages)
        with sqlite3.connect(self.profile / "state.db") as db:
            self.assertEqual(
                db.execute("SELECT count(*) FROM messages WHERE active = 1 AND _compressed_summary = 1").fetchone(),
                (1,),
            )
            self.assertGreater(
                db.execute("SELECT count(*) FROM messages WHERE active = 0 AND compacted = 1").fetchone()[0],
                0,
            )

        # `/compact` alias also works on a resumed session.
        session = self.sessions()[0][0]
        more = [
            "Sixth turn with extra details " + ("e" * 240),
            "Seventh turn with extra details " + ("f" * 240),
            "Eighth turn with extra details " + ("g" * 240),
        ]
        second, _ = self.start("--resume", session, tty=False)
        out2, err2 = second.communicate("\n".join(more + ["/compact release checklist"]) + "\n", timeout=30)
        self.assertEqual(second.returncode, 0, err2)
        self.assertIn("archived ", out2)
        summaries2 = [r for r in self.requests() if r["messages"][0]["content"].startswith("Summarize completed")]
        self.assertEqual(len(summaries2), 2)
        self.assertIn('FOCUS TOPIC: "release checklist"', summaries2[-1]["messages"][0]["content"])


if __name__ == "__main__":
    unittest.main()
