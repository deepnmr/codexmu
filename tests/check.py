#!/usr/bin/env python3
"""Offline end-to-end checks. Run: cargo build && python3 tests/check.py."""
import base64
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def fake_codex():
    active = None
    waiting = None

    def emit(value):
        print(json.dumps(value), flush=True)

    for line in sys.stdin:
        value = json.loads(line)
        with open(os.environ["FAKE_LOG"], "a") as log:
            log.write(json.dumps(value) + "\n")
        method = value.get("method")
        key = value.get("id")
        if method == "initialize":
            assert value["params"]["capabilities"]["experimentalApi"]
            emit({"id": key, "result": {"userAgent": "fake-codex"}})
        elif method == "account/login/start":
            active = value["params"]["chatgptAccountId"]
            emit({"id": key, "result": {"type": "chatgptAuthTokens"}})
            emit({"method": "account/updated", "params": {"account": active}})
        elif method == "account/read":
            emit({"id": key, "result": {"account": active}})
        elif method == "thread/start":
            emit({"id": key, "result": {"thread": {"id": "thread-1"}}})
        elif method == "turn/start":
            tid = value["params"]["threadId"]
            emit({"id": key, "result": {"turn": {"id": "turn-" + active}}})
            emit({"method": "turn/started", "params": {"threadId": tid, "turn": {"id": "turn-" + active}}})
            if active in os.environ.get("FAKE_LIMITED_ACCOUNTS", "a").split(","):
                emit({"method": "turn/completed", "params": {"threadId": tid, "turn": {
                    "status": "failed", "error": {"codexErrorInfo": os.environ.get("FAKE_ERROR", "usageLimitExceeded")}}}})
            else:
                waiting = tid
                emit({"id": 71, "method": "item/commandExecution/requestApproval", "params": {"threadId": tid, "command": "echo test"}})
        elif key == 71 and "result" in value:
            assert value["result"]["decision"] == "accept"
            emit({"method": "turn/completed", "params": {"threadId": waiting, "turn": {"status": "completed", "error": None}}})
        elif method == "fake/refresh":
            emit({"id": key, "result": {}})
            emit({"id": 72, "method": "account/chatgptAuthTokens/refresh", "params": {"reason": "unauthorized", "previousAccountId": active}})
        elif key == 72 and "result" in value:
            emit({"method": "fake/refreshed", "params": {"account": value["result"]["chatgptAccountId"]}})
        elif method:
            if key is not None:
                emit({"id": key, "result": {"echo": method}})


if "app-server" in sys.argv:
    fake_codex()
    sys.exit(0)


def auth(name):
    claims = {"sub": name, "email": name + "@example.test", "exp": int(time.time()) + 3600,
              "https://api.openai.com/auth": {"chatgpt_user_id": name, "chatgpt_account_id": name, "chatgpt_plan_type": "plus"}}
    token = "e30." + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=") + ".signature"
    return {"auth_mode": "chatgpt", "OPENAI_API_KEY": None, "tokens": {
        "id_token": token, "access_token": token, "refresh_token": "refresh-" + name, "account_id": name}}


