#!/usr/bin/env bash
# End-to-end test of `pack` and `upload` against a throwaway local MinIO (S3-compatible) server.
#
# Builds a ~50 MB bundle with `process`, packs it and checks the archive layout, reproducibility
# (any --threads) and byte-compatibility with Python's tarfile (PAX), then uploads it in 5 MiB
# parts and checks that:
#   * the object reads back identical, with sha256 metadata and content type application/zstd;
#   * re-uploading the same archive is accepted without uploading (uploaded:false), also with
#     credentials from the environment instead of a file;
#   * a different archive for the same key is refused and the object is left unchanged;
#   * a failed upload (archive does not match its pack record) is aborted, nothing is created;
#   * an upload interrupted with SIGINT is aborted, nothing is created;
#   * an object created by someone else during an upload is not overwritten (If-None-Match: *);
#   * requests failing while the server restarts are retried and the upload completes;
#   * the S3 secret never appears in any output.
#
# Not run in CI. Needs docker (DOCKER="sudo docker" if required), curl >= 7.75 (--aws-sigv4),
# python3, tar with zstd, sha256sum and cmp. Uses dummy credentials of a local container only.
#
#   cargo build --release --locked && DOCKER="sudo docker" tests/minio-upload.sh
#
# Environment: BIN (default target/release/arkiv-entity-processor), DOCKER (default docker),
# MINIO_IMAGE (default cgr.dev/chainguard/minio:latest; the minio/minio images are no longer
# published), NAME (container, default ep-minio), PORT (default 9900), RECORDS (default 300),
# KEEP=1 keeps the work directory.
set -euo pipefail

BIN=$(realpath "${BIN:-target/release/arkiv-entity-processor}")
DOCKER=${DOCKER:-docker}
MINIO_IMAGE=${MINIO_IMAGE:-cgr.dev/chainguard/minio:latest}
NAME=${NAME:-ep-minio}
PORT=${PORT:-9900}
RECORDS=${RECORDS:-300}
ACCESS=testkey
SECRET=testsecret123
ENDPOINT=http://127.0.0.1:$PORT
BUCKET=ep-test
WORK=$(mktemp -d "${TMPDIR:-/tmp}/ep-minio-test.XXXXXX")

cleanup() {
  $DOCKER rm -f "$NAME" >/dev/null 2>&1 || true
  if [[ "${KEEP:-0}" == 1 ]]; then echo "work directory kept: $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$*"; }
