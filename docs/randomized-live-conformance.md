# Randomized checks of the deployed APIs

`tests/live/randomized.py` compares the deployed Arkiv API at
`https://sourcify-prefill.arkiv-global.net` with the independently operated official
Sourcify service at `https://sourcify-mirror.arkiv-global.net`. The latter is our own
reference node, with permission for these bounded tests. It never requests the
public `sourcify.dev` API. Python 3's standard library is sufficient: no Parquet,
Bun, Docker, local Arkiv node, credentials or candidate server build is required.

This tests the two deployed services. A pass does **not** establish that an
unreleased server checkout behaves the same way. Rust unit/protocol tests and
local HTTP mocks separately test candidate implementation behavior.

## Run and replay

```sh
python3 tests/live/randomized.py \
  --output data/live-randomized/manual-001 \
  --count 16 \
  --max-match-id 54163210 \
  --seed example-seed
```

The output directory must be new. Omitting `--seed` generates and records a fresh
128-bit seed. `--count` allows 1–64 contracts locally; GitHub Actions offers at
most 16. The default is 8 locally. Endpoint overrides are `--reference-base-url`
and `--server-base-url`; URLs containing credentials, query strings or fragments
are refused, as is comparing one identical URL with itself. The client never
follows redirects. This runner adds no endpoints to our product API.

Replay the exact retained address and query list, without reference discovery:

```sh
python3 tests/live/randomized.py \
  --output data/live-randomized/replay-001 \
  --count 16 \
  --sample-file data/live-randomized/manual-001/sample.json
```

Replay uses the original seed, sample order, field selections, compression choices
and proxy setting from the manifest. It makes fresh HTTP requests; there is no
persistent response cache. Replay accepts different base URLs, which are recorded
in the report. Original discovery evidence remains in the original artifact
archive: `replay-input.json` preserves its manifest, while the new manifest clears
old discovery capture paths to avoid confusing them with newly numbered captures.

## Discovery and snapshot scope

The reference's `GET /v2/contracts/1` endpoint supports sorted, exclusive match-ID
cursors. We first capture the oldest Mainnet entry and newest entry within the
configured maximum, using `limit=1`. For each attempt, SHA-256 of the seed and
attempt index chooses an integer anchor within that frozen span. An ascending
`afterMatchId=anchor-1&limit=1` request discovers the next verified contract. The
manifest retains the anchor, full reference listing response, selected address
and match ID. Duplicate IDs/addresses and empty or out-of-bound seeks are recorded;
at most eight discovery attempts per requested contract are allowed.

This is **random match-ID seek sampling**, with probabilities weighted by gaps in
the global ID sequence. It is not uniform sampling over Mainnet contracts, and it
does not only sample the first page. The same seed reproduces discovery when the
reference population is unchanged; the retained sample manifest is the reliable
way to replay after the reference changes. Insufficient distinct samples fail
with an inconclusive result. Selected contracts are never replaced because the
Arkiv response is missing, slow, malformed or different.

CI pins `--max-match-id 54163210` before any target queries. This comes from the
completed snapshot's `source-manifest.json` at
`operations/runs/sourcify-39d5b75-full/processor/source/source-manifest.json`:
first match ID 1, last match ID 54,163,210, and 7,823,536 selected source contracts.
The imported dataset has 7,823,531 contracts after five recorded source skips.
The bound reduces later-verification drift; it does not guarantee that every
reference entry exists in the mirror. A selected target 404 is a mismatch, even
if a known source skip or snapshot drift explains it. The runner does not silently
excuse those differences. Without `--max-match-id`, it freezes the reference's
current maximum and records that no snapshot cutoff was configured.