usage = {"a": 100, "b": 15, "c": 50}
http_errors = {}
refreshes = []
model_requests = []
model_payloads = []
probe_gate = None
probe_entered = threading.Event()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # Native clients cancel background metadata requests during account changes.

    def do_GET(self):
        if self.path.endswith(("/wham/usage", "/api/codex/usage")):
            name = self.headers["ChatGPT-Account-Id"]
            self.reply(200, {"plan_type":"plus", "rate_limit": {
                "allowed":True, "limit_reached":False,
                "primary_window":{"used_percent":usage[name], "limit_window_seconds":18000, "reset_at":int(time.time())+600},
                "secondary_window":{"used_percent":usage[name], "limit_window_seconds":604800, "reset_at":int(time.time())+86400}}})
            return
        if self.path != "/usage":
            self.reply(200, {"models": []} if "/models" in self.path else {})
            return
        name = self.headers["ChatGPT-Account-Id"]
        assert self.headers["Authorization"].startswith("Bearer ")
        if name == "b" and probe_gate is not None:
            probe_entered.set()
            assert probe_gate.wait(timeout=10)
        if name in http_errors:
            self.reply(http_errors[name], {"error": "simulated"})
        else:
            self.reply(200, {"rate_limit": {
                "primary_window": {"used_percent": usage[name], "reset_at": int(time.time()) + 600},
                "secondary_window": {"used_percent": usage[name], "reset_at": int(time.time()) + 86400}}})

    def do_POST(self):
        data = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path.endswith("/responses"):
            name = self.headers["ChatGPT-Account-Id"]
            model_requests.append(name)
            try:
                request = json.loads(data)
            except (UnicodeDecodeError, json.JSONDecodeError):
                request = {}
            model_payloads.append(request)
            token = self.headers["Authorization"].split(" ")[1]
            payload = token.split(".")[1]
            assert json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))["sub"] == name
            if name == "a":
                self.reply(429, {"error": {"type": "usage_limit_reached", "message": "fixture limit", "resets_at": int(time.time()) + 600, "plan_type": "plus"}})
                return
            is_title = any(part.get("text", "").startswith("Generate a concise, single-line task title")
                           for item in request.get("input", []) if item.get("role") == "user"
                           for part in item.get("content", []) if isinstance(part, dict))
            events = [
                {"type": "response.created", "response": {"id": "response-b"}},
                {"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "id": "message-b", "content": [{"type": "output_text", "text": "Fixture title" if is_title else "Local fixture completed."}]}},
                {"type": "response.completed", "response": {"id": "response-b", "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}}},
            ]
            body = "".join("data: " + json.dumps(v) + "\n\n" for v in events).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path != "/token":
            self.reply(200, {})  # ancillary native Codex requests
            return
        value = json.loads(data)
        assert value["grant_type"] == "refresh_token"
        name = value["refresh_token"].split("-")[-1]
        refreshes.append(name)
        http_errors.pop(name, None)
        tokens = auth(name)["tokens"]
        tokens["refresh_token"] = "rotated-" + name
        self.reply(200, tokens)


class Peer:
    def __init__(self, command, env):
        self.process = subprocess.Popen(command, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, text=True, bufsize=1)
        self.events = queue.Queue()
        self.messages = []
        self.errors = []
        threading.Thread(target=lambda: [self.events.put(json.loads(line)) for line in self.process.stdout], daemon=True).start()
        threading.Thread(target=lambda: self.errors.extend(self.process.stderr.readlines()), daemon=True).start()

    def send(self, value):
        self.process.stdin.write(json.dumps(value) + "\n")
        self.process.stdin.flush()

    def until(self, predicate, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = self.events.get(timeout=max(0.1, deadline - time.monotonic()))
            except queue.Empty:
                raise AssertionError("protocol timeout: " + "".join(self.errors))
            self.messages.append(value)
            if predicate(value):
                return value
        raise AssertionError("protocol timeout")

    def close(self):
        self.process.stdin.close()
        try:
            assert self.process.wait(timeout=6) == 0, self.errors
        finally:
            if self.process.poll() is None:
                self.process.kill()


def main():
    global probe_gate
    binary = Path(os.environ.get("CODEXMU_TEST_BIN", "target/debug/codexmu")).resolve()
    fixture = Path(__file__).resolve()
    fixture.chmod(0o755)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="codexmu-check-") as temp:
        root = Path(temp)
        # Keep PATH isolated, but allow the npm entrypoint to find Node.
        (root / "bin").mkdir()
        if node := shutil.which("node"):
            (root / "bin/node").symlink_to(node)
        env = dict(os.environ, CODEX_HOME=str(root / "home"), CODEXMU_CODEX_BIN=str(fixture),
                   CODEXMU_USAGE_URL=f"http://127.0.0.1:{server.server_port}/usage",
                   CODEXMU_TOKEN_URL=f"http://127.0.0.1:{server.server_port}/token", FAKE_LOG=str(root / "log"),
                   PATH=f"{root / 'bin'}:/usr/bin:/bin", CODEXMU_INTERVAL="60")
        env.pop("CODEXMU_BRIDGE", None)

        def run(*args, ok=True):
            result = subprocess.run([str(binary), *args], env=env, capture_output=True, text=True, timeout=25)
            assert (result.returncode == 0) == ok, (args, result.stderr)
            return result

        for name in usage:
            source = root / (name + ".json")
            source.write_text(json.dumps(auth(name)))
            run("add", name, "--auth-file", str(source))
        run("add", "duplicate", "--auth-file", str(root / "a.json"), ok=False)
        run("add", "../escape", "--auth-file", str(root / "a.json"), ok=False)
        run("switch", "a")
        assert json.loads(run("list").stdout)["accounts"][0]["active"]
        before = (root / "home/auth.json").read_bytes()
        run("watch", "--once", "--dry-run")
        assert (root / "home/auth.json").read_bytes() == before
        http_errors["a"] = 503
        run("watch", "--once")
        assert (root / "home/auth.json").read_bytes() == before
        http_errors.clear()
        run("watch", "--once")
        assert json.loads((root / "home/auth.json").read_text())["tokens"]["account_id"] == "b"
        run("remove", "b", ok=False)
        # API 401 refreshes the token once and persists both copies before retrying.
        http_errors["b"] = 401
        run("list", "--live")
        assert refreshes == ["b"]
        assert json.loads((root / "home/auth.json").read_text())["tokens"]["refresh_token"] == "rotated-b"
        assert json.loads((root / "home/codexmu/accounts/b.json").read_text())["auth"]["tokens"]["refresh_token"] == "rotated-b"
        # No available account: preserve active authentication.
        usage.update(a=100, b=100, c=100)
        before = (root / "home/auth.json").read_bytes()
        run("watch", "--once")
        assert (root / "home/auth.json").read_bytes() == before
        def unblock():
            # Clear persisted cooldowns from the prior independent scenarios.
            for path in (root / "home/codexmu/accounts").glob("*.json"):
                value = json.loads(path.read_text()); value["blocked_until"] = 0; path.write_text(json.dumps(value))

        def active():
            return json.loads((root / "home/auth.json").read_text())["tokens"]["account_id"]

        # Priority tiers win over usage; an exhausted tier falls through to the next one.
        usage.update(a=100, b=15, c=50)
        unblock(); run("switch", "a")
        run("priority", "c", "1")
        assert [row["priority"] for row in json.loads(run("list").stdout)["accounts"]] == [0, 0, 1]
        run("watch", "--once")
        assert active() == "c"
        usage.update(a=100, b=15, c=100)
        unblock(); run("switch", "a")
        run("watch", "--once")
        assert active() == "b"
        # At the limit, a cool lower-tier account beats a hot top-tier one so the next check does not move again.
        usage.update(a=100, b=15, c=65)
        unblock(); run("switch", "a")
        run("--switch-at", "60", "watch", "--once")
        assert active() == "b"
        run("--switch-at", "60", "watch", "--once")
        assert active() == "b"
        # Only hot candidates left: any headroom beats waiting at the limit.
        usage.update(a=100, b=65, c=65)
        unblock(); run("switch", "a")
        run("--switch-at", "60", "watch", "--once")
        assert active() == "c"
        # A refresh round-trip must not drop the tier.
        http_errors["c"] = 401
        run("list", "--live")
        assert [row["priority"] for row in json.loads(run("list").stdout)["accounts"]] == [0, 0, 1]
        run("priority", "c", "0")
        run("priority", "missing", "1", ok=False)
        # Proactive switching needs both the active account at/above the threshold and a candidate below it.
        usage.update(a=70, b=15, c=50)
        unblock(); run("switch", "a")
        run("watch", "--once")
        run("--switch-at", "60", "watch", "--once", "--dry-run")
        assert active() == "a"
        run("--switch-at", "60", "watch", "--once")
        assert active() == "b"
        assert json.loads((root / "home/codexmu/accounts/a.json").read_text())["blocked_until"] == 0  # early switch is not a cooldown
        usage.update(a=70, b=65, c=65)
        unblock(); run("switch", "a")
        assert "no available account" not in run("--switch-at", "60", "watch", "--once").stderr
        assert active() == "a"
        # In a live bridge an early switch happens between turns and sends no continuation turn.
        usage.update(a=70, b=15, c=50)
        unblock(); run("switch", "a")
        (root / "log").write_text("")
        peer = Peer([str(binary), "--switch-at", "60", "app-server"], dict(env, FAKE_LIMITED_ACCOUNTS=""))
        try:
            peer.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "early", "version": "1"}}})
            peer.until(lambda v: v.get("id") == 1)
            peer.send({"method": "initialized"})
            peer.until(lambda v: v.get("method") == "account/updated" and v["params"]["account"] == "b")
        finally:
            peer.close()
        early = [json.loads(line) for line in (root / "log").read_text().splitlines()]
        assert [v["params"]["chatgptAccountId"] for v in early if v.get("method") == "account/login/start"] == ["a", "b"]
        assert not any(v.get("method") == "turn/start" for v in early)
        usage.update(a=0, b=15, c=50)
        unblock()
        run("switch", "a")
        peer = Peer([str(binary), "app-server"], env)
        try:
            peer.send({"id": "codexmu-login", "method": "initialize", "params": {"clientInfo": {"name": "test", "version": "1"}}})
            peer.until(lambda v: v.get("id") == "codexmu-login")  # colliding client IDs are remapped
            peer.send({"method": "initialized"})
            peer.until(lambda v: v.get("method") == "account/updated")
            run("watch", "--once", "--dry-run")  # a bridge no longer owns the whole home
            peer.send({"id": 2, "method": "turn/start", "params": {"threadId": "thread-1", "input": [{"type": "text", "text": "original work"}]}})
            peer.until(lambda v: v.get("method") == "turn/completed")
            peer.until(lambda v: v.get("method") == "item/commandExecution/requestApproval")
            peer.send({"id": 71, "result": {"decision": "accept"}})
            done = peer.until(lambda v: v.get("method") == "turn/completed")
            assert done["params"]["turn"]["status"] == "completed"
            assert json.loads((root / "home/auth.json").read_text())["tokens"]["account_id"] == "b"
            # The usage report said A was fine; the limit error still excludes A until its next reset.
            assert json.loads((root / "home/codexmu/accounts/a.json").read_text())["blocked_until"] > time.time() + 300
            peer.send({"id": 3, "method": "fake/refresh"})
            peer.until(lambda v: v.get("method") == "fake/refreshed")
            peer.send({"id": 4, "method": "account/read"})
            assert peer.until(lambda v: v.get("id") == 4)["result"]["account"] == "b"
            peer.send({"id": 5, "method": "account/logout"})
            assert "error" in peer.until(lambda v: v.get("id") == 5)
            assert all("accessToken" not in json.dumps(v) for v in peer.messages)
        finally:
            peer.close()
        incoming = [json.loads(line) for line in (root / "log").read_text().splitlines()]
        turns = [v["params"] for v in incoming if v.get("method") == "turn/start"]
        assert len(turns) == 2 and turns[0]["threadId"] == turns[1]["threadId"]
        assert turns[0]["input"][0]["text"] == "original work"
        assert "original work" not in turns[1]["input"][0]["text"]  # never replay original side effects
        # Non-quota failures must never trigger account switching or prompt replay.
        for path in (root / "home/codexmu/accounts").glob("*.json"):
            value = json.loads(path.read_text()); value["blocked_until"] = 0; path.write_text(json.dumps(value))
        run("switch", "a")
        peer = Peer([str(binary), "app-server"], dict(env, FAKE_ERROR="contextWindowExceeded"))
        try:
            peer.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "test", "version": "1"}}})
            peer.until(lambda v: v.get("id") == 1)
            peer.send({"method": "initialized"})
            peer.until(lambda v: v.get("method") == "account/updated")
            peer.send({"id": 2, "method": "turn/start", "params": {"threadId": "thread-2", "input": [{"type": "text", "text": "fail"}]}})
            peer.until(lambda v: v.get("method") == "turn/completed")
            peer.send({"id": 3, "method": "account/read"})
            assert peer.until(lambda v: v.get("id") == 3)["result"]["account"] == "a"
        finally:
            peer.close()
        # Ctrl+C exits even while the client's stdin pipe remains open.
        peer = Peer([str(binary), "app-server"], env)
        peer.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "test", "version": "1"}}})
        peer.until(lambda v: v.get("id") == 1)
        peer.process.send_signal(signal.SIGINT)
        assert peer.process.wait(timeout=5) == 130
        peer.process.stdin.close()
        for mode in ["no-resume", "cancel"]:
            run("switch", "a")
            (root / "log").write_text("")
            probe_entered.clear()
            probe_gate = threading.Event() if mode == "cancel" else None
            peer = Peer([str(binary), *(["--no-resume"] if mode == "no-resume" else []), "app-server"], env)
            try:
                peer.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "test", "version": "1"}}})
                peer.until(lambda v: v.get("id") == 1)
                peer.send({"method": "initialized"})
                peer.until(lambda v: v.get("method") == "account/updated")
                peer.send({"id": 2, "method": "turn/start", "params": {"threadId": "thread-1", "input": [{"type": "text", "text": "original"}]}})
                peer.until(lambda v: v.get("method") == "turn/completed")
                if mode == "cancel":
                    assert probe_entered.wait(timeout=5)
                    peer.send({"id": 30, "method": "turn/start", "params": {"threadId": "thread-2", "input": [{"type": "text", "text": "must not run"}]}})
                    peer.send({"id": 31, "method": "turn/interrupt", "params": {"threadId": "thread-2", "turnId": "pending"}})
                    assert "error" in peer.until(lambda v: v.get("id") == 30)
                    peer.send({"id": 32, "method": "turn/interrupt", "params": {"threadId": "thread-1", "turnId": "turn-a"}})
                    peer.until(lambda v: v.get("id") == 32)
                    probe_gate.set()
                peer.until(lambda v: v.get("method") == "account/updated" and v["params"]["account"] == "b")
                peer.send({"id": 33, "method": "account/read"})
                assert peer.until(lambda v: v.get("id") == 33)["result"]["account"] == "b"
            finally:
                if probe_gate is not None:
                    probe_gate.set()
                probe_gate = None
                peer.close()
            assert sum(json.loads(line).get("method") == "turn/start" for line in (root / "log").read_text().splitlines()) == 1
        print("PASS: account CRUD, private atomic switch, dry run, HTTP failure, OAuth refresh, exhausted pool, priority tiers, early switching, RPC IDs, limit failover, same-thread continuation, approvals, token redaction, locking")
        if "--native" in sys.argv:
            native = str(Path(sys.argv[sys.argv.index("--native") + 1]).resolve())
            usage.update(a=0, b=15, c=50)
            run("switch", "a")
            native_env = dict(env, CODEXMU_CODEX_BIN=native, CODEXMU_INTERVAL="5")
            url = f"http://127.0.0.1:{server.server_port}"
            (root / "home/config.toml").write_text(f'''model = "gpt-5.1"
model_provider = "fixture"
chatgpt_base_url = "{url}"
[model_providers.fixture]
name = "fixture"
base_url = "{url}/v1"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
''')
            peer = Peer([str(binary), "app-server"], native_env)
            try:
                peer.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "codexmu_native_check", "version": "0.1.0"}}})
                assert "result" in peer.until(lambda v: v.get("id") == 1)
                peer.send({"method": "initialized"})
                # Native Codex must use B's bearer token on the same thread after A's HTTP 429.
                peer.until(lambda v: v.get("method") == "account/updated", timeout=40)
                peer.send({"id": 2, "method": "thread/start", "params": {"cwd": str(root), "approvalPolicy": "never", "sandbox": "read-only"}})
                tid = peer.until(lambda v: v.get("id") == 2, timeout=40)["result"]["thread"]["id"]
                peer.send({"id": 3, "method": "turn/start", "params": {"threadId": tid, "input": [{"type": "text", "text": "Say hello. Do not use tools."}]}})
                failed = peer.until(lambda v: v.get("method") == "turn/completed", timeout=40)
                assert failed["params"]["turn"]["error"]["codexErrorInfo"] == "usageLimitExceeded"
                completed = peer.until(lambda v: v.get("method") == "turn/completed", timeout=40)
                assert completed["params"]["threadId"] == tid
                assert completed["params"]["turn"]["status"] == "completed"
                assert model_requests == ["a", "b"], model_requests
                peer.send({"id": 4, "method": "account/read", "params": {"refreshToken": False}})
                account = peer.until(lambda v: v.get("id") == 4)["result"]["account"]
                assert account["email"] == "b@example.test", account
            finally:
                peer.close()
            # Desktop shim also forwards ordinary Codex commands without recursion.
            result = subprocess.run([str(binary), "--version"], env=dict(native_env, CODEXMU_BRIDGE="1"), capture_output=True, text=True, timeout=10)
            assert result.returncode == 0 and "codex-cli" in result.stdout
            print("PASS: official Codex HTTP 429 -> live A/B bearer-token switch -> same-thread model completion, account/read confirmed B, desktop command forwarding")
        # Multiple exhausted accounts must continue through the entire registered pool.
        usage.update(a=0, b=15, c=50)
        for path in (root / "home/codexmu/accounts").glob("*.json"):
            value = json.loads(path.read_text()); value["blocked_until"] = 0; path.write_text(json.dumps(value))
        run("switch", "a")
        (root / "log").write_text("")
        assert len(json.loads(run("list").stdout)["accounts"]) == 3
        peer = Peer([str(binary), "app-server"], dict(env, FAKE_LIMITED_ACCOUNTS="a,b"))
        try:
            peer.send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "test", "version": "1"}}})
            peer.until(lambda v: v.get("id") == 1)
            peer.send({"method": "initialized"})
            peer.until(lambda v: v.get("method") == "account/updated")
            peer.send({"id": 2, "method": "turn/start", "params": {"threadId": "thread-multi", "input": [{"type": "text", "text": "continue through the account pool"}]}})
            for _ in range(2):
                failed = peer.until(lambda v: v.get("method") == "turn/completed")
                assert failed["params"]["turn"]["error"]["codexErrorInfo"] == "usageLimitExceeded"
            peer.until(lambda v: v.get("method") == "item/commandExecution/requestApproval")
            peer.send({"id": 71, "result": {"decision": "accept"}})
            completed = peer.until(lambda v: v.get("method") == "turn/completed")
            assert completed["params"]["turn"]["status"] == "completed"
            assert completed["params"]["threadId"] == "thread-multi"
            assert json.loads((root / "home/auth.json").read_text())["tokens"]["account_id"] == "c"
        finally:
            peer.close()
        requests = [json.loads(line) for line in (root / "log").read_text().splitlines()]
        assert [v["params"]["chatgptAccountId"] for v in requests if v.get("method") == "account/login/start"] == ["a", "b", "c"]
        assert [v["params"]["threadId"] for v in requests if v.get("method") == "turn/start"] == ["thread-multi"] * 3
        print("PASS: three registered accounts, A limit -> B limit -> C, same-thread recovery completed")
        # Concurrent servers keep their busy account when the shared default changes.
        usage.update(a=0, b=15, c=50)
        run("switch", "a")
        peers = [Peer([str(binary), "app-server"], dict(env, FAKE_LIMITED_ACCOUNTS="")) for _ in range(2)]
        try:
            # Either process can acquire the startup lock first.
            for peer in peers:
                peer.send({"id":1, "method":"initialize", "params":{"clientInfo":{"name":"concurrent", "version":"1"}}})
            for i, peer in enumerate(peers):
                peer.until(lambda v: v.get("id") == 1)
                peer.send({"method":"initialized"})
                peer.until(lambda v: v.get("method") == "account/updated")
                peer.send({"id":2, "method":"turn/start", "params":{"threadId":f"parallel-{i}", "input":[{"type":"text", "text":"wait for approval"}]}})
                peer.until(lambda v: v.get("method") == "item/commandExecution/requestApproval")
            run("switch", "b")
            before = (root / "home/auth.json").read_bytes()
            count = refreshes.count("a")
            for peer in peers:
                peer.send({"id":3, "method":"fake/refresh"})
            for peer in peers:
                assert peer.until(lambda v: v.get("method") == "fake/refreshed")["params"]["account"] == "a"
                peer.send({"id":4, "method":"account/read"})
                assert peer.until(lambda v: v.get("id") == 4)["result"]["account"] == "a"
            assert refreshes.count("a") == count + 1, "refresh rotation must be shared across sessions"
            assert (root / "home/auth.json").read_bytes() == before, "refreshing A must preserve the shared default B"
            for i, peer in enumerate(peers):
                peer.send({"id":71, "result":{"decision":"accept"}})
                assert peer.until(lambda v: v.get("method") == "turn/completed")["params"]["threadId"] == f"parallel-{i}"
        finally:
            for peer in peers:
                peer.close()
        print("PASS: concurrent servers, independent busy turns, shared OAuth rotation, preserved default account")
    server.shutdown()


if __name__ == "__main__":
    main()
