#!/usr/bin/env python3
"""Nadir reference fixture — a minimal HTTP API for integration testing.

Modes (set via --mode):
  corrected  — valid behavior: auth works, records persist, normal responses.
  defective  — createRecord with reserved title "RESERVED" returns 422,
               and the created record is NOT stored (getRecordByReference → 404).
               This exercises the `rejected-create-does-not-persist` oracle.
  exit_on_reserved — same as defective, but the process exits on reserved title.
  error_on_reserved — createRecord with reserved title "RESERVED" returns 500.
  lenient          — like corrected, but /probe accepts any value instead of
                     validating it against its schema (string, at most 5 chars).
  drop_on_reserved  — same as defective, but drops the connection on reserved.
  slow             — introduces artificial delay for timeout tests.
  auth_matrix      — multi-user: admin can create and read, user can only read.
                     admin password from NADIR_ADMIN_PASSWORD, user from NADIR_USER_PASSWORD.
  auth_matrix_open — like auth_matrix, but user can also create (policy discrepancy).
"""

import argparse
import json
import os
import signal
import sys
import time
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse

# ── In-memory state ─────────────────────────────────────────────────

# Fake user database — populated at startup based on mode
USERS: dict[str, str] = {}

# Token → username mapping (simple bearer tokens)
TOKENS: dict[str, str] = {}

def _init_users(mode: str):
    global USERS
    if mode in ("auth_matrix", "auth_matrix_open"):
        USERS = {
            "admin": os.environ.get("NADIR_ADMIN_PASSWORD", "admin-secret"),
            "user": os.environ.get("NADIR_USER_PASSWORD", "user-secret"),
        }
    else:
        USERS = {"fixture-user": os.environ.get("NADIR_FIXTURE_PASSWORD", "test-password")}

# Stored records: reference → record dict
RECORDS: dict[str, dict] = {}

# Templates
TEMPLATES: dict[str, dict] = {
    "tpl-123": {
        "id": "tpl-123",
        "title": "Invoice Template",
        "fields": ["amount", "date", "description"],
    },
}

# Counter for record IDs
_next_record_id = 1


def _next_id() -> str:
    global _next_record_id
    rid = f"rec-{_next_record_id}"
    _next_record_id += 1
    return rid


# ── Request handler ─────────────────────────────────────────────────

