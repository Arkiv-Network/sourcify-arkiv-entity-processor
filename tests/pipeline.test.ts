/**
 * End-to-end tests on tests/fixture/fixture.sql (real Sourcify rows), loaded into a fresh schema of a test PostgreSQL
 * (tests/fixture/postgres.ts). expected.json holds the output of the original sourcify-arkiv-helper pipeline: legacy
 * records retain their available data, while complete decoded contract payloads now equal each adapter input.
 * Contract identities and bundle hashes deliberately change; the historical whole-bundle checksums are not a
 * payload-parity oracle.
 *
 * Needs a built processor: `cargo build --release` (or PROCESSOR_BIN=/path/to/arkiv-entity-processor), and a
 * PostgreSQL server in the PG* environment variables.
 */
import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { gunzipSync } from 'node:zlib';
import { dirname, join, resolve } from 'node:path';
import { localMatchIdStart, prepareSource } from '../source/prepare';
import { Database } from '../source/database';
import { createFixtureDatabase, fixtureSql, type FixtureDatabase } from './fixture/postgres';

const root = resolve(import.meta.dir, '..');
let fixture: FixtureDatabase;
const expected = JSON.parse(readFileSync(join(root, 'tests/fixture/expected.json'), 'utf8'));
const recoveredIds = ['23813307', '28759260'];
// Skip reasons no longer name internal row IDs (the helper's said "Missing deployed contract: <uuid>").
const remainingSkipped = expected.skipped.filter((item: { matchId: string }) => !recoveredIds.includes(item.matchId))
  .map((item: { matchId: string; reason: string }) => ({ ...item, reason: item.reason.replace(/^(Missing (?:verification|deployment|compilation|deployed contract)): .*$/, '$1') }));
const processor = resolve(process.env.PROCESSOR_BIN ?? join(root, 'target/release/arkiv-entity-processor'));
let scratch: string;

const sha256 = (data: string | Uint8Array) => createHash('sha256').update(data).digest('hex');
// Hermetic: no pipeline settings from the caller's environment (Bun also auto-loads a local .env).
const baseEnv = Object.fromEntries(['PATH', 'HOME', 'TMPDIR', 'LANG'].filter(name => process.env[name]).map(name => [name, process.env[name]!]));
// The test database's libpq settings, for commands that prepare a source.
const pgEnv = () => ({ ...Object.fromEntries(['PGHOST', 'PGPORT', 'PGDATABASE', 'PGUSER', 'PGPASSWORD'].filter(name => process.env[name]).map(name => [name, process.env[name]!])),
  PG_SCHEMA: fixture.database.schema });
const run = (args: string[], env: Record<string, string> = {}) => {
  const result = Bun.spawnSync(args, { cwd: root, env: { ...baseEnv, ...env } });
  return { code: result.exitCode, stdout: result.stdout.toString(), stderr: result.stderr.toString() };
};
const bundleSums = (bundle: string) => Object.fromEntries(readFileSync(join(bundle, 'SHA256SUMS'), 'utf8').trim().split('\n')
  .map(line => line.split('  ')).filter(([, path]) => path !== 'report.json').map(([digest, path]) => [path, digest]));

// Read the actual import wire format, not the auxiliary uncompressed payload file.
function decodeBundle(bundle: string) {
  const entities = readdirSync(join(bundle, 'entities')).sort().flatMap(name =>
    readFileSync(join(bundle, 'entities', name), 'utf8').trim().split('\n').map(line => JSON.parse(line)));
  const decoded = entities.map(entity => {
    const wire = Buffer.from(entity.$payload, 'base64');
    expect(['str:gzip', 'str:identity']).toContain(entity.contentencoding);
    const payload = entity.contentencoding === 'str:gzip' ? gunzipSync(wire) : wire;
    expect(`bytes32:0x${sha256(payload)}`).toBe(entity.payloadsha256);
    return { entity, payload };
  });
  const spills = decoded.filter(({ payload }) => {
    try { return Boolean(JSON.parse(payload.toString()).$spill); } catch { return false; }
  }).length;
  const hydrated = run([processor, 'hydrate', bundle]);
  if (hydrated.code !== 0) throw new Error(hydrated.stderr);
  const contracts = hydrated.stdout.trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  return { entities, contracts, spills };
}

