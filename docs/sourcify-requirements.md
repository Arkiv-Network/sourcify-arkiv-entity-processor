# Sourcify mirror requirements and evidence

This is a planning synthesis, not a signed acceptance agreement. It separates the
supplied documents, reported observations, and recommended implementation scope.
The [server plan](processor-server-plan.md) translates it into repository changes.

## Sources and their status

| Source | What it establishes | Status |
| --- | --- | --- |
| [Full shared PoC document](sourcify-arkiv-poc-shared.md) | Collaboration goals, successive grant proposals, support, and chain-side acceptance criteria | Explicitly a working document; no bilateral sign-off established here |
| Supplied *Sourcify Query Profile.pdf*, pages 2–3 | Observed seven-day, all-chain route and field distribution | Workload evidence, not a Mainnet demand estimate or service-level commitment |
| [November proposal](november-2026-milestone-proposal.md) | Earlier snapshot/fidelity scope and subsequently proposed HTTP contract | Proposed scope; some statements conflict with the full document's final criteria |
| Processor code and fixtures | Existing storage and reconstruction behavior | Implementation evidence, not proof of full-dataset coverage or a deployed server |

The source documents remain intact. This synthesis does not silently replace their
dates or commitments. Raw traffic logs and consumer reports are not needed for
implementation; any future publication of those materials must follow the agreed
publication terms.

## Product purpose

Arkiv provides a queryable mirror of Sourcify's verified-contract data. It does not
replace Sourcify's verifier or Postgres service. A node holds data and indexes and
answers queries locally; one query is not distributed across multiple nodes.

The adoption hypothesis is that heavy consumers want to operate nodes rather than
maintain their own ingest pipelines or depend on a shared API. Two outside node
operators is a collaboration stretch outcome, not a server release gate. UI filters
and entity exploration are a separate workstream from HTTP lookup compatibility.

## Reported evidence and its limits

- The PoC reportedly loaded 3,144 Unichain contracts. Its detailed comparison found
  matching field values for 3,136 contracts and signature differences for eight;
  the broad “24/24” statement is not evidence of universal byte identity.
- The query profile reports 81.4% of requests going to single-contract lookup.
  Approximately 154 million of 190.8 million lookups requested
  `fields=abi,compilation`; negative lookups outnumbered hits by nearly 3:1.
- These measurements motivate cheap misses and dependency-aware field selection.
  They do not establish an expected Mainnet request rate or a latency target.
- Shared compilation, source, and bytecode entities reduce repetition. Chunked
  payloads still require reassembly and integrity checks. Selecting fewer API fields
  does not guarantee fewer bytes in every stored envelope.
- Existing processor fixtures establish input reconstruction for their inputs.
  They do not establish full Parquet-to-live-API parity. Live proxy state and
  metadata-string differences are explicit limits in the current processor docs.

## Provisional baseline from the final chain-side criteria

Use the final section of the shared document as the working baseline for planning,
pending confirmation of the decisions below:

- Ethereum Mainnet snapshot captured on **1 October 2026 at 09:21:28 UTC**, hosted on
  Sorbet. Publish the exact manifest, final contract/entity counts, and load window;
  approximately 7.8 million contracts is an estimate, not the acceptance count.
- A dedicated Arkiv-controlled wallet owns the entities. All migrated entities,
  shared dependencies, and chunks use `expiresAt = 2^64 - 1`, verified by readback.
- Reconstruct the stated 23 fields, derive signatures from ABI for the serving
  projection, and exclude live proxy resolution. The source does not enumerate
  those 23 fields or settle the exact response to requesting excluded fields.
- Serve current state, without a public historical-query interface. Maximum
  lifetime is not a promise of historical retrieval or transaction-history erasure.
- Publish reproducible latency, throughput, errors, and costs. Numerical performance
  targets remain undefined until measurement.

The document proposes operation through at least 31 May 2027. Its V2 funding split
is $25,000 for the pilot and $10,000 for optional node operation. Those are commercial
and operational context, not HTTP implementation requirements.

## Decisions requiring reconciliation

