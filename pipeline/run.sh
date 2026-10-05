#!/usr/bin/env bash
# Sourcify Parquet export -> Arkiv prefill seed bundle -> (optionally) R2 object.
#
#   run.sh run      prepare -> process -> verify       offline: reads PARQUET_DIR, writes only WORK_DIR
#   run.sh publish  pack -> upload                     needs network and R2 credentials
#   run.sh all      run, then publish
#
# Resumable per phase: rerunning the same command reuses completed phases. Progress is JSON on stdout,
# per-phase logs are in WORK_DIR/logs and WORK_DIR/timings.json records every phase. See README.md.
set -euo pipefail

COMMAND=${1:-run}
[[ $COMMAND =~ ^(run|publish|all)$ ]] || { echo "usage: run.sh [run|publish|all]" >&2; exit 2; }
export PIPELINE_COMMAND=$COMMAND
export APP_DIR=${APP_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
export WORK_DIR=${WORK_DIR:-/work} PARQUET_DIR=${PARQUET_DIR:-/parquet}
export PREPARE_LIMIT=${PREPARE_LIMIT:-} PREPARE_THREADS=${PREPARE_THREADS:-8} PREPARE_MEMORY_LIMIT=${PREPARE_MEMORY_LIMIT:-24GB}
export SOURCE_BATCH_SIZE=${SOURCE_BATCH_SIZE:-250} SOURCE_MEMORY_LIMIT=${SOURCE_MEMORY_LIMIT:-6GB}
export CONTRACT_LIMIT=${CONTRACT_LIMIT:-} SKIP_MISSING_REFERENCES=${SKIP_MISSING_REFERENCES:-1} MAX_SKIPPED=${MAX_SKIPPED:-100000}
export PROCESS_WORKERS=${PROCESS_WORKERS:-4} COMPRESSION_WORKERS=${COMPRESSION_WORKERS:-4}
export DESTINATION_CHAIN_ID=${DESTINATION_CHAIN_ID:-7706815} PROGRESS_INTERVAL=${PROGRESS_INTERVAL:-60}
export ENTITY_OWNER=${ENTITY_OWNER:?Set ENTITY_OWNER to the public owner address of every entity}
export ZSTD_LEVEL=${ZSTD_LEVEL:-3} PACK_THREADS=${PACK_THREADS:-8}
export R2_BUCKET=${R2_BUCKET:-} R2_KEY=${R2_KEY:-} R2_CREDENTIALS_FILE=${R2_CREDENTIALS_FILE:-/run/secrets/r2}
export UPLOAD_CONCURRENCY=${UPLOAD_CONCURRENCY:-8} UPLOAD_PART_SIZE_MIB=${UPLOAD_PART_SIZE_MIB:-64}
PROCESSOR_BIN=${PROCESSOR_BIN:-arkiv-entity-processor}

SOURCE=$WORK_DIR/source BUNDLE=$WORK_DIR/bundle ARCHIVE=$WORK_DIR/archive/seed.tar.zst
LOGS=$WORK_DIR/logs TMP=$WORK_DIR/tmp
initialized=0 current_phase= failure= monitor_pid=

die() { failure=$*; echo "pipeline: $*" >&2; exit 1; }
# --no-env-file: the settings come from this script, never from a .env file in the working directory.
state() { bun --no-env-file "$APP_DIR/pipeline/state.ts" "$@"; }
completed() { [[ -f $1/source-manifest.json ]] && grep -q '"status": "completed"' "$1/source-manifest.json"; }
# Each process attempt gets fresh logs (progress is parsed from them); earlier ones keep a timestamped copy.
rotate() {
  if [[ -s $1 ]]; then mv "$1" "${1%.log}.$(date -u +%Y%m%dT%H%M%SZ).log"; fi
  : >"$1"
}
# The monitor must not hold the lock (fd 9): its last `sleep` can outlive this script by a few seconds.
start_monitor() { "$@" 9>&- & monitor_pid=$!; }
stop_monitor() {
  if [[ -n $monitor_pid ]]; then kill "$monitor_pid" 2>/dev/null || true; wait "$monitor_pid" 2>/dev/null || true; monitor_pid=; fi
}
# Container anonymous memory (cgroup v2 `anon`: process memory without page cache); unavailable outside Docker.
anon_bytes() {
  local key value
  [[ -f /.dockerenv && -r /sys/fs/cgroup/memory.stat ]] || return 0
  while read -r key value; do
    if [[ $key == anon ]]; then echo "$value"; return 0; fi
  done </sys/fs/cgroup/memory.stat
}
# Samples memory every few seconds (peak -> $TMP/memory-peak-PHASE, read into timings.json) and prints a
# progress line every PROGRESS_INTERVAL seconds; for process, with rate and ETA (from the selection size).
monitor() {
  local phase=$1 total=${2:-0} started=$SECONDS next=$((SECONDS + PROGRESS_INTERVAL)) last=0 last_at=$SECONDS
  local sample=$((PROGRESS_INTERVAL < 5 ? PROGRESS_INTERVAL : 5)) anon peak=0
  echo 0 >"$TMP/memory-peak-$phase"
  while sleep "$sample"; do
    anon=$(anon_bytes)
    if [[ -n $anon ]] && ((anon > peak)); then peak=$anon; echo "$peak" >"$TMP/memory-peak-$phase"; fi
    ((SECONDS >= next)) || continue
    next=$((SECONDS + PROGRESS_INTERVAL))
    local now=$SECONDS elapsed=$((SECONDS - started)) memory=null
    if [[ -n $anon ]]; then memory=$((anon >> 20)); fi
    if [[ $phase != process ]]; then
      printf '{"pipeline":"progress","phase":"%s","elapsedSeconds":%d,"anonMemoryMiB":%s}\n' "$phase" "$elapsed" "$memory"
      continue
    fi
    local line contracts=0 skipped window rate recent eta=null percent=null
    line=$(grep -a '"contracts":' "$LOGS/process.log" | tail -n 1 || true)
    if [[ $line =~ \"contracts\":([0-9]+) ]]; then contracts=${BASH_REMATCH[1]}; fi
    skipped=$(grep -ac '"skippedMatchId"' "$LOGS/source.log" || true)
    window=$((now - last_at))
    rate=$((elapsed > 0 ? contracts * 10 / elapsed : 0)) recent=$((window > 0 ? (contracts - last) * 10 / window : 0))
    if ((total > 0)); then percent=$(((contracts + skipped) * 100 / total)); fi
    if ((contracts > 0 && total > 0)); then eta=$(((total - contracts - skipped) * elapsed / contracts)); fi
    printf '{"pipeline":"progress","phase":"process","contracts":%d,"skipped":%d,"total":%d,"percent":%s,"elapsedSeconds":%d,"contractsPerSecond":%d.%d,"recentContractsPerSecond":%d.%d,"etaSeconds":%s,"anonMemoryMiB":%s}\n' \
      "$contracts" "$skipped" "$total" "$percent" "$elapsed" $((rate / 10)) $((rate % 10)) $((recent / 10)) $((recent % 10)) "$eta" "$memory"
    last=$contracts last_at=$now
  done
}
on_exit() {
  local code=$?
  stop_monitor
  ((code != 0)) || return 0
  if ((initialized)); then
    if [[ -n $current_phase ]]; then state end "$current_phase" failed "${failure:-exit status $code}" || true; fi
    state finish failed "${failure:-phase ${current_phase:-setup} exited with status $code}" || true
  fi
  echo "pipeline: FAILED in phase ${current_phase:-setup} (exit $code). Logs: $LOGS. Rerunning the same command resumes; completed phases are reused." >&2
}
trap on_exit EXIT
# With TINI_KILL_PROCESS_GROUP=1 (Compose), `docker stop` also stops the children, so this runs promptly.
trap 'failure="interrupted by signal"; exit 143' TERM INT

[[ $SKIP_MISSING_REFERENCES =~ ^[01]$ ]] || die "SKIP_MISSING_REFERENCES must be 0 or 1"
for number in PREPARE_THREADS SOURCE_BATCH_SIZE PROCESS_WORKERS COMPRESSION_WORKERS PROGRESS_INTERVAL DESTINATION_CHAIN_ID \
  ZSTD_LEVEL PACK_THREADS UPLOAD_CONCURRENCY UPLOAD_PART_SIZE_MIB; do
  [[ ${!number} =~ ^[1-9][0-9]*$ ]] || die "$number must be a positive integer"
done
[[ $MAX_SKIPPED =~ ^[0-9]+$ ]] || die "MAX_SKIPPED must be a nonnegative integer"
for number in PREPARE_LIMIT CONTRACT_LIMIT; do
  [[ -z ${!number} || ${!number} =~ ^[1-9][0-9]*$ ]] || die "$number must be empty or a positive integer"
done
[[ $ENTITY_OWNER =~ ^0x[0-9a-fA-F]{40}$ ]] || die "ENTITY_OWNER must be a 0x-prefixed 20-byte address"
if [[ $COMMAND != publish ]]; then
  for table in code compiled_contracts compiled_contracts_metadata compiled_contracts_sources contract_deployments contracts sources sourcify_matches verified_contracts; do
    compgen -G "$PARQUET_DIR/$table/*.parquet" >/dev/null || die "missing Parquet files: $PARQUET_DIR/$table/*.parquet"
  done
fi
if [[ $COMMAND != run ]]; then
  [[ -n $R2_BUCKET && -n $R2_KEY ]] || die "publish needs R2_BUCKET and R2_KEY"
  [[ -r $R2_CREDENTIALS_FILE ]] || die "publish needs a readable R2 credentials file at $R2_CREDENTIALS_FILE"
fi
[[ -d $WORK_DIR && -w $WORK_DIR ]] || die "WORK_DIR $WORK_DIR must be an existing directory writable by $(id -u):$(id -g)"
PROCESSOR_PATH=$(command -v "$PROCESSOR_BIN") || die "processor binary $PROCESSOR_BIN not found"
export PROCESSOR_PATH
mkdir -p "$LOGS" "$TMP"
# DuckDB spill files of the read-only source go to the work volume, not the container layer.
export TMPDIR=$TMP
exec 9>>"$WORK_DIR/.lock"
flock -n 9 || die "another pipeline is using $WORK_DIR"

phase_prepare() {
  current_phase=prepare
  state start prepare
  local status=completed args=(--parquet-dir "$PARQUET_DIR" --source-dir "$SOURCE" --threads "$PREPARE_THREADS" --memory-limit "$PREPARE_MEMORY_LIMIT")
  if completed "$SOURCE"; then status=reused; else rm -rf "$SOURCE"/sourcify-spill-*; fi # Stale spill of an interrupted attempt (lock held).
  if [[ -n $PREPARE_LIMIT ]]; then args+=(--limit "$PREPARE_LIMIT"); else args+=(--all); fi
  echo "{\"pipelineAttempt\":\"$(date -u +%FT%TZ)\"}" >>"$LOGS/prepare.log"
  start_monitor monitor prepare
  if ! bun --no-env-file "$APP_DIR/source/prepare.ts" "${args[@]}" 2>&1 | tee -a "$LOGS/prepare.log"; then
    die "source preparation failed (see $LOGS/prepare.log). An interrupted preparation resumes on rerun; $SOURCE only accepts its original PREPARE_LIMIT and PARQUET_DIR selection."
  fi
  stop_monitor
  state end prepare "$status"
  current_phase=
}

phase_process() {
  current_phase=process
  state start process
  local total args codes
  if [[ -f $BUNDLE/seed.json ]]; then
    state process-reusable || die "refusing to reuse $BUNDLE: it is not a recorded completed bundle with the current settings. Move it aside to reprocess, or use a fresh WORK_DIR."
    state end process reused "completed bundle exists"
    current_phase=
    return
  fi
  if [[ -e $BUNDLE ]]; then
    die "$BUNDLE exists without seed.json: an earlier attempt did not complete and processing does not resume. Inspect $LOGS/process.log and $LOGS/source.log, move it aside (mv $BUNDLE $BUNDLE.failed-\$(date +%s)) and rerun; the prepared source is reused."
  fi
  completed "$SOURCE" || die "no completed source at $SOURCE"
  total=${CONTRACT_LIMIT:-$(grep -o '"selectedCount": [0-9]*' "$SOURCE/source-manifest.json" | grep -o '[0-9]*$')}
  args=(--source-dir "$SOURCE" --batch-size "$SOURCE_BATCH_SIZE" --memory-limit "$SOURCE_MEMORY_LIMIT")
  if [[ -n $CONTRACT_LIMIT ]]; then args+=(--limit "$CONTRACT_LIMIT"); fi
  if [[ $SKIP_MISSING_REFERENCES == 1 ]]; then args+=(--skip-missing-references --max-skipped "$MAX_SKIPPED"); fi
  rm -rf "$TMP"/sourcify-source-* # Stale spill of an interrupted attempt (lock held).
  state forget process
  rotate "$LOGS/source.log"
  rotate "$LOGS/process.log"
  start_monitor monitor process "$total"
  set +e
  bun --no-env-file "$APP_DIR/source/stream.ts" "${args[@]}" 2>>"$LOGS/source.log" |
    "$PROCESSOR_BIN" process --workers "$PROCESS_WORKERS" --compression-workers "$COMPRESSION_WORKERS" --input - \
      --output "$BUNDLE" --require-source-complete --owner "$ENTITY_OWNER" --destination-chain-id "$DESTINATION_CHAIN_ID" >>"$LOGS/process.log" 2>&1
  codes=("${PIPESTATUS[@]}")
  set -e
  stop_monitor
  if ((codes[0] != 0 || codes[1] != 0)); then
    echo "--- tail of $LOGS/source.log" >&2 && tail -n 5 "$LOGS/source.log" >&2
    echo "--- tail of $LOGS/process.log" >&2 && tail -n 5 "$LOGS/process.log" >&2
    if [[ -f $BUNDLE/seed.json ]]; then
      # Not recorded as completed, so a rerun refuses to reuse it.
      die "pipeline failed (source exit ${codes[0]}, processor exit ${codes[1]}) after the processor published $BUNDLE/seed.json; do not use it: inspect the logs, move it aside and rerun"
    fi
    die "processing failed (source exit ${codes[0]}, processor exit ${codes[1]}); $BUNDLE is incomplete and has no seed.json"
  fi
  [[ -f $BUNDLE/seed.json ]] || die "processor exited 0 without publishing $BUNDLE/seed.json"
  state end process completed
  current_phase=
}

phase_verify() {
  current_phase=verify
  state start verify
  [[ -f $BUNDLE/seed.json ]] || die "no seed.json in $BUNDLE"
  "$PROCESSOR_BIN" verify "$BUNDLE" 2>&1 | tee -a "$LOGS/verify.log" || die "bundle verification failed (see $LOGS/verify.log)"
  state check-counts || die "record counts in $BUNDLE/report.json do not match the source selection"
  state end verify completed
  current_phase=
}

phase_pack() {
  current_phase=pack
  state start pack
  [[ -f $BUNDLE/seed.json ]] || die "no completed bundle at $BUNDLE; run the pipeline first"
  mkdir -p "$(dirname "$ARCHIVE")"
  # Reuses an existing archive whose record matches the bundle; never overwrites a different one.
  start_monitor monitor pack
  "$PROCESSOR_BIN" pack "$BUNDLE" --output "$ARCHIVE" --level "$ZSTD_LEVEL" --threads "$PACK_THREADS" 2>>"$LOGS/pack.log" | tee -a "$LOGS/pack.log" ||
    die "packing failed (see $LOGS/pack.log). Move $ARCHIVE aside if it belongs to another bundle."
  stop_monitor
  state end pack completed
  current_phase=
}

phase_upload() {
  current_phase=upload
  state start upload
  [[ -f $ARCHIVE && -f $ARCHIVE.json ]] || die "no packed archive at $ARCHIVE"
  # Existing identical objects are accepted, different ones refused; the object is read back and checked.
  start_monitor monitor upload
  "$PROCESSOR_BIN" upload "$ARCHIVE" --bucket "$R2_BUCKET" --key "$R2_KEY" --credentials-file "$R2_CREDENTIALS_FILE" \
    --concurrency "$UPLOAD_CONCURRENCY" --part-size-mib "$UPLOAD_PART_SIZE_MIB" 2>>"$LOGS/upload.log" >"$TMP/upload-result.json" ||
    die "upload failed (see $LOGS/upload.log); rerun to retry"
  stop_monitor
  mv "$TMP/upload-result.json" "$LOGS/upload-result.json"
  cat "$LOGS/upload-result.json"
  state end upload completed
  current_phase=
}

state init "$COMMAND"
initialized=1
if [[ $COMMAND != publish ]]; then phase_prepare; phase_process; phase_verify; fi
if [[ $COMMAND != run ]]; then phase_pack; phase_upload; fi
state finish completed
