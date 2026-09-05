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

from check import Handler, ThreadingHTTPServer, auth, model_requests, model_payloads, usage


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
    parser.add_argument("--sessions", type=int, choices=range(1, 5), default=1, help="run simultaneous terminals using the same CODEX_HOME")
    args = parser.parse_args()
    assert args.codex_bin, "official Codex with --remote support is required"
    binary = str(Path(os.environ.get("CODEXMU_TEST_BIN", "target/debug/codexmu")).resolve())
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    usage.update(a=0, b=15)
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
        (home / "config.toml").write_text(f'''model = "gpt-5.1"
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
                    os.execve(binary, [binary] + (["--plain"] if args.plain else []), env)
                fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
                sessions.append(dict(pid=pid, terminal=terminal, original=termios.tcgetattr(terminal),
                                     transcript=bytearray(), submitted=False, quitting=False, completed_at=None,
                                     resize_stage=0, exit_status=None, ready=False,
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
                    s["ready"] = s["ready"] or ("gpt-5.1" in text and "›" in text)
                    # Both native terminals must be open before either receives work.
                    if not s["submitted"] and all(v["ready"] for v in sessions):
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
                            fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 24 if narrow else 40, 72 if narrow else 120, 0, 0))
                            s["resize_stage"] += 1
                            s["completed_at"] = time.monotonic()
                            continue
                        if not args.plain:
                            assert "b@example.test" in text and "5h 85%" in text, text[-12000:]
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
