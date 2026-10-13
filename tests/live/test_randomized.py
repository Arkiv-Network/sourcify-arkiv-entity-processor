"""Independent localhost-only exercises for randomized deployed conformance."""
from contextlib import contextmanager, redirect_stdout
import copy
import gzip
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit
import uuid

import randomized as runner


@contextmanager
def endpoints():
    state = {"requests": [], "fault": None, "mode": None}
    rows = [{"matchId": str(100 + i), "address": "0x" + str(i + 1) * 40,
             "chainId": "1", "match": "exact_match", "creationMatch": "exact_match",
             "runtimeMatch": "exact_match", "verifiedAt": "2026-09-01T00:00:00Z"} for i in range(3)]

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            state["requests"].append((self.path, self.headers.get("Accept-Encoding")))
            split = urlsplit(self.path)
            query = parse_qs(split.query, keep_blank_values=True)
            side = split.path.split("/")[1]
            status = 200
            if split.path.endswith("/contracts/1"):
                selected = sorted(rows, key=lambda row: int(row["matchId"]), reverse=query["sort"] == ["desc"])
                if "afterMatchId" in query:
                    cursor = int(query["afterMatchId"][0])
                    selected = [row for row in selected if (int(row["matchId"]) > cursor if query["sort"] == ["asc"] else int(row["matchId"]) < cursor)]
                body = {"results": selected[:1]}
                if state["mode"] == "empty":
                    body = {"results": []}
                elif state["mode"] == "malformed-list":
                    body = {"results": [None]}
            else:
                address = split.path.split("/")[-1]
                row = next((row for row in rows if row["address"] == address), None)
                if (address == "not-an-address" or "surprise" in query or len(query.get("fields", [])) > 1
                        or ("fields" in query and "omit" in query) or query.get("fields") in (["__unknown__"], ["proxyResolution.isProxy"])):
                    status = 400
                    body = {"customCode": "invalid_parameter", "message": "invalid request", "errorId": str(uuid.uuid4())}
                elif row or address in (runner.SAFE, runner.WETH):
                    body = copy.deepcopy(row or {**rows[0], "address": address})
                    fields = query.get("fields", [""])[0].split(",")
                    if "all" in fields or "proxyResolution" in fields or ("omit" in query and "proxyResolution" not in query["omit"][0].split(",")):
                        body["proxyResolution"] = {"isProxy": address == runner.SAFE, "proxyType": "GnosisSafeProxy" if address == runner.SAFE else None, "implementations": []}
                    body["abi"] = ["exact", "order"]
                    if side == "server" and state["mode"] == "mismatch":
                        body["abi"].reverse()
                    if side == "server" and state["mode"] == "missing":
                        status = 404
                        body = {"match": None}
                    if side == "server" and state["mode"] == "null-proxy":
                        body["proxyResolution"] = None
                    if state["mode"] == "proxy-error":
                        body["proxyResolution"] = {"proxyResolutionError": {"errorId": str(uuid.uuid4()), "customCode": "proxy_resolution_error"}}
                    if side == "reference" and state["mode"] == "population-drift":
                        body["matchId"] = "999"
                else:
                    status = 404
                    body = {"address": address, "chainId": "1", "match": None, "creationMatch": None, "runtimeMatch": None}
            raw = json.dumps(body).encode()
            headers = {"Content-Type": "application/json"}
            if self.headers.get("Accept-Encoding") == "gzip":
                raw = gzip.compress(raw)
                headers.update({"Content-Encoding": "gzip", "Vary": "Accept-Encoding"})
            if state["fault"]:
                status, raw, fault_headers = state["fault"](side, self.path, status, raw)
                headers.update(fault_headers)
            self.send_response(status)
            for key, value in headers.items():
                self.send_header(key, value)
            if "Content-Length" not in headers:
                self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            try:
                self.wfile.write(raw)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
    thread.start()
    try:
        base = f"http://127.0.0.1:{server.server_port}"
        yield base, state
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class RandomizedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="randomized-conformance-")
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def options(self, base, name="run", *extra):
        return runner.parse_options(["--output", str(self.root / name), "--count", "2", "--seed", "independent-seed",
                                     "--reference-base-url", base + "/reference", "--server-base-url", base + "/server",
                                     "--delay-seconds", "0", "--max-match-id", "102", *extra])

    def execute(self, options):
        with redirect_stdout(io.StringIO()):
            code = runner.run(options)
        return code, json.loads((options.output / "report.json").read_text()), json.loads((options.output / "sample.json").read_text())

    def test_reproducible_sampling_full_matrix_and_exact_replay_without_discovery(self):
        with endpoints() as (base, state):
            code, report, sample = self.execute(self.options(base))
            self.assertEqual(code, 0, report)
            self.assertEqual(report["counts"]["matched"], 20)
            self.assertEqual(len(sample["samples"]), 2)
            self.assertEqual(sample["bounds"]["maximumMatchId"], "102")
            self.assertEqual(sample["bounds"]["configuredMaximumMatchId"], "102")
            self.assertEqual(len({item["address"] for item in sample["samples"]}), 2)
            self.assertTrue(any("afterMatchId=103" in path for path, _ in state["requests"]))
            self.assertTrue(any(encoding == "gzip" for _, encoding in state["requests"]))
            self.assertTrue(all((self.root / "run" / attempt["bodyPath"]).exists() for result in report["results"] for side in ("reference", "server") for attempt in result[side]["attempts"]))
            _, _, again = self.execute(self.options(base, "again"))
            self.assertEqual(sample["samples"], again["samples"])
            state["requests"].clear()
            code, report, replay = self.execute(self.options(base, "replay", "--sample-file", str(self.root / "run/sample.json"), "--seed", "ignored-when-replaying"))
            self.assertEqual(code, 0, report)
            self.assertEqual(report["seed"], "independent-seed")
            self.assertFalse(any("/contracts/" in path for path, _ in state["requests"]))
            self.assertEqual(sample["samples"], replay["samples"])
            self.assertEqual(sample["fixedCases"], replay["fixedCases"])
            self.assertEqual(replay["discovery"], [])
            self.assertEqual(json.loads((self.root / "replay/replay-input.json").read_text()), sample)

    def test_missing_mismatch_and_population_drift_never_resample_or_pass(self):
        for mode in ("missing", "mismatch", "population-drift", "null-proxy"):
            with self.subTest(mode=mode), endpoints() as (base, state):
                state["mode"] = mode
                code, report, sample = self.execute(self.options(base, mode))
                self.assertEqual(code, 1, report)
                self.assertEqual(len(sample["samples"]), 2)
                self.assertEqual(report["counts"]["completedCases"], 20)
                self.assertGreater(report["counts"]["mismatched"], 0)
                self.assertEqual(sum(d["outcome"] == "selected" for d in sample["discovery"] if d["kind"] == "seek"), 2)

    def test_empty_or_malformed_population_and_both_proxy_errors_cannot_pass(self):
        for mode in ("empty", "malformed-list", "proxy-error"):
            with self.subTest(mode=mode), endpoints() as (base, state):
                state["mode"] = mode
                code, report, _ = self.execute(self.options(base, mode))
                self.assertEqual(code, 2, report)
                self.assertEqual(report["status"], "inconclusive")

    def test_5xx_and_429_cannot_pass_even_when_both_sides_match(self):
        for status in (503, 429):
          with self.subTest(status=status), endpoints() as (base, state):
            state["fault"] = lambda side, path, _status, raw: (status, b'{"busy":true}', {})
            output = self.root / ("outage" + str(status))
            output.mkdir()
            client = runner.Client(output, delay=0, sleep=lambda _: None)
            result = runner.compare_case(client, base + "/reference", base + "/server", runner.case("outage", runner.SAFE))
            self.assertEqual(result["outcome"], "inconclusive")
            self.assertEqual(len(result["reference"]["attempts"]), 2)
            self.assertEqual(len(result["server"]["attempts"]), 2)
            self.assertEqual(len(list((output / "raw").glob("*.body"))), 4)

    def test_truncated_and_malformed_json_do_not_pass(self):
        for fault in (lambda _s, _p, _c, _b: (200, b'{}', {"Content-Length": "100"}),
                      lambda _s, _p, _c, _b: (200, b'not JSON', {}),
                      lambda _s, _p, _c, _b: (200, b'broken gzip', {"Content-Encoding": "gzip"}),
                      lambda _s, _p, _c, _b: (200, b'{"customCode":"upstream_error"}', {})):
            with endpoints() as (base, state):
                state["fault"] = fault
                output = self.root / str(len(list(self.root.iterdir())))
                output.mkdir()
                client = runner.Client(output, delay=0, sleep=lambda _: None)
                result = runner.compare_case(client, base + "/reference", base + "/server", runner.case("bad", runner.SAFE))
                self.assertEqual(result["outcome"], "inconclusive")

    def test_body_and_request_budgets_bound_capture_without_retrying_limits(self):
        with endpoints() as (base, _):
            output = self.root / "limits"
            output.mkdir()
            client = runner.Client(output, delay=0, max_body=1, max_requests=1)
            result = client.fetch(base + "/reference/v2/contract/1/" + runner.SAFE)
            self.assertIn("body limit", result["error"])
            self.assertEqual(len(result["attempts"]), 1)
            with self.assertRaises(runner.BudgetError):
                client.fetch(base + "/reference/v2/contract/1/" + runner.SAFE)

    def test_timeout_and_long_retry_after_never_pass_or_retry_early(self):
        with endpoints() as (base, state):
            output = self.root / "timeout"
            output.mkdir()
            def delayed(_side, _path, status, raw):
                time.sleep(0.05)
                return status, raw, {}
            state["fault"] = delayed
            client = runner.Client(output, delay=0, timeout=0.01, sleep=lambda _: None)
            result = client.fetch(base + "/reference/v2/contract/1/" + runner.SAFE)
            self.assertIn("error", result)
            self.assertEqual(len(result["attempts"]), 2)
        with endpoints() as (base, state):
            output = self.root / "retry-after"
            output.mkdir()
            state["fault"] = lambda _s, _p, _c, _b: (429, b'{}', {"Retry-After": "60"})
            client = runner.Client(output, delay=0)
            result = client.fetch(base + "/reference/v2/contract/1/" + runner.SAFE)
            self.assertIn("required delay", result["error"])
            self.assertEqual(len(state["requests"]), 1)
            self.assertEqual(len(result["attempts"]), 1)

    def test_expected_errors_validate_uuid_and_success_arrays_strings_types_remain_exact(self):
        with self.assertRaises(ValueError):
            runner.normalize_expected_error({"errorId": "not-a-uuid"})
        body = {"errorId": str(uuid.uuid4()), "customCode": "invalid_parameter"}
        self.assertEqual(runner.normalize_expected_error(body), {"customCode": "invalid_parameter"})
        self.assertTrue(runner.compare_json({"abi": [1, 2], "metadata": '{ "a":1}', "bool": True},
                                            {"abi": [2, 1], "metadata": '{"a":1}', "bool": 1}))

    def test_replay_validation_rejects_foreign_paths_inconsistent_addresses_and_excess_cases(self):
        with endpoints() as (base, _):
            _, _, sample = self.execute(self.options(base))
        for path in ("https://other.example/v2/contract/1/0x0", "/v2/contracts/1", "/v2/contract/1/other"):
            bad = copy.deepcopy(sample)
            bad["samples"][0]["cases"][0]["path"] = path
            with self.assertRaises(ValueError):
                runner.validate_replay(bad, 2)
        bad = copy.deepcopy(sample)
        bad["fixedCases"] *= 40
        with self.assertRaises(ValueError):
            runner.validate_replay(bad, 2)

    def test_selectors_are_seeded_and_live_proxy_can_be_disabled_explicitly(self):
        item = {"matchId": "7", "address": runner.SAFE}
        self.assertNotEqual(runner.contract_cases("a", item), runner.contract_cases("b", item))
        cases = runner.contract_cases("a", item, False)
        self.assertEqual(cases[-1]["path"].split("?")[1], "omit=proxyResolution")
        self.assertFalse(any(c["name"] == "known-proxy" for c in runner.fixed_cases("a", runner.SAFE, False)))
        for entry in cases:
            self.assertNotIn("fields=all", entry["path"])

    def test_existing_mismatch_takes_precedence_over_later_run_budget_failure(self):
        with endpoints() as (base, state):
            state["mode"] = "mismatch"
            real = runner.compare_case
            calls = 0
            def bounded(*args, **kwargs):
                nonlocal calls
                calls += 1
                if calls > 1:
                    raise runner.BudgetError("stop")
                return real(*args, **kwargs)
            with patch.object(runner, "compare_case", bounded):
                code, report, _ = self.execute(self.options(base))
            self.assertEqual(code, 1, report)
            self.assertIn("stop", report["error"])
            self.assertEqual(report["counts"]["completedCases"], 1)


if __name__ == "__main__":
    unittest.main()
