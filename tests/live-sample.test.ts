import { afterAll, beforeAll, expect, test } from 'bun:test';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { prepareSource } from '../source/prepare';
import { iterateRecords, openSource } from '../source/records';
import { sampleSource, selectMatchIds, type SampleManifest } from './live/sample';

const root = resolve(import.meta.dir, '..');
const hash = (algorithm: string, bytes: string | Uint8Array) => createHash(algorithm).update(bytes).digest('hex');
let scratch: string, sourceDir: string;
let matchIds: string[], successfulSeed: string, failingSeed: string;
const reconstructed = new Map<string, { record?: any; sourcifyIds?: any; error?: string }>();
let initialDatabase: { sha256: string; mtimeMs: number; size: number };
let initialSourceManifest: Buffer;
function expectedIds(seed: string, count: number) {
  return [...matchIds].sort((a, b) => hash('md5', `${seed}:${a}`).localeCompare(hash('md5', `${seed}:${b}`))
    || (BigInt(a) < BigInt(b) ? -1 : BigInt(a) > BigInt(b) ? 1 : 0)).slice(0, count);
}
function seedWhere(count: number, predicate: (ids: string[]) => boolean) {
  for (let i = 0; i < 1000; i++) {
    const seed = `sampler-fixture-${i}`;
    if (predicate(expectedIds(seed, count))) return seed;
  }
  throw new Error('No suitable deterministic fixture seed');
}
async function lines(output: string) {
  return (await readFile(join(output, 'records.ndjson'), 'utf8')).trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
}
async function manifestAt(output: string): Promise<SampleManifest> {
  return JSON.parse(await readFile(join(output, 'sample.json'), 'utf8'));
}
beforeAll(async () => {
  scratch = await mkdtemp(join(tmpdir(), 'source-live-sample-test-'));
  sourceDir = join(scratch, 'source');
  await prepareSource({ all: true, parquetDir: join(root, 'tests/fixture/parquet'), sourceDir, threads: 1, memoryLimit: '1GB' });
  const source = await openSource(sourceDir, '1GB');
  try {
    matchIds = (await source.db.rows('SELECT id::VARCHAR AS id FROM matches ORDER BY id')).map(row => String(row.id));
    for await (const item of iterateRecords(source)) reconstructed.set(item.matchId, item);
  } finally { await source.close(); }
  successfulSeed = seedWhere(5, ids => ids.every(id => !reconstructed.get(id)?.error));
  failingSeed = seedWhere(1, ids => Boolean(reconstructed.get(ids[0])?.error));
  const dbStat = await stat(join(sourceDir, 'subset.duckdb'));
  initialDatabase = { sha256: hash('sha256', await readFile(join(sourceDir, 'subset.duckdb'))), mtimeMs: dbStat.mtimeMs, size: dbStat.size };
  initialSourceManifest = await readFile(join(sourceDir, 'source-manifest.json'));
}, 60_000);
afterAll(async () => { if (scratch) await rm(scratch, { recursive: true, force: true }); });

test('hash selection is reproducible, distinct and independently matches the entire population', async () => {
  const source = await openSource(sourceDir, '1GB');
  try {
    const seeds = ['not-a-low-id-limit', "literal'; DROP TABLE matches; --"];
    for (const seed of seeds) {
      const selected = await selectMatchIds(source, seed, 10);
      expect(selected.map(item => item.matchId)).toEqual(expectedIds(seed, 10));
      expect(new Set(selected.map(item => item.matchId)).size).toBe(10);
      expect(selected).toEqual(await selectMatchIds(source, seed, 10));
      expect(selected.map(item => item.matchId)).not.toEqual(matchIds.slice(0, 10));
    }
    expect(await selectMatchIds(source, seeds[0], 10)).not.toEqual(await selectMatchIds(source, seeds[1], 10));
  } finally { await source.close(); }
});

