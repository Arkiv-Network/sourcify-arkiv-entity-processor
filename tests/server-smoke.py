#!/usr/bin/env python3
"""Exercise executable startup, real JSON-RPC, HTTP errors and graceful shutdown.

Positive imported-entity roundtrips and RPC fault cases live in server/tests/arkiv.rs.
This mock is test-only; the executable cannot open bundles or Parquet.
"""
import gzip
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request

root = Path(__file__).resolve().parents[1]
server = os.environ.get("SERVER_BIN", str(root / "target/release/arkiv-sourcify-server"))
state_root = "0x" + "1" * 64
owner = "0x986c08292e6c327a310273a45965a3d2e2eac44c"

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        method, params = request["method"], request["params"]
        if method == "eth_chainId":
            result = "0x7598bf"
        elif method == "eth_getBlockByNumber":
            result = {"number": "0x0", "hash": "0x" + "2" * 64, "stateRoot": state_root}
        elif method == "arkiv_query":
            assert owner in params[0] and "projectid = str('sourcify')" in params[0]
            assert params[1]["atBlock"] == "0x0"
            result = {"data": [], "blockNumber": "0x0"}
        else:
            raise AssertionError(method)
        body = json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

rpc = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=rpc.serve_forever, daemon=True).start()
with socket.socket() as probe:
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
base = f"http://127.0.0.1:{port}"
command = [server, "--rpc-url", f"http://127.0.0.1:{rpc.server_port}", "--owner", owner,
           "--arkiv-chain-id", "7706815", "--genesis-state-root", state_root,
           "--listen", f"127.0.0.1:{port}"]
process = subprocess.Popen(command)
try:
    def get(path):
        try:
            response = urllib.request.urlopen(base + path, timeout=3)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, json.load(response)
    deadline = time.monotonic() + 10
    while True:
        assert process.poll() is None, "server exited before readiness"
        try:
            status, ready = get("/_ops/ready")
            break
        except urllib.error.URLError:
            assert time.monotonic() < deadline, "server never became ready"
            time.sleep(0.05)
    assert status == 200 and ready["mode"] == "arkiv"
    assert ready["dataset"]["genesisStateRoot"] == state_root
    status, body = get("/v2/contract/1/0x1111111111111111111111111111111111111111")
    assert status == 404 and body == {"address": "0x1111111111111111111111111111111111111111",
                                    "chainId": "1", "match": None, "creationMatch": None, "runtimeMatch": None}
    request = urllib.request.Request(base + "/_ops/health", headers={"Accept-Encoding": "gzip"})
    with urllib.request.urlopen(request, timeout=3) as response:
        assert response.headers["Content-Encoding"] == "gzip"
        assert "accept-encoding" in response.headers["Vary"].lower()
        assert json.loads(gzip.decompress(response.read())) == {"status": "ok"}
    assert get("/v2/contract/1/invalid")[0] == 400
    process.send_signal(signal.SIGTERM)
    assert process.wait(timeout=10) == 0
    invalid = command.copy()
    invalid[invalid.index("7706815")] = "1"
    assert subprocess.run(invalid, capture_output=True, timeout=10).returncode != 0
finally:
    if process.poll() is None:
        process.kill()
        process.wait()
    rpc.shutdown()
    rpc.server_close()
print("Arkiv RPC HTTP smoke test passed")
