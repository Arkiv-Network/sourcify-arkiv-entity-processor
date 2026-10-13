#!/usr/bin/env python3
"""Opt-in, bounded comparison of independently sourced snapshot and live responses.

No live API data is fed to the processor. Run --help for the local prerequisites.
Importing this module performs no I/O; offline tests use a local HTTP mock.
"""
import argparse
from datetime import datetime, timezone
from decimal import Decimal
from email.utils import parsedate_to_datetime
import hashlib
import http.client
import json
from pathlib import Path
import secrets
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_USER_AGENT = (
    "arkiv-sourcify-conformance/0.1 "
    "(+https://github.com/Arkiv-Network/sorcify-arkiv-entity-processor)"
)
API_BASE = "https://sourcify.dev/server"
OPTIONAL_FIELDS = {
    "abi", "compilation", "deployment", "creationBytecode", "runtimeBytecode", "sources",
    "metadata", "storageLayout", "transientStorageLayout", "userdoc", "devdoc", "sourceIds",
    "stdJsonInput", "stdJsonOutput", "additionalInput", "signatures", "proxyResolution",
}


def utc_now():
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def sha256(body):
    return hashlib.sha256(body).hexdigest()


def load_json(raw):
    def invalid(value):
        raise ValueError(f"Invalid JSON number: {value}")
    return json.loads(raw, parse_float=Decimal, parse_constant=invalid)


def json_kind(value):
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, (int, float, Decimal)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    raise TypeError(f"Not a JSON value: {type(value).__name__}")


def describe(value):
    kind = json_kind(value)
    if kind in ("object", "array"):
        return {"type": kind, "length": len(value)}
    if kind == "string" and len(value) > 160:
        return {"type": kind, "length": len(value), "sha256": sha256(value.encode()),
                "preview": value[:160]}
    return {"type": kind, "value": str(value) if kind == "number" else value}


def compare_json(expected, actual, path=""):
    """Semantic JSON diffs; ordering matters in arrays, never in objects."""
    differences = []
    expected_kind, actual_kind = json_kind(expected), json_kind(actual)
    if expected_kind != actual_kind:
        return [{"path": path, "kind": "type", "expected": describe(expected),
                 "actual": describe(actual)}]
    if expected_kind == "object":
        for key in sorted(expected.keys() | actual.keys()):
            child = path + "/" + key.replace("~", "~0").replace("/", "~1")
            if key not in actual:
                differences.append({"path": child, "kind": "missing", "expected": describe(expected[key])})
            elif key not in expected:
                differences.append({"path": child, "kind": "unexpected", "actual": describe(actual[key])})
            else:
                differences.extend(compare_json(expected[key], actual[key], child))
    elif expected_kind == "array":
        if len(expected) != len(actual):
            differences.append({"path": path, "kind": "length", "expected": len(expected), "actual": len(actual)})
        for index, (left, right) in enumerate(zip(expected, actual)):
            differences.extend(compare_json(left, right, path + f"/{index}"))
    elif expected != actual:
        differences.append({"path": path, "kind": "value", "expected": describe(expected),
                            "actual": describe(actual)})
    return differences


def save_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")
    temporary.replace(path)


