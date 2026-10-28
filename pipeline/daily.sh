#!/usr/bin/env bash
# Daily seed: a Sourcify mirror's database -> Arkiv prefill seed bundle -> R2, with Docker Compose. See README.md,
# "Daily seed". Runs as a user that may use Docker (systemd: deploy/sourcify-daily-seed.service, and
# deploy/sourcify-daily-seed-udc.service for the v4 seed).
#
#   pipeline/daily.sh                   today's v3 seed (UTC date); rerunning the same day resumes it, a published day is skipped
#   SEED_FORMAT=udc pipeline/daily.sh   today's v4 (UDC) seed, the same way
#
#  1. stops the mirror's sync, so the dataset is the database at one matchId;
#  2. prepare (the only database access) into RUNS_DIR/YYYYMMDD, then starts the sync again (also on any failure or
#     signal). The sync is paused for prepare only (minutes; DAILY_PREPARE_TIMEOUT stops a longer one), never for
#     process (v4: about 6 hours); a same-day resume whose prepare completed does not pause it again, and a sync an
#     earlier run could not start again is started by the next run before anything else;
#  3. run (process -> verify, no network), then publish (pack -> upload PREFIX-YYYYMMDD.tar.zst, read back);
#  4. writes PREFIX-latest.json, deletes the daily seeds it uploaded beyond the newest DAILY_KEEP_REMOTE, and the
#     local runs older than this one.
# PREFIX is R2_KEY_PREFIX for v3 and UDC_R2_KEY_PREFIX for v4. The two must not overlap and each format has its own
# RUNS_DIR, so a v4 seed, pointer or deletion never touches a v3 key (nor the reverse).
# Settings: the processor's .env (ENTITY_OWNER, R2_BUCKET, R2_CREDENTIALS_FILE, PG*, ...) and these variables.
set -euo pipefail

APP_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
MIRROR_DIR=${MIRROR_DIR:?Set MIRROR_DIR to the sourcify-mirror-node directory (its compose.yaml has the sync service)}
# v3 (the default) or udc (v4, docs/udc-proposal.md).
SEED_FORMAT=${SEED_FORMAT:-v3}
# The v3 seed's prefix (production reads R2_KEY_PREFIX-latest.json); a v4 run only checks that it does not overlap it.
R2_KEY_PREFIX=${R2_KEY_PREFIX:-seed-inputs/sourcify-ethereum}
UDC_R2_KEY_PREFIX=${UDC_R2_KEY_PREFIX:-seed-inputs/udc/sourcify-ethereum}
case $SEED_FORMAT in
  v3) prefix=$R2_KEY_PREFIX RUNS_DIR=${RUNS_DIR:-$APP_DIR/data/runs} tag='' ;;
  udc) prefix=$UDC_R2_KEY_PREFIX RUNS_DIR=${RUNS_DIR:-$APP_DIR/data/runs-udc} tag=udc- ;;
  *) printf '{"daily":"failed","error":"SEED_FORMAT must be v3 or udc, not %s"}\n' "$SEED_FORMAT" >&2; exit 2 ;;
esac
# Every seed job pausing this mirror's sync takes this lock first, so one job's restart never ends another's pause;
# both units must therefore run from this one checkout. DAILY_SYNC_LOCK.paused exists while the sync is stopped.
DAILY_SYNC_LOCK=${DAILY_SYNC_LOCK:-$APP_DIR/data/.mirror-sync-pause.lock} DAILY_SYNC_LOCK_WAIT=${DAILY_SYNC_LOCK_WAIT:-7200}
# prepare takes about 6 minutes; one running longer is stopped, and the sync started again.
DAILY_PREPARE_TIMEOUT=${DAILY_PREPARE_TIMEOUT:-3600}
DAILY_KEEP_REMOTE=${DAILY_KEEP_REMOTE:-2}
DAILY_MIN_FREE_GB=${DAILY_MIN_FREE_GB:-500}
DAILY_MEMORY=${DAILY_MEMORY:-40g} DAILY_CPUS=${DAILY_CPUS:-8}
# prepare's index steps need several GB beyond the DuckDB limit (README, "Reference run").
DAILY_PREPARE_MEMORY_LIMIT=${DAILY_PREPARE_MEMORY_LIMIT:-6GB} DAILY_PREPARE_THREADS=${DAILY_PREPARE_THREADS:-4}
AWS_CLI_IMAGE=${AWS_CLI_IMAGE:-amazon/aws-cli:2.31.13}

day=$(date -u +%Y%m%d) started=$(date -u +%FT%TZ) SECONDS=0
work=$RUNS_DIR/$day key=$prefix-$day.tar.zst pointer=$prefix-latest.json
uploaded=$RUNS_DIR/uploaded.tsv status=$RUNS_DIR/status.jsonl paused=$DAILY_SYNC_LOCK.paused
sync_stopped=0 secrets='' failure='' prepare_container=''

