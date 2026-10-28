# Sourcify → Arkiv entity processor

Turns a [Sourcify](https://sourcify.dev) PostgreSQL database, such as the
[sourcify-mirror-node](https://github.com/Arkiv-Network/sourcify-mirror-node) (a mirror whose API returns what
sourcify.dev returns), into a deduplicated [Arkiv](https://github.com/Arkiv-Network) prefill seed bundle for every
verified Ethereum mainnet contract, and optionally publishes it to Cloudflare R2 for arkiv-prefill. Only `prepare`
reads the database (SELECT only); everything after it up to the upload runs offline: no PostgreSQL, RPC, Sourcify
API, wallets or transactions.

```text
Sourcify PostgreSQL database (read-only)
  │ prepare   source/prepare.ts       DuckDB: Ethereum matches + referenced rows        → WORK/source/subset.duckdb
  │ process   source/stream.ts | arkiv-entity-processor process                       → WORK/bundle/
  │ verify    arkiv-entity-processor verify + record counts against the selection
  │ pack      arkiv-entity-processor pack       (publish)                             → WORK/archive/seed.tar.zst
  └ upload    arkiv-entity-processor upload     (publish)                             → s3://BUCKET/KEY on R2
```

Each contract becomes `sourcefile`, `code`, `compilation` and `verified_contract` entities (large
payloads spill into `blob` parts) with permanent expiry. Entities are deduplicated globally by
semantic staging ID, raw payload bytes are stored once by SHA-256, and output is written in match
ID order with dependencies first. The output is deterministic across worker counts.

The stored `verified_contract` payload is a **stripped, versioned JSON envelope**:
`sourcify.verified-contract.v3` contains `data` plus an explicit `references` manifest.
Shared `sourcify.compilation.v3` envelopes hold compiler-owned fields; `sourcefile` and `code`
entities store repeated source text and bytecode. References identify content by hash and place
it back at exact JSON Pointer paths. They are not fabricated fields inside the original JSON.

`arkiv-entity-processor hydrate BUNDLE_DIR` reconstructs the **complete original input JSON**
from the import-ready seed entities. It verifies dependencies and content hashes, decodes gzip
and spill parts, and restores shared values. String contents, absent fields, nulls, arrays, hex
casing and unknown fields are preserved. JSON whitespace and object-key order need not match.
The hydrated response is a reader result; the full response is not stored a second time.
[`read-arkiv`](docs/arkiv-reader.md) reconstructs one contract directly from an Arkiv
prefill node at an explicit block, without a local bundle. Canonical JSON here sorts object keys while preserving exact number representations; it does
not claim RFC 8785 normalization.

This guarantee starts at the processor input. A captured full API response retains every supplied
field, including standard compiler JSON and signatures. The database adapter (`source/records.ts`)
builds the same record as `GET /v2/contract/1/<address>?fields=all` of the Sourcify server on that
database, without the live `proxyResolution`: `stdJsonInput` and `stdJsonOutput` with Sourcify’s
stored-field mapping (the metadata text as stored), and `signatures` from the signature tables in
the database’s collation order. **Records and entities carry Sourcify API fields only:** the
database’s internal row IDs (verification, deployment, compilation, contract) only join rows in
`prepare`; `matchId` is the API field. The processor refuses an input envelope with anything
beside the record (such as the former `sourcifyIds`). The HTTP server can resolve proxies using a
configured Ethereum RPC; full live API and dataset parity are not claimed.

Checked against the sourcify-mirror-node: records equal its API response field by field (see
[Checking records against an API](#checking-records-against-an-api)). In embedded output metadata strings, the HTTP server removes
insignificant JSON whitespace and decodes non-ASCII Unicode escapes to match captured API
responses. It preserves ASCII/control escapes, literal backslashes, number spelling and
key order. Stored signatures remain stored;
the HTTP server derives serving signatures from ABI. See the
[HTTP compatibility contract](docs/sourcify-http-compatibility.md) for these policies.

| Path | Contents |
| --- | --- |
| `src/` | `arkiv-entity-processor` (Rust): `process`, `verify`, `hydrate`, `read-arkiv`, `pack`, `upload`, each also for the [v4 UDC dataset](#the-v4-udc-dataset-branch-udc) (`src/udc*.rs`) |
| `crates/server/` | `arkiv-sourcify-server`: Arkiv-backed Mainnet contract reads; [API and runbook](docs/server.md) |
| `source/` | Bun + DuckDB source stage: `prepare.ts` (PostgreSQL → DuckDB), `stream.ts` (DuckDB → NDJSON records) |
| `pipeline/` | `run.sh` (resumable phases, progress, `timings.json`), `state.ts` (its bookkeeping) |
| `arkiv/` | [Postgres → Arkiv sync](#postgres--arkiv-sync) (`sync.ts`; `udc.ts`, its v4 writer) and a helper CLI (`cli.ts`), Bun + `@arkiv-network/sdk` (v3) and `@arkiv-network/sdk-udc` (0.9.0-next.2, v4) |
| `udc/` | The [UDC proposal](docs/udc-proposal.md)'s prototype, measurements (`measure/`) and end-to-end run on our node (`e2e/`) |
| `dashboard/` | [Sync dashboard](#sync-dashboard): `status.ts` (writes `status.json`) and the static page nginx serves |
| `Dockerfile`, `compose.yaml`, `.env.example` | container deployment |
| `tests/` | end-to-end tests on a small real-data PostgreSQL fixture; [opt-in live contract comparison](docs/live-conformance.md) |

## The v4 (UDC) dataset (branch `udc`)

[docs/udc-proposal.md](docs/udc-proposal.md) moves the dataset to Arkiv's user-defined-columns entities: no payloads,
only typed attributes, five kinds tagged `sourcify.v4.<kind>` in `arkiv.schema`, content in codec-tagged `bytes`
attributes (identity, raw DEFLATE or a chunk list) and references as `key` attributes. Hydration rebuilds the v3
record byte for byte. On this branch every stage has a v4 mode next to the v3 one, which stays unchanged:

```sh
arkiv-entity-processor process -i records.ndjson -o bundle --format udc --owner 0x… [--schema-prefix sourcify.v4]
arkiv-entity-processor verify bundle            # also line by line: nonces, keys, schema, every reference
arkiv-entity-processor hydrate bundle           # the v3-identical records
arkiv-entity-processor pack bundle -o seed.tar.zst && arkiv-entity-processor upload seed.tar.zst … --expect-format udc
arkiv-entity-processor read-arkiv 0x… --format udc --rpc-url … --owner 0x… [--read-view auto|pinned|head]
arkiv-entity-processor udc-page --rpc-url … --owner 0x… [--descending] [--filter "language = str('Vyper')"]
SYNC_FORMAT=udc SYNC_SCHEMA_PREFIX=sourcify.v4 bun arkiv/sync.ts --once        # patches re-verified contracts in place
arkiv-sourcify-server --format udc --rpc-url … --arkiv-chain-id … --owner 0x…  # same HTTP contract as v3
```

A v4 bundle is `seed.json` version 2 (`sourcify-arkiv.udc-bundle.v1`), `report.json` (`sourcify-udc-import.v1`),
`SHA256SUMS` and `entities/NNNNNN.jsonl`, one entity per line in creation order: `{"nonce","key","expiresAt",
"attributes":[{"name","type","indexed","value"}]}` with `bytes` values in base64, keys the salt-0 keys of the bundle's
chain, owner and nonce. It has no `payloads.bin` or `metadata.jsonl`. Test entities on our node (chain 7706815) are
listed in [docs/udc-examples/sync-node/](docs/udc-examples/sync-node/); `udc/e2e/run.sh` reproduces them.

## Entity parity documentation

Start with the [overview](docs/index.html) and [one-contract example](docs/example.html).
The reference covers Sourcify-to-entity field mapping, storage, and known parity gaps.
Those pages document the offline processor. The [Arkiv read server](docs/server.md)
has a separate API contract, with optional live Ethereum proxy resolution. Open `docs/index.html`
in a browser, or serve `docs/` with any static file server.

## Run with Docker Compose

### 1. Requirements

* Docker with Compose v2.
* A Sourcify PostgreSQL database reachable on a Docker network, e.g. the
  [sourcify-mirror-node](https://github.com/Arkiv-Network/sourcify-mirror-node)'s `db` service.
* Disk for the prepared DuckDB source (~150 GB for Ethereum), the bundle (~210 GB) and the
  archive (~39 GB, publish only).
* 12 CPUs and 48 GB memory for the container (the defaults); see the reference run below for measured peaks.

### 2. Give the pipeline read access to the database

`prepare` reads these eleven tables, so a login role with `SELECT` on them is enough (and safer
than the owner):

```sql
CREATE ROLE sourcify_export LOGIN PASSWORD '...';
ALTER ROLE sourcify_export SET default_transaction_read_only = on;
GRANT USAGE ON SCHEMA public TO sourcify_export;
GRANT SELECT ON code, sources, signatures, compiled_contracts, compiled_contracts_sources,
  compiled_contracts_metadata, compiled_contracts_signatures, contracts, contract_deployments,
  verified_contracts, sourcify_matches TO sourcify_export;
```

The connection uses libpq settings: `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER` (Compose defaults:
`db`, `5432`, `sourcify`, `sourcify_export`), `PG_SCHEMA` (default `public`) and the Docker network
`SOURCE_NETWORK` (default `sourcify-node_default`). The password goes in a libpq password file
(default `./pgpass`, mode 600, readable by `PIPELINE_UID`; never commit it), mounted as a secret:

```text
db:5432:sourcify:sourcify_export:<password>
```

Matches with IDs from 1,000,000,000,000 on are refused: the sourcify-mirror-node gives that range to
verifications the node creates itself, which have no sourcify.dev `matchId`. Writes that commit a
match with its rows in one transaction (Sourcify, the mirror's sync) cannot make a preparation
inconsistent, but stop them for a dataset of a definite point in time: the preparation's last
match ID is in `source-manifest.json`.

### 3. Configure

```sh
cp .env.example .env      # then set ENTITY_OWNER (public owner address of every entity)
mkdir -p data/work        # WORK_DIR; must be writable by PIPELINE_UID:PIPELINE_GID (default 1000:1000)
```

All settings are documented in [`.env.example`](.env.example). Set `PIPELINE_UID`/`PIPELINE_GID`
to `$(id -u)`/`$(id -g)` to own the output as your user. For a quick trial, set
`PREPARE_LIMIT=10000` (first 10,000 matches) and use a separate `WORK_DIR`.

### 4. Run

```sh
docker compose build
docker compose --profile prepare run --rm prepare   # database -> WORK_DIR/source (the only database access)
docker compose up -d pipeline       # process -> verify on the prepared source, with no network access
docker compose logs -f pipeline     # JSON progress: phase, contracts/s, ETA, memory
```

Rerunning resumes: a completed `prepare` is reused, an interrupted
one continues, and a completed bundle is reused only if it was produced with the same output
settings. Processing itself does not resume: if it fails, the pipeline stops with the bundle
directory left without `seed.json`; move it aside (`mv data/work/bundle data/work/bundle.failed`)
and rerun. `docker compose stop pipeline` interrupts cleanly and records the failure.

`WORK_DIR` afterwards:

| Path | Contents |
| --- | --- |
| `source/` | prepared DuckDB source and `source-manifest.json` (database, selection, per-step timings, row counts) |
| `bundle/` | the seed bundle (see [Output](#output)); `seed.json` exists only if processing completed |
| `logs/` | `prepare.log`, `source.log` (incl. skipped records), `process.log`, `verify.log`, … |
| `timings.json` | per-phase status, durations, CPU seconds, peak memory, counts, configuration, build hashes |

Records missing required referenced rows (for example deployed contracts or bytecode; a Sourcify
database's foreign keys rule this out, a copy without them may not), or with
malformed ABI values other than an array or null, are skipped with
`SKIP_MISSING_REFERENCES=1` (default), listed in `bundle/report.json` (`sourceSkipped`) and counted
in `timings.json`; more than `MAX_SKIPPED` aborts. With `0`, any such record aborts the run.

A missing Solidity metadata row is represented as `metadata: null`; the available contract data
is still imported. Yul and other records with a null ABI retain `abi: null` (not an empty array).
The ABI hash is SHA-256 of its canonical exact JSON value, including `null` and lossless numbers.

Source NDJSON lines are `{"record": <contract JSON>}`. Raw API responses are accepted as well and
preserved without stripping any fields. Skip reasons name missing rows by content hash (bytecode,
source, signature) or by kind only, never by an internal row ID.

**Migration:** v3 changes verification and compilation payload schemas, content hashes and
entity keys. Regenerate bundles in a fresh output directory; never treat older full-record
bundles as v3 or resume their processing. Sources are prepared from PostgreSQL (source manifest
v3); sources prepared from the Parquet export (v1, v2) are refused, and bundles made from them carry
the internal `sourcify*id` attributes with different `verified_contract` keys: regenerate. Compression and
spill layers remain, with reference hydration added after decoding. Update readers to hydrate
v3; a transport-decoded verification payload is intentionally stripped. Existing deployed
readers have not been upgraded by this change. Size figures from old
full-record bundles do not predict v3 size; no full-dataset savings have been measured.

### 5. Publish to R2

Create a credentials file (default `./r2.env`, readable only by `PIPELINE_UID`; never commit it):

```sh
AWS_ACCESS_KEY_ID=...
AWS_SECRET_ACCESS_KEY=...
AWS_ENDPOINT_URL_S3=https://<account-id>.r2.cloudflarestorage.com
```

Set `R2_BUCKET` and `R2_KEY` in `.env` (use a new key per bundle), then:

```sh
docker compose --profile publish run --rm publish
```

This packs the verified bundle into `WORK_DIR/archive/seed.tar.zst` with a `seed.tar.zst.json`
record, uploads it as a multipart object with its SHA-256 in the object metadata, and finally
downloads it again to check size and SHA-256. An existing object is never overwritten: one with
the same size and SHA-256 is accepted, a different one is refused. Rerunning retries; a finished
archive is reused.

The archive holds what arkiv-prefill loads, at the archive root: `seed.json`, the entity parts,
`report.json` and a `SHA256SUMS` of exactly these files (prefill checks it). The audit files
`payloads.bin` and `metadata.jsonl` stay in the local bundle; `PACK_CONTENTS=full` packs them too
(~130 GB more for Ethereum).

### Reference run

The sourcify-mirror-node's database, frozen at matchId 56101108 (2026-10-14), 8 CPUs, default
worker settings:

| Phase | Time | Notes |
| --- | ---: | --- |
| prepare | ~6 min | 7,877,517 Ethereum matches; 175 GB DuckDB file; `PREPARE_MEMORY_LIMIT=6GB`, peak ~7 GB memory (the index steps need several GB beyond the DuckDB limit: 12GB peaked above 20 GB) |
| process | 86 min | 7,877,517 contracts, 0 skipped; 15,272,328 entities; peak ~10.6 GB memory (40 GB container) |
| verify | 3 min | 1,528 entity parts, 213 GB bundle |

The database is read only during the copy steps of `prepare` (about 3 minutes); the source stage
(`stream.ts`, one JavaScript thread) bounds `process`, not memory or CPUs.

## Daily seed

[`pipeline/daily.sh`](pipeline/daily.sh) publishes a fresh seed every day from a sourcify-mirror-node:

1. stops the mirror's sync, runs `prepare` into `data/runs/YYYYMMDD`, and starts the sync again (also on
   failure), so the dataset is the database at one `matchId`;
2. `run` (process -> verify, no network) and `publish` to `R2_KEY_PREFIX-YYYYMMDD.tar.zst` (default
   `seed-inputs/sourcify-ethereum-…`), read back and checked;
3. writes `R2_KEY_PREFIX-latest.json` (key, `lastMatchId`, counts, size, SHA-256) for readers;
4. deletes the seeds it uploaded itself beyond the newest `DAILY_KEEP_REMOTE` (default 2), listed in
   `data/runs/uploaded.tsv` (nothing else in the bucket is ever deleted), and the local runs older than today's.

Rerunning on the same day resumes; a published day is skipped. Each run appends a line to `data/runs/status.jsonl`.
Settings: the processor's `.env` (`ENTITY_OWNER`, `R2_BUCKET`, `R2_CREDENTIALS_FILE`, `PG*`, `./pgpass`) and
`MIRROR_DIR` (the mirror's directory), `DAILY_MEMORY` (40g), `DAILY_CPUS` (8), `DAILY_PREPARE_MEMORY_LIMIT` (6GB),
`DAILY_KEEP_REMOTE`, `DAILY_MIN_FREE_GB` (500). A run takes about 1.5 hours; the sync is stopped for about 5 minutes.

Install the systemd units ([`deploy/`](deploy/); adjust the paths in the service), then:

```sh
sudo cp deploy/sourcify-daily-seed.service deploy/sourcify-daily-seed.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl start sourcify-daily-seed.service        # one run now
sudo systemctl enable --now sourcify-daily-seed.timer   # 02:00 UTC daily (catches up after downtime)
journalctl -u sourcify-daily-seed.service               # progress (JSON lines)
```

## Postgres → Arkiv sync

[`arkiv/sync.ts`](arkiv/sync.ts) keeps live Arkiv chains up to date after a seed, one sync per chain (a
[target](#sync-targets)), all reading the same mirror. It writes the matches the mirror gained after a cursor (a
`matchId`, kept in `SYNC_DIR/state.json`) as the same entities a seed holds, so the Arkiv-backed server reads both
alike. One batch (`SYNC_BATCH`, default 200 matches):

1. `prepare --after-match-id` reads only the selected matches' rows, then `stream.ts` and `process` as for a seed;
2. dependencies (`sourcefile`, `code`, `compilation`, `blob`): one already on the chain (same owner, kind, hash and
   part, the server's own lookup) is reused, the others are created;
3. then each `verified_contract`, its `compilationref` naming the compilation's chain key. It is what makes a contract
   readable, so a contract is never visible before its dependencies. An address that already has a
   `verified_contract` is not written again: the same entity counts as synced, a different one is skipped and listed
   in `SYNC_DIR/skipped.jsonl` (changed contracts are not updated yet,
   [#13](https://github.com/Arkiv-Network/sourcify-arkiv-entity-processor/issues/13); the UDC sync below patches them);
4. every created entity is read back and compared; only then does the cursor move. Each batch appends a line
   (counts, gas, phase timings) to `SYNC_DIR/sync.jsonl`.

Entities never expire (`expiresAt` 2^64-1, like a seed's). Creates are packed into transactions of up to
`SYNC_MAX_TX_BYTES` (120,000) bytes of calldata, `SYNC_IN_FLIGHT` (4) of them unmined at a time. A chain that cannot
take large transactions gets smaller ones in its target file (e.g. `SYNC_MAX_TX_BYTES=16000`, `SYNC_IN_FLIGHT=1`);
an entity larger than the limit still travels alone, as payloads are up to 122,880 bytes (the seed's entity format;
of 110,000 sampled from a seed, 2.5% were above 16 kB, 0.09% above 32 kB and none above 64 kB). A chain that cannot take
writes at full speed also gets a pause after each batch that sent transactions: `SYNC_BATCH_DELAY_SECONDS` (0), with
`SYNC_BATCH=1` a pause between contracts. Batches that only find their entities on the chain do not wait.

The chain is the record of what was written: a rerun after any failure (or a crash between two transactions) looks
the entities up again instead of creating them twice. That needs one writer whose transactions are all mined, so the
sync holds `SYNC_DIR/sync.lock` (refreshed every 30 s; the lock of a container that stopped without releasing it is
taken over after 2 minutes) and starts only once the writer has no pending transaction. **One sync per chain
signs with the writer key; never sign with it elsewhere on that chain while it runs.**

The writer must be `ENTITY_OWNER`, the owner the seeds are exported with, and the sync refuses any other key: its lookups
only see the writer's own entities, so on a chain prefilled from a seed of another owner it would write everything again,
and the server reads a single owner.

The server accepts the keys the chain assigns: entity identity is the staging ID it recomputes from the content.

### UDC sync (`SYNC_FORMAT=udc`)

With `SYNC_FORMAT=udc` the same loop (cursor, lock, prepare, records) writes the [v4 dataset](docs/udc-proposal.md)
through [`arkiv/udc.ts`](arkiv/udc.ts): `process --format udc --schema-prefix SYNC_SCHEMA_PREFIX`, then

1. sources, codes and compilations already on the chain (`$owner`, `arkiv.schema` and `hash`) are reused, their chunks
   not written again; contracts are looked up by address;
2. new entities go in four levels, each mined and read back before the next names it: chunks; sources and codes;
   compilations; contracts. References are rewritten from bundle keys to the chain keys of the receipts
   (`EntityCreated`, salt 0);
3. a contract whose address already has one is **patched in place** when its `matchId` is at least the chain's (a
   re-verification under a new `matchId`, or a correction under the same one): every attribute is set, attributes the
   new version lacks are unset, and its key stays. The same content counts as synced; an older `matchId` than the
   chain's is skipped.

At most `SYNC_MAX_TX_OPS` (16, our node's limit) operations per transaction; gas is estimated unless `SYNC_GAS` says
otherwise. Our node answers HTTP 429 / `-32010 ServerBusy` when about eight requests are in flight: lookups run four
at a time and every call is asked again with a backoff. Each target names its own `ENTITY_OWNER` and writer key: the
v4 dataset has one owner per chain.

### Sync targets

Each chain the mirror is synced to is a target: `targets/NAME.env` holds the chain's settings (`ARKIV_RPC_URL`,
`ARKIV_CHAIN_ID`, optionally `ARKIV_WRITER_KEY_FILE` and `SYNC_*`; start from
[`sync-target.env.example`](sync-target.env.example)), and `data/sync/NAME/` its cursor (`state.json`), `status.json`,
logs (`sync.jsonl`, `skipped.jsonl`) and work files. What all targets share stays in `.env`: the database (`PG*`,
`PG_PASSFILE`, `SOURCE_NETWORK`), `PIPELINE_UID`, `ENTITY_OWNER` and `ARKIV_WRITER_KEY_FILE`. Each target runs as the
Compose project `arkiv-sync-NAME` ([`sync.compose.yaml`](sync.compose.yaml); 1 CPU, 2 GB, no swap by default), managed
with [`pipeline/sync-targets.sh`](pipeline/sync-targets.sh):

```sh
cp sync-target.env.example targets/prefill-dev.env    # the chain's RPC URL and chain ID
pipeline/sync-targets.sh up prefill-dev                # start (or recreate after an image build); prints where it starts
pipeline/sync-targets.sh ls                            # every target: container, status, cursor
pipeline/sync-targets.sh compose prefill-dev logs -f   # any compose command, e.g. run --rm sync --dry-run (plan the next batch)
pipeline/sync-targets.sh down prefill-dev              # stop; cursor and logs stay
pipeline/sync-targets.sh rm prefill-dev                # stop, and move its settings and data to data/sync/_removed/
```

`up` refuses a second target for one chain (the same RPC URL, or the same hash of block 1 at another URL).

### Where a sync starts

A sync reads its position off the chain when it starts: the writer's highest `verified_contract` `matchid`. A chain
prefilled from any seed of `ENTITY_OWNER` needs no cursor: the sync continues after that seed, whichever day it is
from. Only the batch in flight when a sync stopped can have holes below that `matchid` (batches run in order and the
cursor moves after a whole one), so it starts 1,000 Ethereum matches (at least 2 × `SYNC_BATCH`) before it; the
lookups find what is already there and count it as synced, which takes seconds. With a `state.json`, the cursor stays
unless the chain is behind it, and then the chain decides. Each start logs its decision (`progress`: the mirror's
newest match, `state`, `chain`, the `cursor` and where it is `from`) and keeps it in `status.json`.

The node answers `matchid > X` in time that grows with the entities above X, of every owner (about 30,000 a second on
the dev chain; with an upper bound as well, or `matchid > 0` on a prefilled chain, it times out). So the search only
asks within 2^21 match IDs (about a month) below the mirror's newest match. A chain without the writer's contracts
there and no `state.json` waits (`needs-seed`, checked every 10 minutes), unless its first start names the cursor:
`pipeline/sync-targets.sh up NAME --start-after <the seed's lastMatchId>`.

A running sync retries a failed batch after 1, 2, 4, ... minutes (at most 15) instead of exiting; `status.json` shows
`retrying` and the error.

Measured on the dev chain (2 s blocks; 2026-10-15), the RPC endpoint's block import is the limit:

| | |
| --- | --- |
| 200 matches | ~1,000-1,500 entities (about half reused), 32-60 transactions, 3.5-6.6 MB calldata, ~90-140 s |
| Rate | ~8,000 contracts/hour; prepare, records, process and lookups take ~5 s of a batch |
| Gas | 33-38 gas per calldata byte (~115 kB transaction: ~3.9M gas) |

`arkiv/cli.ts` (`bun run arkiv info|get|query|create|delete`) inspects the chain and tries single writes with the same
settings; writes are dry runs without `--execute`.

### Sync dashboard

<https://sourcify-mirror.arkiv-global.net/sync/> shows every target: its state (`stale` when the sync stopped
refreshing `sync.lock` though its status says it runs), cursor and that contract, the Ethereum matches it is behind the
mirror and an ETA at the last hour's net rate, contracts, transactions, gas and calldata by hour and in total, and its
chain (head, block time, gas price, the writer's balance and nonces, 24 h spend and runway); also the mirror's newest
match and hourly inflow, and the last daily seed runs. [`dashboard/status.ts`](dashboard/status.ts) writes it as
`status.json` (shape: [`dashboard/sample-status.json`](dashboard/sample-status.json)) every 15 s from `targets/*.env`,
`data/sync/NAME`, `data/runs/status.jsonl`, each target's RPC (read-only calls) and the mirror (SELECT only, at most
once a minute, scans bounded by match ID), next to the static page it copies on start; the page polls it. It runs as
the Compose project `arkiv-sync-dashboard` ([`dashboard.compose.yaml`](dashboard.compose.yaml); 0.25 CPU, 256 MB, no
swap, read-only, no ports), publishing into `DASHBOARD_OUT` (`/var/www/arkiv-sync`), which nginx serves:

```sh
sudo install -d -o "$PIPELINE_UID" -g "$PIPELINE_GID" -m 755 /var/www/arkiv-sync   # PIPELINE_UID/GID of .env
docker compose -f dashboard.compose.yaml --env-file .env up -d      # again after an image build
docker compose -f dashboard.compose.yaml --env-file .env logs -f    # a JSON line when a source starts or stops failing
```

```nginx
location = /sync { return 301 /sync/; }
location ^~ /sync/ {
    limit_except GET { deny all; }   # GET also allows HEAD
    alias /var/www/arkiv-sync/;
    index index.html;
    disable_symlinks on;
    gzip on;
    gzip_vary on;
    gzip_types application/json application/javascript text/css;
    add_header Cache-Control "no-cache" always;
    add_header X-Content-Type-Options "nosniff" always;
    add_header Referrer-Policy "no-referrer" always;
    add_header Content-Security-Policy "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'" always;
}
```

On sourcify-mirror.arkiv-global.net these locations are part of the mirror's site configuration,
[`nginx/sourcify-mirror.conf`](https://github.com/Arkiv-Network/sourcify-mirror-node/blob/main/nginx/sourcify-mirror.conf)
in sourcify-mirror-node, which also has the API's locations; a change to them goes there as well as to the host.

It publishes no secrets: of an RPC URL (one may carry an access token in its path) only the hostname, and every error
text is sanitized with every RPC URL seen, also a running sync's own (the URL and the token in its path masked, other
URLs cut to `scheme://host`, no filesystem paths or database credentials, at most 300 characters); the mirror's errors
only as their kind; never key files, pgpass or a seed run's error text. To try a change, write it once from the local
directories: `DASHBOARD_TARGETS_DIR=targets DASHBOARD_ENV_FILE=.env DASHBOARD_SYNC_DIR=data/sync DASHBOARD_RUNS_DIR=data/runs
DASHBOARD_OUT_DIR=/tmp/sync-page PG...=... bun --no-env-file dashboard/status.ts --once`.

## Checking records against an API

`tests/compare-records.py` requests each record's contract from a Sourcify API (`fields` = every
field except the live `proxyResolution`) and compares the two field by field:

```sh
bun source/stream.ts --source-dir work/source --limit 1000 > records.ndjson
python3 -I tests/compare-records.py records.ndjson http://127.0.0.1:5555        # the database's own server
python3 -I tests/compare-records.py records.ndjson https://sourcify.dev/server --every 100 --pause 1
```

## Run without Docker

Needs Bun 1.4 and a Rust toolchain (or a release binary on `PATH`):

```sh
bun install --frozen-lockfile
cargo build --release --locked
PGHOST=... PGPORT=... PGDATABASE=... PGUSER=... PGPASSWORD=... \
PATH=$PWD/target/release:$PATH WORK_DIR=$PWD/data/work \
  ENTITY_OWNER=0x... bash pipeline/run.sh run      # or: prepare, publish, all
```

or each stage by hand:

```sh
PGHOST=... PGPORT=... PGDATABASE=... PGUSER=... PGPASSWORD=... \
  bun source/prepare.ts --source-dir work/source --all --threads 8 --memory-limit 24GB
bun source/stream.ts --source-dir work/source --skip-missing-references --max-skipped 100000 \
  | arkiv-entity-processor process --require-source-complete --output work/bundle \
      --owner 0x... --workers 4 --compression-workers 4
arkiv-entity-processor verify work/bundle
```

# The `arkiv-entity-processor` CLI

`process` turns Sourcify Ethereum contract records (NDJSON) into the seed bundle; `verify` checks
a finished bundle; `hydrate` reconstructs full contract JSON; `pack` and `upload` publish it. `process` derives from the former
`sourcify-offline-importer`; the v3 shared-reference payload format changes its output.

## Input

Newline-delimited JSON, one verified Ethereum mainnet (`chainId` 1) contract record per line,
either bare or wrapped as `{"record": {...}}` / `{"contract": {...}}`. Blank lines are ignored.

A source that knows it is complete ends with a marker line:

```json
{"type":"source-complete","records":300}
```

`records` must equal the number of records before it, and nothing may follow it. An optional
`"skipped": [{"matchId": "...", "reason": "..."}]` list is copied into `report.json`. With
`--require-source-complete` the bundle is published only if this marker is present and valid;
without it, EOF counts as complete (use that only for trusted, finished files).

## Hydrating the original record

```sh
arkiv-entity-processor hydrate work/bundle > hydrated.ndjson
```

One full contract JSON is emitted per line in bundle order. The command reads `seed.json` and
entity parts, so it also works on an extracted prefill archive without audit-only
`metadata.jsonl` or `payloads.bin`. Missing, corrupt or unsupported dependencies fail with a
nonzero exit status; do not accept partial stdout as a complete dataset. Inspect the
[stored and hydrated example](docs/example.html) to compare both representations.

## Output

`--output` must name a directory that does not exist yet; runs are never resumed or overwritten.

| File | Contents |
| --- | --- |
| `entities/NNNNNN.jsonl` | import-ready prefill entities, `--part-size` (10,000) per file; gzip payloads when smaller, permanent expiry, resolved references |
| `payloads.bin` | unique raw payload bytes, back to back |
| `metadata.jsonl` | one row per entity: id, key, kind, attributes, payload SHA-256/offset/length |
| `report.json` | counts, configuration, timings, per-part checksums |
| `SHA256SUMS` | checksums for all of the above and `seed.json` |
| `seed.json` | version-1 prefill manifest, written atomically last |

The tool is fail-closed: on any error (malformed record, missing or mismatched completion marker,
data after the marker, write failure) it exits non-zero and leaves the partial directory for
diagnosis **without** `seed.json`. Only `report.json` timing/configuration fields differ between
runs with different `--workers` / `--compression-workers`.

## Usage

```text
arkiv-entity-processor process [OPTIONS] --output <DIR>
arkiv-entity-processor verify <BUNDLE_DIR>
arkiv-entity-processor hydrate <BUNDLE_DIR>
arkiv-entity-processor pack <BUNDLE_DIR> --output <FILE.tar.zst> [--contents prefill|full] [--level 3] [--threads N]
arkiv-entity-processor upload <FILE.tar.zst> --bucket <B> --key <K> [--credentials-file <FILE>]
arkiv-entity-processor --help | --version
```

```sh
# Piped source with a completion marker (recommended)
source-adapter | arkiv-entity-processor process --require-source-complete \
    --output bundle --owner 0x1111111111111111111111111111111111111111 \
    --workers 4 --compression-workers 4

# A finished, trusted NDJSON file
arkiv-entity-processor process -i records.ndjson -o bundle \
    --owner 0x1111111111111111111111111111111111111111

# Check SHA256SUMS, report.json status and seed.json; non-zero exit on any problem
arkiv-entity-processor verify bundle
```

`process` options (see `arkiv-entity-processor process --help` for details):

| Option | Default | Meaning |
| --- | --- | --- |
| `-i, --input <FILE>` | `-` | NDJSON file, or `-` for stdin |
| `-o, --output <DIR>` | required | new bundle directory |
| `--owner <ADDRESS>` | `0x…0001` | nonzero owner/creator address of every entity (metadata, not a key) |
| `--destination-chain-id <ID>` | `7706815` | Arkiv chain ID written to `seed.json` (alias `--chain-id`) |
| `--part-size <N>` | `10000` | entities per part file (alias `--part-entities`) |
| `--require-source-complete` | off | publish only after a valid completion marker |
| `--limit <N>` | none | stop after N records; not allowed with `--require-source-complete` |
| `-w, --workers <N>` | `1` | parse/transform threads (1–256), at most 8·N records in flight |
| `--compression-workers <N>` | `1` | gzip threads (1–256), at most 8·N entities in flight |

Migrating from `sourcify-offline-importer`: replace the binary name with
`arkiv-entity-processor process`; all previous flags, including `--chain-id` and
`--part-entities`, are still accepted.

## Publishing: `pack` and `upload`

```sh
arkiv-entity-processor pack bundle --output sourcify-2026-10-05.tar.zst
arkiv-entity-processor upload sourcify-2026-10-05.tar.zst --bucket arkiv-seeds \
    --key sourcify/2026-10-05.tar.zst --credentials-file ~/.config/arkiv/r2.env
```

Both print progress as JSON lines on stderr and one JSON result line on stdout.

### pack

`pack` runs `verify` first and refuses an unverified bundle, symlinks, special files and files not
listed in `SHA256SUMS`; every file is hashed again while it is packed, so the archive holds exactly
what its `SHA256SUMS` describes.

* Contents (`--contents`):
  * `prefill` (default): what arkiv-prefill loads, i.e. `seed.json`, `entities/*.jsonl` and
    `report.json`, plus a `SHA256SUMS` with the bundle's lines for exactly these files. The audit
    files `payloads.bin` and `metadata.jsonl` (more than half of a bundle; every payload is also
    inside the entity parts) stay local. For the full Ethereum bundle: 1,710 files, 74.7 GB, a
    38.8 GB archive (`full`: 207 GB of files).
  * `full`: every bundle file with the bundle's own `SHA256SUMS`.
* Format: PAX tar compressed with zstd (`application/zstd`, content checksum on). Files sit at
  the archive root (`seed.json`, `SHA256SUMS`, `entities/NNNNNN.jsonl`, …, no top-level
  directory), sorted by path, mode 0644, uid/gid 0, empty owner names, mtime 0. The tar stream is
  byte-identical to Python's `tarfile` PAX output for the same files (the previous packer). After
  extracting, `sha256sum -c SHA256SUMS` checks every file.
* Reproducible: the same bundle, `--contents` and `--level` (default 3, 1–19) give a
  byte-identical archive for any `--threads` (zstd workers, default: CPUs, at most 8) and
  platform. Memory stays bounded (files are streamed; ~200 MB with 8 threads).
* Atomic: written to `<FILE>.partial` (locked against a second `pack`), synced, and only then
  renamed to `<FILE>`. Next to it goes the record `<FILE>.json`, also printed to stdout:

  ```json
  {"schema":"arkiv-entity-processor.archive.v1","archive":"sourcify-2026-10-05.tar.zst",
   "bytes":N,"sha256":"<archive>","sourceSha256":"<SHA-256 of the bundle's SHA256SUMS>",
   "contents":"prefill","files":N,"bundleBytes":N,"zstdLevel":3}
  ```
* Never overwrites: an existing `<FILE>` is reused (exit 0) if its record matches the bundle
  (same `sourceSha256`, size, `--contents` and `--level`; `--verify-existing` also re-hashes it),
  otherwise refused.

### upload

`upload` stores the archive as one object, path-style (`{endpoint}/{bucket}/{key}`), signed with
AWS Signature V4 over HTTPS (rustls). Guarantees:

* Never overwrites. The key is checked first: an existing object is accepted only if its size and
  `x-amz-meta-sha256` equal the archive's (`"uploaded":false`, nothing sent); anything else is
  refused. The multipart upload is completed with `If-None-Match: *`, so an object created by
  someone else meanwhile is not replaced either.
* Integrity: the object gets metadata `sha256=<archive SHA-256>`; each part is signed with its
  real SHA-256 (`x-amz-content-sha256`), so the server rejects corrupted parts. The archive digest
  comes from the `pack` record when it matches the file's name and size (the file is then re-hashed
  during the upload and the upload is completed only if it matches), else the file is hashed first
  (always with `--verify-local`). Finally the object is read back (ranged GETs pinned to its ETag)
  and its size and SHA-256 compared; a mismatch fails the command (`--no-verify-remote` skips it).
* Clean failure: any error, or SIGINT/SIGTERM, aborts the multipart upload (a second signal exits
  at once). Requests are retried up to 5 times with exponential backoff (1–8 s) on network errors,
  HTTP 5xx, 429 and `RequestTimeout`, never on other 4xx.
* Parts: `--part-size-mib` (default 64, at least 5), raised to whole MiB so that at most 9,000
  parts are needed; `--concurrency` (default 8) threads each read their own byte range, so memory
  is about concurrency × part size.

Result line: `{"uri":"s3://B/K","bytes":N,"sha256":"…","uploaded":true,"verifiedRemote":true}`.

Credentials come from `--credentials-file`, an env-style file (when given, only the file is
used), or else from the same variables in the environment. Keep the file `chmod 600` (a warning is
printed otherwise); the key pair never appears in output, errors or command lines.

```sh
# ~/.config/arkiv/r2.env: KEY=VALUE lines, optional `export `, # comments, optional quotes
AWS_ACCESS_KEY_ID=<R2 access key id>
AWS_SECRET_ACCESS_KEY=<R2 secret access key>
AWS_ENDPOINT_URL_S3=https://<account-id>.r2.cloudflarestorage.com
# AWS_DEFAULT_REGION=auto   (optional, default auto)
```

`tests/minio-upload.sh` exercises the whole flow against a throwaway local MinIO container
(reproducibility, reuse, refusal, abort on failure and SIGINT, `If-None-Match`, retries, secret
hygiene); it is not run in CI: `cargo build --release --locked && tests/minio-upload.sh`.

## Build and test

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.90.0 with rustfmt and clippy); the source
stage needs Bun 1.4.

```sh
cargo build --workspace --release --locked # processor and Arkiv server
cargo test --workspace --locked
cargo fmt --all --check && cargo clippy --workspace --locked --all-targets -- -D warnings
python3 tests/server-smoke.py          # HTTP RPC mock, TCP requests, SIGTERM shutdown
bun install --frozen-lockfile
PGHOST=... PGUSER=... PGPASSWORD=... PGDATABASE=... \
  bun run test                        # end-to-end on tests/fixture (needs the release build and a PostgreSQL server)
```

zstd (`zstd-sys`) and `ring` compile C, so a C compiler is needed (`gcc`; cross builds use the
target's GCC). A static binary: `sudo apt-get install musl-tools && rustup target add
x86_64-unknown-linux-musl && cargo build --release --locked --target x86_64-unknown-linux-musl`.

`tests/fixture/fixture.sql` holds every row behind 19 real matches (Solidity, Vyper, Fe, a large
source with blob entities, a non-Ethereum match and one per skip reason), loaded with
`tests/fixture/schema.sql` into a fresh schema of the PostgreSQL server in the `PG*` variables
(any disposable one, e.g. `docker run -p 127.0.0.1:5432:5432 -e POSTGRES_PASSWORD=test postgres:15`)
and dropped afterwards. `tests/fixture/expected.json` retains
the original source-stream baseline. The pipeline tests check full hydrated-contract
equality with each input record and deterministic bundles across worker counts.

## CI and releases

* `.github/workflows/ci.yml` runs fmt, clippy, tests, a release build and a small
  process/verify smoke test, the end-to-end Bun tests against a PostgreSQL service, and builds the
  Docker image and runs it on the fixture (`prepare` from PostgreSQL, then `run` without network),
  on every push and pull request to `main`.
* `.github/workflows/release.yml` builds `x86_64-unknown-linux-gnu`,
  `x86_64-unknown-linux-musl` (static, with `musl-tools`) and `aarch64-unknown-linux-gnu`
  (smoke-tested with process/verify/pack), each packaged as
  `arkiv-entity-processor-<version>-<target>.tar.gz` (binary + README) with a `.sha256` file.
  * Push a tag `vX.Y.Z` matching the `Cargo.toml` version to publish a GitHub Release with
    these assets (tags containing `-` are marked pre-release).
  * Run it manually (Actions → Release → Run workflow) for a dry run: the archives are uploaded
    as workflow artifacts and no release is created.

To cut a release:

```sh
# bump version in Cargo.toml (and Cargo.lock via `cargo build`), commit, then:
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0
```