class CachedClient:
    """Single-flight HTTP reader, with a reusable cache and bounded polite retries."""
    def __init__(self, cache_dir, delay_seconds=1.0, max_attempts=3, timeout_seconds=30.0,
                 user_agent=DEFAULT_USER_AGENT, sleep=time.sleep, monotonic=time.monotonic):
        if delay_seconds < 1 or not 1 <= max_attempts <= 3 or not 0 < timeout_seconds <= 120:
            raise ValueError("delay >= 1s, attempts 1..3, and timeout 0..120s are required")
        self.cache_dir = Path(cache_dir).resolve()
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.delay = delay_seconds
        self.max_attempts = max_attempts
        self.timeout = timeout_seconds
        self.user_agent = user_agent
        self.sleep, self.monotonic = sleep, monotonic
        self.last_request = None
        self.next_request = 0

    @staticmethod
    def retry_after(value):
        if not value:
            return 0
        if value.strip().isdigit():
            return int(value.strip())
        try:
            when = parsedate_to_datetime(value)
            if when.tzinfo is None:
                when = when.replace(tzinfo=timezone.utc)
            return max(0, (when - datetime.now(timezone.utc)).total_seconds())
        except (ValueError, TypeError, OverflowError):
            return 0

    def fetch(self, url):
        key = sha256(url.encode())
        metadata_path = self.cache_dir / (key + ".json")
        body_path = self.cache_dir / (key + ".body")
        if metadata_path.exists():
            try:
                cached = json.loads(metadata_path.read_text())
                body = body_path.read_bytes()
                if cached["url"] != url or cached["bodySha256"] != sha256(body):
                    raise ValueError("URL or checksum mismatch")
                if not isinstance(cached["status"], int) or cached["status"] == 429 or cached["status"] >= 500 or cached.get("error"):
                    raise ValueError("invalid cache status")
                return {**cached, "bodyPath": str(body_path), "cacheHit": True}
            except (OSError, ValueError, KeyError) as error:
                return {"url": url, "status": None, "cacheHit": True, "attempts": [],
                        "error": f"Cache validation failed; no refetch: {error}"}

        attempts = []
        backoff = 0
        result = None
        for attempt in range(self.max_attempts):
            interval = 0 if self.last_request is None else max(0, self.last_request + self.delay - self.monotonic())
            wait = max(interval, backoff, self.next_request - self.monotonic())
            if wait > 60:
                if result is None:
                    result = {"url": url, "status": None, "cacheHit": False, "attempts": [], "capturedAt": utc_now()}
                result["error"] = f"Required retry delay {wait:.1f}s exceeds this run's 60s wait budget; no early retry"
                break
            if wait:
                self.sleep(wait)
            self.last_request = self.monotonic()
            request = urllib.request.Request(url, headers={
                "User-Agent": self.user_agent, "Accept": "application/json", "Accept-Encoding": "identity",
            })
            status, body, response_headers, error = None, b"", {}, None
            try:
                try:
                    response = urllib.request.urlopen(request, timeout=self.timeout)
                except urllib.error.HTTPError as http_error:
                    response = http_error
                with response:
                    status = response.status
                    response_headers = {key.lower(): response.headers[key] for key in
                                        ("Content-Type", "ETag", "Last-Modified", "Retry-After")
                                        if response.headers.get(key) is not None}
                    body = response.read()
            except (urllib.error.URLError, TimeoutError, OSError, http.client.HTTPException) as transport_error:
                error = f"Transport failure: {transport_error}"
                body = getattr(transport_error, "partial", b"")
            captured = utc_now()
            attempt_key = key + "-" + secrets.token_hex(8)
            attempt_body = self.cache_dir / (attempt_key + ".body")
            attempt_body.write_bytes(body)
            info = {"url": url, "status": status, "capturedAt": captured, "bodySha256": sha256(body),
                    "bodyPath": str(attempt_body), "headers": response_headers,
                    "userAgent": self.user_agent, "error": error}
            save_json(self.cache_dir / (attempt_key + ".json"), info)
            attempts.append(info)
            result = {**info, "cacheHit": False, "attempts": list(attempts)}
            transient = error is not None or status is None or status == 429 or status >= 500
            if not transient:
                body_path.write_bytes(body)
                result["bodyPath"] = str(body_path)
                save_json(metadata_path, result)
                return result
            result["error"] = error or f"Live HTTP {status}; retry budget exhausted"
            backoff = max(2 ** (attempt + 1), self.retry_after(response_headers.get("retry-after")))
            self.next_request = self.monotonic() + backoff
        save_json(self.cache_dir / (key + ".transient.json"), result)
        return result


