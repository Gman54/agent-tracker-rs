"""Talk to Houndbit Protect from Python (example, not a finished SDK).

Shows that any program can use Protect, not just Houndbit: pair once, sign in, find your
folder, ask to do something, and read back what Protect decided. Protect still decides and
records only; it does not run anything yet.

Needs: Python 3.9+ and the `cryptography` package (pip install cryptography).

    python protect_client.py --app-id com.example.mytool --name "My tool" --read notes.txt
"""
import argparse
import base64
import json
import os
import socket
import struct
import sys
import time

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

PROTOCOL_MAJOR = 1
MAX_FRAME = 1024 * 1024


def default_endpoint():
    if os.name == "nt":
        user = os.environ.get("USERNAME", "user")
        tag = "".join(c.lower() if c.isascii() and c.isalnum() else "_" for c in user)
        return r"\\.\pipe\houndbit-protect-v1-" + tag
    return os.path.join(os.path.expanduser("~"), ".local", "share", "houndbit-protect", "engine.sock")


class ProtectError(Exception):
    def __init__(self, code, message):
        super().__init__(f"{code}: {message}")
        self.code = code  # None means the connection failed; the outcome is unknown.


class Protect:
    def __init__(self, endpoint):
        if os.name == "nt":
            self._f = open(endpoint, "r+b", buffering=0)  # Windows named pipe
            self._read, self._write = self._f.read, self._f.write
        else:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(endpoint)
            self._f = s
            self._read, self._write = s.recv, s.sendall
        self._next = 1

    def _read_exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self._read(n - len(buf))
            if not chunk:
                raise ProtectError(None, "the engine closed the connection")
            buf += chunk
        return buf

    def call(self, method, params=None):
        req = {"jsonrpc": "2.0", "id": f"py-{self._next}", "method": method, "params": params or {}}
        self._next += 1
        body = json.dumps(req).encode("utf-8")
        self._write(struct.pack(">I", len(body)) + body)
        (n,) = struct.unpack(">I", self._read_exact(4))
        if n > MAX_FRAME:
            raise ProtectError(None, "reply too large")
        resp = json.loads(self._read_exact(n))
        if "error" in resp:
            e = resp["error"]
            raise ProtectError((e.get("data") or {}).get("protect_code"), e.get("message", ""))
        return resp.get("result")

    def sign_in(self, app_id, key):
        ch = self.call("v1.auth.challenge", {"protocol_major": PROTOCOL_MAJOR, "app_id": app_id})
        msg = f"houndbit-protect-auth-v1\n{ch['engine_id']}\n{ch['nonce']}".encode()
        sig = base64.b64encode(key.sign(msg)).decode()
        return self.call("v1.auth.prove", {"protocol_major": PROTOCOL_MAJOR, "app_id": app_id, "signature": sig})


def load_or_create_key(path):
    """Same file format as the Rust client: base64 of the 32-byte secret."""
    if os.path.exists(path):
        with open(path) as f:
            return Ed25519PrivateKey.from_private_bytes(base64.b64decode(f.read().strip())), False
    key = Ed25519PrivateKey.generate()
    from cryptography.hazmat.primitives.serialization import NoEncryption, PrivateFormat
    raw = key.private_bytes(Encoding.Raw, PrivateFormat.Raw, NoEncryption())
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "w") as f:
        f.write(base64.b64encode(raw).decode())
    if os.name != "nt":
        os.chmod(path, 0o600)
    return key, True


def wait_for(what, check, seconds):
    end = time.time() + seconds
    while time.time() < end:
        r = check()
        if r:
            return r
        time.sleep(0.5)
    sys.exit(f"Gave up waiting for {what}.")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--app-id", required=True)
    ap.add_argument("--name", required=True)
    ap.add_argument("--key-file", help="where to keep this app's key (default: <app-id>.key here)")
    ap.add_argument("--endpoint", default=default_endpoint())
    ap.add_argument("--read", default="notes.txt", help="file to ask about, inside the granted folder")
    ap.add_argument("--wait", type=int, default=120, help="seconds to wait for you to accept in Protect")
    a = ap.parse_args()

    key, new = load_or_create_key(a.key_file or f"{a.app_id}.key")
    pub = base64.b64encode(key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)).decode()
    try:
        p = Protect(a.endpoint)
    except OSError as e:
        sys.exit(f"Couldn't reach Houndbit Protect at {a.endpoint} ({e}). Is it running?")

    try:
        p.sign_in(a.app_id, key)
    except ProtectError as e:
        if e.code != "AUTH_FAILED":
            raise
        r = p.call("v1.pairing.request", {"protocol_major": PROTOCOL_MAJOR, "app_id": a.app_id, "display_name": a.name, "public_key": pub})
        print(f"Asked to pair. Accept \"{a.name}\" in Houndbit Protect (pairing id {r['pairing_id']}).", flush=True)
        state = wait_for("the pairing to be accepted", lambda: (s := p.call("v1.pairing.status", {"protocol_major": PROTOCOL_MAJOR, "pairing_id": r["pairing_id"]})["state"]) != "pending" and s, a.wait)
        if state != "accepted":
            sys.exit(f"The pairing was {state}.")
        p.sign_in(a.app_id, key)
    print("Signed in.", flush=True)

    hello = p.call("v1.hello", {"protocol_major": PROTOCOL_MAJOR})
    print(f"Protect {hello['engine_version']}, protocol {hello['protocol_version']}", flush=True)
    caps = p.call("v1.capabilities.get")
    print(f"Gate: {caps['modes']['gate']}, isolate: {caps['modes']['isolate']}", flush=True)

    print("Waiting for a folder to be added for this app...", flush=True)
    grants = wait_for("a folder", lambda: p.call("v1.grants.list")["grants"], a.wait)
    print(f"Folder: {grants[0]['folder']}", flush=True)
    s = p.call("v1.sessions.request", {"resource_grant_id": grants[0]["grant_id"]})["id"]
    p.call("v1.sessions.start", {"session_id": s})
    act = p.call("v1.actions.submit", {"session_id": s, "idempotency_key": f"py-{int(time.time() * 1000)}", "operation_id": "files.read", "arguments": {"path": a.read}})
    print(f"Asked to read {a.read}: decision={act['decision']} state={act['state']} executed={act['executed']}", flush=True)
    events = p.call("v1.events.read", {"session_id": s})["events"]
    print("Record: " + ", ".join(e["type"] for e in events), flush=True)
    p.call("v1.sessions.stop", {"session_id": s})
    print("RESULT " + json.dumps({"decision": act["decision"], "executed": act["executed"], "events": [e["type"] for e in events]}), flush=True)


if __name__ == "__main__":
    main()
