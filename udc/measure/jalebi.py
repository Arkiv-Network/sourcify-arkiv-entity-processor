#!/usr/bin/env python3
"""Logged, read-only JSON-RPC probes of Jalebi (anonymous budget: about 100 calls per window).

    python3 -I udc/measure/jalebi.py LOG.jsonl LABEL METHOD 'PARAMS_JSON'

Appends one line to LOG.jsonl: label, method, params, HTTP status, the Arkiv-Cost and RateLimit
headers, time, and the result (entity rows reduced to key, createdAt, updatedAt and attributes) or
the error. Never sends a transaction.
"""
import json, sys, time, urllib.error, urllib.request

URL = "https://rpc.jalebi.db-chain.testnet.arkiv.network"
log, label, method, params = sys.argv[1], sys.argv[2], sys.argv[3], json.loads(sys.argv[4])
if method.startswith("eth_send") or method in ("eth_sendRawTransaction",):
    sys.exit("refusing to send a transaction to Jalebi")
body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
request = urllib.request.Request(URL, body, {"content-type": "application/json", "user-agent": "sourcify-udc-probe"})
started = time.time()
entry = {"label": label, "method": method, "params": params, "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
try:
    with urllib.request.urlopen(request, timeout=30) as response:
        status, headers, payload = response.status, response.headers, json.loads(response.read())
except urllib.error.HTTPError as e:
    status, headers, payload = e.code, e.headers, {"error": e.read().decode(errors="replace")[:500]}
entry.update(status=status, cost=headers.get("Arkiv-Cost"), ratelimit=headers.get("RateLimit"), ms=round((time.time() - started) * 1000))
if "result" in payload:
    result = payload["result"]
    if isinstance(result, dict) and isinstance(result.get("data"), list):
        result = {"blockNumber": result.get("blockNumber"), "n": len(result["data"]), "cursor": result.get("cursor"),
                  "rows": [{k: row.get(k) for k in ("key", "owner", "createdAt", "updatedAt", "attributes") if k in row} for row in result["data"]]}
    entry["result"] = result
else:
    entry["error"] = payload.get("error")
with open(log, "a") as f:
    f.write(json.dumps(entry) + "\n")
print(json.dumps(entry)[:4000])
