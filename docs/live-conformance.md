# Random contract comparison against Sourcify

This opt-in check compares our real HTTP server with an independent Sourcify API.
Its processor input comes from the existing local Parquet-derived DuckDB source,
through `source/records.ts`. Live responses are only expected values; they never
feed the processor. The tool creates a small bundle, starts the release server on
localhost, compares responses, and shuts the server down. It does not upload or
deploy anything, query Ethereum RPC, or read an Arkiv node.

## Run a small spot check

Install the pinned Bun dependencies and build the workspace first. Python 3 uses
only its standard library. Supply a completed Mainnet source directory containing
`source-manifest.json` and `subset.duckdb`; it is opened read-only.

```sh
bun install --frozen-lockfile
cargo build --workspace --release --locked
python3 tests/live/compare.py \
  --source-dir /path/to/prepared/source \
  --output data/live-conformance/run-001 \
  --snapshot-label 'Sourcify Mainnet snapshot 2026-09-30'
```

The default is five contracts and `fields=abi,compilation,sources`. The minimal
response fields are compared too. A generated seed is recorded in `report.json`
and `sample.json`; pass `--seed VALUE` to reproduce the selection against the same
source. Selection ranks every match by MD5 of `seed:decimal-match-id`, with the
numeric ID as a tie-breaker. It does not select the lowest IDs, prefilter convenient
records, or replace contracts that fail reconstruction. Changing the seed selects
a different sample; this tool is for occasional bounded checks, not repeated
public-API sampling or crawling.

`--count` accepts 1–10. `--fields` accepts up to three distinct supported optional
fields, or `minimal`. This is a **test request limit**, not a production API field
or response-byte limit. `all`, nested selectors and `proxyResolution` are refused.
Signatures can be requested explicitly; differences are reported without any
signature exception. No live proxy RPC or Sourcify fallback is added to the server.

`--api-base-url http://127.0.0.1:5555` can point to a separately operated official
Sourcify instance. The default is `https://sourcify.dev/server`. Include any base
path used by that instance. URLs with credentials, queries or fragments are refused.
The same bounded request policy applies to either base URL. Hosting Sourcify and
loading its database are separate tasks.

## Request policy and cache

The runner follows the public API guidance in the
[official Sourcify API specification](https://sourcify.dev/server/api-docs/swagger.json):
one request at a time, a descriptive User-Agent containing this repository's URL,
and at least one second between request starts. It never lists contracts or calls
`fields=all`. Each selected contract gets one field selection, with at most three
attempts for transport errors, HTTP 429 or 5xx. Retries back off by two then four
seconds, honor a longer `Retry-After` (seconds or date), and preserve the cooldown
across contracts. If a required wait exceeds 60 seconds, the run records an
inconclusive result instead of retrying early. The HTTP timeout is 30 seconds.

Responses are saved **before local comparison**, under
`data/live-conformance/cache` by default (`--cache` overrides it). Cache identity
includes the full API base URL, address and normalized field selection. Stable
responses, including 404s, are reused on reruns with their original capture time;
the cache is not a freshness claim. A cache checksum mismatch fails closed without
a fresh request. Transient errors and partial responses have separate attempt
artifacts and never become reusable successful cache entries. Keep this cache
between runs; reproducing a selection should not duplicate successful API calls.

## Evidence and result meanings

Each run requires a new output directory and retains:

| Artifact | Contents |
| --- | --- |
| `sample.json` | Seed, selection algorithm, match IDs, addresses, source manifest digest, source database size/mtime, reconstruction status and original Sourcify IDs |
| `records.ndjson`, `bundle/` | Actual locally reconstructed processor input and the resulting small bundle |
| `report.json` | Counts, source and server dataset provenance, capture times, URLs, HTTP status, response hashes, cache usage and differences |
| `MATCH_ID.live.body`, `MATCH_ID.local.json` | Captured public API bytes and local response bytes |
| `*.log` | Sampler, processor and server diagnostics |

All examples keep public response data and generated files under ignored `data/`.
No credentials are needed or captured. Reusable cache entries also retain each
request attempt, status, timestamp, selected headers and body checksum.

Comparison uses parsed JSON values. Object key order and whitespace outside
strings do not matter; array order, strings, missing versus null, and booleans
versus numbers do. Integer and decimal values are compared without rounding large
numbers through floating point. Diffs use JSON Pointer paths, with **expected =
Sourcify** and **actual = our server**. There are no ignored fields. Large differing
strings get a preview and hash in the report; full response bytes remain available.

| Exit | Status | Meaning |
| --- | --- | --- |
| `0` | `passed` | Every requested contract reconstructed and every independent HTTP 200 response matched |
| `1` | `failed` | A value/status mismatch or local server HTTP failure was observed |
| `2` | `inconclusive` | Incomplete sampling, unavailable/invalid external response, transport, startup or shutdown failure; any observed mismatches remain in the report |

Configuration errors are rejected before running. No responses or fewer responses
than requested cannot produce a pass. A live 404 against a locally reconstructed
contract is a mismatch. Snapshot and live capture dates differ: a new verification
or changed API behavior can produce drift. Such differences need diagnosis; the
runner never silently excuses them.

On 2026-10-12 UTC, seed `782369dc0049a90c` sampled five matches from the 7,823,536-record
prepared Mainnet source (September 30 snapshot, prepared October 9). All five
matched for `abi,compilation,sources` and the minimal fields, using five fresh HTTP
200 responses and no retries. This checks those five responses only. It does not
establish parity for every field, snapshot, contract or later API version.

## Offline regression tests

```sh
bun run test
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests/live -p 'test_*.py'
```

CI runs these offline tests, never the public API comparison. The Bun tests check
reproducible selection over the full fixture population, independent hash ranking,
read-only source handling, reconstruction and failures without sample replacement.
Python tests use local HTTP mocks for semantic comparisons, caching, corruption,
status handling, retry delays, cooldowns and truncated responses.
