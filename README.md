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
ID order with dependencies first. The output is deterministic: byte-identical for any worker count,
and identical to the original `sourcify-arkiv-helper` pipeline (checked in CI on real export rows).

| Path | Contents |
| --- | --- |
| `src/` | `arkiv-entity-processor` (Rust): `process`, `verify`, `pack`, `upload` |
| `source/` | Bun + DuckDB source stage: `prepare.ts` (Parquet → DuckDB), `stream.ts` (DuckDB → NDJSON records) |
| `pipeline/` | `run.sh` (resumable phases, progress, `timings.json`), `state.ts` (its bookkeeping) |
| `Dockerfile`, `compose.yaml`, `.env.example` | container deployment |
| `tests/` | end-to-end test on a small real-data Parquet fixture (`tests/fixture/`) |

## Run with Docker Compose

### 1. Requirements

* Docker with Compose v2.
* Disk for a full Ethereum run: ~120 GB Parquet input, ~140 GB prepared DuckDB source, ~210 GB
  bundle, and for publishing another ~210 GB archive. Fast local SSD/NVMe strongly recommended.
* 12 CPUs and 48 GB memory for the container (the defaults); `prepare` peaks at ~36 GB.

### 2. Put the Parquet files in place

The pipeline reads `PARQUET_DIR/<table>/*.parquet` (default `./data/parquet`, mounted read-only)
for these nine tables of the Sourcify v2 export:

```text
data/parquet/
  code/  compiled_contracts/  compiled_contracts_metadata/  compiled_contracts_sources/
  contract_deployments/  contracts/  sources/  sourcify_matches/  verified_contracts/
```

Other tables (e.g. `signatures`, `compiled_contracts_signatures`) may be present and are ignored.
Download the export (public, no credentials; ~120 GB without the signature tables):

```sh
aws s3 sync s3://sourcify-production-parquet-export/v2/ ./data/parquet \
  --endpoint-url https://storage.googleapis.com --no-sign-request \
  --exclude 'signatures/*' --exclude 'compiled_contracts_signatures/*'
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

Records whose referenced rows are missing from the export (e.g. old Solidity compilations without
metadata) or that the transform cannot take (Yul without an ABI) are skipped with
`SKIP_MISSING_REFERENCES=1` (default), listed in `bundle/report.json` (`sourceSkipped`) and counted
in `timings.json`; more than `MAX_SKIPPED` aborts. With `0`, any such record aborts the run.

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

This packs the verified bundle into `WORK_DIR/archive/seed.tar.zst` (`seed.json` at the archive
root, deterministic) with a `seed.tar.zst.json` record, uploads it as a multipart object with its
SHA-256 in the object metadata, and finally downloads it again to check size and SHA-256. An
existing object is never overwritten: one with the same size and SHA-256 is accepted, a different
one is refused. Rerunning retries; a finished archive is reused.

### Reference run

Full Ethereum mainnet export of 2026-10, 12 CPUs / 48 GB container, default settings:

| Phase | Time | Notes |
| --- | ---: | --- |
| prepare | 5.2 min | 7,823,536 current matches; 147 GB DuckDB file; peak ~36 GB memory |
| process | 42.8 min | 7,770,258 contracts (~3,000/s), 53,278 skipped; 17,062,922 entities |
| verify | 2.7 min | 1,707 entity parts, 207 GB bundle (`payloads.bin` 114 GB) |

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
a finished bundle; `pack` and `upload` publish it. `process` is the former
`sourcify-offline-importer` (rust-importer in sourcify-arkiv-helper); its output is byte-identical
for the same input and settings.

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

`tests/fixture/parquet` holds every row behind 19 real matches (Solidity, Vyper, Fe, a large
source with blob entities, a two-revision match, a non-Ethereum match and one per skip reason),
extracted from the export by `tests/fixture/extract.ts`. `tests/fixture/expected.json` is the
output of the original helper pipeline for it; `bun run test` requires byte-identical NDJSON and
bundle files.

## CI and releases

* `.github/workflows/ci.yml` runs fmt, clippy, tests, a release build and a small
  process/verify smoke test, the end-to-end Bun tests, and builds the Docker image and runs it on
  the fixture, on every push and pull request to `main`.
* `.github/workflows/release.yml` builds `x86_64-unknown-linux-gnu`,
  `x86_64-unknown-linux-musl` (static) and `aarch64-unknown-linux-gnu`, each packaged as
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
