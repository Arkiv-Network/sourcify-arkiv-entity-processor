/** Deterministic, read-only sampling of an existing prepared source. No network requests. */
import { createHash } from 'node:crypto';
import { mkdir, open, readFile, stat, unlink, type FileHandle } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { literal } from '../../source/database';
import { iterateRecords, openSource, type Source } from '../../source/records';
import type { SourceManifest } from '../../source/prepare';

export const sampleAlgorithm = {
  name: 'md5-seed-colon-decimal-match-id-v1',
  population: 'Every row in the prepared source matches table; no eligibility or reconstruction filter',
  ordering: 'Ascending MD5 UTF-8 hex digest of seed + ":" + decimal match ID, then ascending numeric match ID',
  replacement: false,
};
export interface SampleOptions {
  sourceDir: string;
  output: string;
  seed: string;
  count?: number;
  memoryLimit?: string;
}
export interface SampleItem {
  matchId: string;
  rank: string;
  status: 'pending' | 'completed' | 'failed';
  address?: string;
  chainId?: string;
  verifiedAt?: string;
  sourcifyIds?: Record<string, string>;
  error?: string;
}
export interface SampleManifest {
  schema: 'sourcify-arkiv.live-sample.v1';
  seed: string;
  count: number;
  memoryLimit: string;
  algorithm: typeof sampleAlgorithm;
  source: {
    path: string;
    manifestSha256: string;
    manifest: Partial<SourceManifest>;
    database: { path: string; bytes: number; mtimeMs: number; modifiedAt: string };
    readOnly: true;
  };
  startedAt: string;
  finishedAt?: string;
  status: 'sampling' | 'completed' | 'incomplete';
  completedCount: number;
  samples: SampleItem[];
  errors: string[];
}
const message = (error: unknown) => error instanceof Error ? error.message : String(error);
function validate(seed: string, count: number) {
  if (typeof seed !== 'string' || seed.length === 0) throw new Error('A nonempty --seed is required');
  if (!Number.isSafeInteger(count) || count < 1 || count > 10) throw new Error('Count must be between 1 and 10');
}

/** Stable hash ranking covers the complete prepared population, including broken records. */
export async function selectMatchIds(source: Source, seed: string, count: number): Promise<{ matchId: string; rank: string }[]> {
  validate(seed, count);
  const rows = await source.db.rows(`SELECT id::VARCHAR AS match_id, md5(${literal(seed)} || ':' || id::VARCHAR) AS sample_rank FROM matches ORDER BY sample_rank, id LIMIT ${count}`);
  const ids = rows.map(row => ({ matchId: String(row.match_id), rank: String(row.sample_rank) }));
  if (ids.some(item => !/^\d+$/.test(item.matchId))) throw new Error('Prepared source contains invalid match IDs');
  if (new Set(ids.map(item => item.matchId)).size !== ids.length) throw new Error('Prepared source contains duplicate sampled match IDs');
  return ids;
}

function provenance(manifest: SourceManifest): Partial<SourceManifest> {
  // Preserve source identity without copying SQL statements or arbitrary manifest extensions.
  const { schema, status, signatures, all, limit, selectedCount, firstMatchId, lastMatchId,
    startedAt, finishedAt, databaseBytes, counts } = manifest;
  return { schema, status, signatures, all, limit, selectedCount, firstMatchId, lastMatchId,
    startedAt, finishedAt, databaseBytes, counts };
}
async function save(file: FileHandle, manifest: SampleManifest) {
  const bytes = Buffer.from(JSON.stringify(manifest, null, 2) + '\n');
  await file.truncate(0);
  // Explicit positioning allows repeated writes through the exclusively owned handle.
  let position = 0;
  while (position < bytes.length) {
    const { bytesWritten } = await file.write(bytes, position, bytes.length - position, position);
    if (!bytesWritten) throw new Error('Could not write sample manifest');
    position += bytesWritten;
  }
}

