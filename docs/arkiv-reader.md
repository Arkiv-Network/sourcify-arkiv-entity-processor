# Read a contract from an Arkiv prefill node

`read-arkiv` is the first direct RPC read path for processor v3 data imported by
`arkiv-prefill`. It looks up one Ethereum Mainnet contract by address, reads its
shared entities and spill chunks from the node, and reconstructs the complete
stored contract through the same integrity engine as bundle hydration. It does
not require a local bundle or contact Sourcify or Ethereum RPC.

```sh
cargo build --release --locked -p arkiv-entity-processor
target/release/arkiv-entity-processor read-arkiv \
  0x622F9a2360c42cda18d07D61C2F333f5E460B76f \
  --rpc-url http://127.0.0.1:8745 \
  --owner 0x986c08292e6c327a310273a45965a3d2e2eac44c \
  --block 0 > contract.json
```

The owner in this example belongs to the local imported dataset; pass the expected
public owner of your own import explicitly. No signing key or credentials are
needed. Output is one complete JSON document plus a newline. Read statistics go to
stderr: block number/hash, RPC requests, queries, entity reads, RPC response bytes
and elapsed time. Lookup, dependency or validation failure exits nonzero before
writing contract JSON. The existing offline `hydrate BUNDLE_DIR` command retains
its behavior.

## Pinned reads and validation

The source chain is Mainnet (`1`). The expected Arkiv chain defaults to `7706815`
and can be set with `--arkiv-chain-id`. `--block` takes a decimal block number and
defaults to genesis `0`, where the current prefill dataset was imported. The node
must retain historical Arkiv state for the selected block.

Every contract/dependency query includes the project and owner scope, a limit of
two and the explicit block. Any second result or continuation cursor is ambiguous
and fails. The reader fetches entity bodies at that same block, verifies the query
result's block, and checks the block hash before and after hydration. This uses the
node's historical API conventions: query `atBlock` is hexadecimal; the second
`arkiv_getEntity` parameter is a JSON integer. The API accepts block numbers rather
than EIP-1898 block-hash selectors, so this is a trusted node read with a block-hash
consistency check, not a cryptographic state proof.

Entity owner, creator, project, upload schema, typed attributes and deterministic
prefill keys are checked. RPC address attributes are restored to the processor's
EIP-55 representation; `u64` quantities become exact decimal strings. Key attributes
are translated back to staging IDs using referenced entities at the same block.
Duplicate or unsupported attributes fail. The shared decoder and hydrator then
verify transport payload hashes, staging identities, gzip sizes, shared content
hashes, references and complete spill manifests. Missing contract lookup is reported
separately from a missing dependency or an RPC failure.

## Work limits

| Option | Default | Meaning |
| --- | --- | --- |
| `--timeout-seconds` | `30` | Per-RPC deadline, including the response body |
| `--deadline-seconds` | `120` | Overall deadline checked at RPC boundaries, including the final block check |
| `--max-rpc-requests` | `100000` | Total RPC calls, including queries and block checks |
| `--max-rpc-response-bytes` | `1048576` | Maximum individual JSON-RPC envelope |

Individual entities retain the existing 122,880-byte decoded payload limit. These
are wire-entity and work limits, not a small cap on the reconstructed contract:
logical payloads can span many verified chunks. RPC calls are serial and entity
bodies are cached for the duration of the read. The current contract, decoded JSON
and fetched entities are buffered in memory. Individual parsing/hydration steps are
synchronous; they are not forcibly interrupted mid-operation. The final deadline
check happens before successful JSON output.

## Scope and checks

This is a CLI vertical slice for v3 prefill keys. It does not change the fixture
HTTP server or supply its production Arkiv backend. It has no contract listing,
all-chain lookup, field projection, verification endpoint, live proxy resolution,
Sourcify fallback or arbitrary-upload compatibility.

Offline mock RPC tests cover full hydration with real spill/chunk resolution,
address and numeric normalization, staging-key references and caching, block
pinning/reorganization, namespace/type validation, ambiguity, missing records,
corrupt payloads/chunks, decoded-size bounds and request/deadline/transport limits.
Run them with the existing workspace suite:

```sh
cargo test --workspace --locked
cargo clippy --workspace --locked --all-targets -- -D warnings
```

On 2026-10-13 UTC, seven contract reads from the populated local Arkiv node matched
their independently retained Parquet-derived source JSON exactly. They included
SafeProxy, five previously sampled contracts and the largest measured record
(5,997,755 JSON bytes before the output newline). That largest read used 297 RPC
calls and fetched 147 entities; a debug build took about 3.3 seconds wall time.
This is evidence for those stored records, not a full-dataset or live Sourcify API
parity claim. No source-file fallback participated in those reads.
