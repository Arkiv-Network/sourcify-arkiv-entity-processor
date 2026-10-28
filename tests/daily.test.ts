/**
 * pipeline/daily.sh against a stub `docker` that records every call in order: the mirror's sync is paused for
 * prepare only (started again before process, also after a failed prepare and after a TERM), a same-day resume
 * does not pause it again, and the v4 (SEED_FORMAT=udc) seed publishes under its own prefix, pointer and RUNS_DIR,
 * never touching a v3 key. No Docker, database or network: prepare, process and publish are stubs.
 */
import { afterAll, describe, expect, test } from 'bun:test';
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const root = resolve(import.meta.dir, '..');
const scratch = mkdtempSync(join(tmpdir(), 'daily-test-'));
afterAll(() => rmSync(scratch, { recursive: true, force: true }));
const uid = process.getuid!(), gid = process.getgid!();
const V3 = 'seed-inputs/sourcify-ethereum', UDC = 'seed-inputs/udc/sourcify-ethereum';

// The stub: one line per call (`DIR|ARGS`) in calls.log; STUB_PREPARE=ok|fail|hang, STUB_START, STUB_PROCESS and
// STUB_PUBLISH=ok|fail, STUB_PROCESS_FORMAT (the bundle process makes; default SEED_FORMAT). process also records
// whether the sync-pause lock (STUB_LOCK) is free and its .paused marker gone. Uploads and `aws s3 cp/rm` act on
// the directory STUB_BUCKET (one file per key).
const stub = String.raw`#!/usr/bin/env bash
set -u
echo "$PWD|$*" >>"$STUB_LOG"
case "$*" in
  "compose stop sync") exit 0 ;;
  "compose start sync") [[ ${'${STUB_START:-ok}'} == fail ]] && exit 1; exit 0 ;;
  "stop --time "*) exit 0 ;;
  *"--profile prepare run"*)
    echo "$PWD|prepare-start" >>"$STUB_LOG"
    case ${'${STUB_PREPARE:-ok}'} in
      fail) exit 1 ;;
      hang) echo "$$ $PPID" >"$STUB_LOG.prepare-pid"; exec sleep 60 ;;
    esac
    mkdir -p "$WORK_DIR/source"
    printf '{\n  "status": "completed",\n  "lastMatchId": "42",\n  "selectedCount": 3\n}\n' >"$WORK_DIR/source/source-manifest.json"
    echo "$PWD|prepare-end" >>"$STUB_LOG" ;;
  *" pipeline")
    echo "$PWD|process-start|$SEED_FORMAT|$SCHEMA_PREFIX|$R2_KEY" >>"$STUB_LOG"
    if flock -n "$STUB_LOCK" true; then echo "$PWD|pause-lock-free" >>"$STUB_LOG"; else echo "$PWD|pause-lock-held" >>"$STUB_LOG"; fi
    [[ -e $STUB_LOCK.paused ]] && echo "$PWD|paused-marker" >>"$STUB_LOG"
    [[ ${'${STUB_PROCESS:-ok}'} == fail ]] && exit 1
    mkdir -p "$WORK_DIR/bundle"
    if [[ ${'${STUB_PROCESS_FORMAT:-$SEED_FORMAT}'} == udc ]]; then
      printf '{"version":2,"format":"sourcify-arkiv.udc-bundle.v1","chainId":7706815,"owner":"0x%040d","schemaPrefix":"%s","firstNonce":0,"entities":[]}\n' 1 "$SCHEMA_PREFIX" >"$WORK_DIR/bundle/seed.json"
    else
      echo '{"version":1}' >"$WORK_DIR/bundle/seed.json"
    fi
    echo '{"phases":{"verify":{"status":"completed"}},"records":{"contracts":3,"entities":7}}' >"$WORK_DIR/timings.json" ;;
  *"--profile publish run"*)
    echo "$PWD|publish|$R2_KEY" >>"$STUB_LOG"
    [[ ${'${STUB_PUBLISH:-ok}'} == fail ]] && exit 1
    mkdir -p "$WORK_DIR/logs" "$STUB_BUCKET/$(dirname "$R2_KEY")"
    echo "archive $R2_KEY" >"$STUB_BUCKET/$R2_KEY"
    format=sourcify-arkiv.upload.v1; [[ $SEED_FORMAT == udc ]] && format=sourcify-arkiv.udc-bundle.v1
    printf '{"uri":"s3://test-bucket/%s","bytes":10,"sha256":"%064d","bundleFormat":"%s","verifiedRemote":true}\n' "$R2_KEY" 0 "$format" >"$WORK_DIR/logs/upload-result.json" ;;
  "run --rm --memory"*)
    host=''
    while (($#)); do
      case $1 in
        -v) host=${'${2%%:*}'}; shift 2 ;;
        s3) shift; break ;;
        *) shift ;;
      esac
    done
    case $1 in
      cp) key=${'${3#s3://*/}'}; mkdir -p "$STUB_BUCKET/$(dirname "$key")"; cp "$host/${'${2#/run/}'}" "$STUB_BUCKET/$key" ;;
      rm) key=${'${2#s3://*/}'}; rm "$STUB_BUCKET/$key" ;;
    esac ;;
  *) echo "unexpected docker call: $*" >&2; exit 99 ;;
esac
`;

