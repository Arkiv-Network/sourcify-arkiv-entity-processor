#!/usr/bin/env bash
# Postgres -> Arkiv sync targets: one sync per Arkiv chain, all reading the same mirror database. See README.md,
# "Postgres -> Arkiv sync". A target NAME is
#   targets/NAME.env   its chain: ARKIV_RPC_URL, ARKIV_CHAIN_ID, optionally ARKIV_WRITER_KEY_FILE and SYNC_* settings
#                      (start from sync-target.env.example); .env holds what all targets share (database, owner, key)
#   data/sync/NAME/    its cursor (state.json), status.json, logs (sync.jsonl, skipped.jsonl) and work files
# and runs as the Compose project arkiv-sync-NAME (sync.compose.yaml).
#
#   pipeline/sync-targets.sh ls                          the targets, their containers and progress
#   pipeline/sync-targets.sh up NAME [--start-after ID]  starts (or recreates, e.g. after an image build) NAME's sync;
#                                                        a new one finds its start on the chain (the seed it was
#                                                        prefilled with), or starts after match ID
#   pipeline/sync-targets.sh down NAME                   stops it; cursor and logs stay
#   pipeline/sync-targets.sh rm NAME                     stops it and moves its file and data to data/sync/_removed/
#   pipeline/sync-targets.sh compose NAME ARGS...        docker compose for NAME, e.g. logs -f, run --rm sync --dry-run
#
# Runs as a user that may use Docker (or with sudo); the data directories belong to PIPELINE_UID.
set -euo pipefail

APP_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TARGETS_DIR=$APP_DIR/targets DATA_DIR=$APP_DIR/data/sync

die() { echo "sync-targets: $*" >&2; exit 1; }
usage() { sed -n '/^#   pipeline/s/^# *//p' "${BASH_SOURCE[0]}" >&2; exit 2; }
# A KEY=value line of an env file (the last one), without surrounding quotes.
env_value() { sed -n "s/^$2=//p" "$1" 2>/dev/null | tail -n 1 | sed -E "s/^\"(.*)\"$/\1/; s/^'(.*)'$/\1/"; }
setting() { local value; value=$(env_value "$TARGETS_DIR/$1.env" "$2"); echo "${value:-$(env_value "$APP_DIR/.env" "$2")}"; }
target_file() {
  [[ $1 =~ ^[a-z0-9][a-z0-9-]{0,39}$ ]] || die "target names are lowercase letters, digits and dashes: $1"
  [[ -f $TARGETS_DIR/$1.env ]] || die "no target $1 (no $TARGETS_DIR/$1.env)"
}
compose_target() {
  local name=$1; shift
  SYNC_TARGET=$name docker compose -f "$APP_DIR/sync.compose.yaml" -p "arkiv-sync-$name" \
    --env-file "$APP_DIR/.env" --env-file "$TARGETS_DIR/$name.env" "$@"
}
# A chain's identity: the hash of block 1 (two chains from one genesis file differ there), else of block 0.
chain_identity() {
  local block hash
  for block in 0x1 0x0; do
    hash=$(curl -s --max-time 10 -H 'content-type: application/json' \
      -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBlockByNumber\",\"params\":[\"$block\",false]}" "$1" | jq -r '.result.hash // empty' 2>/dev/null) || true
    if [[ -n $hash ]]; then echo "$hash"; return; fi
  done
}

