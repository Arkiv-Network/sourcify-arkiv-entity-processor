#!/usr/bin/env bash
# Daily seed: a Sourcify mirror's database -> Arkiv prefill seed bundle -> R2, with Docker Compose. See README.md,
# "Daily seed". Runs as a user that may use Docker (systemd: deploy/sourcify-daily-seed.service).
#
#   pipeline/daily.sh            today's run (UTC date); rerunning the same day resumes it, a published day is skipped
#
#  1. stops the mirror's sync, so the dataset is the database at one matchId;
#  2. prepare (the only database access) into RUNS_DIR/YYYYMMDD, then starts the sync again (also on any failure);
#  3. run (process -> verify, no network), then publish (pack -> upload R2_KEY_PREFIX-YYYYMMDD.tar.zst, read back);
#  4. writes R2_KEY_PREFIX-latest.json, deletes the daily seeds it uploaded beyond the newest DAILY_KEEP_REMOTE, and
#     the local runs older than this one.
# Settings: the processor's .env (ENTITY_OWNER, R2_BUCKET, R2_CREDENTIALS_FILE, PG*, ...) and these variables.
set -euo pipefail

APP_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
MIRROR_DIR=${MIRROR_DIR:?Set MIRROR_DIR to the sourcify-mirror-node directory (its compose.yaml has the sync service)}
RUNS_DIR=${RUNS_DIR:-$APP_DIR/data/runs}
R2_KEY_PREFIX=${R2_KEY_PREFIX:-seed-inputs/sourcify-ethereum}
DAILY_KEEP_REMOTE=${DAILY_KEEP_REMOTE:-2}
DAILY_MIN_FREE_GB=${DAILY_MIN_FREE_GB:-500}
DAILY_MEMORY=${DAILY_MEMORY:-40g} DAILY_CPUS=${DAILY_CPUS:-8}
# prepare's index steps need several GB beyond the DuckDB limit (README, "Reference run").
DAILY_PREPARE_MEMORY_LIMIT=${DAILY_PREPARE_MEMORY_LIMIT:-6GB} DAILY_PREPARE_THREADS=${DAILY_PREPARE_THREADS:-4}
AWS_CLI_IMAGE=${AWS_CLI_IMAGE:-amazon/aws-cli:2.31.13}

day=$(date -u +%Y%m%d) started=$(date -u +%FT%TZ) SECONDS=0
work=$RUNS_DIR/$day key=$R2_KEY_PREFIX-$day.tar.zst
uploaded=$RUNS_DIR/uploaded.tsv status=$RUNS_DIR/status.jsonl
sync_stopped=0 secrets='' failure=''

log() { printf '{"daily":"%s","day":"%s","at":"%s"%s}\n' "$1" "$day" "$(date -u +%FT%TZ)" "${2:+,$2}"; }
die() { failure=$*; log failed "\"error\":$(jq -Rn --arg e "$*" '$e')" >&2; exit 1; }
env_value() { sed -n "s/^$1=//p" "$APP_DIR/.env" | tail -n 1; }
compose() { (cd "$APP_DIR" && docker compose "$@"); }
start_sync() {
  if ((sync_stopped)); then
    (cd "$MIRROR_DIR" && docker compose start sync) >/dev/null && sync_stopped=0 && log sync-started
  fi
}
on_exit() {
  local code=$?
  start_sync || true
  [[ -n $secrets ]] && rm -f "$secrets"
  local result=completed; ((code == 0)) || result=failed
  mkdir -p "$RUNS_DIR"
  jq -cn --arg day "$day" --arg started "$started" --arg result "$result" --arg key "$key" --argjson seconds "$SECONDS" \
    --arg error "${failure:-}" '{day: $day, startedAt: $started, result: $result, key: $key, seconds: $seconds} +
    (if $error == "" then {} else {error: $error} end)' >>"$status" || true
}
trap on_exit EXIT
trap 'failure="interrupted by signal"; exit 143' TERM INT

for tool in docker jq flock; do command -v "$tool" >/dev/null || die "$tool is required"; done
[[ -f $APP_DIR/.env ]] || die "no $APP_DIR/.env"
[[ -f $MIRROR_DIR/compose.yaml ]] || die "no compose.yaml in MIRROR_DIR $MIRROR_DIR"
[[ $DAILY_KEEP_REMOTE =~ ^[1-9][0-9]*$ ]] || die "DAILY_KEEP_REMOTE must be a positive integer"
bucket=$(env_value R2_BUCKET) credentials=$(env_value R2_CREDENTIALS_FILE) uid=$(env_value PIPELINE_UID) gid=$(env_value PIPELINE_GID)
[[ -n $bucket ]] || die "R2_BUCKET is not set in $APP_DIR/.env"
credentials=$(cd "$APP_DIR" && realpath "${credentials:-./r2.env}")
[[ -r $credentials ]] || die "R2 credentials file $credentials is not readable"

