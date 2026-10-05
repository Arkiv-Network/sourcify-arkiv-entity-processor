/**
 * End-to-end tests on tests/fixture/parquet (real export rows, see tests/fixture/extract.ts). expected.json holds the
 * output of the original sourcify-arkiv-helper pipeline for the same input: this pipeline must stay byte-identical.
 *
 * Needs a built processor: `cargo build --release` (or PROCESSOR_BIN=/path/to/arkiv-entity-processor).
 */
import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, readFileSync, renameSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { prepareSource } from '../source/prepare';

const root = resolve(import.meta.dir, '..');
const parquetDir = join(root, 'tests/fixture/parquet');
const expected = JSON.parse(readFileSync(join(root, 'tests/fixture/expected.json'), 'utf8'));
const processor = resolve(process.env.PROCESSOR_BIN ?? join(root, 'target/release/arkiv-entity-processor'));
let scratch: string;

const sha256 = (data: string | Uint8Array) => createHash('sha256').update(data).digest('hex');
// Hermetic: no pipeline settings from the caller's environment (Bun also auto-loads a local .env).
const baseEnv = Object.fromEntries(['PATH', 'HOME', 'TMPDIR', 'LANG'].filter(name => process.env[name]).map(name => [name, process.env[name]!]));
const run = (args: string[], env: Record<string, string> = {}) => {
  const result = Bun.spawnSync(args, { cwd: root, env: { ...baseEnv, ...env } });
  return { code: result.exitCode, stdout: result.stdout.toString(), stderr: result.stderr.toString() };
};
const bundleSums = (bundle: string) => Object.fromEntries(readFileSync(join(bundle, 'SHA256SUMS'), 'utf8').trim().split('\n')
  .map(line => line.split('  ')).filter(([, path]) => path !== 'report.json').map(([digest, path]) => [path, digest]));

beforeAll(() => {
  if (!existsSync(processor)) throw new Error(`Processor binary not found at ${processor}: run cargo build --release or set PROCESSOR_BIN`);
  scratch = mkdtempSync(join(tmpdir(), 'entity-processor-test-'));
});
afterAll(() => rmSync(scratch, { recursive: true, force: true }));

describe('source stage', () => {
  let sourceDir: string;
  beforeAll(async () => {
    sourceDir = join(scratch, 'source');
    await prepareSource({ all: true, parquetDir, sourceDir, threads: 2, memoryLimit: '1GB' });
  }, 60_000);

  test('prepare selects the current Ethereum mainnet matches only', () => {
    const manifest = JSON.parse(readFileSync(join(sourceDir, 'source-manifest.json'), 'utf8'));
    expect(manifest).toMatchObject({ status: 'completed', all: true, selectedCount: expected.selectedCount, firstMatchId: '1', signatures: false });
    expect(manifest.counts.matches).toBe(String(expected.selectedCount)); // match 100 is on chain 35443
  });

  test('prepare reuses an identical selection and refuses a different one', async () => {
    expect((await prepareSource({ all: true, parquetDir, sourceDir })).status).toBe('completed');
    expect(prepareSource({ limit: 3, parquetDir, sourceDir })).rejects.toThrow('different selection');
  });

  test('prepare --limit takes the first matches by ID', async () => {
    const manifest = await prepareSource({ limit: 3, parquetDir, sourceDir: join(scratch, 'source-3'), threads: 1, memoryLimit: '1GB' });
    expect(manifest).toMatchObject({ selectedCount: 3, firstMatchId: '1', lastMatchId: '235' });
  });

  test('stream output is byte-identical to the original helper', () => {
    const result = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--skip-missing-references']);
    expect(result.code).toBe(0);
    expect(sha256(result.stdout)).toBe(expected.ndjsonSha256);
    const marker = JSON.parse(result.stdout.trim().split('\n').at(-1)!);
    expect(marker).toEqual({ type: 'source-complete', records: expected.records, lastMatchId: expected.lastMatchId, skipped: expected.skipped });
  });

  test('stream fails closed on a missing reference unless skipping', () => {
    const result = run(['bun', 'source/stream.ts', '--source-dir', sourceDir]);
    expect(result.code).toBe(1);
    expect(result.stderr).toContain('Source match 15040941: Missing bytecode');
    expect(result.stdout).not.toContain('source-complete');
  });

  test('stream --limit and --max-skipped', () => {
    const limited = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--limit', '2']);
    expect(limited.code).toBe(0);
    expect(limited.stdout.trim().split('\n').map(line => JSON.parse(line).matchId ?? JSON.parse(line).records)).toEqual(['1', '32', 2]);
    const capped = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--skip-missing-references', '--max-skipped', '3']);
    expect(capped.code).toBe(1);
    expect(capped.stderr).toContain('More than 3 records skipped');
  }, 30_000);
});

describe('pipeline/run.sh', () => {
  const env = (workDir: string, extra: Record<string, string> = {}) => ({
    PATH: `${dirname(processor)}:${process.env.PATH}`, PROCESSOR_BIN: processor, WORK_DIR: workDir, PARQUET_DIR: parquetDir,
    ENTITY_OWNER: expected.owner, PREPARE_THREADS: '2', PREPARE_MEMORY_LIMIT: '1GB', SOURCE_MEMORY_LIMIT: '1GB', ...extra,
  });

  test('run produces the reference bundle, then reuses it', () => {
    const workDir = join(scratch, 'work');
    Bun.spawnSync(['mkdir', '-p', workDir]);
    const first = run(['bash', 'pipeline/run.sh', 'run'], env(workDir));
    expect(first.code).toBe(0);
    expect(bundleSums(join(workDir, 'bundle'))).toEqual(expected.bundleSha256sums);
    const timings = JSON.parse(readFileSync(join(workDir, 'timings.json'), 'utf8'));
    expect(timings).toMatchObject({ status: 'completed', records: { contracts: expected.records, sourceSkipped: expected.skipped.length, entities: expected.entities } });
    expect(Object.keys(timings.phases)).toEqual(['prepare', 'process', 'verify']);

    const second = run(['bash', 'pipeline/run.sh', 'run'], env(workDir));
    expect(second.code).toBe(0);
    const phases = JSON.parse(readFileSync(join(workDir, 'timings.json'), 'utf8')).phases;
    expect([phases.prepare.status, phases.process.status, phases.verify.status]).toEqual(['reused', 'reused', 'completed']);

    // A bundle made for another owner is never silently reused.
    const other = run(['bash', 'pipeline/run.sh', 'run'], env(workDir, { ENTITY_OWNER: '0x2222222222222222222222222222222222222222' }));
    expect(other.code).toBe(1);
    expect(other.stderr).toContain('refusing to reuse');
  }, 60_000);

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
