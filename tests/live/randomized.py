#!/usr/bin/env python3
"""Bounded, replayable conformance checks of two deployed Mainnet APIs.

Discovery uses only the reference listing API. No local dataset, node, build,
credentials, or third-party Python packages are needed. This does not exercise
an unreleased server checkout. See docs/randomized-live-conformance.md.
"""
import argparse
from collections import Counter
import gzip
import hashlib
import http.client
import io
import json
from pathlib import Path
import re
import secrets
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from compare import CachedClient, compare_json, load_json, save_json, sha256, utc_now

REFERENCE = "https://sourcify-mirror.arkiv-global.net"
SERVER = "https://sourcify-prefill.arkiv-global.net"
ALGORITHM = "sha256-v1-random-match-id-seek-gap-weighted"
SCHEMA = "sourcify-arkiv.randomized-conformance.v1"
OPTIONAL = (
    "abi", "compilation", "deployment", "creationBytecode", "runtimeBytecode", "sources",
    "metadata", "storageLayout", "transientStorageLayout", "userdoc", "devdoc", "sourceIds",
    "stdJsonInput", "stdJsonOutput", "additionalInput", "signatures", "proxyResolution",
)
SUBFIELDS = {
    "creationBytecode": ("onchainBytecode", "recompiledBytecode", "sourceMap", "linkReferences", "cborAuxdata", "transformations", "transformationValues"),
    "runtimeBytecode": ("onchainBytecode", "recompiledBytecode", "sourceMap", "linkReferences", "cborAuxdata", "transformations", "transformationValues", "immutableReferences"),
    "deployment": ("transactionHash", "blockNumber", "transactionIndex", "deployer"),
    "compilation": ("language", "compiler", "compilerVersion", "compilerSettings", "name", "fullyQualifiedName"),
    "signatures": ("function", "event", "error"),
    "proxyResolution": ("onchainRuntimeBytecode", "onchainCreationBytecode"),
}
SAFE = "0x000000000034065b3a94C2118CFe5B4C0067B615"
WETH = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
ADDRESS = re.compile(r"0x[0-9a-fA-F]{40}\Z")
PATH = re.compile(r"/v2/contract/1/[^/?#]+(?:\?[^#]*)?\Z")
HEADERS = ("content-type", "content-encoding", "vary", "retry-after", "date", "access-control-allow-origin")


def number(seed, label, bound):
    return int.from_bytes(hashlib.sha256((seed + "\0" + label).encode()).digest(), "big") % bound


def subset(seed, label, values, maximum):
    ranked = sorted(values, key=lambda value: hashlib.sha256((seed + "\0" + label + "\0" + value).encode()).digest())
    return ranked[:1 + number(seed, label + "-size", min(maximum, len(values)))]


def case(name, address, query="", status=200, encoding="identity", **extra):
    return {"name": name, "path": "/v2/contract/1/" + address + query,
            "expectedStatus": status, "encoding": encoding, **extra}


def contract_cases(seed, item, include_proxy=True):
    label = item["matchId"]
    values = list(OPTIONAL if include_proxy else OPTIONAL[:-1])
    nested = [group + "." + field for group, fields in SUBFIELDS.items() for field in fields]
    fields = subset(seed, label + "-fields", values, 5)
    omissions = subset(seed, label + "-omit", values, 4)
    if not include_proxy:
        omissions.append("proxyResolution")
    return [
        case("minimal", item["address"]),
        case("random-fields", item["address"], "?fields=" + ",".join(fields)),
        case("random-nested", item["address"], "?fields=" + ",".join(subset(seed, label + "-nested", nested, 4))),
        case("random-omit", item["address"], "?omit=" + ",".join(omissions)),
        case("full", item["address"], "?fields=all" if include_proxy else "?omit=proxyResolution",
             encoding="gzip" if number(seed, label + "-encoding", 2) else "identity"),
    ]