log() { printf '{"daily":"%s","day":"%s","at":"%s"%s}\n' "$1" "$day" "$(date -u +%FT%TZ)" "${2:+,$2}"; }
die() { failure=$*; log failed "\"error\":$(jq -Rn --arg e "$*" '$e')" >&2; exit 1; }
env_value() { sed -n "s/^$1=//p" "$APP_DIR/.env" | tail -n 1; }
compose() { (cd "$APP_DIR" && docker compose "$@"); }
# The sync-pause lock, on fd 9 until start_sync.
take_pause_lock() {
  mkdir -p "$(dirname "$DAILY_SYNC_LOCK")"
  exec 9>>"$DAILY_SYNC_LOCK"
  flock -w "$DAILY_SYNC_LOCK_WAIT" 9 ||
    die "another seed job has held $DAILY_SYNC_LOCK (the mirror's sync pause) for over ${DAILY_SYNC_LOCK_WAIT}s"
}
# The prepare container, while one may run: stopped before the sync starts again (a timed-out, signalled or killed
# compose run can leave it reading the database).
stop_prepare() {
  if [[ -n $prepare_container ]]; then
    docker stop --time 60 "$prepare_container" >/dev/null 2>&1 || true
    prepare_container=''
  fi
}
start_sync() {
  if ((sync_stopped)); then
    (cd "$MIRROR_DIR" && docker compose start sync) >/dev/null || return 1
    rm -f "$paused"
    sync_stopped=0
    log sync-started
  fi
  # The pause is over (or was never ours): the next job may pause the sync.
  exec 9>&-
}
on_exit() {
  local code=$?
  # A second signal must not cut the sync's start short.
  trap '' TERM INT
  stop_prepare
  start_sync || true
  [[ -n $secrets ]] && rm -f "$secrets"
  local result=completed; ((code == 0)) || result=failed
  # No status line in a RUNS_DIR that belongs to the other format.
  [[ -n $status ]] || return 0
  mkdir -p "$RUNS_DIR"
  jq -cn --arg day "$day" --arg started "$started" --arg result "$result" --arg key "$key" --argjson seconds "$SECONDS" \
    --arg error "${failure:-}" --arg format "$SEED_FORMAT" '{day: $day, startedAt: $started, result: $result, key: $key, seconds: $seconds} +
    (if $format == "v3" then {} else {format: $format} end) + (if $error == "" then {} else {error: $error} end)' >>"$status" || true
}
trap on_exit EXIT
trap 'failure="interrupted by signal"; exit 143' TERM INT

# A RUNS_DIR serves one format: its uploaded.tsv drives the remote deletions and its old runs are deleted. A RUNS_DIR
# from before SEED_FORMAT is v3's. Checked first, before anything (a lock, a status line) is written to it.
mkdir -p "$RUNS_DIR"
marker=$RUNS_DIR/.seed-format
if [[ ! -f $marker ]]; then
  if [[ $SEED_FORMAT == udc && -n $(find "$RUNS_DIR" -mindepth 1 -maxdepth 1 -print -quit) ]]; then
    status=''; die "RUNS_DIR $RUNS_DIR is not empty and not marked udc: the v4 seed needs its own RUNS_DIR"
  fi
  echo "$SEED_FORMAT" >"$marker"
fi
[[ $(<"$marker") == "$SEED_FORMAT" ]] || { status=''; die "RUNS_DIR $RUNS_DIR belongs to the $(<"$marker") seed, not $SEED_FORMAT"; }

for tool in docker jq flock timeout; do command -v "$tool" >/dev/null || die "$tool is required"; done
[[ -f $APP_DIR/.env ]] || die "no $APP_DIR/.env"
[[ -f $MIRROR_DIR/compose.yaml ]] || die "no compose.yaml in MIRROR_DIR $MIRROR_DIR"
[[ $DAILY_KEEP_REMOTE =~ ^[1-9][0-9]*$ ]] || die "DAILY_KEEP_REMOTE must be a positive integer"
[[ $DAILY_SYNC_LOCK_WAIT =~ ^[0-9]+$ ]] || die "DAILY_SYNC_LOCK_WAIT must be a number of seconds"
[[ $DAILY_PREPARE_TIMEOUT =~ ^[1-9][0-9]*$ ]] || die "DAILY_PREPARE_TIMEOUT must be a positive number of seconds"
[[ -n $R2_KEY_PREFIX && -n $prefix ]] || die "R2_KEY_PREFIX and UDC_R2_KEY_PREFIX must not be empty"
# Every key of a prefix starts with "PREFIX-": when neither "PREFIX-" starts with the other, no v4 key (seed or
# pointer) can equal a v3 one.
if [[ $SEED_FORMAT == udc && ($prefix- == "$R2_KEY_PREFIX-"* || $R2_KEY_PREFIX- == "$prefix-"*) ]]; then
  die "UDC_R2_KEY_PREFIX $prefix overlaps the v3 R2_KEY_PREFIX $R2_KEY_PREFIX"