Listing cursor semantics are pinned in Sourcify 4.1.2's
[OpenAPI](https://github.com/argotorg/sourcify/blob/6048af7dfb36a3f5b7b2b30ede5daca3e701f7b0/services/server/src/apiv2.yaml)
and [lookup handler](https://github.com/argotorg/sourcify/blob/6048af7dfb36a3f5b7b2b30ede5daca3e701f7b0/services/server/src/server/apiv2/lookup/lookup.handlers.ts).
Reference-only listing is test discovery; our server still supports only the
Mainnet single-contract product endpoint.

## Cases and comparison

Each randomly discovered contract gets five seeded cases:

- Minimal response.
- A random subset of optional top-level fields.
- A random subset of supported nested selectors.
- A random subset of top-level omissions.
- The full response, with seeded identity or gzip transport.

Every run also checks invalid addresses/selectors, conflicting or repeated query
parameters, unknown parameters, a seeded presumed-missing address, and an explicit
gzip request. The missing-address case expects an independent 404 from both
services; an unexpectedly verified address fails rather than being replaced.
Known Safe and WETH addresses additionally require a positive proxy detection and
a negative detection respectively. `--without-proxy` explicitly skips those two
sentinels and uses `omit=proxyResolution` for full responses; raw nested bytecode
selectors remain valid. CI includes live proxy resolution by default.

Comparisons preserve JSON scalar types, exact integer/decimal values, array order,
missing versus null, and every string, including embedded compiler metadata JSON.
Object-key order and whitespace outside strings are insignificant. The sole
normalization removes `errorId` from expected HTTP 400 responses **after** checking
that both contain UUIDv4 values. All other error fields still compare exactly.
HTTP 200 proxy-resolution errors or top-level error envelopes cannot count as a
successful comparison. Live Ethereum block differences and implementation-name
snapshot differences are reported as differences, not normalized away.

Every expected HTTP status is asserted independently, and a reference detail
response whose match ID changed since discovery is flagged as population drift.
The gzip case requires gzip and `Vary: Accept-Encoding` on the Arkiv response;
the reference may legally choose identity. Both payloads are decoded with a size
bound and compared normally. Broken gzip, truncated bodies, invalid JSON and
unexpected media types are inconclusive, never passes.

## Bounds and evidence

Requests are sequential, defaulting to 0.2 seconds between starts (CI uses one
second). `--delay-seconds` permits 0–5 seconds for our own reference node. Each
request gets at most two attempts for transport errors, 429 or 5xx, with at least
two seconds of backoff and honored `Retry-After`. Required retry delays over
30 seconds or beyond the remaining budget abort the request without retrying
early. Both attempts are retained. Stable 4xx responses are never retried.

The request timeout defaults to 65 seconds, with `--timeout-seconds` allowed up
to 65. The cooperative run deadline defaults to 900 seconds, configurable down
with `--max-seconds`; an already blocked socket read may finish up to one request
timeout later. GitHub Actions imposes a separate 16-minute step and 20-minute job
limit. Other fixed bounds are 1,024 HTTP attempts, 32 MiB per encoded/decoded
response, and 256 MiB of cumulative received wire bytes (checked per 64 KiB
chunk). Hitting a bound is recorded as incomplete work, not success. A larger
local sample may exhaust the same bounds rather than bypass them.

Artifacts are written throughout the run:

| Artifact | Contents |
| --- | --- |
| `sample.json` | Seed, algorithm, captured bounds, discovery attempts, selected addresses/IDs and exact cases |
| `report.json` | Endpoint identities, coverage statement, status, case outcomes, semantic differences, raw paths, attempts and counts |
| `raw/NNNN.body` | Original received HTTP body bytes, including compressed or incomplete/error responses |
| `raw/NNNN.json` | URL, status, selected headers, timestamp, SHA-256, size, elapsed time and transport error |
| `replay-input.json` | Original sample manifest when replaying |

At most 100 semantic differences per case are printed into the report, with the
full difference count and complete raw response evidence retained. An interrupted
GitHub step still uploads evidence written before cancellation.

| Exit | Status | Meaning |
| --- | --- | --- |
| 0 | `passed` | Complete discovery and every planned case matched |
| 1 | `failed` | At least one mismatch, including missing data, wrong status or a failed sentinel assertion |
| 2 | `inconclusive` | No established mismatch, but discovery, networking, decoding, live proxy resolution or a budget prevented completion |

A known mismatch takes precedence over later inconclusive work. Neither matching
5xx responses nor fewer completed cases can produce a pass.

## GitHub Actions

`.github/workflows/live-conformance.yml` runs localhost-only runner tests first,
then checks the deployed APIs on pull requests (three random contracts), daily at
04:23 UTC (16), and manually (1, 4, 8 or 16, with an optional seed). The deployed
job name explicitly distinguishes this coverage from testing the checkout's server.
The existing CI workflow also runs candidate Rust/protocol tests and the Python
suite. No untrusted PR code receives secrets; this is a normal `pull_request`
workflow with `contents: read`, credentials disabled in checkout, fixed service
URLs and no `pull_request_target`. Input values enter quoted shell arguments via
environment variables. A concurrency group serializes live jobs.

The report, raw responses and runner log upload with `if: always()` and a 14-day
retention period. A failure remains a failed workflow; artifacts support diagnosis.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests/live -p 'test_*.py'
```

The offline suite covers reproducibility/replay, discovery failures, missing data
without replacement, array/string/type sensitivity, expected-error IDs, malformed
and truncated responses, gzip, retry evidence, byte/request bounds and mismatch
precedence. It uses only local HTTP fixtures.