def fixed_cases(seed, address, include_proxy=True):
    cases = [
        case("invalid-field", address, "?fields=__unknown__", 400),
        case("invalid-nested", address, "?fields=proxyResolution.isProxy", 400),
        case("conflicting-selectors", address, "?fields=abi&omit=sources", 400),
        case("repeated-parameter", address, "?fields=abi&fields=sources", 400),
        case("unknown-parameter", address, "?surprise=1", 400),
        case("invalid-address", "not-an-address", status=400),
        case("missing-address", "0x" + hashlib.sha256((seed + "\0missing").encode()).hexdigest()[:40], status=404),
        case("gzip", address, "?fields=abi,compilation", encoding="gzip"),
    ]
    if include_proxy:
        cases.extend([
            case("known-proxy", SAFE, "?fields=proxyResolution", expectedIsProxy=True),
            case("known-non-proxy", WETH, "?fields=proxyResolution", expectedIsProxy=False),
        ])
    return cases


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


class BudgetError(RuntimeError):
    pass


class Client:
    """Fresh single-flight HTTP captures with byte, call, retry and time budgets."""
    def __init__(self, output, delay=0.2, timeout=65, duration=900,
                 max_requests=1024, max_body=32 * 1024 * 1024, max_bytes=256 * 1024 * 1024,
                 sleep=time.sleep, monotonic=time.monotonic):
        self.output = Path(output)
        (self.output / "raw").mkdir(parents=True)
        self.delay, self.timeout = delay, timeout
        self.sleep, self.monotonic = sleep, monotonic
        self.deadline = monotonic() + duration
        self.next_request = 0
        self.max_requests, self.max_body, self.max_bytes = max_requests, max_body, max_bytes
        self.requests = self.bytes = 0
        self.opener = urllib.request.build_opener(NoRedirect())

    def check(self):
        if self.monotonic() >= self.deadline:
            raise BudgetError("run deadline exhausted")
        if self.requests >= self.max_requests:
            raise BudgetError("run HTTP request budget exhausted")
        if self.bytes >= self.max_bytes:
            raise BudgetError("run received-byte budget exhausted")

    def fetch(self, url, encoding="identity"):
        result = {"url": url, "encoding": encoding, "attempts": [], "status": None}
        for attempt in range(2):
            self.check()
            wait = max(0, self.next_request - self.monotonic())
            if wait > 30 or self.monotonic() + wait >= self.deadline:
                result["error"] = "required delay exceeds remaining run/retry budget"
                break
            if wait:
                self.sleep(wait)
            self.check()
            self.requests += 1
            stem = f"raw/{self.requests:04d}"
            capture = {"url": url, "capturedAt": utc_now(), "status": None, "headers": {}}
            body = bytearray()
            error = None
            budget_failure = False
            started = self.monotonic()
            self.next_request = started + self.delay
            try:
                request = urllib.request.Request(url, headers={
                    "Accept-Encoding": encoding,
                    "User-Agent": "arkiv-randomized-conformance/1.0 (+https://github.com/Arkiv-Network/sorcify-arkiv-entity-processor)",
                })
                try:
                    response = self.opener.open(request, timeout=min(self.timeout, self.deadline - started))
                except urllib.error.HTTPError as response_error:
                    response = response_error
                with response:
                    capture["status"] = response.status
                    capture["headers"] = {name: response.headers[name] for name in HEADERS if name in response.headers}
                    declared = response.headers.get("Content-Length")
                    if declared and not 0 <= int(declared) <= self.max_body:
                        raise BudgetError("response Content-Length exceeds body limit")
                    while True:
                        if self.monotonic() >= min(self.deadline, started + self.timeout):
                            raise BudgetError("response read deadline exhausted")
                        chunk = response.read1(65536)
                        if not chunk:
                            break
                        self.bytes += len(chunk)
                        if len(body) + len(chunk) > self.max_body or self.bytes > self.max_bytes:
                            raise BudgetError("response or cumulative byte budget exhausted")
                        body.extend(chunk)
                    if declared and len(body) != int(declared):
                        raise http.client.IncompleteRead(bytes(body), int(declared) - len(body))
            except (OSError, ValueError, http.client.HTTPException, BudgetError) as exception:
                error = f"{type(exception).__name__}: {exception}"
                budget_failure = isinstance(exception, BudgetError)
            raw = bytes(body)
            raw_path = stem + ".body"
            (self.output / raw_path).write_bytes(raw)
            capture.update(bodyPath=raw_path, bodySha256=sha256(raw), bodyBytes=len(raw), elapsedSeconds=round(self.monotonic() - started, 3))
            if error:
                capture["error"] = error
            save_json(self.output / (stem + ".json"), capture)
            result["attempts"].append(capture)
            result.update({key: value for key, value in capture.items() if key != "url"})
            transient = error is not None or capture["status"] == 429 or (capture["status"] or 0) >= 500
            if not transient:
                result.pop("error", None)
                return result
            result["error"] = error or f"HTTP {capture['status']}"
            backoff = max(2, CachedClient.retry_after(capture["headers"].get("retry-after")))
            self.next_request = max(self.next_request, self.monotonic() + backoff)
            if attempt == 1 or budget_failure:
                break
        return result

    def json(self, capture):
        if capture.get("error"):
            raise ValueError(capture["error"])
        content_type = capture["headers"].get("content-type", "").split(";")[0].strip().lower()
        if content_type != "application/json":
            raise ValueError("response Content-Type is not application/json")
        raw = (self.output / capture["bodyPath"]).read_bytes()
        encoding = capture["headers"].get("content-encoding", "identity").lower().strip()
        if encoding == "gzip":
            with gzip.GzipFile(fileobj=io.BytesIO(raw)) as stream:
                raw = stream.read(self.max_body + 1)
                if len(raw) > self.max_body:
                    raise ValueError("decoded gzip body exceeds response limit")
        elif encoding != "identity":
            raise ValueError("unexpected response Content-Encoding: " + encoding)
        return load_json(raw)


