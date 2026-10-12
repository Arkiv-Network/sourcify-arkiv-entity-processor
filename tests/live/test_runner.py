"""Offline tests for live comparison; every HTTP request stays on localhost."""

from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from email.utils import format_datetime
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
from pathlib import Path
import socket
import sys
import tempfile
import threading
import unittest


SPEC = importlib.util.spec_from_file_location(
    "live_compare_runner", Path(__file__).with_name("compare.py")
)
runner = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = runner
SPEC.loader.exec_module(runner)


class FakeClock:
    def __init__(self):
        self.now = 1_000.0
        self.sleeps = []

    def monotonic(self):
        return self.now

    def sleep(self, duration):
        if duration < 0:
            raise AssertionError("negative sleep")
        self.sleeps.append(duration)
        self.now += duration


@contextmanager
def mock_http(clock):
    """Serve queued responses, repeating the final response for each path."""
    routes = {}
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append(
                {
                    "path": self.path,
                    "userAgent": self.headers.get("User-Agent"),
                    "at": clock.monotonic(),
                }
            )
            queue = routes[self.path]
            status, body, headers = queue[0]
            if len(queue) > 1:
                queue.pop(0)
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", headers.get("Content-Length", str(len(body))))
            for name, value in headers.items():
                if name.lower() != "content-length":
                    self.send_header(name, value)
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(
        target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True
    )
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", routes, requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class ComparatorTests(unittest.TestCase):
    def differences(self, expected, actual, path=""):
        differences = runner.compare_json(expected, actual, path=path)
        self.assertIsInstance(differences, list)
        return {(item["path"], item["kind"]) for item in differences}

    def test_object_order_is_irrelevant_and_array_order_is_significant(self):
        self.assertEqual(
            runner.compare_json({"a": 1, "b": [None, True]}, {"b": [None, True], "a": 1}),
            [],
        )
        self.assertEqual(
            self.differences([1, 2], [2, 1]),
            {("/0", "value"), ("/1", "value")},
        )

    def test_missing_null_and_unexpected_values_are_distinct(self):
        self.assertEqual(self.differences({"a": None}, {}), {("/a", "missing")})
        self.assertEqual(self.differences({}, {"a": None}), {("/a", "unexpected")})
        self.assertEqual(self.differences({"a": None}, {"a": {}}), {("/a", "type")})
        self.assertEqual(runner.compare_json(None, None), [])

    def test_json_pointer_escaping_and_starting_path(self):
        self.assertEqual(
            self.differences({"a/b": {"~key": [1, 2]}}, {"a/b": {"~key": [1, 3]}}),
            {("/a~1b/~0key/1", "value")},
        )
        self.assertEqual(self.differences({"a": 1}, {"a": 2}, "/prefix"), {("/prefix/a", "value")})

    def test_boolean_and_number_are_different_json_types(self):
        for expected, actual in [(True, 1), (False, 0), (1, True), ("1", 1), ({}, [])]:
            with self.subTest(expected=expected, actual=actual):
                self.assertEqual(self.differences(expected, actual), {("", "type")})

    def test_array_length_difference_is_explicit(self):
        self.assertIn(("/items", "length"), self.differences({"items": [1, 2]}, {"items": [1]}))

    def test_json_numbers_remain_exact_beyond_binary_float_precision(self):
        expected = runner.load_json(b'{"big":9007199254740993,"precise":0.123456789012345678901}')
        actual = runner.load_json('{"precise":0.123456789012345678902,"big":9007199254740992}')
        self.assertEqual(expected["big"], 9007199254740993)
        self.assertIsInstance(expected["precise"], Decimal)
        self.assertEqual(expected["precise"], Decimal("0.123456789012345678901"))
        self.assertEqual(self.differences(expected, actual), {("/big", "value"), ("/precise", "value")})
        self.assertEqual(runner.compare_json(1, Decimal("1.000")), [])


class CachedClientTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="live-compare-test-")
        self.cache = Path(self.temporary.name)
        self.clock = FakeClock()

    def tearDown(self):
        self.temporary.cleanup()

    def client(self, **kwargs):
        return runner.CachedClient(
            self.cache,
            delay_seconds=1.0,
            sleep=self.clock.sleep,
            monotonic=self.clock.monotonic,
            **kwargs,
        )

    def body_path(self, result):
        path = Path(result["bodyPath"])
        return path if path.is_absolute() else self.cache / path

    def assert_capture(self, result, url, status, body):
        self.assertEqual(result["url"], url)
        self.assertEqual(result["status"], status)
        self.assertIsInstance(result["cacheHit"], bool)
        self.assertIsInstance(result["attempts"], list)
        self.assertEqual(result["bodySha256"], hashlib.sha256(body).hexdigest())
        self.assertEqual(self.body_path(result).read_bytes(), body)
        captured = datetime.fromisoformat(result["capturedAt"].replace("Z", "+00:00"))
        self.assertIsNotNone(captured.tzinfo)

    def test_success_cache_persists_exact_raw_bytes_across_clients(self):
        body = b'{ "precise": 0.123456789012345678901, "text": "raw spacing" }\n'
        with mock_http(self.clock) as (base, routes, requests):
            routes["/ok"] = [(200, body, {})]
            url = base + "/ok"
            first = self.client().fetch(url)
            self.assert_capture(first, url, 200, body)
            self.assertFalse(first["cacheHit"])
            self.assertFalse(first.get("error"))
            self.assertEqual(len(first["attempts"]), 1)
            second = self.client().fetch(url)
            self.assert_capture(second, url, 200, body)
            self.assertTrue(second["cacheHit"])
            self.assertEqual(len(requests), 1)
            self.assertEqual(requests[0]["userAgent"], runner.DEFAULT_USER_AGENT)
            self.assertNotIn("Python-urllib", requests[0]["userAgent"])

    def test_stable_client_errors_are_cached_without_retry(self):
        with mock_http(self.clock) as (base, routes, requests):
            for status in [400, 404, 410]:
                with self.subTest(status=status):
                    path = f"/status/{status}"
                    body = f'{{"status":{status}}}'.encode()
                    routes[path] = [(status, body, {})]
                    first = self.client().fetch(base + path)
                    self.assert_capture(first, base + path, status, body)
                    self.assertEqual(len(first["attempts"]), 1)
                    self.assertFalse(first["cacheHit"])
                    self.assertTrue(self.client().fetch(base + path)["cacheHit"])
            self.assertEqual(len(requests), 3)

    def test_rate_limit_and_configured_user_agent_apply_to_distinct_urls(self):
        agent = "arkiv-live-check-tests/1.0 (offline localhost verification)"
        with mock_http(self.clock) as (base, routes, requests):
            routes["/first"] = [(200, b"{}", {})]
            routes["/second"] = [(200, b"{}", {})]
            client = self.client(user_agent=agent)
            client.fetch(base + "/first")
            client.fetch(base + "/second")
            self.assertEqual(len(requests), 2)
            self.assertGreaterEqual(requests[1]["at"] - requests[0]["at"], 1.0)
            self.assertEqual([request["userAgent"] for request in requests], [agent, agent])

    def test_retry_after_seconds_and_exponential_backoff_recover(self):
        body = b'{"recovered":true}'
        with mock_http(self.clock) as (base, routes, requests):
            routes["/retry"] = [
                (429, b'{"busy":true}', {"Retry-After": "5"}),
                (503, b'{"busy":true}', {}),
                (200, body, {}),
            ]
            result = self.client().fetch(base + "/retry")
            self.assert_capture(result, base + "/retry", 200, body)
            self.assertEqual(len(result["attempts"]), 3)
            self.assertFalse(result.get("error"))
            self.assertGreaterEqual(requests[1]["at"] - requests[0]["at"], 5.0)
            self.assertGreaterEqual(requests[2]["at"] - requests[1]["at"], 4.0)
            self.assertTrue(self.client().fetch(base + "/retry")["cacheHit"])
            self.assertEqual(len(requests), 3)

    def test_retry_after_http_date_is_honored(self):
        retry_at = format_datetime(datetime.now(timezone.utc) + timedelta(seconds=6), usegmt=True)
        with mock_http(self.clock) as (base, routes, requests):
            routes["/date"] = [(429, b"{}", {"Retry-After": retry_at}), (200, b"{}", {})]
            result = self.client().fetch(base + "/date")
            self.assertEqual(result["status"], 200)
            self.assertEqual(len(result["attempts"]), 2)
            # HTTP dates have one-second precision; tolerate test setup time.
            self.assertGreaterEqual(requests[1]["at"] - requests[0]["at"], 4.0)

    def test_retry_exhaustion_is_preserved_but_not_reused_as_stable_cache(self):
        busy = b'{"busy":true}'
        with mock_http(self.clock) as (base, routes, requests):
            routes["/outage"] = [(503, busy, {})] * 3 + [(200, b'{"ready":true}', {})]
            url = base + "/outage"
            failed = self.client().fetch(url)
            self.assert_capture(failed, url, 503, busy)
            self.assertEqual(len(failed["attempts"]), 3)
            self.assertTrue(failed.get("error"))
            self.assertFalse(failed["cacheHit"])
            recovered = self.client().fetch(url)
            self.assertEqual(recovered["status"], 200)
            self.assertFalse(recovered["cacheHit"])
            self.assertEqual(len(recovered["attempts"]), 1)
            self.assertEqual(len(requests), 4)
            self.assertGreaterEqual(requests[1]["at"] - requests[0]["at"], 2.0)
            self.assertGreaterEqual(requests[2]["at"] - requests[1]["at"], 4.0)

    def test_connection_errors_are_retried_with_a_bounded_attempt_count(self):
        # A bound, non-listening local port deterministically refuses connections.
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            port = reserved.getsockname()[1]
            result = self.client(timeout_seconds=0.2).fetch(f"http://127.0.0.1:{port}/offline")
        self.assertIsNone(result["status"])
        self.assertEqual(len(result["attempts"]), 3)
        self.assertTrue(result.get("error"))
        self.assertFalse(result["cacheHit"])
        self.assertGreaterEqual(sum(self.clock.sleeps), 6.0)

    def test_configured_attempt_limit_bounds_repeated_rate_limit_responses(self):
        with mock_http(self.clock) as (base, routes, requests):
            routes["/limited"] = [(429, b'{"limited":true}', {})]
            result = self.client(max_attempts=2).fetch(base + "/limited")
            self.assertEqual(result["status"], 429)
            self.assertEqual(len(result["attempts"]), 2)
            self.assertEqual(len(requests), 2)
            self.assertTrue(result.get("error"))

    def test_retry_exhaustion_cooldown_applies_to_the_next_url(self):
        with mock_http(self.clock) as (base, routes, requests):
            routes["/busy"] = [(503, b"{}", {})]
            routes["/next"] = [(200, b"{}", {})]
            client = self.client(max_attempts=2)
            self.assertTrue(client.fetch(base + "/busy").get("error"))
            self.assertEqual(client.fetch(base + "/next")["status"], 200)
            self.assertEqual(len(requests), 3)
            self.assertGreaterEqual(requests[2]["at"] - requests[1]["at"], 4.0)

    def test_long_retry_after_does_not_retry_or_fetch_another_url_early(self):
        with mock_http(self.clock) as (base, routes, requests):
            routes["/long"] = [(429, b"{}", {"Retry-After": "120"})]
            routes["/next"] = [(200, b"{}", {})]
            client = self.client()
            first = client.fetch(base + "/long")
            self.assertEqual(first["status"], 429)
            self.assertEqual(len(first["attempts"]), 1)
            self.assertTrue(first.get("error"))
            early = client.fetch(base + "/next")
            self.assertIsNone(early["status"])
            self.assertEqual(early["attempts"], [])
            self.assertTrue(early.get("error"))
            self.assertEqual(len(requests), 1)
            self.clock.sleep(120)
            self.assertEqual(client.fetch(base + "/next")["status"], 200)
            self.assertEqual(len(requests), 2)
            self.assertGreaterEqual(requests[1]["at"] - requests[0]["at"], 120)

    def test_truncated_http_200_is_a_transport_failure_and_never_stable_cache(self):
        partial, complete = b'{"partial":', b'{"complete":true}'
        with mock_http(self.clock) as (base, routes, requests):
            routes["/truncated"] = [
                (200, partial, {"Content-Length": "100"}),
                (200, complete, {}),
            ]
            url = base + "/truncated"
            client = self.client(max_attempts=1)
            failed = client.fetch(url)
            self.assert_capture(failed, url, 200, partial)
            self.assertTrue(failed.get("error"))
            self.assertEqual(len(failed["attempts"]), 1)
            recovered = client.fetch(url)
            self.assert_capture(recovered, url, 200, complete)
            self.assertFalse(recovered["cacheHit"])
            self.assertFalse(recovered.get("error"))
            self.assertTrue(self.client().fetch(url)["cacheHit"])
            self.assertEqual(len(requests), 2)

    def test_corrupt_cache_fails_closed_without_refetching(self):
        with mock_http(self.clock) as (base, routes, requests):
            routes["/cached"] = [(200, b'{"original":true}', {})]
            url = base + "/cached"
            first = self.client().fetch(url)
            self.body_path(first).write_bytes(b'{"tampered":true}')
            corrupted = self.client().fetch(url)
            self.assertIsNone(corrupted["status"])
            self.assertTrue(corrupted.get("error"))
            self.assertEqual(len(requests), 1)


if __name__ == "__main__":
    unittest.main()