beforeAll(async () => {
  if (!existsSync(processor)) throw new Error(`Processor binary not found at ${processor}: run cargo build --release or set PROCESSOR_BIN`);
  scratch = mkdtempSync(join(tmpdir(), 'entity-processor-test-'));
  fixture = await createFixtureDatabase();
}, 60_000);
afterAll(async () => {
  await fixture?.drop();
  rmSync(scratch, { recursive: true, force: true });
});

describe('source stage', () => {
  let sourceDir: string;
  beforeAll(async () => {
    sourceDir = join(scratch, 'source');
    await prepareSource({ all: true, database: fixture.database, sourceDir, threads: 2, memoryLimit: '1GB' });
  }, 60_000);

  test('prepare selects the Ethereum mainnet matches only and records no password', () => {
    const manifest = JSON.parse(readFileSync(join(sourceDir, 'source-manifest.json'), 'utf8'));
    expect(manifest).toMatchObject({ schema: 'sourcify-arkiv.import-source.v3', status: 'completed', all: true, selectedCount: expected.selectedCount, firstMatchId: '1', database: fixture.database });
    expect(manifest.counts.matches).toBe(String(expected.selectedCount)); // match 100 is on chain 35443
    if (process.env.PGPASSWORD) expect(readFileSync(join(sourceDir, 'source-manifest.json'), 'utf8')).not.toContain(process.env.PGPASSWORD);
  });

  test('prepare reuses an identical selection, also offline, and refuses a different one', async () => {
    expect((await prepareSource({ all: true, database: fixture.database, sourceDir })).status).toBe('completed');
    expect((await prepareSource({ all: true, sourceDir })).status).toBe('completed');
    expect(prepareSource({ limit: 3, database: fixture.database, sourceDir })).rejects.toThrow('different selection');
    expect(prepareSource({ all: true, database: { ...fixture.database, schema: 'other' }, sourceDir })).rejects.toThrow('different selection');
    expect(prepareSource({ all: true, sourceDir: join(scratch, 'never-prepared') })).rejects.toThrow('reads the Sourcify database');
  });

  test('sources prepared from Parquet (v2) are refused', async () => {
    const oldDir = join(scratch, 'old-source');
    const { mkdirSync } = await import('node:fs');
    mkdirSync(oldDir);
    writeFileSync(join(oldDir, 'source-manifest.json'), JSON.stringify({ schema: 'sourcify-arkiv.import-source.v2', status: 'completed', all: true, dataDir: '/parquet', signatures: true }));
    await expect(prepareSource({ all: true, database: fixture.database, sourceDir: oldDir })).rejects.toThrow('outdated');
    expect(run(['bun', 'source/stream.ts', '--source-dir', oldDir]).stderr).toContain('completed sourcify-arkiv.import-source.v3');
  });

  test('matches the node created itself (local IDs) are refused', async () => {
    const local = await createFixtureDatabase(`${fixtureSql()}
      INSERT INTO sourcify_matches (id, verified_contract_id, creation_match, runtime_match, chain_id)
        SELECT ${localMatchIdStart}, v.id, 'perfect', 'perfect', 1 FROM verified_contracts v
        WHERE v.id NOT IN (SELECT verified_contract_id FROM sourcify_matches) ORDER BY v.id LIMIT 1;`);
    try {
      await expect(prepareSource({ all: true, database: local.database, sourceDir: join(scratch, 'local-ids'), threads: 1, memoryLimit: '1GB' }))
        .rejects.toThrow(`1 selected matches have IDs from ${localMatchIdStart} on`);
    } finally { await local.drop(); }
  }, 30_000);

  test('missing signature bodies fail closed or are explicitly reported', async () => {
    const missingDir = join(scratch, 'missing-signature');
    await prepareSource({ limit: 3, database: fixture.database, sourceDir: missingDir, threads: 1, memoryLimit: '1GB' });
    const db = await Database.open(join(missingDir, 'subset.duckdb'), {}, missingDir, 'test-');
    try { await db.connection.run('DELETE FROM signatures'); } finally { await db.close(); }
    const strict = run(['bun', 'source/stream.ts', '--source-dir', missingDir]);
    expect(strict.code).toBe(1);
    expect(strict.stderr).toContain('Missing signature');
    expect(strict.stdout).not.toContain('source-complete');
    const bounded = run(['bun', 'source/stream.ts', '--source-dir', missingDir, '--skip-missing-references']);
    expect(bounded.code).toBe(0);
    const marker = JSON.parse(bounded.stdout.trim().split('\n').at(-1)!);
    expect(marker.skipped.some((item: any) => item.matchId === '1' && item.reason.startsWith('Missing signature:'))).toBe(true);
  });

  test('prepare --limit takes the first matches by ID', async () => {
    const manifest = await prepareSource({ limit: 3, database: fixture.database, sourceDir: join(scratch, 'source-3'), threads: 1, memoryLimit: '1GB' });
    expect(manifest).toMatchObject({ selectedCount: 3, firstMatchId: '1', lastMatchId: '235' });
  });

  test('every real adapter record equals its decoded wire contract payload and carries no database IDs', () => {
    const result = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--skip-missing-references']);
    expect(result.code).toBe(0);
    const envelopes = result.stdout.trim().split('\n').map(line => JSON.parse(line));
    for (const envelope of envelopes.slice(0, -1)) expect(Object.keys(envelope)).toEqual(['record']);
    // The database's internal row IDs (compilations, deployments, contracts, ... are UUIDs) never reach the output.
    const uuids = new Set(fixtureSql().match(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g));
    expect(uuids.size).toBeGreaterThan(50);
    for (const uuid of uuids) expect(result.stdout).not.toContain(uuid);
    const lines = envelopes.map(item => item.record ?? item);
    expect(lines.pop()).toEqual({ type: 'source-complete', records: expected.records + 2, lastMatchId: '28759260', skipped: remainingSkipped });
    expect(lines.find(record => record.matchId === '23813307').metadata).toBeNull();
    const yul = lines.find(record => record.matchId === '28759260');
    expect(yul.compilation.language).toBe('Yul');
    expect(yul.abi).toBeNull();
    const { proxyResolution: _liveProxy, ...capturedApi } = JSON.parse(readFileSync(join(root, 'docs/data/sourcify-response.json'), 'utf8'));
    const snapshotApi = lines.find(record => record.matchId === '607');
    const capturedMetadata = capturedApi.stdJsonOutput.contracts['contracts/proxies/SafeProxy.sol'].SafeProxy.metadata;
    const snapshotMetadata = snapshotApi.stdJsonOutput.contracts['contracts/proxies/SafeProxy.sol'].SafeProxy.metadata;
    // The database's stored metadata text is what the API returns (the Parquet export had spaced text instead).
    expect(snapshotMetadata).toBe(capturedMetadata);
    expect(snapshotApi).toEqual(capturedApi);
    const signed = lines.find(record => record.matchId === '1');
    for (const type of ['function', 'event', 'error']) {
      expect(signed.signatures[type]).toHaveLength(type === 'function' ? 2 : 1);
      expect(signed.signatures[type][0].signatureHash32).toMatch(/^0x[0-9a-f]{64}$/);
      expect(signed.signatures[type][0].signatureHash4).toBe(signed.signatures[type][0].signatureHash32.slice(0, 10));
    }
    expect(signed.signatures.function.map((item: any) => item.signature)).toEqual(['allowance(address,address)', 'transfer(address,uint256)']);
    expect(signed.signatures.function[0].signatureHash32 > signed.signatures.function[1].signatureHash32).toBe(true);
    expect(lines.some(record => Object.values(record.signatures).every((group: any) => group.length === 0))).toBe(true);
    for (const record of lines) {
      expect(record.stdJsonInput).toHaveProperty('settings');
      expect(record.stdJsonOutput).toHaveProperty('contracts');
      expect(record).not.toHaveProperty('sourcifyIds');
    }
    const legacy = lines.filter(record => !recoveredIds.includes(record.matchId)).map(({ signatures, stdJsonInput, stdJsonOutput, ...record }) => record);
    legacy.push({ type: 'source-complete', records: expected.records, lastMatchId: expected.lastMatchId, skipped: expected.skipped });
    const legacyStream = legacy.map(record => JSON.stringify(record) + '\n').join('');
    // API reconstruction deliberately adds null artifact fields and checksummed addresses.
    expect(signed.creationBytecode).toHaveProperty('sourceMap');
    expect(signed.runtimeBytecode).toHaveProperty('immutableReferences');
    const input = join(scratch, 'legacy.ndjson'), bundle = join(scratch, 'legacy-bundle');
    writeFileSync(input, legacyStream);
    const processed = run([processor, 'process', '-i', input, '-o', bundle, '--owner', expected.owner, '--require-source-complete']);
    expect(processed.code).toBe(0);
    expect(decodeBundle(bundle).contracts.sort((a, b) => a.matchId.localeCompare(b.matchId)))
      .toEqual(legacy.slice(0, -1).sort((a, b) => a.matchId.localeCompare(b.matchId)));
    const fullInput = join(scratch, 'complete.ndjson'), fullBundle = join(scratch, 'complete-bundle');
    writeFileSync(fullInput, result.stdout);
    expect(run([processor, 'process', '-i', fullInput, '-o', fullBundle, '--owner', expected.owner, '--require-source-complete']).code).toBe(0);
    const decoded = decodeBundle(fullBundle);
    expect(decoded.contracts.sort((a, b) => a.matchId.localeCompare(b.matchId)))
      .toEqual([...lines].sort((a, b) => a.matchId.localeCompare(b.matchId)));
    expect(decoded.spills).toBeGreaterThan(0); // Large real contracts exercise gzip plus blob reconstruction.
    // Additional compilation fields change compilation identity, but sources/code remain shared.
    const shared = (entities: any[]) => entities.filter(entity => ['str:sourcefile', 'str:code'].includes(entity.kind));
    const currentShared = new Map(shared(decoded.entities).map(entity => [entity.$key, entity]));
    for (const entity of shared(decodeBundle(bundle).entities)) expect(currentShared.get(entity.$key)).toEqual(entity);
  }, 30_000);

  test('stream fails closed on a missing reference unless skipping', () => {
    const result = run(['bun', 'source/stream.ts', '--source-dir', sourceDir]);
    expect(result.code).toBe(1);
    expect(result.stderr).toContain('Source match 15040941: Missing bytecode');
    expect(result.stdout).not.toContain('source-complete');
  });

  test('stream --limit and --max-skipped', () => {
    const limited = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--limit', '2']);
    expect(limited.code).toBe(0);
    expect(limited.stdout.trim().split('\n').map(line => JSON.parse(line).record?.matchId ?? JSON.parse(line).records)).toEqual(['1', '32', 2]);
    const capped = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--skip-missing-references', '--max-skipped', '1']);
    expect(capped.code).toBe(1);
    expect(capped.stderr).toContain('More than 1 records skipped');
  }, 30_000);
});

