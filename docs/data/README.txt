One-contract evidence: SafeProxy, Ethereum mainnet

sourcify-response.json is the saved public HTTP response. Its capture URL/time
are in provenance.json. processor-input.ndjson contains that unchanged record
followed by a source-complete marker. This source capture has not been rewritten.

stored-contract.json is the actual transport-decoded verified_contract v3
payload: a stripped envelope containing data and explicit references.
hydrated-contract.json is actual `hydrate` CLI output reconstructed from the
seed bundle. Its JSON values equal the complete captured source.

processor-entities.json lists every generated entity with its real key, decoded
stored payload, typed attributes, decoded byte count, wire byte count and original
seed entry. Binary payloads are displayed as hex. seed-entities.jsonl concatenates
all import-ready entity parts in order. example-bundle.tar.gz contains the full
verified bundle including seed.json and SHA256SUMS. None of this implies an Arkiv
deployment. The 0x111...111 owner is an illustrative CLI setting.

Reproduce from this saved response (in the processor repository):

  cargo build --release --locked
  example_work=$(mktemp -d)
  cp docs/data/sourcify-response.json "$example_work/sourcify-response.json"
  cp docs/data/processor-input.ndjson "$example_work/input.ndjson"
  target/release/arkiv-entity-processor process \
    --input "$example_work/input.ndjson" --output "$example_work/bundle" \
    --require-source-complete --owner 0x1111111111111111111111111111111111111111
  target/release/arkiv-entity-processor hydrate "$example_work/bundle"
  python3 docs/export-example.py --work "$example_work" \
    --captured-at 2026-10-09T13:34:56Z

The exporter runs bundle verify and hydrate, compares the complete hydrated JSON
with the source, and validates every decoded seed payload against audit bytes and
SHA-256. It records binary and Rust source digests as well as the baseline Git
revision; a dirty working tree is explicit. Bundle timing/report fields and
archive timestamps can vary. No entity count or layout is hardcoded.

Size figures distinguish decoded payload bytes, compressed/identity wire payload
bytes and JSONL framing (attributes plus base64). The compact original JSON byte
count uses a stated serialization, not the HTTP file size. Single-sample figures
do not establish full-dataset savings.

This is API-input evidence. The database adapter cannot resolve current proxies
(it leaves out proxyResolution). Like every source record, the API response
carries no internal database IDs; none are stored.