test('default five records reproduce existing reconstruction and preserve source provenance read-only', async () => {
  const first = join(scratch, 'first'), second = join(scratch, 'second');
  const one = await sampleSource({ sourceDir, output: first, seed: successfulSeed });
  const two = await sampleSource({ sourceDir, output: second, seed: successfulSeed, memoryLimit: '1GB' });
  expect(one).toMatchObject({ schema: 'sourcify-arkiv.live-sample.v1', seed: successfulSeed, count: 5,
    completedCount: 5, status: 'completed', errors: [], memoryLimit: '2GB' });
  expect(one.samples).toEqual(two.samples);
  expect(one.samples.map(item => item.matchId)).toEqual(expectedIds(successfulSeed, 5));
  const records = await lines(first);
  expect(records.at(-1)).toEqual({ type: 'source-complete', records: 5, lastMatchId: one.samples.at(-1)!.matchId });
  expect(records.slice(0, -1)).toEqual(one.samples.map(sample => {
    const item = reconstructed.get(sample.matchId)!;
    return { record: item.record, sourcifyIds: item.sourcifyIds };
  }));
  expect(await readFile(join(first, 'records.ndjson'), 'utf8')).toBe(await readFile(join(second, 'records.ndjson'), 'utf8'));
  expect(await manifestAt(first)).toEqual(one);
  expect(one.source).toMatchObject({ path: sourceDir, readOnly: true, manifestSha256: hash('sha256', initialSourceManifest),
    manifest: { schema: 'sourcify-arkiv.import-source.v2', status: 'completed', selectedCount: matchIds.length },
    database: { path: join(sourceDir, 'subset.duckdb'), bytes: initialDatabase.size, mtimeMs: initialDatabase.mtimeMs } });
  expect(one.source.manifest).not.toHaveProperty('steps');
  expect(await readFile(join(sourceDir, 'source-manifest.json'))).toEqual(initialSourceManifest);
  expect(hash('sha256', await readFile(join(sourceDir, 'subset.duckdb')))).toBe(initialDatabase.sha256);
  expect((await stat(join(sourceDir, 'subset.duckdb'))).mtimeMs).toBe(initialDatabase.mtimeMs);
});

test('CLI preserves a failed actual sample, exits two and does not replace it or write completion', async () => {
  const output = join(scratch, 'failed-cli');
  const env = Object.fromEntries(['PATH', 'HOME', 'TMPDIR', 'LANG'].filter(key => process.env[key]).map(key => [key, process.env[key]!]));
  const result = Bun.spawnSync(['bun', '--no-env-file', 'tests/live/sample.ts', '--source-dir', sourceDir,
    '--output', output, '--seed', failingSeed, '--count', '1', '--memory-limit', '1GB'], { cwd: root, env });
  expect(result.exitCode).toBe(2);
  const manifest = await manifestAt(output);
  expect(JSON.parse(result.stdout.toString())).toEqual(manifest);
  expect(manifest).toMatchObject({ count: 1, completedCount: 0, status: 'incomplete' });
  expect(manifest.samples).toHaveLength(1);
  expect(manifest.samples[0]).toMatchObject({ matchId: expectedIds(failingSeed, 1)[0], status: 'failed',
    error: reconstructed.get(expectedIds(failingSeed, 1)[0])!.error });
  expect(manifest.errors).toHaveLength(1);
  expect(await lines(output)).toEqual([]);
});

test('existing files are never overwritten, including when only records.ndjson exists', async () => {
  const output = join(scratch, 'existing');
  const manifest = await sampleSource({ sourceDir, output, seed: successfulSeed });
  const recordsBefore = await readFile(join(output, 'records.ndjson'), 'utf8');
  await expect(sampleSource({ sourceDir, output, seed: 'different' })).rejects.toThrow('EEXIST');
  expect(await manifestAt(output)).toEqual(manifest);
  expect(await readFile(join(output, 'records.ndjson'), 'utf8')).toBe(recordsBefore);
  await rm(join(output, 'sample.json'));
  await expect(sampleSource({ sourceDir, output, seed: successfulSeed })).rejects.toThrow('EEXIST');
  expect(await readFile(join(output, 'records.ndjson'), 'utf8')).toBe(recordsBefore);
  await expect(readFile(join(output, 'sample.json'))).rejects.toThrow('ENOENT');
});

test('too-small populations are incomplete even if all available records reconstruct', async () => {
  const small = join(scratch, 'small-source');
  await prepareSource({ limit: 1, parquetDir: join(root, 'tests/fixture/parquet'), sourceDir: small, threads: 1, memoryLimit: '1GB' });
  const output = join(scratch, 'too-few');
  const manifest = await sampleSource({ sourceDir: small, output, seed: 'small', count: 2, memoryLimit: '1GB' });
  expect(manifest).toMatchObject({ count: 2, completedCount: 1, status: 'incomplete', errors: ['Only 1 matches available, requested 2'] });
  const records = await lines(output);
  expect(records).toHaveLength(1);
  expect(records[0].record.matchId).toBe('1');
  expect(records.some(item => item.type === 'source-complete')).toBe(false);
});

test('invalid arguments fail before creating output', async () => {
  for (const count of [0, 11, 1.5, Number.NaN]) {
    await expect(sampleSource({ sourceDir, output: join(scratch, `invalid-${count}`), seed: 'seed', count })).rejects.toThrow('between 1 and 10');
  }
  await expect(sampleSource({ sourceDir, output: join(scratch, 'invalid-seed'), seed: '' })).rejects.toThrow('nonempty');
  await expect(sampleSource({ sourceDir, output: join(scratch, 'invalid-memory'), seed: 'seed', memoryLimit: 'unlimited' })).rejects.toThrow('Memory limit');
});
