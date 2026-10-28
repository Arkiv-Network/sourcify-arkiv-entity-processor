#!/usr/bin/env bash
# End-to-end run of the UDC (v4) sync and server against our node (docs/udc-proposal.md, step 5). Writes to the chain:
# run it against our node only (chain 7706815), never Jalebi.
#
#   ARKIV_PRIVATE_KEY_FILE=… udc/e2e/run.sh WORK_DIR
#
#  1. a disposable PostgreSQL container with tests/fixture (127.0.0.1:E2E_PG_PORT, default 55433);
#  2. arkiv/sync.ts --once --start-after 0 with SYNC_FORMAT=udc: the fixture's matches as v4 entities;
#  3. udc/e2e/reverify.sql (a re-verification under a newer matchId with a new compilation), then the sync again: the
#     contract is patched in place, everything else is already synced;
#  4. every contract read back (read-arkiv --format udc) against v3 hydration of the same records, byte for byte;
#  5. crates/server --format udc against the node, every contract x every query against the v3 path, byte for byte.
#
# Settings: ARKIV_RPC_URL (default http://127.0.0.1:18545), ARKIV_CHAIN_ID (7706815), ENTITY_OWNER (the key's address),
# SYNC_SCHEMA_PREFIX (sourcify-udc-proposal.v4), DOCKER (docker), E2E_PG_PORT (55433), E2E_SERVER_PORT (3101). The container
# is removed at the end.
set -euo pipefail
cd "$(dirname "$0")/../.."
work=${1:?usage: udc/e2e/run.sh WORK_DIR}
mkdir -p "$work"
: "${ARKIV_PRIVATE_KEY_FILE:?ARKIV_PRIVATE_KEY_FILE: the writer key file}"
export ARKIV_RPC_URL=${ARKIV_RPC_URL:-http://127.0.0.1:18545} ARKIV_CHAIN_ID=${ARKIV_CHAIN_ID:-7706815}
export ENTITY_OWNER=${ENTITY_OWNER:?ENTITY_OWNER: the public address of the writer key}
export SYNC_FORMAT=udc SYNC_SCHEMA_PREFIX=${SYNC_SCHEMA_PREFIX:-sourcify-udc-proposal.v4} SYNC_BATCH=200
export PGHOST=127.0.0.1 PGPORT=${E2E_PG_PORT:-55433} PGDATABASE=postgres PGUSER=postgres PGPASSWORD=test
port=${E2E_SERVER_PORT:-3101}
docker=${DOCKER:-docker}
bin=target/release/arkiv-entity-processor
cargo build --release --locked --workspace
container=udc-e2e-fixture-$$
$docker run -d --name "$container" -p "127.0.0.1:$PGPORT:5432" -e POSTGRES_PASSWORD=test postgres:17-alpine >/dev/null
trap '$docker rm -f "$container" >/dev/null' EXIT
until $docker exec "$container" pg_isready -q -U postgres; do sleep 1; done
psql() { $docker exec -i "$container" psql -q -U postgres -d postgres -v ON_ERROR_STOP=1; }
psql < tests/fixture/schema.sql
psql < tests/fixture/fixture.sql

SYNC_DIR="$work/sync" bun --no-env-file arkiv/sync.ts --once --start-after 0 | tee "$work/sync-run1.jsonl"
# The records the sync wrote, for the comparisons: a dry run keeps its work directory.
SYNC_DIR="$work/dry1" bun --no-env-file arkiv/sync.ts --once --start-after 0 --dry-run >/dev/null
psql < udc/e2e/reverify.sql
SYNC_DIR="$work/sync" bun --no-env-file arkiv/sync.ts --once | tee "$work/sync-run2.jsonl"
SYNC_DIR="$work/dry2" bun --no-env-file arkiv/sync.ts --once --start-after 54163146 --dry-run >/dev/null

# v3 hydration of the records, the later record of an address replacing the earlier one.
for n in 1 2; do
  after=$([ $n = 1 ] && echo 0 || echo 54163146)
  rm -rf "$work/v3-$n"
  $bin process -i "$work/dry$n/work/after-$after/records.ndjson" -o "$work/v3-$n" --owner 0x1111111111111111111111111111111111111111 --require-source-complete 2>/dev/null
  $bin hydrate "$work/v3-$n" > "$work/v3-$n.ndjson"
done
python3 -I - "$work" <<'EOF'
import json, sys
work = sys.argv[1]
latest = {}
for n in (1, 2):
    for line in open(f"{work}/v3-{n}.ndjson"):
        latest[json.loads(line)["address"].lower()] = line
open(f"{work}/records.ndjson", "w").write("".join(latest.values()))
EOF
failed=0
while read -r line; do
  address=$(jq -r .address <<< "$line")
  got=$($bin read-arkiv "$address" --format udc --rpc-url "$ARKIV_RPC_URL" --arkiv-chain-id "$ARKIV_CHAIN_ID" --owner "$ENTITY_OWNER" --schema-prefix "$SYNC_SCHEMA_PREFIX" 2>/dev/null)
  if [ "$got" = "$line" ]; then echo "read-arkiv $address: identical"; else echo "read-arkiv $address: DIFFERENT"; failed=1; fi
done < "$work/records.ndjson"

target/release/arkiv-sourcify-server --format udc --rpc-url "$ARKIV_RPC_URL" --arkiv-chain-id "$ARKIV_CHAIN_ID" --owner "$ENTITY_OWNER" \
  --schema-prefix "$SYNC_SCHEMA_PREFIX" --listen "127.0.0.1:$port" > "$work/server.log" 2>&1 &
server=$!
trap 'kill $server 2>/dev/null; $docker rm -f "$container" >/dev/null' EXIT
until curl -sf "http://127.0.0.1:$port/_ops/ready" >/dev/null; do sleep 1; done
UDC_LIVE_SERVER="http://127.0.0.1:$port" UDC_LIVE_RECORDS="$work/records.ndjson" UDC_LIVE_OUT="$work/server" \
  cargo test --release --locked -p arkiv-sourcify-server --test udc live_server -- --ignored || failed=1
exit $failed