# One sync per chain: two would sign with one key and create everything twice.
check_target() {
  local name=$1 rpc chain_id key other identity
  [[ -f $APP_DIR/.env ]] || die "no $APP_DIR/.env"
  for key in ARKIV_RPC_URL ARKIV_CHAIN_ID; do
    [[ -z $(env_value "$APP_DIR/.env" $key) ]] || die "$key is set in .env: chain settings belong in targets/NAME.env"
    [[ -n $(env_value "$TARGETS_DIR/$name.env" $key) ]] || die "set $key in targets/$name.env"
  done
  rpc=$(setting "$name" ARKIV_RPC_URL) chain_id=$(setting "$name" ARKIV_CHAIN_ID)
  key=$(setting "$name" ARKIV_WRITER_KEY_FILE)
  key=$(cd "$APP_DIR" && realpath -m "${key:-./arkiv-writer.key}")
  [[ -r $key ]] || die "the writer key file $key is not readable"
  identity=$(chain_identity "$rpc")
  [[ -n $identity ]] || echo "sync-targets: warning: $rpc did not return block 1 or 0; compared by URL only" >&2
  for other in "$TARGETS_DIR"/*.env; do
    other=$(basename "$other" .env)
    [[ $other == "$name" ]] && continue
    [[ $(setting "$other" ARKIV_RPC_URL) != "$rpc" ]] || die "target $other already syncs $rpc"
    [[ -z $identity || $(setting "$other" ARKIV_CHAIN_ID) != "$chain_id" ]] && continue
    [[ $(chain_identity "$(setting "$other" ARKIV_RPC_URL)") != "$identity" ]] || die "target $other already syncs this chain (block hash $identity) at $(setting "$other" ARKIV_RPC_URL)"
  done
}

data_dir() {
  local dir=$DATA_DIR/$1 uid gid
  uid=$(env_value "$APP_DIR/.env" PIPELINE_UID) gid=$(env_value "$APP_DIR/.env" PIPELINE_GID)
  mkdir -p "$dir"
  if ((EUID == 0)); then chown "${uid:-1000}:${gid:-1000}" "$DATA_DIR" "$dir"
  elif [[ $(stat -c %u "$dir") != "${uid:-1000}" ]]; then die "$dir must belong to PIPELINE_UID ${uid:-1000} (run with sudo)"; fi
}

cmd_ls() {
  local file name status
  printf '%-16s %-9s %-22s %-14s %-11s %-24s %s\n' TARGET CHAIN CONTAINER STATUS CURSOR UPDATED RPC
  for file in "$TARGETS_DIR"/*.env; do
    [[ -f $file ]] || continue
    name=$(basename "$file" .env)
    status=$DATA_DIR/$name/status.json
    printf '%-16s %-9s %-22s %-14s %-11s %-24s %s\n' "$name" "$(setting "$name" ARKIV_CHAIN_ID)" \
      "$(docker ps -a --filter "label=com.docker.compose.project=arkiv-sync-$name" --format '{{.State}}' | head -n 1 | grep . || echo none)" \
      "$(jq -r '.status // "-"' "$status" 2>/dev/null || echo -)" \
      "$(jq -r '.lastMatchId // "-"' "$DATA_DIR/$name/state.json" 2>/dev/null || echo -)" \
      "$(jq -r '.at // "-"' "$status" 2>/dev/null || echo -)" "$(setting "$name" ARKIV_RPC_URL)"
    jq -r 'select(.error) | "  error: \(.error)"' "$status" 2>/dev/null || true
  done
}

cmd_up() {
  local name=$1 start_after='' since
  shift
  while (($#)); do
    case $1 in
      --start-after) [[ ${2:-} =~ ^[0-9]+$ ]] || die "--start-after needs a match ID"; start_after=$2; shift 2 ;;
      *) usage ;;
    esac
  done
  target_file "$name"
  check_target "$name"
  data_dir "$name"
  since=$(date -u +%FT%T.%3NZ)
  if [[ -n $start_after ]]; then
    [[ ! -f $DATA_DIR/$name/state.json ]] || die "data/sync/$name/state.json already holds a cursor; --start-after is for a target's first start"
    compose_target "$name" run --rm sync --once --start-after "$start_after"
  fi
  compose_target "$name" up -d sync
  # Show where it starts: the sync writes status.json once it has its cursor (or found nothing to start from).
  for _ in $(seq 60); do
    if jq -e --arg since "$since" '.at >= $since and (.start != null or .status == "failed" or .status == "syncing" or .status == "caught-up")' \
      "$DATA_DIR/$name/status.json" >/dev/null 2>&1; then
      jq -c '{status, start, error}' "$DATA_DIR/$name/status.json"; return
    fi
    sleep 2
  done
  echo "sync-targets: $name has not reported its start yet; see: pipeline/sync-targets.sh compose $name logs" >&2
}

cmd_rm() {
  local name=$1 archive
  target_file "$name"
  compose_target "$name" down
  archive=$DATA_DIR/_removed/$name-$(date -u +%Y%m%dT%H%M%SZ)
  mkdir -p "$DATA_DIR/_removed"
  if [[ -d $DATA_DIR/$name ]]; then mv "$DATA_DIR/$name" "$archive"; else mkdir "$archive"; fi
  mv "$TARGETS_DIR/$name.env" "$archive/target.env"
  echo "removed $name; its settings, cursor and logs are in $archive"
}

command -v jq >/dev/null || die "jq is required"
case ${1:-} in
  ls) cmd_ls ;;
  up) (($# >= 2)) || usage; shift; cmd_up "$@" ;;
  down) (($# == 2)) || usage; target_file "$2"; compose_target "$2" down ;;
  rm) (($# == 2)) || usage; cmd_rm "$2" ;;
  compose) (($# >= 3)) || usage; target_file "$2"; name=$2; shift 2; compose_target "$name" "$@" ;;
  *) usage ;;
esac