class FixtureHandler(BaseHTTPRequestHandler):
    """Handle API requests for the Nadir reference fixture."""

    server_mode = "corrected"

    def log_message(self, fmt, *args):
        """Send log messages to stderr for Nadir's ring buffer."""
        print(f"[fixture] {fmt % args}", file=sys.stderr)

    def _read_body(self) -> dict:
        length = int(self.headers.get("Content-Length", "0"))
        if length == 0:
            return {}
        raw = self.rfile.read(length)
        return json.loads(raw)

    def _send_json(self, data: dict, status: int = 200):
        body = json.dumps(data).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _authenticate(self) -> str | None:
        """Return username if bearer token is valid, else None."""
        auth = self.headers.get("Authorization", "")
        if not auth.startswith("Bearer "):
            return None
        token = auth[7:]
        return TOKENS.get(token)

    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path

        # Slow mode: delay before responding
        if self.server_mode == "slow":
            time.sleep(5)

        # Health check (no auth)
        if path == "/health":
            self._send_json({"status": "ok"})
            return

        if path.startswith(("/templates/", "/records/")) and self._authenticate() is None:
            self._send_json({"error": "authentication required"}, 401)
            return

        # Get template
        if path.startswith("/templates/"):
            tpl_id = path.split("/")[-1]
            tpl = TEMPLATES.get(tpl_id)
            if tpl is None:
                self._send_json({"error": "not found"}, 404)
                return
            if self.server_mode == "title_regression" and self.headers.get("Accept-Language") == "es":
                tpl = {**tpl, "title": "Invoice Template (draft)"}
            self._send_json(tpl)
            return

        # Get record by reference
        if path.startswith("/records/"):
            ref = path.split("/")[-1]
            rec = RECORDS.get(ref)
            if rec is None:
                self._send_json({"error": "not found"}, 404)
                return
            self._send_json(rec)
            return

        self._send_json({"error": "not found"}, 404)

    def do_POST(self):
        parsed = urlparse(self.path)
        path = parsed.path

        # Slow mode: delay before responding
        if self.server_mode == "slow":
            time.sleep(5)

        # Login
        if path == "/login":
            body = self._read_body()
            user = body.get("user", "")
            password = body.get("pass", "")
            expected = USERS.get(user)
            if expected is None or password != expected:
                self._send_json({"error": "invalid credentials"}, 401)
                return
            token = f"tok-{user}-{int(time.time())}"
            TOKENS[token] = user
            self._send_json({"access_token": token})
            return

        # Create record
        if path == "/records":
            username = self._authenticate()
            if username is None:
                self._send_json({"error": "authentication required"}, 401)
                return

            # Role-based access: in auth_matrix mode, only admin can create
            if self.server_mode == "auth_matrix" and username != "admin":
                self._send_json({"error": "forbidden", "detail": "only admin can create records"}, 403)
                return

            body = self._read_body()
            ref = body.get("reference", "")
            title = body.get("title", "")

            # Handle reserved title
            if title == "RESERVED":
                mode = self.server_mode
                if mode == "error_on_reserved":
                    self._send_json({"error": "internal error"}, 500)
                    return
                if mode == "exit_on_reserved":
                    print("[fixture] exiting on reserved title", file=sys.stderr)
                    sys.exit(1)
                if mode == "drop_on_reserved":
                    # Drop the connection abruptly
                    self.send_response(422)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    # Don't send body — simulate connection drop
                    self.wfile.write(b'{"error":')
                    self.wfile.flush()
                    # Close prematurely
                    self.connection.shutdown(1)
                    return
                # defective/corrected: return 422 without storing
                self._send_json({"error": "reserved title", "reference": ref}, 422)
                return

            rec_id = _next_id()
            rec = {
                "id": rec_id,
                "template_id": body.get("template_id"),
                "reference": ref,
                "title": title,
                "metadata": body.get("metadata", {}),
            }

            # In "defective" mode, don't actually store
            if self.server_mode not in ("defective", "exit_on_reserved", "drop_on_reserved"):
                RECORDS[ref] = rec

            self._send_json(rec, 201)
            return

        # Probe string (for fuzz-probe)
        if path == "/probe":
            body = self._read_body()
            value = body.get("value")
            # ProbeRequest: a required string of at most 5 characters.
            valid = isinstance(value, str) and len(value) <= 5
            if not valid and self.server_mode != "lenient":
                self._send_json({"error": "value must be a string of at most 5 characters"}, 400)
                return
            self._send_json({"echo": str(value)})
            return

        self._send_json({"error": "not found"}, 404)

    def do_PUT(self):
        self._send_json({"error": "not implemented"}, 501)

    def do_DELETE(self):
        self._send_json({"error": "not implemented"}, 501)


# ── Main ────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Nadir reference fixture")
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--mode", default="corrected",
                        choices=["corrected", "defective", "exit_on_reserved", "error_on_reserved", "lenient",
                                 "drop_on_reserved", "slow", "title_regression",
                                 "auth_matrix", "auth_matrix_open"])
    args = parser.parse_args()

    _init_users(args.mode)
    FixtureHandler.server_mode = args.mode

    server = HTTPServer(("127.0.0.1", args.port), FixtureHandler)
    print(f"[fixture] listening on 127.0.0.1:{server.server_port} mode={args.mode}",
          file=sys.stderr, flush=True)

    def _shutdown(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, _shutdown)
    signal.signal(signal.SIGINT, _shutdown)

    with server:
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass



if __name__ == "__main__":
    main()