mkdir -p "$RUNS_DIR"
exec 8>"$RUNS_DIR/.daily.lock"
flock -n 8 || die "another daily run holds $RUNS_DIR/.daily.lock"
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
  PREPARE_MEMORY_LIMIT=$DAILY_PREPARE_MEMORY_LIMIT PREPARE_THREADS=$DAILY_PREPARE_THREADS

if ! grep -qs '"status": "completed"' "$work/source/source-manifest.json"; then
  (cd "$MIRROR_DIR" && docker compose stop sync) >/dev/null
  sync_stopped=1
  log sync-stopped
  compose --profile prepare run --rm --name "sourcify-daily-prepare-$day" prepare >>"$work/daily.log" 2>&1 ||
    die "prepare failed (see $work/daily.log and $work/logs)"
fi
start_sync
last=$(jq -r .lastMatchId "$work/source/source-manifest.json")
log prepared "\"lastMatchId\":\"$last\",\"selectedCount\":$(jq .selectedCount "$work/source/source-manifest.json")"

# A same-day resume whose bundle is complete and verified only publishes it again: run.sh reuses a bundle only for the
# processor build that made it, which would reprocess the same dataset after a fix elsewhere (e.g. the uploader).
if [[ -f $work/bundle/seed.json ]] && jq -e '.phases.verify.status == "completed"' "$work/timings.json" >/dev/null 2>&1; then
  log verified '"reused":true'
else
  compose run --rm --name "sourcify-daily-run-$day" pipeline >>"$work/daily.log" 2>&1 || die "process/verify failed (see $work/daily.log and $work/logs)"
  log verified
fi
compose --profile publish run --rm --name "sourcify-daily-publish-$day" publish >>"$work/daily.log" 2>&1 || die "publish failed (see $work/daily.log and $work/logs)"
jq -e --arg key "$key" '.verifiedRemote == true and (.uri | endswith($key))' "$work/logs/upload-result.json" >/dev/null ||
  die "upload result does not confirm s3://$bucket/$key"
printf '%s\t%s\t%s\n' "$day" "$key" "$(jq -r .sha256 "$work/logs/upload-result.json")" >>"$uploaded"
log published "\"key\":\"$key\",\"bytes\":$(jq .bytes "$work/logs/upload-result.json")"

# R2 calls below with the AWS CLI; its env file holds the credentials and is removed on exit.
secrets=$(mktemp)
grep -vE '^\s*(#|$)' "$credentials" | sed -E 's/^export +//; s/^([A-Z0-9_]+)="?([^"]*)"?$/\1=\2/' >"$secrets"
aws() { docker run --rm --memory 512m --cpus 1 --env-file "$secrets" -v "$work:/run:ro" "$AWS_CLI_IMAGE" "$@"; }
jq -n --arg key "$key" --arg day "$day" --arg at "$(date -u +%FT%TZ)" --slurpfile manifest "$work/source/source-manifest.json" \
  --slurpfile timings "$work/timings.json" --slurpfile upload "$work/logs/upload-result.json" \
  '{key: $key, day: $day, publishedAt: $at, lastMatchId: $manifest[0].lastMatchId, selectedCount: $manifest[0].selectedCount,
    contracts: $timings[0].records.contracts, entities: $timings[0].records.entities, bytes: $upload[0].bytes, sha256: $upload[0].sha256}' \
  >"$work/latest.json"
aws s3 cp /run/latest.json "s3://$bucket/$R2_KEY_PREFIX-latest.json" --content-type application/json --only-show-errors ||
  die "could not write s3://$bucket/$R2_KEY_PREFIX-latest.json"
log latest "\"pointer\":\"$R2_KEY_PREFIX-latest.json\""

# Only seeds this job uploaded (listed in uploaded.tsv) are ever deleted, oldest first, keeping DAILY_KEEP_REMOTE.
sort -u -k1,1 "$uploaded" -o "$uploaded"
while (($(wc -l <"$uploaded") > DAILY_KEEP_REMOTE)); do
  read -r old_day old_key _ <"$uploaded"
  [[ $old_key == "$R2_KEY_PREFIX-$old_day.tar.zst" && $old_key != "$key" ]] || die "refusing to delete unexpected key $old_key"
  aws s3 rm "s3://$bucket/$old_key" --only-show-errors || die "could not delete s3://$bucket/$old_key"
  sed -i 1d "$uploaded"
  log deleted-remote "\"key\":\"$old_key\""
done
# Local runs: only this one is kept.
for old in "$RUNS_DIR"/[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]; do
  if [[ -d $old && $old != "$work" ]]; then rm -rf "$old"; log deleted-local "\"workDir\":\"$old\""; fi
done
log completed "\"key\":\"$key\",\"lastMatchId\":\"$last\",\"seconds\":$SECONDS"
