# Sourcify → Arkiv entity processor

Turns the public [Sourcify](https://sourcify.dev) Parquet export into a deduplicated
[Arkiv](https://github.com/Arkiv-Network) prefill seed bundle for every verified Ethereum mainnet
contract, and optionally publishes it to Cloudflare R2 for arkiv-prefill. Everything up to the
upload runs offline: no PostgreSQL, RPC, Sourcify API, wallets or transactions.

```text
Sourcify Parquet export (read-only)
  │ prepare   source/prepare.ts       DuckDB: current Ethereum matches + referenced rows → WORK/source/subset.duckdb
  │ process   source/stream.ts | arkiv-entity-processor process                       → WORK/bundle/
  │ verify    arkiv-entity-processor verify + record counts against the selection
  │ pack      arkiv-entity-processor pack       (publish)                             → WORK/archive/seed.tar.zst
  └ upload    arkiv-entity-processor upload     (publish)                             → s3://BUCKET/KEY on R2
```

Each contract becomes `sourcefile`, `code`, `compilation` and `verified_contract` entities (large
payloads spill into `blob` parts) with permanent expiry. Entities are deduplicated globally by
semantic staging ID, raw payload bytes are stored once by SHA-256, and output is written in match
ID order with dependencies first. The output is deterministic across worker counts.

The decoded `verified_contract` payload equals the **complete original input record as JSON**.
Nested objects, extra fields, nulls, source text and bytecode remain in their original locations;
the processor adds no schema marker, `codeRefs`, renamed fields or defaults to this payload.
This is JSON-value equality, not preservation of the input file’s whitespace or key order.
Shared source, code and compilation entities and query attributes remain available.

This guarantee starts at the processor input. A captured full API response retains every supplied
field, including standard compiler JSON and signatures. The Parquet adapter reconstructs
`stdJsonInput` and `stdJsonOutput` using Sourcify’s stored-field mapping and reads `signatures`
from the export’s signature tables. Original database IDs travel outside the contract JSON,
in an NDJSON wrapper, and become entity attributes. Live `proxyResolution` is not available
from an offline snapshot; full live API parity is not claimed.

The real SafeProxy Parquet fixture matches the saved API response except for live proxy
resolution and whitespace inside the `stdJsonOutput` metadata **string**. The exported
metadata text is retained, not rewritten to hide that difference. Signatures come from exported tables and are ordered by signature text, following the
production selector. Snapshot/API metadata text and live proxy state remain explicit limits
on strict cross-source JSON equality.

| Path | Contents |
| --- | --- |
| `src/` | `arkiv-entity-processor` (Rust): `process`, `verify`, `pack`, `upload` |
| `source/` | Bun + DuckDB source stage: `prepare.ts` (Parquet → DuckDB), `stream.ts` (DuckDB → NDJSON records) |
| `pipeline/` | `run.sh` (resumable phases, progress, `timings.json`), `state.ts` (its bookkeeping) |
| `Dockerfile`, `compose.yaml`, `.env.example` | container deployment |
| `tests/` | end-to-end test on a small real-data Parquet fixture (`tests/fixture/`) |

## Entity parity documentation

Start with the [overview](docs/index.html) and [one-contract example](docs/example.html).
The reference covers Sourcify-to-entity field mapping, storage, and known parity gaps.
It documents the offline processor; no HTTP API server is included. Open `docs/index.html`
in a browser, or serve `docs/` with any static file server.

## Run with Docker Compose

### 1. Requirements

* Docker with Compose v2.
* Historical nine-table run: ~120 GB Parquet input, ~140 GB prepared DuckDB source, ~210 GB
  bundle, and ~39 GB archive. The expanded payloads and signature tables need new sizing
  measurements; those historical numbers are not capacity estimates for this format.
* 12 CPUs and 48 GB memory for the container (the defaults); `prepare` peaks at ~36 GB.

### 2. Put the Parquet files in place

The pipeline reads `PARQUET_DIR/<table>/*.parquet` (default `./data/parquet`, mounted read-only)
for these eleven tables of the Sourcify v2 export:

```text
data/parquet/
  code/  compiled_contracts/  compiled_contracts_metadata/  compiled_contracts_sources/
  contract_deployments/  contracts/  sources/  sourcify_matches/  verified_contracts/
  compiled_contracts_signatures/  signatures/
```

Both signature tables are required, even if the selected contracts have no signatures.
Download the export (public, no credentials):

```sh
aws s3 sync s3://sourcify-production-parquet-export/v2/ ./data/parquet \
  --endpoint-url https://storage.googleapis.com --no-sign-request
```

An existing copy elsewhere works too: point `PARQUET_DIR` at the directory that contains the table
directories.

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
docker compose up -d pipeline       # prepare -> process -> verify, with no network access
docker compose logs -f pipeline     # JSON progress: phase, contracts/s, ETA, memory
```

Rerunning `docker compose up pipeline` resumes: a completed `prepare` is reused, an interrupted
one continues, and a completed bundle is reused only if it was produced with the same output
settings. Processing itself does not resume: if it fails, the pipeline stops with the bundle
directory left without `seed.json`; move it aside (`mv data/work/bundle data/work/bundle.failed`)
and rerun. `docker compose stop pipeline` interrupts cleanly and records the failure.

`WORK_DIR` afterwards:

| Path | Contents |
| --- | --- |
| `source/` | prepared DuckDB source and `source-manifest.json` (selection, per-step timings, row counts) |
| `bundle/` | the seed bundle (see [Output](#output)); `seed.json` exists only if processing completed |
| `logs/` | `prepare.log`, `source.log` (incl. skipped records), `process.log`, `verify.log`, … |
| `timings.json` | per-phase status, durations, CPU seconds, peak memory, counts, configuration, build hashes |

Records missing required referenced rows (for example deployed contracts or bytecode), or with
malformed ABI values other than an array or null, are skipped with
`SKIP_MISSING_REFERENCES=1` (default), listed in `bundle/report.json` (`sourceSkipped`) and counted
in `timings.json`; more than `MAX_SKIPPED` aborts. With `0`, any such record aborts the run.

A missing Solidity metadata row is represented as `metadata: null`; the available contract data
is still imported. Yul and other records with a null ABI retain `abi: null` (not an empty array).
The ABI hash is SHA-256 of its JSON value, including `null`. This does not recover metadata absent
from the export.

Parquet NDJSON lines use `{record: <contract JSON>, sourcifyIds: {...}}`. The wrapper IDs
`compilationId`, `deploymentId`, `verificationId`, and `contractId` become the string attributes
`sourcifycompilationid`, `sourcifydeploymentid`, `sourcifyverificationid`, and `sourcifycontractid`.
They are absent from the decoded contract payload; `matchId` remains an API field. Raw API
inputs are still accepted and preserved without stripping any user-supplied fields.

**Migration:** all verified-contract payloads now use the complete input record instead of the
old selected-field envelope, so their entity keys change. Regenerate bundles; do not reuse an
old bundle or resume old processed output as if it contained the new payloads. Source manifest
v2 requires re-preparation in a fresh work directory; v1 sources are refused because they lack
signature coverage. Keep the original Parquet files. The larger
payloads can increase bundle size and produce additional spill parts. Existing compression
and spill decoding rules still apply. Byte equality with the old helper output is no longer
a compatibility guarantee.

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

Full Ethereum mainnet export of 2026-10, 12 CPUs / 48 GB container, default settings:

| Phase | Time | Notes |
| --- | ---: | --- |
| prepare | 5.2 min | 7,823,536 current matches; 147 GB DuckDB file; peak ~36 GB memory |
| process | 42.8 min | 7,770,258 contracts (~3,000/s), 53,278 skipped; 17,062,922 entities |
| verify | 2.7 min | 1,707 entity parts, 207 GB bundle (`payloads.bin` 114 GB) |
| pack | 3.5 min | prefill archive 38.8 GB (re-verifies the 207 GB bundle first; 204 MB memory) |

## Run without Docker

Needs Bun 1.4 and a Rust toolchain (or a release binary on `PATH`):

```sh
bun install --frozen-lockfile
cargo build --release --locked
PATH=$PWD/target/release:$PATH WORK_DIR=$PWD/data/work PARQUET_DIR=$PWD/data/parquet \
  ENTITY_OWNER=0x... bash pipeline/run.sh run      # or: publish, all
```

or each stage by hand:

```sh
bun source/prepare.ts --parquet-dir data/parquet --source-dir work/source --all --threads 8 --memory-limit 24GB
bun source/stream.ts --source-dir work/source --skip-missing-references --max-skipped 100000 \
  | arkiv-entity-processor process --require-source-complete --output work/bundle \
      --owner 0x... --workers 4 --compression-workers 4
arkiv-entity-processor verify work/bundle
```

# The `arkiv-entity-processor` CLI

`process` turns Sourcify Ethereum contract records (NDJSON) into the seed bundle; `verify` checks
a finished bundle; `pack` and `upload` publish it. `process` derives from the former
`sourcify-offline-importer`; the complete-input payload format changes its output.

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
cargo build --release --locked        # target/release/arkiv-entity-processor
cargo test --locked
cargo fmt --all --check && cargo clippy --locked --all-targets -- -D warnings
bun install --frozen-lockfile
bun run test                          # end-to-end on tests/fixture (needs the release build)
```

zstd (`zstd-sys`) and `ring` compile C, so a C compiler is needed (`gcc`; cross builds use the
target's GCC). A static binary: `sudo apt-get install musl-tools && rustup target add
x86_64-unknown-linux-musl && cargo build --release --locked --target x86_64-unknown-linux-musl`.

`tests/fixture/parquet` holds every row behind 19 real matches (Solidity, Vyper, Fe, a large
source with blob entities, a two-revision match, a non-Ethereum match and one per skip reason),
extracted from the export by `tests/fixture/extract.ts`. `tests/fixture/expected.json` retains
the original source-stream baseline. The pipeline tests check full verified-contract payload
equality with each input record and deterministic bundles across worker counts.

## CI and releases

* `.github/workflows/ci.yml` runs fmt, clippy, tests, a release build and a small
  process/verify smoke test, the end-to-end Bun tests, and builds the Docker image and runs it on
  the fixture, on every push and pull request to `main`.
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