describe('pipeline/run.sh', () => {
  const env = (workDir: string, extra: Record<string, string> = {}) => ({
    PATH: `${dirname(processor)}:${process.env.PATH}`, PROCESSOR_BIN: processor, WORK_DIR: workDir, ...pgEnv(),
    ENTITY_OWNER: expected.owner, PREPARE_THREADS: '2', PREPARE_MEMORY_LIMIT: '1GB', SOURCE_MEMORY_LIMIT: '1GB', ...extra,
  });

  test('run imports nullable records, then reuses the verified bundle', () => {
    const workDir = join(scratch, 'work');
    Bun.spawnSync(['mkdir', '-p', workDir]);
    const first = run(['bash', 'pipeline/run.sh', 'run'], env(workDir));
    expect(first.code).toBe(0);
    const bundle = join(workDir, 'bundle');
    const sums = bundleSums(bundle);
    const metadata = readFileSync(join(bundle, 'metadata.jsonl'), 'utf8').trim().split('\n').map(line => JSON.parse(line));
    const { contracts } = decodeBundle(bundle);
    const yul = contracts.find(record => record.matchId === '28759260');
    expect(yul.abi).toBeNull();
    expect(yul).not.toHaveProperty('sourcifyIds');
    expect(contracts.some(record => record.matchId === '23813307')).toBe(true);
    expect(run([processor, 'verify', bundle]).code).toBe(0);
    const timings = JSON.parse(readFileSync(join(workDir, 'timings.json'), 'utf8'));
    expect(timings).toMatchObject({ status: 'completed', records: { contracts: expected.records + 2, sourceSkipped: remainingSkipped.length, entities: metadata.length } });
    expect(Object.keys(timings.phases)).toEqual(['prepare', 'process', 'verify']);

    const second = run(['bash', 'pipeline/run.sh', 'run'], env(workDir));
    expect(second.code).toBe(0);
    expect(bundleSums(bundle)).toEqual(sums);
    const phases = JSON.parse(readFileSync(join(workDir, 'timings.json'), 'utf8')).phases;
    expect([phases.prepare.status, phases.process.status, phases.verify.status]).toEqual(['reused', 'reused', 'completed']);

    // Old runs either omit build identity or name a different processor/source build.
    const timingPath = join(workDir, 'timings.json');
    const reusable = readFileSync(timingPath, 'utf8');
    for (const field of ['processorSha256', 'scriptsSha256']) {
      for (const mismatch of [undefined, 'different-build']) {
        const stale = JSON.parse(reusable);
        stale.phases.process.outputConfig[field] = mismatch;
        if (stale.phases.process.completedEarlier) stale.phases.process.completedEarlier.outputConfig[field] = mismatch;
        writeFileSync(timingPath, JSON.stringify(stale));
        const refused = run(['bash', 'pipeline/run.sh', 'run'], env(workDir));
        expect(refused.code).toBe(1);
        expect(refused.stderr).toContain('refusing to reuse');
        expect(bundleSums(bundle)).toEqual(sums);
      }
    }
    writeFileSync(timingPath, reusable);
    expect(run(['bash', 'pipeline/run.sh', 'run'], env(workDir)).code).toBe(0);

    // A bundle made for another owner is never silently reused.
    const other = run(['bash', 'pipeline/run.sh', 'run'], env(workDir, { ENTITY_OWNER: '0x2222222222222222222222222222222222222222' }));
    expect(other.code).toBe(1);
    expect(other.stderr).toContain('refusing to reuse');
  }, 60_000);

  test('prepare reads the database; run then works without database settings', () => {
    const workDir = join(scratch, 'work-split'), fresh = join(scratch, 'work-fresh');
    Bun.spawnSync(['mkdir', '-p', workDir, fresh]);
    const offline = (dir: string) => Object.fromEntries(Object.entries(env(dir)).filter(([name]) => !name.startsWith('PG')));
    const refused = run(['bash', 'pipeline/run.sh', 'run'], offline(fresh));
    expect(refused.code).toBe(1);
    expect(refused.stderr).toContain('needs PGHOST, PGDATABASE and PGUSER');
    const prepared = run(['bash', 'pipeline/run.sh', 'prepare'], env(workDir));
    expect(prepared.code).toBe(0);
    expect(existsSync(join(workDir, 'bundle'))).toBe(false);
    const processed = run(['bash', 'pipeline/run.sh', 'run'], offline(workDir));
    expect(processed.code).toBe(0);
    const timings = JSON.parse(readFileSync(join(workDir, 'timings.json'), 'utf8'));
    expect([timings.phases.prepare.status, timings.phases.process.status, timings.phases.verify.status]).toEqual(['reused', 'completed', 'completed']);
    expect(timings.config.database).toEqual(fixture.database);
    expect(timings.records).toMatchObject({ contracts: expected.records + 2, sourceSkipped: remainingSkipped.length });
  }, 60_000);

  test('pack: the prefill archive holds what prefill loads, with a matching SHA256SUMS', () => {
    const workDir = join(scratch, 'work'), archive = join(scratch, 'prefill.tar.zst'), extracted = join(scratch, 'prefill');
    expect(run([processor, 'pack', join(workDir, 'bundle'), '--output', archive]).code).toBe(0);
    expect(JSON.parse(readFileSync(`${archive}.json`, 'utf8'))).toMatchObject({ contents: 'prefill', files: 4 });
    Bun.spawnSync(['mkdir', '-p', extracted]);
    expect(run(['tar', '-I', 'zstd', '-xf', archive, '-C', extracted]).code).toBe(0);
    expect(Bun.spawnSync(['sha256sum', '-c', '--strict', 'SHA256SUMS'], { cwd: extracted }).exitCode).toBe(0);
    const listed = readFileSync(join(extracted, 'SHA256SUMS'), 'utf8').trim().split('\n').map(line => line.split('  ')[1]).sort();
    expect(listed).toEqual(['entities/000001.jsonl', 'report.json', 'seed.json']);
    for (const path of listed.filter(path => path !== 'report.json')) expect(expected.bundleSha256sums[path]).toBeDefined();
    expect(existsSync(join(extracted, 'payloads.bin')) || existsSync(join(extracted, 'metadata.jsonl'))).toBe(false);
  }, 30_000);

  test('an incomplete bundle is refused, not resumed', () => {
    const workDir = join(scratch, 'work');
    renameSync(join(workDir, 'bundle', 'seed.json'), join(workDir, 'seed.json.moved'));
    const result = run(['bash', 'pipeline/run.sh', 'run'], env(workDir));
    expect(result.code).toBe(1);
    expect(result.stderr).toContain('exists without seed.json');
  }, 30_000);

  test('publish refuses to start without R2 settings', () => {
    const result = run(['bash', 'pipeline/run.sh', 'publish'], env(join(scratch, 'work')));
    expect(result.code).toBe(1);
    expect(result.stderr).toContain('publish needs R2_BUCKET and R2_KEY');
  }, 30_000);
});

