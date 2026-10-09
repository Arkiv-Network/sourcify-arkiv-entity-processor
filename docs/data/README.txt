One-contract evidence: SafeProxy, Ethereum mainnet

sourcify-response.json is the exact saved public HTTP response. Its capture URL
and time are in provenance.json. processor-input.ndjson contains that JSON as
one record, followed by a completion marker. No fields were added to the record.

processor-entities.json contains every entity generated from that record:
decoded payload, real generated key, actual seed attributes, and original seed
entry. Binary code payloads are displayed as hexadecimal. seed-entities.jsonl
is the original import-ready output. example-bundle.tar.gz is the complete
verified local bundle, including SHA256SUMS and seed.json. No deployment is
implied. The 0x111...111 owner is an explicit example CLI setting.

Reproduce from this saved response (run in the processor repository):

  cargo build --release --locked
  work=$(mktemp -d)
  cp docs/data/sourcify-response.json "$work/sourcify-response.json"
  cp docs/data/processor-input.ndjson "$work/input.ndjson"
  target/release/arkiv-entity-processor process \
    --input "$work/input.ndjson" --output "$work/bundle" \
    --require-source-complete --owner 0x1111111111111111111111111111111111111111
  target/release/arkiv-entity-processor verify "$work/bundle"
  python3 docs/export-example.py --work "$work" --captured-at 2026-10-09T13:34:56Z

The export script checks every decoded payload against the processor audit
bytes and SHA-256, then verifies 42 field and reference relationships. Its
re-export records the current repository revision. Bundle timing/report fields
and archive timestamps can vary; entity keys and payloads remain reproducible.

Scope: this illustrates the transform with live API input. The production
pipeline reads Parquet. The API does not provide sourcifyIds, so those are absent
here. The API supplies current proxyResolution, while the offline Parquet
adapter cannot resolve current proxies. See provenance.json for field coverage
and exclusions; this example is not a claim of full API response parity.
