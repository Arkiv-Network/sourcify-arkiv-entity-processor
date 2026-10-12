#!/usr/bin/env python3
"""Exercise actual process -> verify -> hydrate -> HTTP -> graceful shutdown."""
import json
import gzip
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

root = Path(__file__).resolve().parents[1]
processor = os.environ.get("PROCESSOR_BIN", str(root / "target/release/arkiv-entity-processor"))
server = os.environ.get("SERVER_BIN", str(root / "target/release/arkiv-sourcify-server"))
record = json.loads((root / "docs/data/sourcify-response.json").read_text())

with tempfile.TemporaryDirectory(prefix="processor-server-smoke-") as temporary:
    bundle = str(Path(temporary) / "bundle")
    subprocess.run([processor, "process", "--require-source-complete", "--output", bundle,
                    "--owner", "0x1111111111111111111111111111111111111111"],
                   input=json.dumps(record) + '\n{"type":"source-complete","records":1}\n',
                   text=True, check=True, stdout=subprocess.DEVNULL)
    subprocess.run([processor, "verify", bundle], check=True)
    hydrated = json.loads(subprocess.check_output([processor, "hydrate", bundle]))
    assert hydrated == record, "CLI roundtrip differs"
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    process = subprocess.Popen([server, "--bundle", bundle, "--listen", f"127.0.0.1:{port}"])
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
        assert status == 200 and ready["mode"] == "fixture"
        assert ready["dataset"]["contracts"] == 1
        route = "/v2/contract/1/" + record["address"]
        status, body = get(route + "?fields=all")
        expected = {k: v for k, v in record.items() if k != "proxyResolution"}
        assert status == 200 and body == expected, body
        compressed_request = urllib.request.Request(base + route + "?fields=all",
                                                    headers={"Accept-Encoding": "gzip"})
        with urllib.request.urlopen(compressed_request, timeout=3) as compressed:
            assert compressed.headers["Content-Encoding"] == "gzip"
            assert "accept-encoding" in compressed.headers["Vary"].lower()
            assert compressed.headers["Content-Length"] is None
            assert compressed.headers["Transfer-Encoding"] == "chunked"
            assert json.loads(gzip.decompress(compressed.read())) == expected
        status, hot = get(route.lower() + "?fields=abi,compilation")
        assert status == 200 and hot["abi"] == record["abi"]
        assert hot["compilation"] == record["compilation"] and "sources" not in hot
        assert get(route + "?fields=proxyResolution")[0] == 400
        before = get("/_ops/metrics")[1]
        status, missing = get("/v2/contract/1/0x1111111111111111111111111111111111111111")
        assert status == 404 and missing["error"]["code"] == "contract_not_found"
        after = get("/_ops/metrics")[1]
        assert after["lookups"] == before["lookups"] + 1
        assert after["dependencyReads"] == before["dependencyReads"]
        process.send_signal(signal.SIGTERM)
        assert process.wait(timeout=10) == 0, "graceful shutdown failed"
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
    # A corrupt published bundle must fail before binding/listening.
    seed = json.loads((Path(bundle) / "seed.json").read_text())
    part = Path(bundle) / seed["entities"][0]["path"]
    part.write_bytes(part.read_bytes()[:-1])
    failed = subprocess.run([server, "--bundle", bundle, "--listen", f"127.0.0.1:{port}"],
                            capture_output=True, text=True, timeout=10)
    assert failed.returncode != 0 and "checksum mismatch" in failed.stderr, failed.stderr
print("Bundle HTTP smoke test passed")
