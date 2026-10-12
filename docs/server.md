# Fixture contract read server

`arkiv-sourcify-server` is a separate workspace crate and executable for phases 1–2
of the [server architecture plan](https://github.com/Arkiv-Network/sorcify-arkiv-entity-processor/blob/docs/november-2026-milestone/docs/processor-server-plan.md).
It serves completed **local v3 processor bundles**. It is a usable development and
fixture service, not a production Arkiv reader or a claim of full Sourcify API parity.
The processor CLI and its release/container pipeline remain separate.

## Run

```sh
cargo build --workspace --release --locked
target/release/arkiv-sourcify-server --bundle /path/to/bundle --listen 127.0.0.1:3000
curl 'http://127.0.0.1:3000/_ops/ready'
curl 'http://127.0.0.1:3000/v2/contract/1/0x000000000034065b3a94C2118CFe5B4C0067B615?fields=abi,compilation'
```

The bundle must contain `seed.json`, `report.json`, `SHA256SUMS`, and every entity
part named by the seed. An extracted prefill archive works; `payloads.bin` and
`metadata.jsonl` are not required. Keep the directory immutable and mount it
read-only. Start a new process to switch datasets. There is no writer key, RPC
URL, live Sourcify fallback, or network credential in the server configuration.

Startup verifies seed/report/part checksums, completion status, part byte and
entity counts, wire upload schema, and every contract's v3 envelope version,
including spilled contract envelopes. It fails before listening if these checks
fail. It builds compact indexes by entity key, content reference, and source
chain/address. Indexes and startup scans are proportional to entity count; this
has not been sized for the full Mainnet dataset.

Local manifest consistency is not proof of source-export completeness. A completed
bundle built without `--require-source-complete` remains a valid fixture and exposes
that fact in readiness metadata. Full transitive content integrity is checked on
selected reads. An unrequested broken dependency does not force unrelated source
fetches. Accessed bytes are checked against the indexed line digest; edits to
unaccessed files are not detected. This fixture model is immutable, not a model for
current-state Arkiv deletion or historical reads.

## HTTP contract: `fixture-v1`

Only `GET /v2/contract/1/{address}` reads contracts. The source chain must be exactly
`1`; other chains and alternate spellings such as `01` receive `400`. The address
must be `0x` plus 40 hexadecimal digits. Lookup is case-insensitive and does not
require a valid mixed-case checksum. The response preserves the stored address
and chain ID representation (including numeric chain IDs accepted by the processor).

The default response contains these seven required stored fields:

`address`, `chainId`, `match`, `matchId`, `creationMatch`, `runtimeMatch`, `verifiedAt`.

`fields=abi,compilation` adds the selected optional fields. `fields=all` returns the
seven required fields and all 16 supported optional fields. It deliberately excludes
`proxyResolution`. The server returns one complete contract JSON object, never a
page, truncated object, or metadata wrapper.

| Supported optional fields | Serving rule |
| --- | --- |
| `abi`, `compilation` | Select compilation paths before expanding child dependencies |
| `deployment`, `creationBytecode`, `runtimeBytecode` | Preserve stored values and bytecode representation |
| `sources`, `metadata`, `storageLayout`, `transientStorageLayout`, `userdoc`, `devdoc`, `sourceIds`, `stdJsonInput`, `stdJsonOutput`, `additionalInput` | Hydrate only selected dependency closures, including local references |
| `signatures` | Preserve stored input; this initial policy does not derive signatures from ABI |

The value contract preserves null, empty values, absent optional keys, array order,
source text, and exact JSON number representations. Missing required minimal fields
are corrupt data. Selecting an absent optional field leaves it absent. Unknown stored
top-level fields are not exposed by `fields=all`; the offline `hydrate` command still
preserves them. JSON whitespace and object-key order are not a byte-parity contract.

Only one query parameter, `fields`, is supported. Names are case-sensitive. Comma
lists accept duplicate field names and collapse them; minimal fields always remain.
Percent encoding is decoded strictly. Empty HTTP query strings mean minimal fields.
The following receive `400` before a backend lookup:

- Empty selectors (`fields=`, trailing/doubled commas), whitespace or unknown names.
- Repeated `fields` parameters, extra query parameters, or malformed percent escapes.
- Nested/dotted selectors, `omit`, or mixing `all` with other fields.
- An explicit `proxyResolution` selector, including nested proxy selectors.

Proxy resolution is deferred. Stored proxy values are neither exposed nor replaced
with fabricated null/default values. A future implementation will query Ethereum
RPC directly; it will never fetch live Sourcify as a fallback. ABI-derived signatures,
known exported-signature mismatches, and exact upstream selector/error compatibility
remain separate acceptance work. The `fixture-v1` behavior above is intentional and
tested, not a claim that upstream Sourcify rejects or formats requests identically.

## Errors and operations

Error bodies have the stable shape:

```json
{"error":{"code":"contract_not_found","message":"Contract is absent from the configured fixture dataset"}}
```

| Status | Codes / meaning |
| --- | --- |
| `400` | `invalid_request`: invalid path, chain, query, or unsupported selector |
| `404` | `contract_not_found`: valid lookup absent from this fixture; `route_not_found`: unknown endpoint |
| `405` | `method_not_allowed`: only GET is supported on the contract route |
| `502` | `missing_dependency`, `corrupt_data`, `unsupported_schema`, `ambiguous_record` |
| `503` | `backend_unavailable`, `overloaded`, or configured `work_limit` exceeded |
| `504` | `deadline_exceeded` |
| `500` | `internal_error`: unexpected worker failure |

Only initial contract absence yields a contract `404`. Multiple distinct records
at one chain/address are an error; there is no arbitrary winner. A dependency error
never yields a successful partial response. Successful responses use `application/json`
and `Cache-Control: no-store`. Internal file/error details are logged, not returned.

- `GET /_ops/health`: process liveness, independent of worker saturation.
- `GET /_ops/ready`: startup readiness, `mode: fixture`, API contract version, seed
  and manifest digests, schema version, source/Arkiv chain IDs, observed counts,
  and source-completion policy. This is not a continuous scan of bundle files.
- `GET /_ops/metrics`: low-cardinality JSON counters for requests, successes,
  failures, lookups, dependency reads, response bytes, and active workers. It is
  basic diagnostic instrumentation, not the production latency/field-group suite.

SIGINT/SIGTERM marks the application unready, stops accepting new connections and
drains accepted requests. JSON composition and serialization finish before response
headers are sent. Health, metadata and metrics stay outside Sourcify response bodies.

## Work and capacity

| Option | Default | Effect |
| --- | --- | --- |
| `--listen` | `127.0.0.1:3000` | Listening socket |
| `--max-concurrent-requests` | `8` | Concurrent hydration/serialization workers; excess work gets `503` without queuing |
| `--request-timeout-seconds` | `120` | Lookup, hydration and serialization deadline |
| `--max-dependency-reads` | `100000` | Unique entity-read work budget per request |

The finite selector set is not a response byte cap. There is no small response-size
limit: a selected source/artifact can legitimately be large. The existing v3 wire
format bounds each decoded entity; logical payloads are reassembled from validated
chunks. Request-local caches deduplicate shared entities and blob reads. A spilled
compilation must be fully read and hashed even when selecting only its ABI; unrelated
source/code entities are still not fetched.

Each fixture request uses one immutable bundle session on a bounded blocking worker,
with serial dependency reads. Tokio workers do not perform blocking file reads or
JSON reconstruction. Dropping/timing out a request sets a cooperative cancellation
flag; the worker retains its permit until it stops. Checks run between dependency
reads and during reference traversal. An individual filesystem read, hash, parse,
clone or serialization cannot be forcibly interrupted. Completed response transmission
and slow-client socket buffers are outside this hydration concurrency limit.

Responses and logical payloads are currently buffered in memory. Provision RAM and
the reverse proxy for actual largest responses and reduce concurrency if needed;
there is no published Mainnet capacity envelope yet. No cross-request payload cache,
automatic retry, response compression, or public historical query is implemented.
The future Arkiv adapter needs an asynchronous transport driver with a pinned read
view, dataset/owner scope, actual-node wire tests and deletion-aware behavior.

## Verification and remaining scope

```sh
cargo test --workspace --locked
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo build --workspace --release --locked
python3 tests/server-smoke.py
bun install --frozen-lockfile && bun run test
```

HTTP value tests use the independent captured Mainnet response in
[`data/sourcify-response.json`](data/sourcify-response.json), with capture provenance
in [`data/provenance.json`](data/provenance.json). Synthetic error cases test this
document's explicit contract. Coverage includes selective/full projection equivalence,
local and escaped references, spills, missing/corrupt dependencies, ambiguity, cheap
misses, cancellation, overload and a complete 9 MiB single-field response. The TCP
smoke test runs process → verify → CLI hydrate → bundle HTTP reads → SIGTERM shutdown.
Existing deterministic processor and Bun/Parquet tests remain in CI.

Production Arkiv serving, dataset acceptance/ownership, live proxy RPC, signature
derivation policy, broad API parity, full-dataset benchmarks and a dedicated server
release/image/deployment remain later phases. Contract listing, pagination, all-chain
lookup and verification endpoints are outside this service's scope.