def listing(client, base, direction, after=None):
    query = {"sort": direction, "limit": 1}
    if after is not None:
        query["afterMatchId"] = str(after)
    capture = client.fetch(base + "/v2/contracts/1?" + urllib.parse.urlencode(query))
    if capture["status"] != 200:
        raise ValueError(f"reference listing failed: HTTP {capture['status']}")
    payload = client.json(capture)
    if not isinstance(payload, dict) or not isinstance(payload.get("results"), list) or len(payload["results"]) > 1:
        raise ValueError("invalid reference listing shape")
    for row in payload["results"]:
        if (not isinstance(row, dict) or row.get("chainId") != "1"
                or not isinstance(row.get("matchId"), str) or not row["matchId"].isdecimal()
                or int(row["matchId"]) < 1 or not ADDRESS.fullmatch(row.get("address", ""))):
            raise ValueError("invalid reference listing contract")
        if after is not None and not (int(row["matchId"]) > after if direction == "asc" else int(row["matchId"]) < after):
            raise ValueError("reference listing violates exclusive match-ID cursor")
    return payload["results"], capture


def discover(client, options, sample):
    oldest, capture = listing(client, options.reference_base_url, "asc")
    sample["discovery"].append({"kind": "minimum", "response": capture})
    newest, capture = listing(client, options.reference_base_url, "desc", options.max_match_id + 1 if options.max_match_id else None)
    sample["discovery"].append({"kind": "maximum", "response": capture})
    if not oldest or not newest:
        raise ValueError("reference population is empty within the requested bound")
    lower, upper = int(oldest[0]["matchId"]), int(newest[0]["matchId"])
    if lower > upper:
        raise ValueError("invalid reference match-ID bounds")
    sample["bounds"] = {"minimumMatchId": str(lower), "maximumMatchId": str(upper),
                        "configuredMaximumMatchId": str(options.max_match_id) if options.max_match_id else None}
    seen_ids, seen_addresses = set(), set()
    for index in range(options.count * 8):
        anchor = lower + number(options.seed, "anchor-" + str(index), upper - lower + 1)
        rows, capture = listing(client, options.reference_base_url, "asc", anchor - 1)
        discovery = {"kind": "seek", "index": index, "anchor": str(anchor), "response": capture}
        sample["discovery"].append(discovery)
        if not rows or int(rows[0]["matchId"]) > upper:
            discovery["outcome"] = "empty-or-outside-frozen-bound"
        elif rows[0]["matchId"] in seen_ids or rows[0]["address"].lower() in seen_addresses:
            discovery["outcome"] = "duplicate"
        else:
            row = rows[0]
            seen_ids.add(row["matchId"])
            seen_addresses.add(row["address"].lower())
            item = {"matchId": row["matchId"], "address": row["address"], "listing": row, "anchor": str(anchor)}
            item["cases"] = contract_cases(options.seed, item, options.include_proxy)
            sample["samples"].append(item)
            discovery["outcome"] = "selected"
        save_json(options.output / "sample.json", sample)
        if len(sample["samples"]) == options.count:
            return
    raise ValueError("discovery attempt budget exhausted without a complete distinct sample")