let count = 0;
/** A fresh APP_DIR holding a copy of pipeline/daily.sh, a .env, R2 credentials, a mirror directory and the stub. */
function setup() {
  const app = join(scratch, `app${++count}`), mirror = join(app, 'mirror'), bin = join(app, 'bin'), bucket = join(app, 'bucket');
  for (const dir of [join(app, 'pipeline'), mirror, bin, bucket]) mkdirSync(dir, { recursive: true });
  copyFileSync(join(root, 'pipeline/daily.sh'), join(app, 'pipeline/daily.sh'));
  chmodSync(join(app, 'pipeline/daily.sh'), 0o755);
  writeFileSync(join(app, '.env'), `R2_BUCKET=test-bucket\nR2_CREDENTIALS_FILE=./r2.env\nPIPELINE_UID=${uid}\nPIPELINE_GID=${gid}\n`);
  writeFileSync(join(app, 'r2.env'), 'AWS_ACCESS_KEY_ID=test\nAWS_SECRET_ACCESS_KEY=test\n');
  writeFileSync(join(mirror, 'compose.yaml'), 'services: {sync: {image: busybox}}\n');
  writeFileSync(join(bin, 'docker'), stub);
  chmodSync(join(bin, 'docker'), 0o755);
  const log = join(app, 'calls.log');
  writeFileSync(log, '');
  const env = (extra: Record<string, string>) => ({ PATH: `${bin}:${process.env.PATH}`, HOME: process.env.HOME ?? app, LANG: 'C',
    MIRROR_DIR: mirror, DAILY_MIN_FREE_GB: '0', STUB_LOG: log, STUB_BUCKET: bucket, STUB_LOCK: join(app, 'data/.mirror-sync-pause.lock'), ...extra });
  return {
    app, mirror, bucket, log, paused: join(app, 'data/.mirror-sync-pause.lock.paused'),
    run(extra: Record<string, string> = {}) {
      const before = readFileSync(log, 'utf8').split('\n').filter(Boolean).length;
      const result = Bun.spawnSync([join(app, 'pipeline/daily.sh')], { cwd: app, env: env(extra) });
      return { code: result.exitCode, stdout: result.stdout.toString(), stderr: result.stderr.toString(), calls: calls(log).slice(before) };
    },
    /** Starts daily.sh as its own process group (systemd's control group), for signalling all of it. */
    spawn(extra: Record<string, string> = {}) {
      return Bun.spawn(['setsid', '--wait', 'bash', '-c', `echo $$ >"${log}.group"; exec "${join(app, 'pipeline/daily.sh')}"`],
        { cwd: app, env: env(extra), stdout: 'pipe', stderr: 'pipe' });
    },
    calls: () => calls(log),
  };
}
/** The recorded calls: "sync-stop", "sync-start", "prepare-start", "prepare-end", "process-start|...", "publish|KEY", ... */
const calls = (log: string) => readFileSync(log, 'utf8').split('\n').filter(Boolean).map(line => {
  const [dir, ...rest] = line.split('|'), args = rest.join('|');
  if (args === 'compose stop sync') return `sync-stop@${dir.endsWith('/mirror') ? 'mirror' : dir}`;
  if (args === 'compose start sync') return `sync-start@${dir.endsWith('/mirror') ? 'mirror' : dir}`;
  if (args.startsWith('compose ')) return `compose ${args.match(/--name (\S+)/)?.[1] ?? args}`;
  if (args.startsWith('stop --time ')) return `container-stop ${args.split(' ').at(-1)}`;
  if (args.startsWith('run --rm --memory')) return `aws ${args.split(/ amazon\/aws-cli:\S+ /)[1]}`;
  return args;
});
const today = () => new Date().toISOString().slice(0, 10).replaceAll('-', '');
const at = (list: string[], item: string) => {
  const index = list.findIndex(call => call === item || call.startsWith(`${item}|`));
  expect(index, `${item} in ${JSON.stringify(list)}`).toBeGreaterThanOrEqual(0);
  return index;
};
const lines = (path: string) => readFileSync(path, 'utf8').trim().split('\n').map(line => JSON.parse(line));
const sleep = (ms: number) => new Promise(done => setTimeout(done, ms));

