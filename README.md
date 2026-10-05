# arkiv-entity-processor

Offline processor that turns Sourcify Ethereum contract records (NDJSON) into a deduplicated
[Arkiv](https://github.com/Arkiv-Network) prefill seed bundle. No database, RPC, Sourcify API or
transactions are involved: records in, files out.

Each contract becomes `sourcefile`, `code`, `compilation` and `verified_contract` entities (large
payloads spill into `blob` parts). Entities are deduplicated globally by semantic staging ID, raw
payload bytes are stored once by SHA-256, and output is written in input order with dependencies
first. Parsing/transformation and gzip compression run on bounded, ordered worker pools, so the
published files are byte-identical for any worker count.

This is the former `sourcify-offline-importer` (rust-importer in sourcify-arkiv-helper) as a
standalone tool; its output is byte-identical for the same input and settings.

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

## Build

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.90.0 with rustfmt and clippy).

```sh
cargo build --release --locked        # target/release/arkiv-entity-processor
cargo test --locked
cargo fmt --all --check && cargo clippy --locked --all-targets -- -D warnings
```

A static binary: `rustup target add x86_64-unknown-linux-musl && cargo build --release --locked
--target x86_64-unknown-linux-musl` (all dependencies are pure Rust).

## CI and releases

* `.github/workflows/ci.yml` runs fmt, clippy, tests, a release build and a small
  process/verify smoke test on every push and pull request to `main`.
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