def validate_replay(sample, count):
    if (not isinstance(sample, dict) or sample.get("schema") != SCHEMA or sample.get("algorithm") != ALGORITHM
            or not isinstance(sample.get("samples"), list) or len(sample["samples"]) != count
            or not isinstance(sample.get("fixedCases"), list) or not sample["fixedCases"]):
        raise ValueError("invalid replay sample schema, count or fixed cases")
    seen = set()
    for item in sample["samples"]:
        if (not isinstance(item, dict) or not isinstance(item.get("matchId"), str) or not item["matchId"].isdecimal()
                or not ADDRESS.fullmatch(item.get("address", "")) or item["address"].lower() in seen
                or len(item.get("cases", [])) != 5):
            raise ValueError("invalid replay contract or case count")
        seen.add(item["address"].lower())
        for entry in item["cases"]:
            if not isinstance(entry, dict) or not isinstance(entry.get("path"), str) or entry["path"].split("?")[0] != "/v2/contract/1/" + item["address"]:
                raise ValueError("replay case address differs from sampled contract")
    cases = [entry for item in sample["samples"] for entry in item["cases"]] + sample["fixedCases"]
    if len(cases) > 330:
        raise ValueError("replay case budget exceeded")
    for entry in cases:
        if (not isinstance(entry, dict) or not isinstance(entry.get("name"), str)
                or not PATH.fullmatch(entry.get("path", "")) or len(entry["path"]) > 4096
                or entry.get("expectedStatus") not in (200, 400, 404)
                or entry.get("encoding") not in ("identity", "gzip")):
            raise ValueError("invalid replay case")


def normalize_expected_error(body):
    if not isinstance(body, dict) or not isinstance(body.get("errorId"), str):
        raise ValueError("expected validation error has no errorId")
    parsed = uuid.UUID(body["errorId"])
    if parsed.version != 4:
        raise ValueError("validation errorId is not UUIDv4")
    return {key: value for key, value in body.items() if key != "errorId"}