describe('lossless sharing on actual wire entities', () => {
  test('different deployments share compilation and preserve unknown fields, exact text and nulls', () => {
    const envelope = JSON.parse(readFileSync(join(root, 'docs/data/processor-input.ndjson'), 'utf8').trim().split('\n')[0]);
    const first = envelope.record ?? envelope;
    first.futureField = { nullValue: null, literalReference: { $ref: 'leave me alone' }, ordered: [3, 1, 2] };
    const second = structuredClone(first);
    second.matchId = '999999999';
    second.address = '0x2222222222222222222222222222222222222222';
    second.runtimeBytecode.onchainBytecode = '0x60026000';
    const input = join(scratch, 'shared.ndjson'), bundle = join(scratch, 'shared-bundle');
    writeFileSync(input, [first, second].map(v => JSON.stringify(v)).join('\n'));
    const result = run([processor, 'process', '-i', input, '-o', bundle, '--part-size', '2']);
    expect(result.code).toBe(0);
    const decoded = decodeBundle(bundle);
    expect(decoded.contracts).toEqual([first, second]);
    expect(decoded.entities.filter(e => e.kind === 'str:compilation')).toHaveLength(1);
    expect(decoded.entities.filter(e => e.kind === 'str:verified_contract')).toHaveLength(2);
    // Hydration must use imported wire data, not the auxiliary debugging payload pack.
    rmSync(join(bundle, 'metadata.jsonl'));
    rmSync(join(bundle, 'payloads.bin'));
    expect(decodeBundle(bundle).contracts).toEqual([first, second]);
    const paths = readdirSync(join(bundle, 'entities')).sort().map(name => join(bundle, 'entities', name));
    const sourcePath = paths.find(path => readFileSync(path, 'utf8').split('\n').some(line => line && JSON.parse(line).kind === 'str:sourcefile'))!;
    const original = readFileSync(sourcePath, 'utf8');
    const entries = original.trim().split('\n').map(line => JSON.parse(line));
    const source = entries.find(e => e.kind === 'str:sourcefile');
    source.$payload = Buffer.from('corrupted source').toString('base64');
    source.contentencoding = 'str:identity';
    writeFileSync(sourcePath, entries.map(e => JSON.stringify(e)).join('\n') + '\n');
    const corrupted = run([processor, 'hydrate', bundle]);
    expect(corrupted.code).not.toBe(0);
    expect(corrupted.stderr).toContain('checksum mismatch');
    writeFileSync(sourcePath, original.trim().split('\n').filter(line => JSON.parse(line).kind !== 'str:sourcefile').join('\n') + '\n');
    const missing = run([processor, 'hydrate', bundle]);
    expect(missing.code).not.toBe(0);
    expect(missing.stderr).toContain('missing');
  });
});