describe('daily.sh: the sync pause', () => {
  for (const format of ['v3', 'udc']) {
    test(`${format}: the sync is stopped for prepare only, started before process`, () => {
      const t = setup(), day = today(), result = t.run(format === 'udc' ? { SEED_FORMAT: 'udc' } : {});
      expect(result.code, result.stderr).toBe(0);
      const c = result.calls;
      expect(c.filter(call => call.startsWith('sync-'))).toEqual(['sync-stop@mirror', 'sync-start@mirror']);
      expect(at(c, 'sync-stop@mirror')).toBeLessThan(at(c, 'prepare-start'));
      expect(at(c, 'prepare-end')).toBeLessThan(at(c, 'sync-start@mirror'));
      expect(at(c, 'sync-start@mirror')).toBeLessThan(at(c, 'process-start'));
      // During process the pause lock is free (another job may pause the sync) and no pause is recorded.
      expect(c).toContain('pause-lock-free');
      expect(c).not.toContain('paused-marker');
      const tag = format === 'udc' ? 'udc-' : '';
      // prepare has no terminal: under timeout's own process group a terminal read would stop it.
      expect(readFileSync(t.log, 'utf8')).toContain(`|compose --profile prepare run -T --rm --name sourcify-daily-${tag}prepare-${day} prepare\n`);
      expect(c).not.toContain(`container-stop sourcify-daily-${tag}prepare-${day}`);
      expect(c.filter(call => call.startsWith('compose '))).toEqual([`compose sourcify-daily-${tag}prepare-${day}`,
        `compose sourcify-daily-${tag}run-${day}`, `compose sourcify-daily-${tag}publish-${day}`]);
    });
  }

  test('a failed prepare still starts the sync, and nothing is processed', () => {
    const t = setup(), result = t.run({ SEED_FORMAT: 'udc', STUB_PREPARE: 'fail' });
    expect(result.code).toBe(1);
    expect(result.calls.filter(call => !call.startsWith('compose '))).toEqual(['sync-stop@mirror', 'prepare-start',
      `container-stop sourcify-daily-udc-prepare-${today()}`, 'sync-start@mirror']);
    expect(lines(join(t.app, 'data/runs-udc/status.jsonl'))).toMatchObject([{ result: 'failed', format: 'udc', error: expect.stringContaining('prepare failed') }]);
    expect(existsSync(t.paused)).toBe(false);
  });

  test('a prepare running over DAILY_PREPARE_TIMEOUT is stopped, then the sync is started', () => {
    const t = setup(), started = Date.now(), result = t.run({ SEED_FORMAT: 'udc', STUB_PREPARE: 'hang', DAILY_PREPARE_TIMEOUT: '1' });
    expect(result.code).toBe(1);
    expect(Date.now() - started).toBeLessThan(20_000);
    expect(result.stderr).toContain('ran over DAILY_PREPARE_TIMEOUT=1s');
    expect(result.calls.filter(call => !call.startsWith('compose '))).toEqual(['sync-stop@mirror', 'prepare-start',
      `container-stop sourcify-daily-udc-prepare-${today()}`, 'sync-start@mirror']);
  });

  test('a TERM during prepare starts the sync again and exits 143', async () => {
    const t = setup(), child = t.spawn({ SEED_FORMAT: 'udc', STUB_PREPARE: 'hang' });
    for (let i = 0; i < 200 && !existsSync(`${t.log}.prepare-pid`); i++) await sleep(25);
    expect(existsSync(`${t.log}.prepare-pid`)).toBe(true);
    // As systemd stops a unit: TERM to every process of its control group (daily.sh's process group, and `timeout`'s
    // own group, where the prepare stub runs).
    const group = readFileSync(`${t.log}.group`, 'utf8').trim(), [prepare, timeout] = readFileSync(`${t.log}.prepare-pid`, 'utf8').trim().split(' ');
    expect(Bun.spawnSync(['kill', '-TERM', '--', `-${group}`, timeout, prepare]).exitCode).toBe(0);
    expect(await child.exited).toBe(143);
    // The prepare container is stopped before the sync starts again.
    expect(t.calls().filter(call => !call.startsWith('compose '))).toEqual(['sync-stop@mirror', 'prepare-start',
      `container-stop sourcify-daily-udc-prepare-${today()}`, 'sync-start@mirror']);
    expect(lines(join(t.app, 'data/runs-udc/status.jsonl'))).toMatchObject([{ result: 'failed', error: 'interrupted by signal' }]);
    expect(existsSync(t.paused)).toBe(false);
  });

  test('a sync an earlier run could not start again is started by the next run of either format, before anything else', () => {
    const t = setup();
    const failed = t.run({ SEED_FORMAT: 'udc', STUB_START: 'fail' });
    expect(failed.code).toBe(1);
    expect(failed.stderr).toContain('could not start the mirror\'s sync again');
    expect(failed.calls.filter(call => !call.startsWith('compose '))).toEqual(['sync-stop@mirror', 'prepare-start', 'prepare-end',
      'sync-start@mirror', 'sync-start@mirror']);
    expect(existsSync(t.paused)).toBe(true);
    // The v3 job starts it first, then pauses it for its own prepare.
    const v3 = t.run();
    expect(v3.code, v3.stderr).toBe(0);
    expect(v3.stdout).toContain('"daily":"sync-left-stopped"');
    // The earlier run's prepare container is stopped first, then the sync started.
    expect(v3.calls.indexOf(`container-stop sourcify-daily-udc-prepare-${today()}`)).toBe(0);
    expect(v3.calls[1]).toBe('sync-start@mirror');
    expect(v3.calls.filter(call => call.startsWith('sync-') || call.startsWith('prepare-') || call.startsWith('process-start'))
      .map(call => call.split('|')[0])).toEqual(['sync-start@mirror', 'sync-stop@mirror', 'prepare-start', 'prepare-end', 'sync-start@mirror', 'process-start']);
    expect(existsSync(t.paused)).toBe(false);
    // The v4 resume: prepared, nothing left stopped, so no stop and no start.
    const resumed = t.run({ SEED_FORMAT: 'udc' });
    expect(resumed.code, resumed.stderr).toBe(0);
    expect(resumed.calls.filter(call => call.startsWith('sync-') || call === 'prepare-start')).toEqual([]);
    expect(resumed.calls).toContain('pause-lock-free');
  });

  test('a v4 job left stopped and resumed the same day starts the sync before processing', () => {
    const t = setup();
    expect(t.run({ SEED_FORMAT: 'udc', STUB_START: 'fail' }).code).toBe(1);
    const resumed = t.run({ SEED_FORMAT: 'udc' });
    expect(resumed.code, resumed.stderr).toBe(0);
    expect(resumed.calls.filter(call => call.startsWith('sync-') || call === 'prepare-start')).toEqual(['sync-start@mirror']);
    expect(at(resumed.calls, 'sync-start@mirror')).toBeLessThan(at(resumed.calls, 'process-start'));
    expect(resumed.calls).not.toContain('paused-marker');
  });

  test('an early failure leaves the v4 RUNS_DIR usable', () => {
    const t = setup(), empty = join(t.app, 'not-the-mirror');
    mkdirSync(empty);
    const early = t.run({ SEED_FORMAT: 'udc', MIRROR_DIR: empty });
    expect(early.code).toBe(1);
    expect(early.stderr).toContain('no compose.yaml in MIRROR_DIR');
    expect(early.calls).toEqual([]);
    const next = t.run({ SEED_FORMAT: 'udc' });
    expect(next.code, next.stderr).toBe(0);
    expect(lines(join(t.app, 'data/runs-udc/status.jsonl')).map(line => line.result)).toEqual(['failed', 'completed']);
  });

  test('a v4 job refuses to publish a bundle that is not v4 (an image from an older checkout)', () => {
    const t = setup(), result = t.run({ SEED_FORMAT: 'udc', STUB_PROCESS_FORMAT: 'v3' });
    expect(result.code).toBe(1);
    expect(result.stderr).toContain('is not a udc bundle');
    expect(result.calls.some(call => call.startsWith('publish|') || call.startsWith('aws '))).toBe(false);
    expect(existsSync(join(t.bucket, 'seed-inputs'))).toBe(false);
  });

  test('a same-day resume after prepare does not stop the sync again (failed process, then failed publish)', () => {
    const t = setup();
    const first = t.run({ SEED_FORMAT: 'udc', STUB_PROCESS: 'fail' });
    expect(first.code).toBe(1);
    expect(at(first.calls, 'sync-start@mirror')).toBeLessThan(at(first.calls, 'process-start'));
    // The long v4 process runs again, with the sync running throughout.
    const second = t.run({ SEED_FORMAT: 'udc', STUB_PUBLISH: 'fail' });
    expect(second.code).toBe(1);
    expect(second.calls.filter(call => call.startsWith('sync-') || call === 'prepare-start')).toEqual([]);
    expect(second.calls.some(call => call.startsWith('process-start|udc|'))).toBe(true);
    // The verified bundle is only published again.
    const third = t.run({ SEED_FORMAT: 'udc' });
    expect(third.code, third.stderr).toBe(0);
    expect(third.calls.filter(call => call.startsWith('sync-') || call === 'prepare-start' || call.startsWith('process-start'))).toEqual([]);
    expect(third.calls.filter(call => call.startsWith('publish|'))).toEqual([`publish|${UDC}-${today()}.tar.zst`]);
    // A published day is skipped.
    const fourth = t.run({ SEED_FORMAT: 'udc' });
    expect(fourth.code).toBe(0);
    expect(fourth.calls).toEqual([]);
  });

  test('while another job holds the sync pause, a job never stops the sync; it fails after DAILY_SYNC_LOCK_WAIT', async () => {
    const t = setup(), lock = join(t.app, 'data/.mirror-sync-pause.lock');
    mkdirSync(join(t.app, 'data'), { recursive: true });
    // One process holds the lock (no child keeps it after the kill).
    const holder = Bun.spawn(['bash', '-c', `exec 9>>"${lock}"; flock 9; exec sleep 30`]);
    try {
      for (let i = 0; i < 100; i++) {
        await sleep(20);
        if (Bun.spawnSync(['flock', '-n', lock, 'true']).exitCode !== 0) break;
      }
      const result = t.run({ SEED_FORMAT: 'udc', DAILY_SYNC_LOCK_WAIT: '1' });
      expect(result.code).toBe(1);
      expect(result.stderr).toContain('mirror\'s sync pause');
      expect(result.calls).toEqual([]);
    } finally {
      holder.kill();
      await holder.exited;
    }
    const after = t.run({ SEED_FORMAT: 'udc', DAILY_SYNC_LOCK_WAIT: '1' });
    expect(after.code, after.stderr).toBe(0);
    expect(after.calls.filter(call => call.startsWith('sync-'))).toEqual(['sync-stop@mirror', 'sync-start@mirror']);
  });
});