fi
bucket=$(env_value R2_BUCKET) credentials=$(env_value R2_CREDENTIALS_FILE) uid=$(env_value PIPELINE_UID) gid=$(env_value PIPELINE_GID)
[[ -n $bucket ]] || die "R2_BUCKET is not set in $APP_DIR/.env"
credentials=$(cd "$APP_DIR" && realpath "${credentials:-./r2.env}")
[[ -r $credentials ]] || die "R2 credentials file $credentials is not readable"

schema_prefix=${SCHEMA_PREFIX:-$(env_value SCHEMA_PREFIX)} schema_prefix=${schema_prefix:-sourcify.v4}

exec 8>"$RUNS_DIR/.daily.lock"
flock -n 8 || die "another daily run holds $RUNS_DIR/.daily.lock"
# An earlier run (of either format) that could not start the sync again (its start failed, or it was killed) left
# the marker: start the sync now. Under the lock: a job pausing the sync right now holds it, with its own marker.
if [[ -f $paused ]]; then
  take_pause_lock
  if [[ -f $paused ]]; then
    read -r _ prepare_container _ <"$paused" || true
    [[ $prepare_container =~ ^sourcify-daily-(udc-)?prepare-[0-9]{8}$ ]] || prepare_container=''
    sync_stopped=1
    log sync-left-stopped "\"pausedBy\":$(jq -Rn --arg v "$(<"$paused")" '$v')"
    stop_prepare
  fi
  start_sync || die "could not start the mirror's sync, left stopped by an earlier run (in $MIRROR_DIR: docker compose start sync)"
fi
if [[ -f $work/logs/upload-result.json ]]; then log already-published "\"key\":\"$key\""; exit 0; fi
free_gb=$(df --output=avail -BG "$RUNS_DIR" | tail -n 1 | tr -dc 0-9)
[[ -d $work ]] || ((free_gb >= DAILY_MIN_FREE_GB)) || die "only ${free_gb} GB free in $RUNS_DIR, need $DAILY_MIN_FREE_GB"
mkdir -p "$work"
chown "${uid:-1000}:${gid:-1000}" "$RUNS_DIR" "$work"
# Processing does not resume: an interrupted attempt's bundle (without seed.json) is set aside, then rerun.
if [[ -d $work/bundle && ! -f $work/bundle/seed.json ]]; then mv "$work/bundle" "$work/bundle.failed-$(date -u +%s)"; fi
log start "\"workDir\":\"$work\",\"key\":\"$key\""

# Every compose call gets the run's settings; they override the processor's .env.
export WORK_DIR=$work R2_KEY=$key PIPELINE_MEMORY=$DAILY_MEMORY PIPELINE_CPUS=$DAILY_CPUS PREPARE_LIMIT='' CONTRACT_LIMIT='' \
  PREPARE_MEMORY_LIMIT=$DAILY_PREPARE_MEMORY_LIMIT PREPARE_THREADS=$DAILY_PREPARE_THREADS SEED_FORMAT SCHEMA_PREFIX=$schema_prefix

# The sync is paused for prepare only: a prepared day (same-day resume) does not pause it again.
if ! grep -qs '"status": "completed"' "$work/source/source-manifest.json"; then
  take_pause_lock
  # First: an interrupted or failed stop still starts the sync (and stops the container), on exit or (the marker)
  # in the next run.
  sync_stopped=1 prepare_container=sourcify-daily-${tag}prepare-$day
  printf '%s %s %s\n' "$started" "$prepare_container" "$$" >"$paused"
  (cd "$MIRROR_DIR" && docker compose stop sync) >/dev/null || die "could not stop the mirror's sync"
  log sync-stopped
  # No terminal (-T, no stdin): under timeout's own process group, a terminal read would stop the run.
  (cd "$APP_DIR" && timeout -k 60 "$DAILY_PREPARE_TIMEOUT" docker compose --profile prepare run -T --rm --name "$prepare_container" prepare) \
    </dev/null >>"$work/daily.log" 2>&1 ||
    die "prepare failed or ran over DAILY_PREPARE_TIMEOUT=${DAILY_PREPARE_TIMEOUT}s (see $work/daily.log and $work/logs)"
  prepare_container=''
fi
start_sync || die "could not start the mirror's sync again (in $MIRROR_DIR: docker compose start sync)"
last=$(jq -r .lastMatchId "$work/source/source-manifest.json")
log prepared "\"lastMatchId\":\"$last\",\"selectedCount\":$(jq .selectedCount "$work/source/source-manifest.json")"