/** Writes sample.json and records.ndjson; incomplete selections never get a completion marker. */
export async function sampleSource(options: SampleOptions): Promise<SampleManifest> {
  const count = options.count ?? 5;
  const memoryLimit = options.memoryLimit ?? '2GB';
  validate(options.seed, count);
  if (!/^[1-9][0-9]*(MB|GB)$/i.test(memoryLimit)) throw new Error('Memory limit must be a positive whole MB or GB value');
  const sourceDir = resolve(options.sourceDir), output = resolve(options.output);
  const [manifestBytes, databaseStat] = await Promise.all([
    readFile(join(sourceDir, 'source-manifest.json')),
    stat(join(sourceDir, 'subset.duckdb')),
  ]);
  const sourceManifest: SourceManifest = JSON.parse(manifestBytes.toString('utf8'));
  const manifest: SampleManifest = {
    schema: 'sourcify-arkiv.live-sample.v1', seed: options.seed, count, memoryLimit, algorithm: sampleAlgorithm,
    source: {
      path: sourceDir, manifestSha256: createHash('sha256').update(manifestBytes).digest('hex'),
      manifest: provenance(sourceManifest), readOnly: true,
      database: { path: join(sourceDir, 'subset.duckdb'), bytes: databaseStat.size,
        mtimeMs: databaseStat.mtimeMs, modifiedAt: databaseStat.mtime.toISOString() },
    },
    startedAt: new Date().toISOString(), status: 'sampling', completedCount: 0, samples: [], errors: [],
  };
  await mkdir(output, { recursive: true });
  const samplePath = join(output, 'sample.json');
  const sampleFile = await open(samplePath, 'wx');
  let records: FileHandle;
  try { records = await open(join(output, 'records.ndjson'), 'wx'); }
  catch (error) {
    await sampleFile.close();
    await unlink(samplePath);
    throw error;
  }
  try {
    await save(sampleFile, manifest);
    let source: Source | undefined;
    try {
      source = await openSource(sourceDir, memoryLimit);
      manifest.samples = (await selectMatchIds(source, options.seed, count)).map(item => ({ ...item, status: 'pending' }));
      if (manifest.samples.length !== count) manifest.errors.push(`Only ${manifest.samples.length} matches available, requested ${count}`);
      await save(sampleFile, manifest);
      for (const sample of manifest.samples) {
        try {
          const iterator = iterateRecords(source, { batchSize: 1, afterMatchId: (BigInt(sample.matchId) - 1n).toString() });
          let selected: Awaited<ReturnType<typeof iterator.next>>;
          try { selected = await iterator.next(); }
          finally { await iterator.return(undefined); }
          if (selected.done || selected.value.matchId !== sample.matchId) throw new Error(`Selected match ${sample.matchId} was not returned by the existing assembler`);
          const item = selected.value;
          if (item.error) throw new Error(item.error);
          if (!item.record || !item.sourcifyIds || String(item.record.matchId) !== sample.matchId) throw new Error(`Selected match ${sample.matchId} has no complete reconstructed record`);
          await records.writeFile(JSON.stringify({ record: item.record, sourcifyIds: item.sourcifyIds }) + '\n');
          Object.assign(sample, { status: 'completed', address: item.record.address, chainId: item.record.chainId,
            verifiedAt: item.record.verifiedAt, sourcifyIds: item.sourcifyIds });
          manifest.completedCount += 1;
        } catch (error) {
          sample.status = 'failed'; sample.error = message(error);
          manifest.errors.push(`Match ${sample.matchId}: ${sample.error}`);
        }
        await save(sampleFile, manifest);
      }
    } catch (error) {
      manifest.errors.push(message(error));
    } finally {
      if (source) {
        try { await source.close(); } catch (error) { manifest.errors.push(`Closing source: ${message(error)}`); }
      }
    }
    manifest.status = manifest.errors.length === 0 && manifest.completedCount === count ? 'completed' : 'incomplete';
    if (manifest.status === 'completed') {
      await records.writeFile(JSON.stringify({ type: 'source-complete', records: manifest.completedCount,
        lastMatchId: manifest.samples.at(-1)!.matchId }) + '\n');
    }
    manifest.finishedAt = new Date().toISOString();
    await save(sampleFile, manifest);
    return manifest;
  } finally {
    await records.close();
    await sampleFile.close();
  }
}

const usage = 'bun tests/live/sample.ts --source-dir DIR --output DIR --seed SEED [--count 5] [--memory-limit 2GB]\nSamples 1..10 actual match IDs by stable seeded hash rank. Opens the prepared source read-only.\nWrites sample.json and records.ndjson in a new or existing output directory; existing output files are refused.\nNo live API calls. Exit 0: complete; 2: incomplete selection/reconstruction; 1: invalid arguments or I/O failure.';
async function main() {
  const { values } = parseArgs({ strict: true, options: {
    'source-dir': { type: 'string' }, output: { type: 'string' }, seed: { type: 'string' },
    count: { type: 'string', default: '5' }, 'memory-limit': { type: 'string', default: '2GB' },
    help: { type: 'boolean', default: false },
  } });
  if (values.help) { console.log(usage); return; }
  if (!values['source-dir'] || !values.output || values.seed === undefined) throw new Error('--source-dir, --output and --seed are required (see --help)');
  const manifest = await sampleSource({ sourceDir: values['source-dir'], output: values.output,
    seed: values.seed, count: Number(values.count), memoryLimit: values['memory-limit'] });
  console.log(JSON.stringify(manifest));
  if (manifest.status !== 'completed') process.exitCode = 2;
}
if (import.meta.main) {
  try { await main(); } catch (error) { console.error(message(error)); process.exitCode = 1; }
}