def compare_case(client, reference, server, entry, match_id=None):
    result = {"case": entry, "matchId": match_id, "outcome": "inconclusive", "differences": []}
    for side, base in (("reference", reference), ("server", server)):
        result[side] = client.fetch(base + entry["path"], entry["encoding"])
    try:
        payloads = [client.json(result[side]) for side in ("reference", "server")]
        for side, payload in zip(("reference", "server"), payloads):
            status = result[side]["status"]
            if status != entry["expectedStatus"]:
                result["differences"].append({"path": "/httpStatus", "side": side, "kind": "unexpected_status", "expected": entry["expectedStatus"], "actual": status})
            if status == 200 and isinstance(payload, dict) and "customCode" in payload:
                raise ValueError(side + " returned an error envelope with HTTP 200")
            if isinstance(payload, dict) and isinstance(payload.get("proxyResolution"), dict) and "proxyResolutionError" in payload["proxyResolution"]:
                raise ValueError(side + " returned a proxy resolution error; this is not a successful proxy check")
            proxy = payload.get("proxyResolution") if isinstance(payload, dict) else None
            if "expectedIsProxy" in entry and (not isinstance(proxy, dict) or proxy.get("isProxy") is not entry["expectedIsProxy"]):
                result["differences"].append({"path": "/proxyResolution/isProxy", "side": side, "kind": "unexpected_proxy_detection", "expected": entry["expectedIsProxy"]})
        if match_id and isinstance(payloads[0], dict) and payloads[0].get("matchId") != match_id:
            result["differences"].append({"path": "/matchId", "side": "reference", "kind": "reference_population_changed", "expected": match_id, "actual": payloads[0].get("matchId")})
        if entry["expectedStatus"] == 400 and all(result[side]["status"] == 400 for side in ("reference", "server")):
            payloads = [normalize_expected_error(payload) for payload in payloads]
            result["normalizedPaths"] = ["/errorId (validated UUIDv4 on expected HTTP 400 only)"]
        if entry["encoding"] == "gzip":
            headers = result["server"]["headers"]
            if headers.get("content-encoding", "").lower() != "gzip" or "accept-encoding" not in headers.get("vary", "").lower():
                result["differences"].append({"path": "/httpHeaders", "kind": "gzip_negotiation_failed"})
        differences = compare_json(*payloads)
        result["differenceCount"] = len(differences) + len(result["differences"])
        result["differences"].extend(differences[:100])
        result["outcome"] = "mismatch" if result["differences"] else "matched"
    except (ValueError, OSError, EOFError, RecursionError, TypeError) as error:
        result["error"] = str(error)
        if result["differences"]:
            result["outcome"] = "mismatch"
    return result


def base_url(value):
    parsed = urllib.parse.urlsplit(value)
    if (parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username is not None
            or parsed.password is not None or parsed.query or parsed.fragment):
        raise argparse.ArgumentTypeError("base URL must be HTTP(S), without credentials, query or fragment")
    return value.rstrip("/")