ok() { printf '   ok: %s\n' "$*"; }
fail() { printf '   FAIL: %s\n' "$*" >&2; exit 1; }
field() { python3 -c 'import json,sys; v=json.loads(sys.stdin.read().strip().splitlines()[-1])[sys.argv[1]]; print(json.dumps(v) if isinstance(v,bool) else v)' "$1"; }
# Signed S3 request with curl's own SigV4 implementation (independent of ours).
s3() { curl -sS --aws-sigv4 "aws:amz:us-east-1:s3" --user "$ACCESS:$SECRET" -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" "$@"; }
status() { s3 -o /dev/null -w '%{http_code}' -I "$ENDPOINT/$BUCKET/$1"; }
# Number of unfinished multipart uploads for a key.
incomplete() {
  local list
  list=$(s3 "$ENDPOINT/$BUCKET?uploads&prefix=${1//\//%2F}")
  [[ $list == *ListMultipartUploadsResult* ]] || fail "listing multipart uploads: $list"
  grep -o '<UploadId>' <<<"$list" | wc -l
}
# Waits until a background upload has started its parts (its stderr shows the upload phase).
wait_upload_started() {
  for _ in $(seq 600); do
    grep -q '"phase":"upload"' "$1" && return 0
    sleep 0.05
  done
  fail "upload did not start: $(cat "$1")"
}
upload() { "$BIN" upload --credentials-file "$WORK/r2.env" --bucket "$BUCKET" "$@"; }

step "start MinIO ($MINIO_IMAGE) on $ENDPOINT"
$DOCKER rm -f "$NAME" >/dev/null 2>&1 || true
$DOCKER run -d --rm --name "$NAME" -p "127.0.0.1:$PORT:9000" \
  -e MINIO_ROOT_USER=$ACCESS -e MINIO_ROOT_PASSWORD=$SECRET "$MINIO_IMAGE" server /data >/dev/null
for _ in $(seq 100); do
  s3 -X PUT "$ENDPOINT/$BUCKET" >/dev/null 2>&1 || true
  [[ $(s3 -o /dev/null -w '%{http_code}' -I "$ENDPOINT/$BUCKET" 2>/dev/null) == 200 ]] && break
  sleep 0.2
done
[[ $(s3 -o /dev/null -w '%{http_code}' -I "$ENDPOINT/$BUCKET") == 200 ]] || fail "bucket not created"
ok "bucket $BUCKET"
cat >"$WORK/r2.env" <<EOF
# local MinIO test credentials
export AWS_ACCESS_KEY_ID=$ACCESS
AWS_SECRET_ACCESS_KEY="$SECRET"
AWS_ENDPOINT_URL_S3='$ENDPOINT/'
EOF
chmod 600 "$WORK/r2.env"

step "process $RECORDS records into a bundle"
python3 - "$RECORDS" >"$WORK/records.ndjson" <<'EOF'
import base64, json, random, sys
n = int(sys.argv[1])
for i in range(n):
    rng = random.Random(i)
    source = "// " + base64.b64encode(rng.randbytes(75_000)).decode() + f"\ncontract C{i} {{}}"
    print(json.dumps({
        "chainId": "1", "address": "0x%040x" % (i + 1), "matchId": str(i + 1),
        "verifiedAt": "2020-01-01T00:00:00Z", "abi": [],
        "compilation": {"language": "Solidity", "compiler": "solc", "compilerVersion": "1",
                        "compilerSettings": {}, "fullyQualifiedName": f"C{i}.sol:C{i}"},
        "sources": {f"C{i}.sol": {"content": source}},
        "creationBytecode": {"onchainBytecode": None, "recompiledBytecode": None},
        "runtimeBytecode": {"onchainBytecode": "0x6000", "recompiledBytecode": "0x6000"}}))
print(json.dumps({"type": "source-complete", "records": n}))
EOF
"$BIN" process --require-source-complete -i "$WORK/records.ndjson" -o "$WORK/bundle" \
  --owner 0x1111111111111111111111111111111111111111 --part-size 100 2>/dev/null
"$BIN" verify "$WORK/bundle"

step "pack: layout, record, reproducibility, reuse"
mkdir "$WORK/out"
A=$WORK/out/bundle.tar.zst
"$BIN" pack "$WORK/bundle" -o "$A" --threads 1 >"$WORK/pack1.json" 2>"$WORK/pack1.err"
"$BIN" pack "$WORK/bundle" -o "$WORK/out/again.tar.zst" --threads 4 >"$WORK/pack2.json" 2>/dev/null
cat "$WORK/pack1.json"
SHA=$(field sha256 <"$WORK/pack1.json")
SIZE=$(field bytes <"$WORK/pack1.json")
[[ $(sha256sum "$A" | cut -d' ' -f1) == "$SHA" && $(stat -c %s "$A") == "$SIZE" ]] || fail "record does not match archive"
cmp -s "$A" "$WORK/out/again.tar.zst" || fail "archives differ between --threads 1 and 4"
ok "threads 1 and 4 give the identical archive ($SIZE bytes, sha256 $SHA)"
[[ $(field sourceSha256 <"$WORK/pack1.json") == $(sha256sum "$WORK/bundle/SHA256SUMS" | cut -d' ' -f1) ]] || fail "sourceSha256"
cmp -s <(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])),separators=(",",":")))' "$A.json") "$WORK/pack1.json" || fail "record file differs from stdout"
TZ=UTC tar -I zstd -tvf "$A" >"$WORK/listing"
head -n 3 "$WORK/listing" && echo "   ..." && tail -n 3 "$WORK/listing"
tar -I zstd -tf "$A" >"$WORK/list"
grep -qx seed.json "$WORK/list" || fail "seed.json not at the archive root"
(cd "$WORK/bundle" && find . -type f | sed 's|^\./||' | LC_ALL=C sort) | cmp -s - "$WORK/list" || fail "entries are not exactly the sorted bundle files"
grep -v -- '^-rw-r--r-- 0/0 ' "$WORK/listing" && fail "unexpected mode/owner"
grep -v ' 1970-01-01 00:00 ' "$WORK/listing" && fail "unexpected mtime"
ok "every bundle file at the root, sorted, 0644, 0/0, mtime 0"
mkdir "$WORK/extract" && tar -I zstd -xf "$A" -C "$WORK/extract" && (cd "$WORK/extract" && sha256sum --quiet -c SHA256SUMS)
ok "extracted files match SHA256SUMS"
python3 - "$WORK/bundle" "$WORK/python.tar" <<'EOF'
import os, sys, tarfile
root, out = sys.argv[1], sys.argv[2]
paths = sorted(os.path.relpath(os.path.join(d, f), root) for d, _, fs in os.walk(root) for f in fs)
with tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as tar:
    for p in paths:
        info = tar.gettarinfo(os.path.join(root, p), arcname=p)
        info.mode, info.uid, info.gid, info.uname, info.gname, info.mtime = 0o644, 0, 0, "", "", 0
        with open(os.path.join(root, p), "rb") as f:
            tar.addfile(info, f)
