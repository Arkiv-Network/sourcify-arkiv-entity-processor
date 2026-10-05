# arkiv-entity-processor

Offline processor that turns Sourcify Ethereum contract records (NDJSON) into a deduplicated
[Arkiv](https://github.com/Arkiv-Network) prefill seed bundle. No database, RPC, Sourcify API or
transactions are involved: records in, files out.

Each contract becomes `sourcefile`, `code`, `compilation` and `verified_contract` entities (large
payloads spill into `blob` parts). Entities are deduplicated globally by semantic staging ID, raw
payload bytes are stored once by SHA-256, and output is written in input order with dependencies
first. Parsing/transformation and gzip compression run on bounded, ordered worker pools, so the
published files are byte-identical for any worker count.

`pack` and `upload` then publish a bundle as one deterministic `.tar.zst` object in S3-compatible
storage (Cloudflare R2), from where arkiv-prefill downloads it.

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
arkiv-entity-processor pack <BUNDLE_DIR> --output <FILE.tar.zst> [--level 3] [--threads N]
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
what `SHA256SUMS` describes.

* Format: PAX tar compressed with zstd (`application/zstd`, content checksum on). Every bundle file
  sits at the archive root (`seed.json`, `SHA256SUMS`, `entities/NNNNNN.jsonl`, …, no top-level
  directory), sorted by path, mode 0644, uid/gid 0, empty owner names, mtime 0. The tar stream is
  byte-identical to Python's `tarfile` PAX output for the same files (the previous packer).
* Reproducible: the same bundle and `--level` (default 3, 1–19) give a byte-identical archive for
  any `--threads` (zstd workers, default: CPUs, at most 8) and platform. Memory stays bounded
  (files are streamed; ~200 MB with 8 threads).
* Atomic: written to `<FILE>.partial` (locked against a second `pack`), synced, and only then
  renamed to `<FILE>`. Next to it goes the record `<FILE>.json`, also printed to stdout:

  ```json
  {"schema":"arkiv-entity-processor.archive.v1","archive":"sourcify-2026-10-05.tar.zst",
   "bytes":N,"sha256":"<archive>","sourceSha256":"<SHA-256 of SHA256SUMS>","files":N,
   "bundleBytes":N,"zstdLevel":3}
  ```
* Never overwrites: an existing `<FILE>` is reused (exit 0) if its record matches the bundle
  (same `sourceSha256` and size; `--verify-existing` also re-hashes it), otherwise refused.

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

## Build

The toolchain is pinned in `rust-toolchain.toml` (Rust 1.90.0 with rustfmt and clippy).

```sh
cargo build --release --locked        # target/release/arkiv-entity-processor
cargo test --locked
cargo fmt --all --check && cargo clippy --locked --all-targets -- -D warnings
```

zstd (`zstd-sys`) and `ring` compile C, so a C compiler is needed (`gcc`; cross builds use the
target's GCC). A static binary: `sudo apt-get install musl-tools && rustup target add
x86_64-unknown-linux-musl && cargo build --release --locked --target x86_64-unknown-linux-musl`.

## CI and releases

* `.github/workflows/ci.yml` runs fmt, clippy, tests, a release build and a small
  process/verify smoke test on every push and pull request to `main`.
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
