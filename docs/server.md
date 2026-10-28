# Arkiv-backed Mainnet contract API

`arkiv-sourcify-server` reads imported entities directly from Arkiv JSON-RPC.
The former bundle-backed executable and `--bundle` option have been removed.
Parquet, DuckDB and processor bundles remain offline ingestion/reference tools;
the HTTP process never opens them and never falls back to Sourcify. Optional
Ethereum RPC reads supply live proxy resolution; all stored fields and
implementation names still come from Arkiv.

The supported product scope is `GET /v2/contract/1/{address}` and its
`GET /v2/debug/contract/1/{address}` diagnostic counterpart. The detailed
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
  --ethereum-rpc-url "$ETHEREUM_RPC_URL" \
  --request-timeout-seconds 55 \
  --listen 127.0.0.1:3001
```

`ETHEREUM_RPC_URL` supplies a Mainnet HTTP(S) endpoint supporting EIP-1898
`blockHash` with `requireCanonical: true` for `eth_getStorageAt` and `eth_call`.
The flag is optional: without it, a selected live proxy result contains a
`proxy_resolution_error`; minimal responses and raw nested proxy bytecode
selectors remain available. The example keeps the deployed 55-second deadline.

These example identities refer to the imported Mainnet dataset on the local
Arkiv network. Verify the root against the completed import before configuring
another deployment. The dedicated owner and genesis root identify this import;
reuse of the owner for another concurrent import is unsupported. The initial
load contains 7,823,531 contracts and 15,164,363 entities, with five known source
skips. Contracts the [Postgres → Arkiv sync](../README.md#postgres--arkiv-sync)
writes later, as the same owner, are read alike: their keys are chain-assigned
rather than derived from the staging ID, and an entity's identity is the staging
ID recomputed from its content. A successful absent lookup means absent from this mirror, not globally
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

## Debug RPC trace

`GET /v2/debug/contract/1/{address}` accepts exactly the same `fields` and `omit`
selectors as the normal route, including `?fields=all`:

```sh
curl --compressed 'http://127.0.0.1:3001/v2/debug/contract/1/0x000000000034065b3a94C2118CFe5B4C0067B615?fields=all'
```

The debug route runs one contract read and wraps its actual result:

```json
{
  "response": {"match": "exact_match", "chainId": "1", "address": "..."},
  "trace": {
    "rpcCalls": [
      {"index": 0, "network": "arkiv", "method": "eth_chainId", "params": [], "durationMs": 12, "outcome": "success"}
    ],
    "truncated": false,
    "omittedRpcCalls": 0
  }
}
```

This is an abbreviated example. `response` contains the complete normal Sourcify
body for that same execution and field selection. It is not fetched again or
reconstructed from a proposed query plan. Both routes share validation, HTTP
statuses, CORS, gzip/zstd negotiation, admission, work budgets and cancellation.
The ordinary route does not create a trace recorder.

Each `rpcCalls` entry represents an HTTP JSON-RPC dispatch from this request,
including network/genesis checks, actual scoped Arkiv queries, direct entity and
blob reads, Ethereum proxy reads, implementation-name lookups and final pinned
block checks when executed. `network` is `arkiv` or `ethereum`; the method alone
cannot identify the network because Arkiv also uses `eth_*` methods. `params`
contains the exact submitted parameters. No endpoint URL, authentication header,
RPC response body or upstream error text is included.

`index` is zero-based dispatch order across both networks; parallel calls may
finish in a different order. `durationMs` measures transport plus response-body
reading/JSON-RPC validation, rounded down to milliseconds, excluding time waiting
for the RPC concurrency gate. `outcome` is `success`, `error`, or `cancelled`.
It describes the transport and JSON-RPC envelope, so later data-integrity or
block-consistency validation may still fail after a successful RPC. A failed
transport attempt is recorded even if the remote server never receives it.
Calls rejected before dispatch by admission or work budgets are not recorded.
Cancellation at the overall request deadline records active calls as cancelled.

Trace retention is bounded independently of the normal read budgets: at most
4,096 calls and a conservative 2 MiB encoded-entry budget per debug request.
Once either bound is reached, the complete retained prefix remains in order,
`truncated` becomes true, and `omittedRpcCalls` counts every later dispatch. The
contract read continues under its existing limits; a truncated trace never
claims to be complete. Recorded count plus omitted count gives total dispatches.

Handler errors, including 400 validation, 404 absence, 503 overload/backend
failure and 504 deadline, keep their normal HTTP status and appear under
`response` alongside whatever trace was collected. Invalid selectors and rejected
admission have empty traces. Random error IDs retain their normal per-execution
semantics. Selected proxy errors remain nested in the HTTP 200 response.
Routing/method errors and content-negotiation failures happen before the handler
and retain their existing unwrapped responses (HEAD has no body; unacceptable or
malformed content codings have an empty 406 or 400 response).

## Read architecture and limits

The async request owns its RPC session and all transport futures. A bounded
channel connects it to the pure processor hydrator on a CPU worker. The worker
asks for one unresolved dependency at a time and never performs network I/O.
This preserves the audited local-reference, spill, content-hash and staging-ID
validation without replaying hydration or duplicating it in an async implementation.

A request permit is shared by the driver and worker: finishing CPU work does not
release admission while proxy resolution, implementation names or final block checks are pending, and cancellation does
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
| `--request-timeout-seconds` | 120 | Bound lookup, hydration, proxy resolution, names and final checks; deployment uses 55 |
| `--max-dependency-reads` | 100,000 | Bound logical resolver operations across the contract and implementation names |
| `--max-rpc-concurrency` | 16 (v3), 4 (udc) | Bound in-flight Arkiv HTTP RPC requests across sessions |
| `--rpc-timeout-seconds` | 15 | Bound each RPC transport/body read |
| `--max-rpc-requests` | 100,000 | Bound calls inside one session |
| `--max-rpc-response-bytes` | 8 MiB | Bound one RPC envelope, including hex expansion |
| `--max-read-bytes` | 256 MiB | Bound cumulative RPC bytes and request-local wire storage |

Ethereum has a separate global RPC gate and per-resolution budgets:

| Limit | Default | Purpose |
| --- | --- | --- |
| `--max-ethereum-rpc-concurrency` | 16 | Bound in-flight Ethereum requests across all API reads |
| `--ethereum-rpc-timeout-seconds` | 15 | Bound each Ethereum transport/body read |
| `--max-ethereum-rpc-requests` | 64 | Bound calls, including chain and block checks |
| `--max-ethereum-rpc-response-bytes` | 1 MiB | Bound one Ethereum RPC response |
| `--max-ethereum-read-bytes` | 4 MiB | Bound cumulative Ethereum response bytes |
| `--max-proxy-implementations` | 128 | Bound implementation name lookups |

Ethereum resolution pins lazily: detectors that need no storage/call do not make
Ethereum requests. Before the first read, the adapter checks chain 1 and pins the
latest block hash. Every storage/call uses that hash with `requireCanonical`;
a final numeric block lookup checks that the hash remains unchanged. There is no
fallback to unpinned reads. Implementation names share the original Arkiv session,
its caches and budgets. These are separate consistent views of two networks, not
an atomic snapshot across them. The detector yields cooperatively while scanning
bytecode; dropping the request cancels active Ethereum transport futures.

The wire decoder separately bounds decompressed entity payloads to 122,880 bytes;
large logical payloads are chunked and checked by the shared hydrator. Input/work
limits are separate from field count. There is no blanket HTTP response-byte cap.
The maximum supported field set is fixed by the validated Sourcify selector matrix.
The current driver resolves logical dependencies sequentially; direct staging-key
reads can overlap. Wider dependency batching is a measured follow-up optimization.

## v4 (UDC) entities

`--format udc` reads the v4 dataset of [the UDC proposal](udc-proposal.md) (branch `udc`):
entities tagged `<prefix>.<kind>` in the indexed `arkiv.schema` attribute, all content in
codec-tagged `bytes` attributes, references as `key` attributes. The HTTP contract, field
selection, projection, signature and proxy policies are those of the v3 path; only the backend
and the hydrator differ, and every response is byte-identical to the v3 path's for the same
record (`crates/server/tests/udc.rs` compares 21 selections, absence and an invalid field on
three records).

```sh
RUST_LOG=info target/release/arkiv-sourcify-server --format udc \
  --rpc-url http://127.0.0.1:18545 --arkiv-chain-id 7706815 \
  --owner 0x3ffcb8c66ba2a35df36d73b213399220fd59f42f \
  --schema-prefix sourcify-udc-proposal.v4 \
  --listen 127.0.0.1:3100
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--format` | `v3` | `udc` selects this backend; `--genesis-state-root` is then not used |
| `--schema-prefix` | `sourcify.v4` | The dataset's `arkiv.schema` prefix (the proposal's test entities use `sourcify-udc-proposal.v4`) |
| `--read-view` | `auto` | `pinned`: every query of a request at the head when it started (a node that keeps history, our node); `head`: no `atBlock` (a head-only node, Jalebi); `auto`: decided once at startup by a query 64 blocks behind the head (answered: pinned; "not retained": head). With `--format v3`, `--schema-prefix` and `--read-view` are refused |
| `--genesis-hash` | none | Hash of block 0, checked at the start of every read |
| `ARKIV_RPC_API_KEY` | unset | Sent as `X-API-KEY` (Jalebi's API keys), never logged |

A request reads the contract with one query (`$owner`, `arkiv.schema`, `chainid = u64(1)`,
`address`, limit 2: two rows are ambiguous), then hands it to the processor's `udc::Reader` on a
CPU worker. The reader asks for keys in rounds and the async session answers each round with
`$key = key(…) OR …` queries of at most 8 keys (our node's planned path), run under the global
RPC gate: the compilation and on-chain code; then the sources, recompiled code and chunks the
selected fields need; then their chunks. `fields=abi,compilation` selects only the compilation's
`arkiv.schema`, `hash`, `manifest`, `abi` and `compilation` attributes: 2 queries. A full
SafeProxy read is 3 queries (measured on our node). Every fetched key counts against
`--max-dependency-reads`. The reader checks owner, kind, codecs, chunk lists, digests and content
hashes; a missing key is `missing_dependency`, two contracts for an address `ambiguous_record`, any
failed check `corrupt_data`.

In the pinned view every query carries the request's `atBlock` and must answer at it, and the
block's hash is checked again before responding (`state_changed` otherwise). In the head view the
contract is one query and its dependencies are never changed once written (only contracts are
patched), so a read stays consistent while the head moves.

Our node sheds load with HTTP 429 or the JSON-RPC error `-32010 ServerBusy` when about 8 to 12
requests are in flight. The v4 backend asks again after 100, 200, 400, 800 and 1,600 ms (each
attempt counts against `--max-rpc-requests`, all within the request deadline), then fails with
`backend_unavailable`; any other error fails at once, and so does Jalebi's HTTP 429
`ANON_COST_LIMITED` (its window resets in minutes). Its default `--max-rpc-concurrency` is 4.
The v3 backend does not retry. The request deadline is checked before every fetch, value decode
and reference expansion of the hydration worker, so a timed-out request stops decoding. The API
key is never printed (not even in a debug dump of the configuration).

## Operations

- `GET /_ops/health`: process liveness only.
- `GET /_ops/ready`: a fresh Arkiv network/genesis/head read; returns dataset identity and
  cumulative Arkiv RPC requests/bytes. Arkiv outages or changed configuration fail
  readiness. This endpoint does not probe Ethereum availability.
- `GET /_ops/metrics`: HTTP successes/failures, lookups, dependency reads, serialized
  bytes, and active request permits. There are no raw-address metric labels.
- SIGTERM/SIGINT stops admission and gracefully drains accepted reads.

Internal errors have stable categories: 502 for corrupt, ambiguous or missing
stored dependencies; 503 for unreachable/unready backend, changed read view,
resource budgets or overload; 504 for request deadlines. They never become a false
404 or a partial successful body. Selected proxy failures use the upstream nested
HTTP 200 error shape, except an overall deadline still returns 504. The pinned
upstream Diamond resolver suppresses ordinary remote/ABI failures and tries other
detectors; malformed RPC envelopes, wrong networks/views, local timeouts and
resource limits cannot become successful non-proxy results.

Arkiv RPC endpoints must be HTTP(S), with no embedded credentials, query or
fragment. Ethereum URLs must be HTTP(S) with no fragment; runtime authentication
may be supplied in the URL. Neither client follows redirects. Ethereum errors
and startup logs omit the URL. Keep deployment secrets in protected runtime
configuration; never commit them. No writer credentials are required.

`Dockerfile.server` builds a separate unprivileged API image. It requires network
access to the configured Arkiv and optional Ethereum RPCs, and no data mounts.
The image includes the Sourcify and whatsabi MIT notices:

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
checks. The Ethereum HTTP mock covers pinned reads, names, malformed responses,
Diamond fallback, global concurrency, budgets and cancellation. Independent pinned
whatsabi vectors test detection and resolver order. In-memory tests verify
selective dependency work and HTTP compression.
The CLI smoke test checks real process startup/readiness/negative lookup/shutdown.
Use the [bounded live conformance runner](live-conformance.md) against an already
running Arkiv service; its independent source is sampling evidence only.
