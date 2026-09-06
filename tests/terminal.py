#!/usr/bin/env python3
"""Exercise the real Codex TUI through a PTY, using only local fake inference."""
import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import threading
import time

from check import Handler, ThreadingHTTPServer, auth, model_requests, model_payloads, response_usage, usage


PROMPT = "Say hello. Do not use tools."


def turn_accounts(prompt=PROMPT):
    # The native TUI also generates a task title through a separate model request.
    return [name for name, payload in zip(model_requests, model_payloads)
            if any(part.get("text") == prompt
                   for item in payload.get("input", []) if item.get("role") == "user"
                   for part in item.get("content", []) if isinstance(part, dict))]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-bin", default=shutil.which("codex"))
    parser.add_argument("--plain", action="store_true")
    parser.add_argument("--capture", type=Path, help="save the live terminal bytes before quitting (fake accounts only)")
    parser.add_argument("--resize", action="store_true", help="exercise a narrow terminal and then restore its size")
    parser.add_argument("--model-change", action="store_true", help="check CLI model overrides and /model before submitting a turn")
    parser.add_argument("--usage-change", action="store_true", help="check native response quotas replace older usage polling data")
    parser.add_argument("--sessions", type=int, choices=range(1, 5), default=1, help="run simultaneous terminals using the same CODEX_HOME")
    args = parser.parse_args()
    assert args.codex_bin, "official Codex with --remote support is required"
    assert not ((args.model_change or args.usage_change) and args.plain), "status checks require the dashboard"
    binary = str(Path(os.environ.get("CODEXMU_TEST_BIN", "target/debug/codexmu")).resolve())
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    usage.update(a=0, b=15)
    if args.usage_change:
        response_usage["b"] = 27
    with tempfile.TemporaryDirectory(prefix="codexmu-terminal-") as temp:
        root = Path(temp)
        home = root / "home"
        subprocess.run(["git", "init", "--quiet", "--initial-branch=main", str(root)], check=True)
        url = f"http://127.0.0.1:{server.server_port}"
        env = dict(os.environ, CODEX_HOME=str(home), CODEXMU_CODEX_BIN=str(Path(args.codex_bin).resolve()),
                   CODEXMU_USAGE_URL=url + "/usage", CODEXMU_TOKEN_URL=url + "/token", TERM="xterm-256color")
        env.pop("CODEXMU_BRIDGE", None)
        for name in ["a", "b"]:
            source = root / (name + ".json")
            source.write_text(json.dumps(auth(name)))
            subprocess.run([binary, "add", name, "--auth-file", str(source)], env=env, check=True, capture_output=True)
        subprocess.run([binary, "switch", "a"], env=env, check=True, capture_output=True)
        (home / "config.toml").write_text(f'''model = "{'gpt-5.2' if args.model_change else 'gpt-5.1'}"
model_reasoning_effort = "medium"
model_provider = "fixture"
chatgpt_base_url = "{url}"
check_for_update_on_startup = false
approval_policy = "never"
sandbox_mode = "read-only"
[features]
apps = false
[projects.'{root}']
trust_level = "trusted"
[model_providers.fixture]
name = "fixture"
base_url = "{url}/v1"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
''')
        sessions = []
        try:
            for i in range(args.sessions):
                pid, terminal = pty.fork()
                if pid == 0:
                    os.chdir(root)
                    overrides = ["-m", "gpt-5.1", "-c", 'model_reasoning_effort="high"',
                                 "-c", 'tui.status_line=["context-remaining","fast-mode"]'] if args.model_change and i == 0 else []
                    os.execve(binary, [binary] + (["--plain"] if args.plain else []) + overrides, env)
                fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 200, 0, 0))
                sessions.append(dict(pid=pid, terminal=terminal, original=termios.tcgetattr(terminal),
                                     transcript=bytearray(), submitted=False, quitting=False, completed_at=None,
                                     resize_stage=0, exit_status=None, ready=False,
                                     model_stage=0, model=("gpt-5.1 high" if i == 0 else "gpt-5.2 medium") if args.model_change else "gpt-5.1 medium",
                                     prompt=PROMPT if args.sessions == 1 else f"{PROMPT} Session {i + 1}."))
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline and any(s["exit_status"] is None for s in sessions):
                readable, _, _ = select.select([s["terminal"] for s in sessions if s["exit_status"] is None], [], [], 0.1)
                for s in sessions:
                    if s["exit_status"] is not None:
                        continue
                    terminal = s["terminal"]
                    if terminal in readable:
                        try:
                            chunk = os.read(terminal, 65536)
                        except OSError as error:
                            if error.errno != errno.EIO:
                                raise
                            chunk = b""
                        s["transcript"].extend(chunk)
                        for _ in range(chunk.count(b"\x1b[6n")):
                            os.write(terminal, b"\x1b[1;1R")
                        if b"\x1b]11;?" in chunk:
                            os.write(terminal, b"\x1b]11;rgb:0000/0000/0000\x1b\\")
                        if b"\x1b]10;?" in chunk:
                            os.write(terminal, b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\")
                        if b"\x1b[?u" in chunk:
                            os.write(terminal, b"\x1b[?0u")
                    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", s["transcript"].decode(errors="replace"))
                    headers = re.findall(r"codexmu │ (?:[^│\r\n]+ │ )?(gpt-\S+ \S+) │", text)
                    current_model = headers[-1].strip() if headers else None
                    s["ready"] = s["ready"] or ((current_model == s["model"] if args.model_change else "gpt-5.1" in text) and "›" in text)
                    if args.model_change and all(v["ready"] for v in sessions) and sessions[0]["model_stage"] < 4:
                        if s is sessions[0]:
                            if s["model_stage"] == 0:
                                os.write(terminal, b"/model")
                                time.sleep(0.3)
                                os.write(terminal, b"\r")
                                s["model_stage"] = 1
                            elif s["model_stage"] == 1 and "Select Model and Effort" in text:
                                os.write(terminal, b"\r")
                                s["model_stage"] = 2
                            elif s["model_stage"] == 2 and "Select Reasoning Level for" in text:
                                os.write(terminal, b"\r")
                                s["model_stage"] = 3
                            elif s["model_stage"] == 3:
                                changed = re.search(r"Model changed to (\S+ \S+)", text)
                                if changed and current_model == changed[1]:
                                    assert current_model != s["model"], current_model
                                    assert not turn_accounts(s["prompt"]), "model header must update before the turn"
                                    s["model"] = current_model
                                    s["model_stage"] = 4
                        continue
                    # Both native terminals must be open before either receives work.
                    if not s["submitted"] and all(v["ready"] for v in sessions):
                        if args.model_change:
                            assert current_model == s["model"], (current_model, s["model"])
                        os.write(terminal, b"\x1b[200~" + s["prompt"].encode() + b"\x1b[201~")
                        time.sleep(0.3)
                        os.write(terminal, b"\r")
                        s["submitted"] = True
                    if s["completed_at"] is None and "Local fixture completed." in text:
                        s["completed_at"] = time.monotonic()
                    if not s["quitting"] and s["completed_at"] is not None and time.monotonic() - s["completed_at"] > 0.5:
                        assert turn_accounts(s["prompt"]) == ["a", "b"], turn_accounts(s["prompt"])
                        if args.resize and s["resize_stage"] < 2:
                            narrow = s["resize_stage"] == 0
                            fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 24 if narrow else 40, 72 if narrow else 200, 0, 0))
                            s["resize_stage"] += 1
                            s["completed_at"] = time.monotonic()
                            continue
                        if not args.plain:
                            assert "b@example.test" in text, text[-12000:]
                            header = text.rsplit("codexmu │ ", 1)[-1].split("Context", 1)[0]
                            expected_usage = "5h 73%" if args.usage_change else "5h 85%"
                            assert expected_usage in header, header
                        if args.model_change:
                            assert current_model == s["model"], (current_model, s["model"])
                        if args.capture and s is sessions[0]:
                            args.capture.parent.mkdir(parents=True, exist_ok=True)
                            args.capture.write_bytes(s["transcript"])
                        os.write(terminal, b"/quit")
                        time.sleep(0.3)
                        os.write(terminal, b"\r")
                        s["quitting"] = True
                    exited, status = os.waitpid(s["pid"], os.WNOHANG)
                    if exited:
                        s["exit_status"] = os.waitstatus_to_exitcode(status)
                        assert s["exit_status"] == 0, (s["exit_status"], text[-6000:], [p.read_text()[-3000:] for p in (home / "codexmu").glob("terminal-*.log")])
            for s in sessions:
                assert s["exit_status"] == 0, f"exit={s['exit_status']}; requests={turn_accounts(s['prompt'])}\n{s['transcript'][-12000:]!r}"
                assert s["submitted"] and s["quitting"]
                assert turn_accounts(s["prompt"]) == ["a", "b"]
                assert termios.tcgetattr(s["terminal"]) == s["original"], "terminal settings were not restored"
            assert json.loads((home / "auth.json").read_text())["tokens"]["account_id"] == "b"
            if not args.plain:
                assert len(list((home / "codexmu").glob("terminal-*.log"))) == args.sessions
            print(f"PASS: {args.sessions} real Codex terminal(s) in one CODEX_HOME; each prompt -> A limit -> B response -> /quit, terminal settings restored")
            if args.model_change:
                print("PASS: CLI model/effort override and /model update the current session header before inference")
            if args.usage_change:
                print("PASS: native response quotas update the current header while HTTP polling still reports the older value")
        finally:
            for s in sessions:
                if s["exit_status"] is None:
                    try:
                        os.killpg(s["pid"], signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    os.waitpid(s["pid"], 0)
                os.close(s["terminal"])
    server.shutdown()


if __name__ == "__main__":
    main()