def parse_options(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, required=True, help="Completed local Parquet-derived source directory (read-only)")
    parser.add_argument("--output", type=Path, required=True, help="New artifact directory; existing directories are refused")
    parser.add_argument("--cache", type=Path, default=ROOT / "data/live-conformance/cache")
    parser.add_argument("--api-base-url", default=API_BASE, help="Independent Sourcify server, optionally a self-hosted instance")
    parser.add_argument("--seed", default=None, help="Reproducible sampling seed; default generates and records one")
    parser.add_argument("--count", type=int, default=5, help="Small random spot-check sample, 1..10")
    parser.add_argument("--fields", default="abi,compilation,sources", help="At most 3 supported optional fields, or minimal; never all/nested")
    parser.add_argument("--snapshot-label", default="Local prepared snapshot; see source provenance for capture dates")
    parser.add_argument("--delay-seconds", type=float, default=1.0)
    parser.add_argument("--server-base-url", default="http://127.0.0.1:3000", help="Already running Arkiv-backed server to validate; never starts a fixture server")
    options = parser.parse_args(argv)
    api = urllib.parse.urlsplit(options.api_base_url)
    if api.scheme not in ("http", "https") or not api.netloc or api.username or api.password or api.query or api.fragment:
        parser.error("API base must be an HTTP(S) URL without credentials, query, or fragment")
    options.api_base_url = options.api_base_url.rstrip('/')
    server_url = urllib.parse.urlsplit(options.server_base_url)
    if server_url.scheme not in ("http", "https") or not server_url.netloc or server_url.username or server_url.password or server_url.query or server_url.fragment:
        parser.error("Server base must be an HTTP(S) URL without credentials, query, or fragment")
    options.server_base_url = options.server_base_url.rstrip('/')
    fields = [] if options.fields == "minimal" else options.fields.split(',')
    if not 1 <= options.count <= 10:
        parser.error("count must be 1..10; this is a spot check, not an API crawler")
    if len(fields) > 3 or any(field not in OPTIONAL_FIELDS for field in fields) or len(set(fields)) != len(fields):
        parser.error("fields must be at most 3 distinct supported optional fields; all/nested selectors are excluded")
    if not 1 <= options.delay_seconds <= 60:
        parser.error("delay must be 1..60 seconds")
    if options.seed is None:
        options.seed = secrets.token_hex(8)
    if not options.seed or len(options.seed) > 128:
        parser.error("seed must contain 1..128 characters")
    options.selected_fields = sorted(fields)
    options.output = options.output.resolve()
    options.source_dir = options.source_dir.resolve()
    return options


def local_get(url):
    request = urllib.request.Request(url, headers={"Accept-Encoding": "identity"})
    try:
        response = urllib.request.urlopen(request, timeout=120)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, response.read()