EOF
zstd -dc "$A" | cmp -s - "$WORK/python.tar" || fail "tar stream differs from Python tarfile (PAX)"
ok "tar stream byte-identical to Python tarfile PAX output"
"$BIN" pack "$WORK/bundle" -o "$A" --verify-existing >/dev/null 2>"$WORK/reuse.err"
grep -q '"phase":"reused"' "$WORK/reuse.err" || fail "existing archive not reused"
ok "existing archive with matching record reused (exit 0)"

step "upload in 5 MiB parts"
upload "$A" --key seeds/bundle.tar.zst --part-size-mib 5 --concurrency 4 >"$WORK/up1.json" 2>"$WORK/up1.err"
cat "$WORK/up1.json"
[[ $(field uploaded <"$WORK/up1.json") == true && $(field verifiedRemote <"$WORK/up1.json") == true ]] || fail "first upload"
grep '"phase":"upload"' "$WORK/up1.err" | head -n 1
s3 -o "$WORK/download" "$ENDPOINT/$BUCKET/seeds/bundle.tar.zst"
cmp -s "$A" "$WORK/download" || fail "downloaded object differs"
s3 -I "$ENDPOINT/$BUCKET/seeds/bundle.tar.zst" | tr -d '\r' >"$WORK/head"
grep -qix "x-amz-meta-sha256: $SHA" "$WORK/head" || fail "sha256 metadata missing: $(cat "$WORK/head")"
grep -qix "content-type: application/zstd" "$WORK/head" || fail "content type"
ok "object downloaded identical; x-amz-meta-sha256 and content-type set"

step "same archive again => accepted without upload (credentials from the environment)"
AWS_ACCESS_KEY_ID=$ACCESS AWS_SECRET_ACCESS_KEY=$SECRET AWS_ENDPOINT_URL_S3=$ENDPOINT \
  "$BIN" upload "$A" --bucket "$BUCKET" --key seeds/bundle.tar.zst >"$WORK/up2.json" 2>"$WORK/up2.err"
cat "$WORK/up2.json"
[[ $(field uploaded <"$WORK/up2.json") == false && $(field verifiedRemote <"$WORK/up2.json") == true ]] || fail "re-upload"
ok "uploaded:false"

step "different archive, same key => refused"
"$BIN" pack "$WORK/bundle" -o "$WORK/out/level1.tar.zst" --level 1 >/dev/null 2>&1
if upload "$WORK/out/level1.tar.zst" --key seeds/bundle.tar.zst --part-size-mib 5 >"$WORK/up3.json" 2>"$WORK/up3.err"; then
  fail "different archive was accepted"
fi
tail -n 1 "$WORK/up3.err"
grep -q "refusing to overwrite" "$WORK/up3.err" || fail "unexpected error"
s3 -o "$WORK/download2" "$ENDPOINT/$BUCKET/seeds/bundle.tar.zst"
cmp -s "$A" "$WORK/download2" || fail "object changed"
[[ $(incomplete seeds/bundle.tar.zst) == 0 ]] || fail "multipart upload left behind"
ok "refused, object unchanged, no multipart upload started"

step "archive not matching its pack record => parts uploaded, completion refused, aborted"
mkdir "$WORK/bad" && cp "$A" "$WORK/bad/bad.tar.zst"
python3 - "$A.json" "$WORK/bad/bad.tar.zst.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1])); r["archive"] = "bad.tar.zst"; r["sha256"] = "0" * 64
json.dump(r, open(sys.argv[2], "w"))
EOF
if upload "$WORK/bad/bad.tar.zst" --key seeds/bad.tar.zst --part-size-mib 5 >/dev/null 2>"$WORK/up4.err"; then
  fail "mismatching archive was completed"