describe('daily.sh: v3 and v4 keys never meet', () => {
  test('v3: the keys, pointer and RUNS_DIR are unchanged', () => {
    const t = setup(), day = today(), result = t.run();
    expect(result.code, result.stderr).toBe(0);
    expect(result.calls.some(call => call.startsWith(`process-start|v3|`))).toBe(true);
    expect(readdirSync(join(t.bucket, 'seed-inputs')).sort()).toEqual([`sourcify-ethereum-${day}.tar.zst`, 'sourcify-ethereum-latest.json']);
    const latest = JSON.parse(readFileSync(join(t.bucket, `${V3}-latest.json`), 'utf8'));
    expect(Object.keys(latest).sort()).toEqual(['bytes', 'contracts', 'day', 'entities', 'key', 'lastMatchId', 'publishedAt', 'selectedCount', 'sha256']);
    expect(latest).toMatchObject({ key: `${V3}-${day}.tar.zst`, day, lastMatchId: '42', selectedCount: 3, contracts: 3, entities: 7 });
    expect(readFileSync(join(t.app, 'data/runs/.seed-format'), 'utf8')).toBe('v3\n');
    expect(lines(join(t.app, 'data/runs/status.jsonl'))[0]).not.toHaveProperty('format');
  });

  test('udc: its own prefix, pointer, RUNS_DIR and deletions; every v3 object and run stays', () => {
    const t = setup(), day = today();
    // A v3 deployment's state: two published seeds, its pointer and a local run.
    const v3Objects = { [`${V3}-20260101.tar.zst`]: 'v3 old', [`${V3}-20260102.tar.zst`]: 'v3 new', [`${V3}-latest.json`]: '{"key":"v3"}' };
    for (const [key, body] of Object.entries(v3Objects)) { mkdirSync(join(t.bucket, 'seed-inputs'), { recursive: true }); writeFileSync(join(t.bucket, key), body); }
    mkdirSync(join(t.app, 'data/runs/20260102'), { recursive: true });
    writeFileSync(join(t.app, 'data/runs/uploaded.tsv'), `20260101\t${V3}-20260101.tar.zst\tx\n20260102\t${V3}-20260102.tar.zst\tx\n`);
    // The v4 job's earlier seeds and an old local run.
    const runs = join(t.app, 'data/runs-udc');
    mkdirSync(join(runs, '20260102'), { recursive: true });
    writeFileSync(join(runs, '.seed-format'), 'udc\n');
    writeFileSync(join(runs, 'uploaded.tsv'), `20260101\t${UDC}-20260101.tar.zst\tx\n20260102\t${UDC}-20260102.tar.zst\tx\n`);
    mkdirSync(join(t.bucket, 'seed-inputs/udc'), { recursive: true });
    for (const old of ['20260101', '20260102']) writeFileSync(join(t.bucket, `${UDC}-${old}.tar.zst`), 'udc old');

    const result = t.run({ SEED_FORMAT: 'udc', SCHEMA_PREFIX: 'sourcify.v4' });
    expect(result.code, result.stderr).toBe(0);
    expect(result.calls.filter(call => call.startsWith('process-start'))).toEqual([`process-start|udc|sourcify.v4|${UDC}-${day}.tar.zst`]);
    expect(result.calls.filter(call => call.startsWith('aws '))).toEqual([
      `aws s3 cp /run/latest.json s3://test-bucket/${UDC}-latest.json --content-type application/json --only-show-errors`,
      `aws s3 rm s3://test-bucket/${UDC}-20260101.tar.zst --only-show-errors`]);
    for (const [key, body] of Object.entries(v3Objects)) expect(readFileSync(join(t.bucket, key), 'utf8')).toBe(body);
    expect(readdirSync(join(t.bucket, 'seed-inputs/udc')).sort()).toEqual([`sourcify-ethereum-${day}.tar.zst`, 'sourcify-ethereum-20260102.tar.zst', 'sourcify-ethereum-latest.json'].sort());
    const latest = JSON.parse(readFileSync(join(t.bucket, `${UDC}-latest.json`), 'utf8'));
    expect(latest).toMatchObject({ key: `${UDC}-${day}.tar.zst`, day, contracts: 3, entities: 7, seedFormat: 'udc', bundleFormat: 'sourcify-arkiv.udc-bundle.v1',
      seedVersion: 2, schemaPrefix: 'sourcify.v4', arkivChainId: 7706815 });
    expect(existsSync(join(t.app, 'data/runs/20260102'))).toBe(true);
    expect(existsSync(join(runs, '20260102'))).toBe(false);
    expect(readFileSync(join(t.app, 'data/runs/uploaded.tsv'), 'utf8').split('\n').filter(Boolean)).toHaveLength(2);
  });

  for (const overlap of [V3, 'seed-inputs/sourcify', `${V3}-udc`, `${V3}-latest.json`]) {
    test(`udc refuses UDC_R2_KEY_PREFIX=${overlap} before touching the sync`, () => {
      const t = setup(), result = t.run({ SEED_FORMAT: 'udc', UDC_R2_KEY_PREFIX: overlap });
      expect(result.code).toBe(1);
      expect(result.stderr).toContain('overlaps the v3 R2_KEY_PREFIX');
      expect(result.calls).toEqual([]);
    });
  }

  test('udc refuses a RUNS_DIR that is v3\'s, without writing to it', () => {
    const t = setup(), runs = join(t.app, 'data/runs');
    mkdirSync(runs, { recursive: true });
    writeFileSync(join(runs, 'uploaded.tsv'), `20260101\t${V3}-20260101.tar.zst\tx\n`);
    writeFileSync(join(runs, 'status.jsonl'), '{"day":"20260101"}\n');
    const unmarked = t.run({ SEED_FORMAT: 'udc', RUNS_DIR: runs });
    expect(unmarked.code).toBe(1);
    expect(unmarked.stderr).toContain('needs its own RUNS_DIR');
    expect(unmarked.calls).toEqual([]);
    expect(readFileSync(join(runs, 'status.jsonl'), 'utf8')).toBe('{"day":"20260101"}\n');
    writeFileSync(join(runs, '.seed-format'), 'v3\n');
    const marked = t.run({ SEED_FORMAT: 'udc', RUNS_DIR: runs });
    expect(marked.code).toBe(1);
    expect(marked.stderr).toContain('belongs to the v3 seed');
    expect(marked.calls).toEqual([]);
    expect(readFileSync(join(runs, 'status.jsonl'), 'utf8')).toBe('{"day":"20260101"}\n');
  });

  test('an unknown SEED_FORMAT is refused', () => {
    const t = setup(), result = t.run({ SEED_FORMAT: 'v4' });
    expect(result.code).toBe(2);
    expect(result.calls).toEqual([]);
    expect(existsSync(join(t.app, 'data'))).toBe(false);
  });
});