| Decision | Earlier wording | Final chain-side wording | Proposed handling |
| --- | --- | --- | --- |
| Dataset | Before 30 September, with uncertain load-window inclusion | Exact 1 October capture | Bind every release to an explicit manifest; confirm the authoritative artifact |
| Signatures | Copy exported signature tables | Derive from ABI and compare by value | Retain processor preservation; isolate the serving policy and test known mismatches |
| Proxy | Frozen index-time value, 24 fields | Live proxy resolution excluded, 23 fields | Confirm absent/null/rejected-selection behavior before claiming `fields=all` parity |
| Fidelity | Byte identity for 23 fields | Reconstruction plus signature value comparison | Publish a per-field comparison matrix; do not infer byte parity from JSON equality |
| Retention | Expiry concerns and no archival guarantee | Maximum lifetime with readback | Verify current-state retention separately from history and deletion behavior |
| Owner | Undecided | Arkiv-controlled wallet | Configure trusted dataset ownership; do not query arbitrary writers |
| API shape | Left as an open question in shared text | Not fully specified | Adopt the proposed Sourcify-shaped lookup, then verify its precise contract |

Comparing signatures “by value” does not solve differences in signature content.
The eight PoC mismatches need reproducible fixtures, a fix, or an explicitly accepted
exception. Do not delete exported-signature support from the processor as a side
effect of choosing runtime derivation for the API.

## Proposed server scope

Public read-only `GET /v2/contract/1/{address}` with minimal output, explicit field
selection, the hot `abi,compilation` combination, and `fields=all` for the agreed
field set. Preserve the distinction between absent fields, nulls, empty values,
and unavailable data. No silent upstream fallback or live Ethereum proxy queries.

For the field matrix, start from the current 24-field captured fixture:

| Group | Candidate fields | Decision |
| --- | --- | --- |
| Minimal | `address`, `chainId`, `match`, `matchId`, `creationMatch`, `runtimeMatch`, `verifiedAt` | Verify always-returned behavior against pinned compatibility fixtures |
| Hot shared data | `abi`, `compilation` | Priority selective-hydration path |
| Deployment and bytecode | `deployment`, `creationBytecode`, `runtimeBytecode` | Preserve stored values and representation |
| Source and compiler artifacts | `sources`, `metadata`, `storageLayout`, `transientStorageLayout`, `userdoc`, `devdoc`, `sourceIds`, `stdJsonInput`, `stdJsonOutput`, `additionalInput` | Explicit dependency and comparison rules per field |
| Signatures | `signatures` | Stored-input fidelity and API derivation are separate policies |
| Proxy | `proxyResolution` | Live resolution excluded; exact API representation unresolved |

Removing only `proxyResolution` gives a candidate 23-field set, **an inference to
confirm**, not an enumeration supplied by Sourcify. The implementation specification
must also settle dotted selectors, `omit`, unknown/empty/repeated parameters,
address normalization, and field ordering. Reject unsupported selectors explicitly.

## Acceptance requirements for the server

1. Publish the base URL, API contract, examples, dataset identity, and supported
   field/comparison matrix. Test the documented behavior, not a moving live target.
2. Lookup is scoped to the configured Arkiv network, trusted owner, and dataset.
   A missing contract ends at lookup; duplicate candidates or incomplete data are
   errors, not arbitrary choices or successful partial responses.
3. `404` means a valid address absent from the selected mirror's current dataset.
   It does not mean globally unverified. Invalid requests receive documented client
   errors; backend failure and broken dependencies receive distinct server errors.
4. Hydrate only the dependency closure needed for selected fields. Verify logical
   payloads, chunk counts, lengths, and hashes. Use one consistent read view per
   response, with bounded work and no public historical-read parameter.
5. Reproducible conformance covers minimal/selected/all-supported fields, misses,
   invalid input, nulls, shared data, chunks, corruption, and backend outages.
6. Measure successful and absent requests separately, by field group and cold/warm
   cache state. Publish P50/P95, throughput, errors, response and backend bytes,
   query set, concurrency, client location, hardware, and measurement window.
7. Production acceptance exercises the actual Arkiv reader. Local bundle serving
   is useful for tests but does not establish an Arkiv-backed deployment.
8. Release the processor, API, tests, and runbook under the agreed MIT terms, and
   provide the written deletion answer. Verify licensing during release preparation.

## Explicit exclusions

Verification writes and job polling, legacy endpoints, global selector lookup,
contract listing and sort, all-chain lookup, statistics/count endpoints, other
chains, public historical queries, live replication, and live proxy resolution.
The experimental Arkiv stack's internal cursor and count support does not add those
features to the product scope. The UI and optional future replication work keep
their own acceptance criteria.
