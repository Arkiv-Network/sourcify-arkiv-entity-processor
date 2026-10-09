/**
 * End-to-end tests on tests/fixture/parquet (real export rows, see tests/fixture/extract.ts). expected.json holds the
 * output of the original sourcify-arkiv-helper pipeline: legacy records must stay byte-identical,
 * while complete decoded contract payloads now equal each adapter input. Contract identities and bundle hashes
 * deliberately change; the historical whole-bundle checksums are not a payload-parity oracle.
 *
 * Needs a built processor: `cargo build --release` (or PROCESSOR_BIN=/path/to/arkiv-entity-processor).
 */
import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { gunzipSync } from 'node:zlib';
import { dirname, join, resolve } from 'node:path';
import { prepareSource } from '../source/prepare';

const root = resolve(import.meta.dir, '..');
const parquetDir = join(root, 'tests/fixture/parquet');
const expected = JSON.parse(readFileSync(join(root, 'tests/fixture/expected.json'), 'utf8'));
const recoveredIds = ['23813307', '28759260'];
const remainingSkipped = expected.skipped.filter((item: { matchId: string }) => !recoveredIds.includes(item.matchId));
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
  let spills = 0;
  const contracts = decoded.filter(({ entity }) => entity.kind === 'str:verified_contract').map(({ payload }) => {
    let record = JSON.parse(payload.toString());
    if (Object.keys(record).length === 1 && record.$spill) {
      spills++;
      const stub = record.$spill;
      const parts = decoded.filter(({ entity }) => entity.kind === 'str:blob' && entity.hash === `bytes32:${stub.hash}`)
        .sort((a, b) => Number(a.entity.part.slice(4)) - Number(b.entity.part.slice(4)));
      expect(parts.length).toBe(stub.parts);
      for (const [index, part] of parts.entries()) {
        expect(part.entity.part).toBe(`i32:${index}`);
        expect(part.entity.parts).toBe(`i32:${stub.parts}`);
        expect(part.entity.size).toBe(`i32:${part.payload.length}`);
      }
      const body = Buffer.concat(parts.map(part => part.payload));
      expect(body.length).toBe(stub.bytes);
      expect(`0x${sha256(body)}`).toBe(stub.hash);
      record = JSON.parse(body.toString());
    }
    return record;
  });
  return { entities, contracts, spills };
}

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

  test('every real Parquet adapter record equals its decoded wire contract payload', () => {
    const result = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--skip-missing-references']);
    expect(result.code).toBe(0);
    const lines = result.stdout.trim().split('\n').map(line => JSON.parse(line));
    expect(lines.pop()).toEqual({ type: 'source-complete', records: expected.records + 2, lastMatchId: '28759260', skipped: remainingSkipped });
    expect(lines.find(record => record.matchId === '23813307').metadata).toBeNull();
    const yul = lines.find(record => record.matchId === '28759260');
    expect(yul.compilation.language).toBe('Yul');
    expect(yul.abi).toBeNull();
    for (const record of lines) {
      expect(Object.keys(record.sourcifyIds).sort()).toEqual(['compilationId', 'contractId', 'deploymentId', 'verificationId']);
      for (const id of Object.values(record.sourcifyIds)) expect(typeof id).toBe('string');
    }
    const legacy = lines.filter(record => !recoveredIds.includes(record.matchId)).map(({ sourcifyIds, ...record }) => record);
    legacy.push({ type: 'source-complete', records: expected.records, lastMatchId: expected.lastMatchId, skipped: expected.skipped });
    const legacyStream = legacy.map(record => JSON.stringify(record) + '\n').join('');
    expect(sha256(legacyStream)).toBe(expected.ndjsonSha256);
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
    // Provenance must not change shared entities for the same legacy inputs.
    const shared = (entities: any[]) => entities.filter(entity => ['str:sourcefile', 'str:code', 'str:compilation'].includes(entity.kind));
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
    expect(limited.stdout.trim().split('\n').map(line => JSON.parse(line).matchId ?? JSON.parse(line).records)).toEqual(['1', '32', 2]);
    const capped = run(['bun', 'source/stream.ts', '--source-dir', sourceDir, '--skip-missing-references', '--max-skipped', '1']);
    expect(capped.code).toBe(1);
    expect(capped.stderr).toContain('More than 1 records skipped');
  }, 30_000);
});

describe('pipeline/run.sh', () => {
  const env = (workDir: string, extra: Record<string, string> = {}) => ({
    PATH: `${dirname(processor)}:${process.env.PATH}`, PROCESSOR_BIN: processor, WORK_DIR: workDir, PARQUET_DIR: parquetDir,
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
    expect(yul.sourcifyIds.compilationId).toBeTruthy();
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
