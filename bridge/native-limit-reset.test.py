"""Opt-in installed Codex check; synthetic login and loopback API, no real resets."""
import base64
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


@unittest.skipUnless(os.environ.get("PIAZ_REAL_CODEX"), "Set PIAZ_REAL_CODEX to the native CLI")
class NativeLimitReset(unittest.TestCase):
    def test_external_login_reads_reset_credits_and_preserves_idempotency(self):
        seen = []
        outcome = ["reset"]

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def reply(self, body):
                encoded = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.end_headers()
                self.wfile.write(encoded)

            def do_GET(self):
                seen.append(("GET", self.path))
                if self.path.endswith("/usage"):
                    return self.reply({
                        "plan_type": "plus",
                        "rate_limit": {"allowed": False, "limit_reached": True,
                            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000,
                                "reset_at": int(time.time()) + 3600, "reset_after_seconds": 3600}},
                        "rate_limit_reset_credits": {"available_count": 2},
                    })
                if self.path.endswith("/rate-limit-reset-credits"):
                    return self.reply({"available_count": 2, "credits": [{
                        "id": "credit-test", "reset_type": "codex_rate_limits", "status": "available",
                        "granted_at": (datetime.now(timezone.utc) - timedelta(minutes=1)).isoformat(),
                        "expires_at": (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat(),
                        "title": "Test reset", "description": "Loopback only",
                    }]})
                return self.reply({})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or "{}")
                seen.append(("POST", self.path, body))
                if not self.path.endswith("/rate-limit-reset-credits/consume"):
                    self.send_error(400)
                    return
                self.reply({"code": outcome[0]})

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        with tempfile.TemporaryDirectory(prefix="piaz-native-reset-") as home:
            claims = {"sub": "test-user", "exp": int(time.time()) + 3600,
                "https://api.openai.com/auth": {"chatgpt_user_id": "test-user",
                    "chatgpt_account_id": "personal", "chatgpt_plan_type": "plus"},
                "https://api.openai.com/profile": {"email": "test@example.test"}}
            payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
            token = f"header.{payload}.signature"
            env = {k: v for k, v in os.environ.items()
                if k not in ["OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_BASE_URL"]}
            env["CODEX_HOME"] = home
            process = subprocess.Popen([os.environ["PIAZ_REAL_CODEX"], "-c",
                'cli_auth_credentials_store="file"', "-c",
                f'chatgpt_base_url="http://127.0.0.1:{server.server_port}/backend-api"', "app-server"],
                cwd=home, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True)
            output = queue.Queue()

            def read():
                for line in process.stdout:
                    try:
                        output.put(json.loads(line))
                    except ValueError:
                        pass

            reader = threading.Thread(target=read, daemon=True)
            reader.start()

            def request(identifier, method, params):
                process.stdin.write(json.dumps({"id": identifier, "method": method, "params": params}) + "\n")
                process.stdin.flush()
                until = time.monotonic() + 15
                while True:
                    message = output.get(timeout=max(0.1, until - time.monotonic()))
                    if message.get("id") == identifier:
                        self.assertNotIn("error", message)
                        return message["result"]

            try:
                request(1, "initialize", {"clientInfo": {"name": "piaz-reset-test", "version": "1"},
                    "capabilities": {"experimentalApi": True}})
                process.stdin.write('{"method":"initialized"}\n')
                process.stdin.flush()
                request(2, "account/login/start", {"type": "chatgptAuthTokens", "accessToken": token,
                    "chatgptAccountId": "personal"})
                status = request(3, "account/rateLimits/read", {})["rateLimitResetCredits"]
                self.assertEqual(status["availableCount"], 2)
                self.assertIsNotNone(status["credits"], seen)
                self.assertEqual(status["credits"][0]["id"], "credit-test")
                self.assertTrue(all(call[0] == "GET" for call in seen))
                for identifier, (backend, rpc) in enumerate([
                    ("reset", "reset"), ("already_redeemed", "alreadyRedeemed"),
                    ("nothing_to_reset", "nothingToReset"), ("no_credit", "noCredit"),
                ], start=4):
                    outcome[0] = backend
                    result = request(identifier, "account/rateLimitResetCredit/consume",
                        {"idempotencyKey": "test-stable-id", "creditId": "credit-test"})
                    self.assertEqual(result["outcome"], rpc)
                posts = [call for call in seen if call[0] == "POST"]
                self.assertEqual(len(posts), 4)
                for _, _, body in posts:
                    self.assertEqual(body, {"redeem_request_id": "test-stable-id", "credit_id": "credit-test"})
                self.assertFalse(Path(home, "auth.json").exists())
            finally:
                process.kill()
                process.wait(timeout=5)
                reader.join(timeout=2)
                process.stdin.close()
                process.stdout.close()


if __name__ == "__main__":
    unittest.main()
