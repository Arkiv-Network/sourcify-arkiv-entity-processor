# Arkiv-backed Mainnet contract API

`arkiv-sourcify-server` reads imported entities directly from Arkiv JSON-RPC.
The former bundle-backed executable and `--bundle` option have been removed.
Parquet, DuckDB and processor bundles remain offline ingestion/reference tools;
the HTTP process never opens them and never falls back to Sourcify or Ethereum.

The supported product scope is `GET /v2/contract/1/{address}`. The detailed
[field/error compatibility matrix](sourcify-http-compatibility.md) distinguishes
Sourcify behavior from deliberate scope exclusions. JSON value equality is the
comparison contract; serialization order is not original Sourcify byte identity.

## Run

```sh
cargo build -p arkiv-sourcify-server --release --locked
RUST_LOG=info target/release/arkiv-sourcify-server \
  --rpc-url http://127.0.0.1:8745 \
  --owner 0x986c08292e6c327a310273a45965a3d2e2eac44c \
  --arkiv-chain-id 7706815 \
  --genesis-state-root 0xe1c577a41da3b90bab1d86b95883d4ea7cb84578e5405e8156d5fd587a2021d0 \
  --listen 127.0.0.1:3001
```

These example identities refer to the imported Mainnet dataset on the local
Arkiv network. Verify the root against the completed import before configuring
another deployment. The dedicated owner and genesis root identify this import;
reuse of the owner for another concurrent import is unsupported. The initial
load contains 7,823,531 contracts and 15,164,363 entities, with five known source
skips. A successful absent lookup means absent from this mirror, not globally
unverified. Root verification is a deployment trust anchor, not a Merkle proof of
the remote RPC response or completeness of the original Sourcify snapshot.

The executable validates network and genesis before binding. Each request checks
them again, then pins the current retained head. It reads current state, so a later
deletion is visible on the next request. There is no public historical selector.
All entity queries and direct entity reads use the same numeric block. Before
returning either success or absence, the service rechecks block hash and state
root. A changed/pruned view fails the request; it never silently switches height.

Examples:

```sh
curl 'http://127.0.0.1:3001/v2/contract/1/0x000000000034065b3a94C2118CFe5B4C0067B615?fields=abi,compilation'
curl 'http://127.0.0.1:3001/v2/contract/1/0x000000000034065b3a94C2118CFe5B4C0067B615?fields=deployment.blockNumber'
curl 'http://127.0.0.1:3001/v2/contract/1/0x000000000034065b3a94C2118CFe5B4C0067B615?omit=sources'
curl --compressed 'http://127.0.0.1:3001/v2/contract/1/0x000000000034065b3a94C2118CFe5B4C0067B615?fields=all'
```

Gzip and zstd content negotiation retain `Vary: Accept-Encoding`; identity is also
supported. Zstd decoding requires client support. Every response is completely
hydrated and serialized before a successful HTTP body starts. `Cache-Control:
no-store` avoids stale response reuse after dataset changes.

## Read architecture and limits

The async request owns its RPC session and all transport futures. A bounded
channel connects it to the pure processor hydrator on a CPU worker. The worker
asks for one unresolved dependency at a time and never performs network I/O.
This preserves the audited local-reference, spill, content-hash and staging-ID
validation without replaying hydration or duplicating it in an async implementation.

A request permit is shared by the driver and worker: finishing CPU work does not
release admission while the final block check is pending, and cancellation does
not release admission until the CPU worker exits. Dropping a request or reaching
its deadline drops the active RPC future and channel endpoints, waking any waiting
worker. CPU hashing/parsing/cloning is indivisible between cooperative checkpoints.

Lookups scope owner, project, kind and chain/address or content hash. The query
limit is two; multiple matches or any continuation cursor fail closed. Direct
entity reads verify owner, creator, project, upload schema, requested key, restored
staging identity, wire payload digest and content encoding. Wire key attributes
are resolved concurrently under a global RPC semaphore; all wires are deduplicated
inside the request. Logical source/code/blob dependencies are hydrated only when selected fields need
them. Restoring a contract's staging identity also loads its direct key-reference
wires (including their transported payloads), even on a minimal request. Those
identity reads are cached, but this implementation does not claim zero unrelated
payload I/O for minimal or ABI-only reads. Request caches are discarded after the read; there is no stale
positive/negative cross-request cache.

| Limit | Default | Purpose |
| --- | --- | --- |
| `--max-concurrent-requests` | 8 | Reject excess reads with 503 rather than queue CPU workers |
| `--request-timeout-seconds` | 120 | Bound lookup, hydration and final consistency check |
| `--max-dependency-reads` | 100,000 | Bound unique logical resolver operations |
| `--max-rpc-concurrency` | 16 | Bound all in-flight HTTP RPC requests across sessions |
| `--rpc-timeout-seconds` | 15 | Bound each RPC transport/body read |
| `--max-rpc-requests` | 100,000 | Bound calls inside one session |
| `--max-rpc-response-bytes` | 8 MiB | Bound one RPC envelope, including hex expansion |
| `--max-read-bytes` | 256 MiB | Bound cumulative RPC bytes and request-local wire storage |

The wire decoder separately bounds decompressed entity payloads to 122,880 bytes;
large logical payloads are chunked and checked by the shared hydrator. Input/work
limits are separate from field count. There is no blanket HTTP response-byte cap.
The maximum supported field set is fixed by the validated Sourcify selector matrix.
The current driver resolves logical dependencies sequentially; direct staging-key
reads can overlap. Wider dependency batching is a measured follow-up optimization.

## Operations

- `GET /_ops/health`: process liveness only.
- `GET /_ops/ready`: a fresh network/genesis/head read; returns dataset identity and
  cumulative RPC requests/bytes. Node outages or changed configuration fail readiness.
- `GET /_ops/metrics`: HTTP successes/failures, lookups, dependency reads, serialized
  bytes, and active request permits. There are no raw-address metric labels.
- SIGTERM/SIGINT stops admission and gracefully drains accepted reads.

Internal errors have stable categories: 502 for corrupt, ambiguous or missing
stored dependencies; 503 for unreachable/unready backend, changed read view,
resource budgets or overload; 504 for request deadlines. They never become a false
404 or a partial successful body. RPC endpoints must be HTTP(S), with no embedded
credentials, query, fragment or redirects. No writer credentials are accepted.

`Dockerfile.server` builds a separate unprivileged API image. It requires network
access only to the configured Arkiv RPC and no data mounts:

```sh
docker build -f Dockerfile.server -t arkiv-sourcify-server .
```

Bind loopback for local validation. External publication and gateway policy are
separate deployment steps. The offline pipeline image remains unchanged.

## Validation

```sh
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
python3 tests/server-smoke.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests/live -p 'test_*.py'
```

The protocol suite uses a real local HTTP RPC mock with transformed entity wires.
It checks namespace/integrity faults, duplicate/cursor results, block changes,
deletions, misses, byte/work budgets and cancellation/admission during RPC/final
checks. In-memory tests verify selective dependency work and HTTP compression.
The CLI smoke test checks real process startup/readiness/negative lookup/shutdown.
Use the [bounded live conformance runner](live-conformance.md) against an already
running Arkiv service; its independent source is sampling evidence only.
