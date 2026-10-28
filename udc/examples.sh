#!/usr/bin/env bash
# Regenerates docs/udc-examples/safeproxy from the saved SafeProxy record and checks that the UDC
# entities rebuild docs/data/hydrated-contract.json byte for byte, then re-hydrates the recorded node
# round trip (docs/udc-examples/safeproxy-node) from the entities the node returned.
set -euo pipefail
cd "$(dirname "$0")/.."
bin=target/release/arkiv-entity-processor
cargo build --release --locked -p arkiv-entity-processor
out=docs/udc-examples/safeproxy
"$bin" udc-example --input docs/data/sourcify-response.json --out "$out"
cmp "$out/hydrated-contract.json" docs/data/hydrated-contract.json
echo "safeproxy: hydrated-contract.json equals docs/data/hydrated-contract.json"
node=docs/udc-examples/safeproxy-node
owner=$(jq -r .owner "$node/write.json")
"$bin" udc-example --input docs/data/sourcify-response.json --out "$node/example" \
  --owner "$owner" --first-nonce "$(jq -r .firstNonce "$node/write.json")" --prefix sourcify-udc-proposal.v4
cmp <(jq -c '[.[].key]' "$node/example/entities.json") <(jq -c .created "$node/write.json")
address=0x000000000034065b3a94C2118CFe5B4C0067B615
"$bin" udc-hydrate --entities "$node/readback.json" --address "$address" \
  --prefix sourcify-udc-proposal.v4 --owner "$owner" | cmp - docs/data/hydrated-contract.json
"$bin" udc-hydrate --entities "$node/readback-abi-compilation.json" --address "$address" \
  --prefix sourcify-udc-proposal.v4 --owner "$owner" --fields abi,compilation \
  | cmp - "$node/hydrated-abi-compilation.json"
echo "safeproxy-node: the node's entities rebuild the contract byte for byte"