# A same-day resume whose bundle is complete and verified only publishes it again: run.sh reuses a bundle only for the
# processor build that made it, which would reprocess the same dataset after a fix elsewhere (e.g. the uploader).
if [[ -f $work/bundle/seed.json ]] && jq -e '.phases.verify.status == "completed"' "$work/timings.json" >/dev/null 2>&1; then
  log verified '"reused":true'
else
  compose run --rm --name "sourcify-daily-${tag}run-$day" pipeline >>"$work/daily.log" 2>&1 || die "process/verify failed (see $work/daily.log and $work/logs)"
  log verified
fi
# The bundle must be of SEED_FORMAT (an image built from an older checkout would make a v3 bundle for a v4 job).
if [[ $SEED_FORMAT == udc ]]; then
  want='.version == 2 and .format == "sourcify-arkiv.udc-bundle.v1"' want_upload='.bundleFormat == "sourcify-arkiv.udc-bundle.v1"'
else
  want='.version == 1' want_upload='.bundleFormat != "sourcify-arkiv.udc-bundle.v1"'
fi
jq -e "$want" "$work/bundle/seed.json" >/dev/null || die "$work/bundle is not a $SEED_FORMAT bundle (rebuild the image from this checkout)"
compose --profile publish run --rm --name "sourcify-daily-${tag}publish-$day" publish >>"$work/daily.log" 2>&1 || die "publish failed (see $work/daily.log and $work/logs)"
jq -e --arg key "$key" ".verifiedRemote == true and (.uri | endswith(\$key)) and $want_upload" "$work/logs/upload-result.json" >/dev/null ||
  die "upload result does not confirm a $SEED_FORMAT seed at s3://$bucket/$key"
printf '%s\t%s\t%s\n' "$day" "$key" "$(jq -r .sha256 "$work/logs/upload-result.json")" >>"$uploaded"
log published "\"key\":\"$key\",\"bytes\":$(jq .bytes "$work/logs/upload-result.json")"

# R2 calls below with the AWS CLI; its env file holds the credentials and is removed on exit.
secrets=$(mktemp)
grep -vE '^\s*(#|$)' "$credentials" | sed -E 's/^export +//; s/^([A-Z0-9_]+)="?([^"]*)"?$/\1=\2/' >"$secrets"
aws() { docker run --rm --memory 512m --cpus 1 --env-file "$secrets" -v "$work:/run:ro" "$AWS_CLI_IMAGE" "$@"; }
# v3's pointer keeps its fields; v4's adds the bundle format and what its entities are tagged and owned by.
format_fields='{}'
if [[ $SEED_FORMAT == udc ]]; then
  format_fields=$(jq -c '{seedFormat: "udc", bundleFormat: .format, seedVersion: .version, schemaPrefix: .schemaPrefix,
    owner: .owner, arkivChainId: .chainId}' "$work/bundle/seed.json") || die "no v4 seed.json in $work/bundle"
fi
jq -n --arg key "$key" --arg day "$day" --arg at "$(date -u +%FT%TZ)" --slurpfile manifest "$work/source/source-manifest.json" \
  --slurpfile timings "$work/timings.json" --slurpfile upload "$work/logs/upload-result.json" --argjson format "$format_fields" \
  '{key: $key, day: $day, publishedAt: $at, lastMatchId: $manifest[0].lastMatchId, selectedCount: $manifest[0].selectedCount,
    contracts: $timings[0].records.contracts, entities: $timings[0].records.entities, bytes: $upload[0].bytes, sha256: $upload[0].sha256}
    + $format' >"$work/latest.json"
aws s3 cp /run/latest.json "s3://$bucket/$pointer" --content-type application/json --only-show-errors ||
  die "could not write s3://$bucket/$pointer"
log latest "\"pointer\":\"$pointer\""

# Only seeds this job uploaded (listed in uploaded.tsv) are ever deleted, oldest first, keeping DAILY_KEEP_REMOTE.
sort -u -k1,1 "$uploaded" -o "$uploaded"
while (($(wc -l <"$uploaded") > DAILY_KEEP_REMOTE)); do
  read -r old_day old_key _ <"$uploaded"
  [[ $old_key == "$prefix-$old_day.tar.zst" && $old_key != "$key" ]] || die "refusing to delete unexpected key $old_key"
  aws s3 rm "s3://$bucket/$old_key" --only-show-errors || die "could not delete s3://$bucket/$old_key"
  sed -i 1d "$uploaded"
  log deleted-remote "\"key\":\"$old_key\""
done
# Local runs: only this one is kept.
for old in "$RUNS_DIR"/[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]; do
  if [[ -d $old && $old != "$work" ]]; then rm -rf "$old"; log deleted-local "\"workDir\":\"$old\""; fi
done
log completed "\"key\":\"$key\",\"lastMatchId\":\"$last\",\"seconds\":$SECONDS"