fi
grep -E 'not completing|"aborted"' "$WORK/up4.err" | cut -c1-200
grep -q '"phase":"aborted"' "$WORK/up4.err" || fail "not aborted"
[[ $(status seeds/bad.tar.zst) == 404 && $(incomplete seeds/bad.tar.zst) == 0 ]] || fail "object or multipart upload left"
if upload "$WORK/bad/bad.tar.zst" --key seeds/bad.tar.zst --verify-local >/dev/null 2>"$WORK/up5.err"; then
  fail "--verify-local accepted a wrong record"
fi
tail -n 1 "$WORK/up5.err" | cut -c1-200
ok "aborted, no object, no multipart upload left"

step "SIGINT during the upload => aborted"
truncate -s 2G "$WORK/big.tar.zst" # sparse file: fast to read, slow enough to interrupt
# Started directly (not via the shell function) so that $! is the uploader's PID.
"$BIN" upload --credentials-file "$WORK/r2.env" --bucket "$BUCKET" "$WORK/big.tar.zst" \
  --key seeds/big.tar.zst --part-size-mib 5 --concurrency 1 >/dev/null 2>"$WORK/up6.err" &
pid=$!
wait_upload_started "$WORK/up6.err"
sleep 0.3
kill -INT $pid
if wait $pid; then fail "interrupted upload succeeded"; fi
grep -E '"interrupted"|"aborted"|^Error' "$WORK/up6.err" | cut -c1-200
grep -q '"phase":"aborted"' "$WORK/up6.err" || fail "not aborted"
[[ $(status seeds/big.tar.zst) == 404 && $(incomplete seeds/big.tar.zst) == 0 ]] || fail "object or multipart upload left"
ok "aborted, no object, no multipart upload left"

step "object created by someone else during the upload => not overwritten"
"$BIN" upload --credentials-file "$WORK/r2.env" --bucket "$BUCKET" "$WORK/big.tar.zst" \
  --key seeds/race.tar.zst --part-size-mib 5 --concurrency 1 >/dev/null 2>"$WORK/up7.err" &
pid=$!
wait_upload_started "$WORK/up7.err"
echo "competing object" >"$WORK/competing"
s3 -T "$WORK/competing" "$ENDPOINT/$BUCKET/seeds/race.tar.zst" >/dev/null
if wait $pid; then fail "upload over a concurrently created object succeeded"; fi
grep -E 'Complete|refusing|"aborted"' "$WORK/up7.err" | cut -c1-240
s3 -o "$WORK/download3" "$ENDPOINT/$BUCKET/seeds/race.tar.zst"
cmp -s "$WORK/competing" "$WORK/download3" || fail "competing object was overwritten"
[[ $(incomplete seeds/race.tar.zst) == 0 ]] || fail "multipart upload left"
ok "competing object kept, upload aborted"

step "MinIO restarted during an upload => requests retried, upload completes"
"$BIN" upload --credentials-file "$WORK/r2.env" --bucket "$BUCKET" "$WORK/big.tar.zst" \
  --key seeds/retry.tar.zst --part-size-mib 5 --concurrency 2 >"$WORK/up8.json" 2>"$WORK/up8.err" &
pid=$!
wait_upload_started "$WORK/up8.err"
sleep 0.5
$DOCKER restart -t 1 "$NAME" >/dev/null
wait $pid || fail "upload did not survive the restart: $(tail -n 3 "$WORK/up8.err")"
grep '"retry"' "$WORK/up8.err" | head -n 2 | cut -c1-200
grep -q '"retry"' "$WORK/up8.err" || fail "no request was retried (restart too late?)"
[[ $(field uploaded <"$WORK/up8.json") == true && $(field verifiedRemote <"$WORK/up8.json") == true ]] || fail "upload after restart"
ok "$(grep -c '"retry"' "$WORK/up8.err") retried request(s); object complete and verified"

step "no secret in any output"
if grep -l -- "$SECRET" "$WORK"/*.err "$WORK"/*.json; then fail "secret found in output"; fi
ok "secret not found in $(ls "$WORK"/*.err "$WORK"/*.json | wc -l) captured outputs"

printf '\nALL PASSED\n'
