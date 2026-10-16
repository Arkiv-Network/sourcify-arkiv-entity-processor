/**
 * The sync dashboard's pure parts (dashboard/status.ts): env files and settings, sync.jsonl aggregation (also read
 * incrementally), a target's state, cursor and ETA, money figures, the seed's runs, and the sanitizer that keeps secrets
 * and host paths out of the published status.json. No network, no database.
 */
import { afterAll, describe, expect, test } from 'bun:test';
import { appendFileSync, mkdtempSync, renameSync, rmSync, truncateSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { cursorView, databaseError, etaSeconds, formatEth, hourStarts, parseBatch, parseEnv, runwayDays, sanitize, seedRuns, settingOf, SyncFile, SyncLog, syncState, targetSettings } from '../dashboard/status';

const scratch = mkdtempSync(join(tmpdir(), 'entity-processor-dashboard-test-'));
afterAll(() => rmSync(scratch, { recursive: true, force: true }));

const NOW = Date.parse('2026-10-16T18:35:00.000Z');
const ago = (minutes: number) => new Date(NOW - minutes * 60_000).toISOString();
/** A sync.jsonl line as arkiv/sync.ts appends it after a batch. */
const batch = (at: string, fields: Record<string, unknown> = {}) => JSON.stringify({ at, firstMatchId: '100', lastMatchId: '101', selected: 2, entities: 9, created: 4, reused: 5,
  contracts: 2, alreadySynced: 0, transactions: 1, gasUsed: '1000', calldataBytes: 500, seconds: 3, phases: { prepare: 0.4, createContracts: 2 }, skippedContracts: 0, ...fields });

describe('target settings', () => {
  test('KEY=value lines, the last winning, quotes removed; the target file over .env, where empty is unset; the sync\'s defaults', () => {
    const shared = parseEnv('# what all targets share\nENTITY_OWNER=0x73d44f158a6990b50b9951532ea2032983a9fc72\nSYNC_BATCH=50\nSYNC_POLL_SECONDS=30\n');
    const own = parseEnv('ARKIV_RPC_URL="https://rpc.example.net/rpc"\r\nSYNC_BATCH=1\nSYNC_BATCH=2\nSYNC_POLL_SECONDS=\n#SYNC_IN_FLIGHT=9\n export SYNC_MAX_TX_BYTES=1\nSYNC_BATCH_DELAY_SECONDS=\'5\'\n');
    // Like env_value in pipeline/sync-targets.sh: only lines starting KEY= count.
    expect(own).toEqual({ ARKIV_RPC_URL: 'https://rpc.example.net/rpc', SYNC_BATCH: '2', SYNC_POLL_SECONDS: '', SYNC_BATCH_DELAY_SECONDS: '5' });
    const get = settingOf(own, shared);
    expect(get('ENTITY_OWNER')).toBe('0x73d44f158a6990b50b9951532ea2032983a9fc72');
    expect(targetSettings(get)).toEqual({ batch: 2, inFlight: 4, maxTxBytes: 120_000, batchDelaySeconds: 5, pollSeconds: 30 });
    expect(targetSettings(() => undefined)).toEqual({ batch: 200, inFlight: 4, maxTxBytes: 120_000, batchDelaySeconds: 0, pollSeconds: 60 });
    expect(targetSettings(key => key === 'SYNC_BATCH' ? 'many' : undefined).batch).toBeNull();
  });
});

describe('sync.jsonl', () => {
  test('totals of every batch, the last 24 h by hour, rates over the last hour; dry runs and malformed lines are left out', () => {
    const log = new SyncLog();
    for (const line of [
      batch(ago(31 * 60), { transactions: 2, gasUsed: '0' }),      // a dry run (sorbet's first line): planned, nothing sent
      batch(ago(30 * 60), { lastMatchId: '90' }),                   // more than a day ago: totals only
      'not json', '{"at":"yesterday","gasUsed":"1"}', '',
      batch(ago(90), { contracts: 5, selected: 6, gasUsed: '2000', calldataBytes: 700 }),
      batch(ago(30), { contracts: 3, selected: 3, seconds: 6, alreadySynced: 1, skippedContracts: 1 }),
      batch(ago(5), { contracts: 1, selected: 1, lastMatchId: '120', created: 7, reused: 2 }),
    ]) log.add(line, NOW);
    expect(log.ignored).toBe(3);
    const summary = log.summary(NOW);
    expect(summary.totals).toEqual({ since: ago(30 * 60), batches: 4, matches: 12, contracts: 11, alreadySynced: 1, skipped: 1, entitiesCreated: 19, entitiesReused: 17,
      transactions: 4, gasUsed: '5000', calldataBytes: 2200 });
    expect(summary.rates).toEqual({ contractsLastHour: 4, matchesLastHour: 4, contractsLast24h: 9, secondsPerContract: 2.25 });
    expect(summary.gasUsedLast24h).toBe(4000n);
    expect(summary.hourly).toHaveLength(24);
    expect(summary.hourly[0]!.hour).toBe('2026-10-15T19:00:00.000Z');
    expect(summary.hourly.slice(-2)).toEqual([
      { hour: '2026-10-16T17:00:00.000Z', contracts: 5, matches: 6, transactions: 1, gasUsed: '2000', calldataBytes: 700 },
      { hour: '2026-10-16T18:00:00.000Z', contracts: 4, matches: 4, transactions: 2, gasUsed: '2000', calldataBytes: 1000 },
    ]);
    expect(summary.hourly.slice(0, 22).every(bucket => bucket.contracts === 0 && bucket.gasUsed === '0')).toBe(true);
    expect(summary.lastBatch).toEqual({ at: ago(5), matchId: '120', contracts: 1, alreadySynced: 0, created: 7, reused: 2, transactions: 1, calldataBytes: 500, seconds: 3 });
    // A day later only what is still within 24 h counts; the totals stay.
    const later = log.summary(NOW + 23 * 3_600_000);
    expect(later.rates).toEqual({ contractsLastHour: 0, matchesLastHour: 0, contractsLast24h: 4, secondsPerContract: null });
    expect(later.totals.batches).toBe(4);
  });

  test('dry runs: marked (arkiv/sync.ts), or before the mark planned transactions and no gas; a batch that only found its entities is none', () => {
    expect(parseBatch(batch(ago(1), { transactions: 0, gasUsed: '0', contracts: 0, alreadySynced: 2 }))).toMatchObject({ transactions: 0, gasUsed: 0n, alreadySynced: 2 });
    expect(parseBatch(batch(ago(1), { dryRun: true, transactions: 0, gasUsed: '0', contracts: 0, alreadySynced: 2 }))).toBeUndefined();
    expect(parseBatch(batch(ago(1), { dryRun: true }))).toBeUndefined();
    expect(parseBatch(batch(ago(1), { transactions: 3, gasUsed: '0' }))).toBeUndefined();
    expect(parseBatch(batch(ago(1), { gasUsed: '1.5' }))).toBeUndefined();
    expect(parseBatch(batch(ago(1), { lastMatchId: 'x' }))?.matchId).toBeNull();
    expect(hourStarts(NOW).at(-1)).toBe(Date.parse('2026-10-16T18:00:00.000Z'));
  });

  test('the rate covers the current run of batches when it began within the last hour', () => {
    const rate = (minutesAgo: number[]) => {
      const log = new SyncLog();
      for (const minutes of minutesAgo) log.add(batch(ago(minutes), { selected: 10, seconds: 60 }), NOW);
      return log.summary(NOW).matchesPerHour;
    };
    expect(rate([])).toBe(0);
    // Started 15 minutes ago (its first batch took a minute): 40 matches in 16 minutes.
    expect(rate([15, 10, 5, 1])).toBe(150);
    // Running for longer, without a gap: the whole hour.
    expect(rate([70, 55, 40, 25, 10])).toBe(40);
    // Resumed 10 minutes ago after a stop of an hour: only the new run.
    expect(rate([120, 90, 10, 5])).toBe(Math.round(20 * 60 / 11));
    // A single quick batch just now: counted over a minute at least.
    const single = new SyncLog();
    single.add(batch(ago(0.1), { selected: 1, seconds: 0 }), NOW);
    expect(single.summary(NOW).matchesPerHour).toBe(60);
  });

  test('read incrementally: only appended complete lines, again from the start once replaced or truncated', async () => {
    const path = join(scratch, 'sync.jsonl');
    writeFileSync(path, `${batch(ago(10))}\n${batch(ago(9))}\n`);
    const file = new SyncFile(path, 64); // small reads: every line spans several
    await file.update(NOW);
    expect(file.log.totals.batches).toBe(2);
    const third = batch(ago(8), { lastMatchId: '130' });
    appendFileSync(path, third.slice(0, 100)); // a line being written
    await file.update(NOW);
    expect(file.log.totals.batches).toBe(2);
    appendFileSync(path, `${third.slice(100)}\n`);
    await file.update(NOW);
    await file.update(NOW);
    expect(file.log.totals.batches).toBe(3);
    expect(file.log.last?.matchId).toBe('130');
    // Replaced (a target removed and added again), truncated, removed.
    writeFileSync(`${path}.new`, `${batch(ago(1))}\n`); renameSync(`${path}.new`, path);
    await file.update(NOW);
    expect(file.log.totals.batches).toBe(1);
    truncateSync(path, 0);
    await file.update(NOW);
    expect(file.log.totals.batches).toBe(0);
    appendFileSync(path, `${batch(ago(1))}\n`);
    await file.update(NOW);
    expect(file.log.totals.batches).toBe(1);
    rmSync(path);
    await file.update(NOW);
    expect(file.log.totals.batches).toBe(0);
    expect(file.log.last).toBeUndefined();
  });
});

describe('target state, cursor and ETA', () => {
  test('status.json\'s status while sync.lock is fresh; stale once the sync is gone though the status says it runs', () => {
    expect(syncState('syncing', 10_000)).toEqual({ live: true, state: 'syncing' });
    expect(syncState('caught-up', 119_000)).toEqual({ live: true, state: 'caught-up' });
    expect(syncState('syncing', 121_000)).toEqual({ live: false, state: 'stale' });
    expect(syncState('retrying', undefined)).toEqual({ live: false, state: 'stale' });
    expect(syncState('needs-seed', undefined)).toEqual({ live: false, state: 'stale' });
    expect(syncState('stopped', undefined)).toEqual({ live: false, state: 'stopped' });
    expect(syncState('failed', 500_000)).toEqual({ live: false, state: 'failed' });
    // No status.json: a first start or a one-off run (live lock), or nothing at all.
    expect(syncState(undefined, 5_000)).toEqual({ live: true, state: 'starting' });
    expect(syncState(undefined, undefined)).toEqual({ live: false, state: 'stopped' });
    expect(syncState(undefined, 600_000)).toEqual({ live: false, state: 'stale' });
    expect(syncState('<b>x</b>', 1_000).state).toBe('unknown');
    // Hung: the lock still refreshes (a timer), but status.json has not been rewritten for 30 minutes.
    const minutes = (n: number) => n * 60_000;
    for (const status of ['syncing', 'caught-up', 'starting']) expect(syncState(status, 10_000, minutes(31))).toEqual({ live: true, state: 'stale' });
    expect(syncState('syncing', 10_000, minutes(29))).toEqual({ live: true, state: 'syncing' });
    expect(syncState('syncing', 10_000, minutes(50), minutes(90))).toEqual({ live: true, state: 'syncing' }); // a long SYNC_BATCH_DELAY_SECONDS
    expect(syncState('retrying', 10_000, minutes(31))).toEqual({ live: true, state: 'retrying' });
    expect(syncState('needs-seed', 10_000, minutes(31))).toEqual({ live: true, state: 'needs-seed' });
  });

  test('a cursor that moved on since the database query still resolves; one that moved back is unknown', () => {
    const known = { cursor: '100', behind: 5, next: [{ id: '100', address: '0xaa' }, { id: '103', address: '0xbb' }, { id: '107', address: '0xcc' }] };
    expect(cursorView(known, '100')).toEqual({ address: '0xaa', behind: 5 });
    expect(cursorView(known, '107')).toEqual({ address: '0xcc', behind: 3 });
    expect(cursorView(known, '200')).toEqual({ address: null, behind: 3 });
    expect(cursorView(known, '99')).toEqual({ address: null, behind: null });
    expect(cursorView({ ...known, behind: null }, '103')).toEqual({ address: '0xbb', behind: null });
    expect(cursorView(undefined, '100')).toEqual({ address: null, behind: null });
    expect(cursorView(known, undefined)).toEqual({ address: null, behind: null });
    expect(cursorView({ ...known, error: 'mirror database unavailable (timeout)' }, '100')).toEqual({ address: null, behind: null });
  });

  test('a mirror database failure is published as its kind only', () => {
    const libpq = (message: string) => new Error(message);
    for (const [message, published] of [
      ['IO Error: Unable to connect to Postgres: connection to server at "10.0.3.7", port 5432 failed: Connection refused', 'mirror database unavailable (connection refused)'],
      ['IO Error: Unable to connect to Postgres: connection to server at "db" (172.18.0.2), port 5432 failed: timeout expired', 'mirror database unavailable (timeout)'],
      ['Failed to execute query: ERROR: canceling statement due to statement timeout', 'mirror database unavailable (timeout)'],
      ['connection to server at "db", port 5432 failed: FATAL: password authentication failed for user "sourcify_export"', 'mirror database unavailable (authentication failed)'],
      ['could not translate host name "db" to address: Name or service not known', 'mirror database unavailable (host not found)'],
      ['ERROR: permission denied for table sourcify_matches', 'mirror database unavailable (permission denied)'],
      ['server closed the connection unexpectedly', 'mirror database unavailable (connection failed)'],
      ['Binder Error: column "x" does not exist in database "sourcify"', 'mirror database query failed'],
    ]) expect(databaseError(libpq(message!))).toBe(published!);
  });

  test('ETA at the net rate over the mirror\'s inflow', () => {
    expect(etaSeconds(5288, 498, 129, 'syncing', 1)).toBe(Math.round(5288 / 369 * 3600));
    expect(etaSeconds(0, 0, 129, 'syncing', 60)).toBe(0);
    expect(etaSeconds(100, 120, 129, 'syncing', 60)).toBeNull();     // not gaining on the mirror
    expect(etaSeconds(100, 129, 129, 'retrying', 60)).toBeNull();
    expect(etaSeconds(2, 130, 131, 'caught-up', 60)).toBe(60);       // the rest comes with its next poll
    expect(etaSeconds(null, 500, 129, 'syncing', 60)).toBeNull();
    expect(etaSeconds(10, 500, null, 'syncing', 60)).toBeNull();
  });

  test('ETH with 4 decimals, rounded half up; runway in days', () => {
    expect(formatEth(812_441_700_000_000_000_000n)).toBe('812.4417');
    expect(formatEth(0n)).toBe('0.0000');
    expect(formatEth(49_999n * 10n ** 9n)).toBe('0.0000');
    expect(formatEth(50_000n * 10n ** 9n)).toBe('0.0001');
    expect(formatEth(10n ** 18n + 99_995n * 10n ** 9n)).toBe('1.0001');
    expect(formatEth(-15n * 10n ** 17n)).toBe('-1.5000');
    expect(runwayDays(4_999_825_600_000_000_000_000n, 174_400_000_000_000_000n)).toBe(28_669);
    expect(runwayDays(10n ** 18n, 3n * 10n ** 17n)).toBe(3.3);
    expect(runwayDays(10n ** 18n, 0n)).toBeNull();
  });
});

describe('daily seed', () => {
  test('the last run and the last 5 newest first, without key; a run\'s error is never published', () => {
    const runs = [
      ['20261012', 'completed'], ['20261013', 'completed'], ['20261014', 'failed'], ['20261014', 'completed'], ['20261015', 'completed'], ['20261016', 'failed'],
    ].map(([day, result], i) => JSON.stringify({ day, startedAt: `2026-10-${12 + i}T02:00:00Z`, result, key: `seed-inputs/sourcify-ethereum-${day}.tar.zst`, seconds: 7000 + i,
      ...(result === 'failed' ? { error: 'prepare failed: /home/sourcify/data disk full' } : {}) }));
    const seed = seedRuns(`${runs.join('\n')}\nnot json\n`);
    expect(seed.latest).toEqual({ day: '20261016', result: 'failed', startedAt: '2026-10-17T02:00:00Z', seconds: 7005, key: 'seed-inputs/sourcify-ethereum-20261016.tar.zst' });
    expect(seed.recent.map(run => run.seconds)).toEqual([7005, 7004, 7003, 7002, 7001]);
    expect(seed.recent[0]).toEqual({ day: '20261016', result: 'failed', startedAt: '2026-10-17T02:00:00Z', seconds: 7005 });
    expect(JSON.stringify(seed)).not.toContain('disk full');
    expect(seedRuns('')).toEqual({ latest: null, recent: [] });
    // Only values of the expected form.
    const odd = seedRuns(JSON.stringify({ day: '2026-10-16', result: 'Completed <b>', startedAt: 'yesterday', seconds: 'many', key: '../../etc/passwd x' }));
    expect(odd.latest).toEqual({ day: null, result: null, startedAt: null, seconds: null, key: null });
  });
});

describe('error sanitizing', () => {
  const url = 'https://rpc.example.net/sorbet_SECRET123';
  const secrets = { urls: [url, 'https://sourcify-prefill.arkiv-global.net/rpc', undefined] };

  test('an RPC URL with a token in its path never survives: only its hostname does', () => {
    const viem = `HTTP request failed.\n\nStatus: 502\nURL: ${url}\nRequest body: {"method":"eth_blockNumber"}\n\nDetails: Bad Gateway\nVersion: viem@2.56.3`;
    expect(sanitize(viem, secrets)).toBe('HTTP request failed. Status: 502 URL: rpc.example.net Request body: {"method":"eth_blockNumber"} Details: Bad Gateway Version: viem@2.56.3');
    for (const message of [viem, `fetch ${url} failed`, `POST ${url}/ 404`, `connect rpc.example.net/sorbet_SECRET123: refused`, 'token sorbet_SECRET123 rejected',
      url.toUpperCase(), `${'a'.repeat(280)} ${url}`, `{"url":"${url}"}`]) {
      const clean = sanitize(message, secrets);
      expect(clean).not.toContain('SECRET123');
      expect(clean.toLowerCase()).not.toContain('sorbet_');
      expect(clean.length).toBeLessThanOrEqual(300);
    }
    // A URL the sanitizer was not told about is cut to scheme://host, credentials and all.
    for (const message of [`The request took too long to respond. URL: ${url}`, `URL: ${url}?apikey=abc#x`]) {
      expect(sanitize(message)).not.toContain('SECRET123');
      expect(sanitize(message)).toContain('https://rpc.example.net');
    }
    expect(sanitize('see https://user:pw@db.example.org:5432/sourcify?sslmode=require')).toBe('see https://db.example.org:5432');
    expect(sanitize('rejected key abcd1234', { values: ['abcd1234'] })).toBe('rejected key …');
    expect(sanitize('RPC reports chain 1, ARKIV_CHAIN_ID is 7706815', secrets)).toBe('RPC reports chain 1, ARKIV_CHAIN_ID is 7706815');
  });

  test('a token\'s parts, a query key and credentials are masked anywhere; before the text is cut', () => {
    const body = '0123456789abcdef0123456789abcdef', tokenUrl = `https://user:hunter22@rpc.example.net/v1/sorbet_${body}-region-x?key=abcdefghijklmnop&n=1`;
    const known = { urls: [tokenUrl] };
    for (const message of [`invalid key ${body}`, `gateway: ${body.toUpperCase()} unknown`, 'auth abcdefghijklmnop failed', 'login hunter22 refused', `token sorbet_${body}-region-x`]) {
      const clean = sanitize(message, known);
      for (const secret of [body, 'abcdefghijklmnop', 'hunter22', 'sorbet_']) expect(clean.toLowerCase()).not.toContain(secret);
    }
    expect(sanitize('region-x and v1 stay', known)).toBe('region-x and v1 stay');
    // A token straddling the point where the text is cut is masked first, so no part of it remains.
    for (const tail of [`sorbet_${body}-region-x`, `sorbet_${body}`]) {
      const straddling = sanitize(`${' '.repeat(16 * 1024 - 20)}${tail}`, known);
      expect(straddling).not.toContain(body.slice(0, 8));
      if (tail.endsWith('region-x')) expect(straddling).not.toContain('sorbet_');
    }
  });

  test('a megabyte of adversarial text is sanitized in well under 100 ms', () => {
    const megabyte = 1 << 20;
    for (const input of ['a'.repeat(megabyte), 'a.b+'.repeat(megabyte / 4), 'user "'.repeat(megabyte / 6), 'password='.repeat(megabyte / 9), '<a'.repeat(megabyte / 2),
      ' /x'.repeat(megabyte / 3), 'https://'.repeat(megabyte / 8), `${'x'.repeat(megabyte)}${url}`]) {
      const started = performance.now();
      const clean = sanitize(input, secrets);
      expect(performance.now() - started).toBeLessThan(100);
      expect(clean.length).toBeLessThanOrEqual(300);
    }
  });

  test('host paths, database credentials and markup are dropped; at most 300 characters', () => {
    for (const [message, published] of [
      ["ENOENT: no such file or directory, open '/home/sourcify/arkiv-network/sorcify-arkiv-entity-processor/targets/sorbet.env'", "ENOENT: no such file or directory, open '[path]'"],
      ['cannot read the writer key file /app/arkiv-writer.key', 'cannot read the writer key file [path]'],
      ['another sync (7@51658bad01a7) holds /sync/sync.lock, refreshed 3 s ago', 'another sync (7@51658bad01a7) holds [path], refreshed 3 s ago'],
      ['SYNC_DIR=/sync/work failed (/app/arkiv/sync.ts:12:3)', 'SYNC_DIR=[path] failed ([path])'],
      ['IO Error: Cannot open file "/tmp/arkiv-dashboard-x/duckdb.tmp": Permission denied', 'IO Error: Cannot open file "[path]": Permission denied'],
      ['stream.ts exited 1: at ~/.bun/install/cache/x.js | at ./source/stream.ts', 'stream.ts exited 1: at [path] | at [path]'],
      ['path:/runs/20261016/source', 'path:[path]'],
      ['FATAL: password authentication failed for user "sourcify_export"', 'FATAL: password authentication failed for user "…"'],
      ['Unable to connect: host=db port=5432 user=sourcify_export password=hunter22 dbname=sourcify', 'Unable to connect: host=db port=5432 user=… password=… dbname=sourcify'],
      ['contracts/hour and 1/2 stay', 'contracts/hour and 1/2 stay'],
    ]) expect(sanitize(message, secrets)).toBe(published!);
    const page = sanitize('"<html><head><title>502 Bad Gateway</title></head>\\n<body><center><h1>502 Bad Gateway</h1></center></body></html>"');
    expect(page).not.toContain('<');
    expect(page).toContain('502 Bad Gateway');
    const long = sanitize('x'.repeat(1000));
    expect(long).toHaveLength(300);
    expect(long.endsWith('…')).toBe(true);
    expect(sanitize(undefined)).toBe('');
  });
});