def run(options):
    options.output.mkdir(parents=True, exist_ok=False)
    report = {"schema": "sourcify-arkiv.live-conformance.v1", "startedAt": utc_now(),
              "seed": options.seed, "requestedCount": options.count, "fields": options.selected_fields,
              "apiBaseUrl": options.api_base_url, "serverBaseUrl": options.server_base_url,
              "snapshotLabel": options.snapshot_label,
              "comparison": "Expected=independent Sourcify response; actual=our server; JSON values with significant array order and missing/null distinctions; no exclusions",
              "driftWarning": "Local snapshot and live capture are different times; mismatches may reflect snapshot drift and are never waived",
              "results": [], "status": "inconclusive"}
    try:
        sample_command = ["bun", "--no-env-file", "tests/live/sample.ts", "--source-dir", str(options.source_dir),
                          "--output", str(options.output), "--seed", options.seed, "--count", str(options.count)]
        with (options.output / "sample.stdout.log").open("w") as out, (options.output / "sample.stderr.log").open("w") as err:
            sampled = subprocess.run(sample_command, cwd=ROOT, stdout=out, stderr=err, timeout=600)
        sample_path = options.output / "sample.json"
        if sample_path.exists():
            report["sample"] = json.loads(sample_path.read_text())
        if sampled.returncode != 0 or report.get("sample", {}).get("completedCount") != options.count:
            raise RuntimeError("Sampling/reconstruction was incomplete; no live requests made (see sample.json/logs)")
        base = options.server_base_url
        status, body = local_get(base + "/_ops/ready")
        if status != 200:
            raise RuntimeError("Arkiv server is not ready")
        report["dataset"] = json.loads(body)
        if report["dataset"].get("mode") != "arkiv":
            raise RuntimeError("Conformance requires the Arkiv-backed server")
        client = CachedClient(options.cache, delay_seconds=options.delay_seconds)
        query = "?" + urllib.parse.urlencode({"fields": ",".join(options.selected_fields)}) if options.selected_fields else ""
        for item in report["sample"]["samples"]:
            path = "/v2/contract/1/" + item["address"] + query
            result = {"matchId": item["matchId"], "address": item["address"], "chainId": "1"}
            # Persist independent live evidence before any local comparison.
            live = client.fetch(options.api_base_url + path)
            result["live"] = live
            if live.get("bodyPath"):
                live_artifact = options.output / (str(item["matchId"]) + ".live.body")
                live_artifact.write_bytes(Path(live["bodyPath"]).read_bytes())
                result["liveArtifact"] = str(live_artifact)
            if live["status"] not in (200, 404) or live.get("error"):
                result.update(outcome="inconclusive", reason=live.get("error") or f"Live request rejected: HTTP {live['status']}")
            else:
                try:
                    local_status, local_body = local_get(base + path)
                    local_path = options.output / (str(item["matchId"]) + ".local.json")
                    local_path.write_bytes(local_body)
                    result["local"] = {"status": local_status, "bodyPath": str(local_path), "bodySha256": sha256(local_body)}
                    if local_status != 200:
                        result.update(outcome="local_failure", reason=f"Our server returned HTTP {local_status}")
                    elif live["status"] == 404:
                        result.update(outcome="mismatch", differences=[{"path": "", "kind": "http_status", "expected": 404, "actual": 200}])
                    else:
                        differences = compare_json(load_json(Path(live["bodyPath"]).read_bytes()), load_json(local_body))
                        result.update(outcome="mismatch" if differences else "matched", differences=differences)
                except (OSError, ValueError, urllib.error.URLError, http.client.HTTPException) as error:
                    result.update(outcome="inconclusive", reason=f"Local transport or comparison response JSON failed: {error}")
            report["results"].append(result)
            print(json.dumps({"matchId": item["matchId"], "address": item["address"], "outcome": result["outcome"], "cached": live["cacheHit"]}), flush=True)
            save_json(options.output / "report.json", report)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        report["error"] = str(error)
    outcomes = [result["outcome"] for result in report["results"]]
    report["counts"] = {"requested": options.count, "sampled": len(report.get("sample", {}).get("samples", [])),
                        "reconstructed": report.get("sample", {}).get("completedCount", 0),
                        "live200": sum(result["live"]["status"] == 200 for result in report["results"]),
                        "matched": outcomes.count("matched"), "mismatched": outcomes.count("mismatch"),
                        "localFailures": outcomes.count("local_failure"), "inconclusive": outcomes.count("inconclusive"),
                        "cacheHits": sum(result["live"]["cacheHit"] for result in report["results"])}
    if report.get("error") or report.get("serverExitCode") or len(outcomes) != options.count or "inconclusive" in outcomes:
        report["status"] = "inconclusive"
    elif "mismatch" in outcomes or "local_failure" in outcomes:
        report["status"] = "failed"
    elif outcomes and all(outcome == "matched" for outcome in outcomes):
        report["status"] = "passed"
    report["completedAt"] = utc_now()
    save_json(options.output / "report.json", report)
    print(json.dumps({"status": report["status"], "counts": report["counts"], "report": str(options.output / 'report.json')}), flush=True)
    return {"passed": 0, "failed": 1, "inconclusive": 2}[report["status"]]


if __name__ == "__main__":
    raise SystemExit(run(parse_options()))
