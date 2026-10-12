# Proposed deliverable for November milestone:

The proof of concept proved the following:

- The data model maps cleanly between Sourcify's Postgres schema and Arkiv entities.
- The lookup endpoint returns the same body as Sourcify API for the snapshot (24/24 fields)

***Note:*** *Arkiv's architecture is changing ahead of the November testnet. The Devcon deliverable will be a fresh build on the new stack rather than a continuation of the current code. We prefer to state this now rather than have it be a surprise in October.*

**We will deliver a publicly queryable one-off snapshot of Sourcify's Ethereum Mainnet verified-contract database, loaded onto a dedicated Arkiv test network, with a publicly hosted, read-only HTTP API.**

## In scope

**Chain**: Ethereum Mainnet (chain 1)

**Cutoff**: The mirror contains every contract verified on Sourcify before 30 September 2026. Writing 7.8M addresses will take several days, so the load runs across a window. Contracts verified during the load window may or may not appear. Nothing verified after it will. We publish the cutoff and the load start and end timestamps. The two databases are not synchronized, there is no live listener, and live or incremental sync is explicitly out of scope.

**Data:** the ten tables Sourcify publishes in its Parquet export, including `compiled_contracts_signatures`, stored as Arkiv entities (verified contract, compilation, source file, code, signature, blob) and composed on read into the same 24 fields `GET /v2/contract/{chainId}/{address}?fields=all` returns.

**Target fidelity:** byte-identical JSON on 23 of the 24 fields. proxyResolution is the exception: Sourcify computes it per request, the mirror freezes it at index time, so it is compared by value rather than by byte. The Unichain proof of concept reached identical field values on 3,136 of 3,144 contracts; reaching byte identity needs two changes we have scoped, JSON key ordering and copying the signatures table.

### Public read API

The supported route is `GET /v2/contract/{chainId}/{address}`, with `chainId=1` only. It reads the published snapshot from Arkiv entities on the dedicated testnet. We will publish the API base URL, API documentation, supported field names, error responses, and runnable request examples.

- With no `fields` parameter, return Sourcify's minimal lookup response.
- Support `fields=abi,compilation` and comma-separated selection of the supported lookup fields, using Sourcify's response shape and field-selection semantics.
- Support `fields=all`, composing all 24 fields subject to the `proxyResolution` snapshot exception above.
- Return `200` for a contract present in the snapshot and `404` for a valid Ethereum Mainnet address absent from it. A snapshot miss does not mean that the contract is unverified globally or absent from today's Sourcify database.
- Reject malformed chain IDs, invalid addresses, unsupported chains, and invalid field selections with `400` and a documented JSON error response. Backend failures return a `5xx` response, not a misleading `404`.
- Never silently fall back to live Sourcify. Responses describe the published snapshot; `proxyResolution` is the value captured at index time.

For example, after setting `API_BASE_URL` to the published URL and `ADDRESS` to an Ethereum Mainnet contract address:

```sh
curl "$API_BASE_URL/v2/contract/1/$ADDRESS"
curl "$API_BASE_URL/v2/contract/1/$ADDRESS?fields=abi,compilation"
curl "$API_BASE_URL/v2/contract/1/$ADDRESS?fields=all"
```

The read path starts with an indexed chain-and-address lookup, then hydrates only the entity dependencies needed for the requested fields, all from the same published snapshot. An absent contract can stop at the lookup. Selected responses may still require blob reads, including when compilation data is stored in blobs.

The supplied *Sourcify Query Profile*, pages 2–3, reports seven days of traffic across all chains: 81.4% of requests were single-contract lookups; approximately 154 million of 190.8 million single-contract lookups requested `fields=abi,compilation`; and negative lookups outnumbered positive lookups by nearly 3:1. This makes selected-field reads and fast snapshot misses performance priorities alongside full-field parity. These figures describe Sourcify's observed all-chain workload, not a measured Mainnet workload for this mirror or a throughput commitment.

### Signatures

The proof of concept derived the signatures field from the ABI instead of copying the compiled_contracts_signatures table. It matched on 3,136 of 3,144 contracts and differed on 8, so for the Mainnet load we are copying the table. Everything we need is in your public export, so this needs nothing from you. Measured on a random sample of 130 Mainnet contracts the field averages 2.4 KB, with a sample maximum of 20 KB; we have not bounded the population maximum. Across 7.8M addresses that projects to roughly 19 GB before deduplication and single-digit GB after, modelled on your own compilation deduplication ratio rather than measured. The serving order inside function, event and error is not alphabetical or by selector, but we verified across 3,144 contracts that it follows a single global order consistent with your signatures table ids, so it reproduces from the export without needing a rule from you.

## Out of scope

The following API endpoints will not be supported:

- `POST /v2/verify` — not support to verify smart contract on Arkiv
- `GET /v2/contracts/{chain}` — listing, sort and afterMatchId pagination, blocked on Arkiv server-side ordering and on the absence of cursor or offset
- `GET /v2/contract/all-chains/{address}` — the mirror holds one chain, so it would answer a narrower question than yours
- Verification-job polling and legacy file/check endpoints
- stats / COUNT

Other features not supported:

- Any chain other than Ethereum Mainnet
- Anything verified after the cutoff
- Incremental sync

## Done means all of the following:

1. The publicly hosted read API supports `GET /v2/contract/1/{address}` as specified above, with its base URL, documentation, and examples published. A reproducible test suite both sides can run verifies successful minimal, selected-field (including `abi,compilation`), and all-field responses, plus snapshot misses and invalid requests. Response parity uses references captured for the same snapshot, with the stated field fidelity and frozen `proxyResolution` exception; the excluded endpoints are not part of the parity claim.
2. The replication is a one-off load of everything verified before 30 September 2026, run across several days, with cutoff and load timestamps published.
3. Read latency published at P50 and P95 for the mirror and Sourcify API, using the same client and query set. Results separate successful reads from negative lookups and distinguish minimal, selected-field (including `abi,compilation`), and all-field requests. We publish the query set, response-status differences, measurement window, client location, concurrency, and cache conditions so the comparison is reproducible and honest in both directions.
4. The replication code, read API code, and test suite are open source under MIT License, in the Arkiv GitHub organisation, so any third party can rebuild the mirror, serve the API, and compare it against Sourcify themselves.
5. A written answer on deletion that your team is comfortable defending to a takedown requester.

If we cannot hit all five, we would rather show a smaller scope honestly at Devcon than a demo that overstates what is running.

## Stretch goal

**At least two parties other than Arkiv running a node on the testnet.** This goal depends on input from your side, so Arkiv cannot deliver it alone and should not claim it as something we control. It is the outcome we most want from this collaboration, and it is the thing that tests the hypothesis in tab 1.