def parse_options(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--reference-base-url", type=base_url, default=REFERENCE)
    parser.add_argument("--server-base-url", type=base_url, default=SERVER)
    parser.add_argument("--count", type=int, default=8)
    parser.add_argument("--seed", default=None)
    parser.add_argument("--max-match-id", type=int)
    parser.add_argument("--sample-file", type=Path, help="Replay a retained sample and exact case paths; no listing requests")
    parser.add_argument("--without-proxy", dest="include_proxy", action="store_false", help="Use omit=proxyResolution for full records and skip live proxy sentinels")
    parser.add_argument("--delay-seconds", type=float, default=0.2)
    parser.add_argument("--timeout-seconds", type=float, default=65)
    parser.add_argument("--max-seconds", type=float, default=900)
    options = parser.parse_args(argv)
    if not 1 <= options.count <= 64 or not 0 <= options.delay_seconds <= 5 or not 0 < options.timeout_seconds <= 65 or not 0 < options.max_seconds <= 900:
        parser.error("count must be 1..64, delay 0..5, timeout 0..65, and max-seconds 0..900")
    if options.max_match_id is not None and not 1 <= options.max_match_id <= 2**63 - 1:
        parser.error("max-match-id must be a positive signed 64-bit integer")
    if options.seed is None:
        options.seed = secrets.token_hex(16)
    if not 1 <= len(options.seed) <= 128:
        parser.error("seed must have 1..128 characters")
    if options.reference_base_url.lower() == options.server_base_url.lower():
        parser.error("reference and server URLs must differ")
    options.output = options.output.resolve()
    return options


def run(options):
    options.output.mkdir(parents=True, exist_ok=False)
    sample = {"schema": SCHEMA, "algorithm": ALGORITHM, "seed": options.seed,
              "referenceBaseUrl": options.reference_base_url, "includeProxy": options.include_proxy,
              "samples": [], "discovery": []}
    report = {"schema": SCHEMA, "startedAt": utc_now(), "seed": options.seed, "requestedCount": options.count,
              "coverage": "Two deployed services, not the candidate server checkout",
              "referenceBaseUrl": options.reference_base_url, "serverBaseUrl": options.server_base_url,
              "comparison": "Exact JSON values, array order and embedded strings; only validated UUIDv4 errorId excluded on expected HTTP400",
              "sampling": ALGORITHM, "status": "inconclusive", "results": []}
    client = Client(options.output, delay=options.delay_seconds, timeout=options.timeout_seconds, duration=options.max_seconds)
    try:
        if options.sample_file:
            replay_bytes = options.sample_file.read_bytes()
            sample = json.loads(replay_bytes)
            validate_replay(sample, options.count)
            (options.output / "replay-input.json").write_bytes(replay_bytes)
            # Original discovery body paths belong to the original artifact archive.
            # Do not let them refer to this run's newly numbered raw captures.
            sample["discovery"] = []
            sample["replaySource"] = {"samplePath": str(options.sample_file), "sampleSha256": sha256(replay_bytes),
                                    "originalManifest": "replay-input.json", "capturePaths": "relative to original archive"}
            if options.max_match_id is not None and any(int(item["matchId"]) > options.max_match_id for item in sample["samples"]):
                raise ValueError("replay sample exceeds configured maximum match ID")
            report["seed"] = sample["seed"]
            report["replayedFrom"] = str(options.sample_file)
        else:
            discover(client, options, sample)
            sample["fixedCases"] = fixed_cases(options.seed, sample["samples"][0]["address"], options.include_proxy)
        save_json(options.output / "sample.json", sample)
        work = [(entry, item["matchId"]) for item in sample["samples"] for entry in item["cases"]]
        work.extend((entry, None) for entry in sample["fixedCases"])
        report["expectedCases"] = len(work)
        report["sampleBounds"] = sample.get("bounds")
        for entry, match_id in work:
            result = compare_case(client, options.reference_base_url, options.server_base_url, entry, match_id)
            report["results"].append(result)
            print(json.dumps({"case": entry["name"], "matchId": match_id, "path": entry["path"], "outcome": result["outcome"]}), flush=True)
            save_json(options.output / "report.json", report)
    except (OSError, ValueError, RuntimeError, KeyError, TypeError, RecursionError, KeyboardInterrupt) as error:
        report["error"] = f"{type(error).__name__}: {error}"
    counts = Counter(result["outcome"] for result in report["results"])
    report["counts"] = {"sampled": len(sample.get("samples", [])), "matched": counts["matched"], "mismatched": counts["mismatch"], "inconclusive": counts["inconclusive"], "completedCases": len(report["results"])}
    if counts["mismatch"]:
        report["status"] = "failed"
    elif (not report.get("error") and not counts["inconclusive"] and counts["matched"]
          and len(report["results"]) == report.get("expectedCases") and len(sample.get("samples", [])) == options.count):
        report["status"] = "passed"
    report.update(completedAt=utc_now(), httpRequests=client.requests, receivedBytes=client.bytes)
    save_json(options.output / "sample.json", sample)
    save_json(options.output / "report.json", report)
    print(json.dumps({"status": report["status"], "counts": report["counts"], "report": str(options.output / "report.json")}), flush=True)
    return {"passed": 0, "failed": 1, "inconclusive": 2}[report["status"]]


if __name__ == "__main__":
    raise SystemExit(run(parse_options()))
