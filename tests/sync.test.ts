/**
 * The Postgres -> Arkiv sync's offline parts: the incremental source selection (prepare.ts --after-match-id) against
 * the PostgreSQL fixture (see tests/fixture/postgres.ts), and the bundle-entity conversion and transaction packing of
 * arkiv/sync.ts. Nothing here talks to an Arkiv chain.
 */
import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { localMatchIdStart, prepareSource } from '../source/prepare';
import { createParams, pack } from '../arkiv/sync';
import { createFixtureDatabase, type FixtureDatabase } from './fixture/postgres';

const root = resolve(import.meta.dir, '..');
let fixture: FixtureDatabase;
let scratch: string;

/** stream.ts output as one ordered list of matches: records (full JSON) and skipped ones (reason). */
function streamed(sourceDir: string) {
  const result = Bun.spawnSync(['bun', '--no-env-file', 'source/stream.ts', '--source-dir', sourceDir, '--skip-missing-references'], { cwd: root });
  if (result.exitCode !== 0) throw new Error(result.stderr.toString());
  const lines = result.stdout.toString().trim().split('\n').map(line => JSON.parse(line));
  const marker = lines.pop();
  expect(marker.type).toBe('source-complete');
  const items = [...lines.map(line => ({ matchId: line.record.matchId as string, record: JSON.stringify(line.record) })),
    ...(marker.skipped ?? []).map((s: { matchId: string; reason: string }) => ({ matchId: s.matchId, skipped: s.reason }))];
  return items.sort((a, b) => (BigInt(a.matchId) < BigInt(b.matchId) ? -1 : 1));
}

beforeAll(async () => {
  scratch = mkdtempSync(join(tmpdir(), 'entity-processor-sync-test-'));
  fixture = await createFixtureDatabase();
}, 60_000);
afterAll(async () => {
  await fixture?.drop();
  rmSync(scratch, { recursive: true, force: true });
});

describe('incremental source selection', () => {
  let full: ReturnType<typeof streamed>;
  beforeAll(async () => {
    await prepareSource({ all: true, database: fixture.database, sourceDir: join(scratch, 'full'), threads: 2, memoryLimit: '1GB' });
    full = streamed(join(scratch, 'full'));
  }, 60_000);

  test('after any cursor, the next matches give exactly the records (and skips) of a full preparation', async () => {
    const cursors = ['0', full[0]!.matchId, full[4]!.matchId, full.at(-3)!.matchId];
    for (const [index, after] of cursors.entries()) {
      const sourceDir = join(scratch, `after-${index}`);
      const manifest = await prepareSource({ afterMatchId: after, limit: 4, database: fixture.database, sourceDir, threads: 1, memoryLimit: '1GB' });
      const expected = full.filter(item => BigInt(item.matchId) > BigInt(after)).slice(0, 4);
      expect(manifest).toMatchObject({ status: 'completed', afterMatchId: after, selectedCount: expected.length, firstMatchId: expected[0]!.matchId, lastMatchId: expected.at(-1)!.matchId });
      expect(streamed(sourceDir)).toEqual(expected);
    }
  }, 120_000);

  test('a caught-up cursor selects nothing, and completes', async () => {
    const manifest = await prepareSource({ afterMatchId: full.at(-1)!.matchId, limit: 10, database: fixture.database, sourceDir: join(scratch, 'caught-up'), threads: 1, memoryLimit: '1GB' });
    expect(manifest).toMatchObject({ status: 'completed', selectedCount: 0 });
  }, 30_000);

  test('a cursor needs a limit, must be below the node\'s own IDs, and a directory holds one cursor', async () => {
    await expect(prepareSource({ afterMatchId: '1', all: true, database: fixture.database, sourceDir: join(scratch, 'bad-1') })).rejects.toThrow('After match ID');
    await expect(prepareSource({ afterMatchId: String(localMatchIdStart), limit: 1, database: fixture.database, sourceDir: join(scratch, 'bad-2') })).rejects.toThrow('After match ID');
    await expect(prepareSource({ afterMatchId: '-1', limit: 1, database: fixture.database, sourceDir: join(scratch, 'bad-3') })).rejects.toThrow('After match ID');
    await expect(prepareSource({ afterMatchId: '5', limit: 4, database: fixture.database, sourceDir: join(scratch, 'after-0') })).rejects.toThrow('different selection');
  });
});

describe('one match per address', () => {
  // Second matches for the addresses of fixture matches 1, 32 and 235, each on a copy of the deployment (no
  // transaction hash, like the duplicates Sourcify holds) and of the verification.
  const duplicate = (of: number, id: number, creation: string | null, runtime: string) => `
INSERT INTO contract_deployments (id, chain_id, address, transaction_hash, block_number, transaction_index, deployer, contract_id)
  SELECT '00000000-0000-0000-0000-${String(id).padStart(12, '0')}', d.chain_id, d.address, NULL, d.block_number, d.transaction_index, d.deployer, d.contract_id
  FROM contract_deployments d JOIN verified_contracts v ON v.deployment_id=d.id JOIN sourcify_matches m ON m.verified_contract_id=v.id WHERE m.id=${of};
INSERT INTO verified_contracts (id, deployment_id, compilation_id, creation_match, creation_values, creation_transformations, creation_metadata_match, runtime_match, runtime_values, runtime_transformations, runtime_metadata_match)
  SELECT ${900_000_000 + id}, '00000000-0000-0000-0000-${String(id).padStart(12, '0')}', v.compilation_id, v.creation_match, v.creation_values, v.creation_transformations, v.creation_metadata_match, v.runtime_match, v.runtime_values, v.runtime_transformations, v.runtime_metadata_match
  FROM verified_contracts v JOIN sourcify_matches m ON m.verified_contract_id=v.id WHERE m.id=${of};
INSERT INTO sourcify_matches (id, verified_contract_id, creation_match, runtime_match, chain_id) VALUES (${id}, ${900_000_000 + id}, ${creation === null ? 'NULL' : `'${creation}'`}, '${runtime}', 1);`;
  let duplicates: FixtureDatabase;
  beforeAll(async () => {
    const { fixtureSql } = await import('./fixture/postgres');
    duplicates = await createFixtureDatabase(fixtureSql() +
      duplicate(1, 2, null, 'partial') +                                                       // worse, newer: 1 stays
      duplicate(32, 33, 'perfect', 'perfect') +                                                // as good, newer: 33 wins
      duplicate(235, 200, 'perfect', 'perfect') + "UPDATE sourcify_matches SET runtime_match='partial' WHERE id=235;"); // better, older: 200 wins
  }, 60_000);
  afterAll(async () => { await duplicates?.drop(); });

  test('the better match, then the newer one, in a seed and incrementally alike', async () => {
    const dir = join(scratch, 'duplicates-full');
    const manifest = await prepareSource({ all: true, database: duplicates.database, sourceDir: dir, threads: 1, memoryLimit: '1GB' });
    const full = streamed(dir);
    const ids = full.map(item => item.matchId);
    expect(ids).toEqual(expect.arrayContaining(['1', '33', '200']));
    for (const dropped of ['2', '32', '235']) expect(ids).not.toContain(dropped);
    expect(manifest.selectedCount).toBe(full.length);
    // Incrementally a match is left out even when the better one lies before the cursor (200 vs 235) or after it.
    for (const [index, after] of ['0', '1', '199', '200'].entries()) {
      const sourceDir = join(scratch, `duplicates-after-${index}`);
      await prepareSource({ afterMatchId: after, limit: 4, database: duplicates.database, sourceDir, threads: 1, memoryLimit: '1GB' });
      expect(streamed(sourceDir)).toEqual(full.filter(item => BigInt(item.matchId) > BigInt(after)).slice(0, 4));
    }
  }, 120_000);
});

describe('sync entity conversion', () => {
  const seedCompilation = `0x${'ab'.repeat(32)}`, chainCompilation = `0x${'cd'.repeat(32)}`;
  const wire = {
    $key: `0x${'01'.repeat(32)}`, $creator: '0x986c08292e6c327a310273a45965a3d2e2eac44c', $owner: '0x986c08292e6c327a310273a45965a3d2e2eac44c',
    $expiresAt: 'never', $contentType: 'application/gzip', $payload: Buffer.from('payload').toString('base64'), $creationFlags: [],
    kind: 'str:verified_contract', projectid: 'str:sourcify', matchid: 'u64:54163398', chainid: 'u64:1', optimizer: 'bool:true',
    optimizerruns: 'i32:200', address: 'addr:0x52Fa6240D27F307156B0cf8a7A0C28B69d1A036E', compilationref: `key:${seedCompilation}`,
    stagingid: `bytes32:0x${'ef'.repeat(32)}`,
  };

  test('typed attributes, permanent expiry, and key references through the chain-key map', () => {
    const params = createParams(wire, new Map([[seedCompilation, chainCompilation as `0x${string}`]]));
    expect(Buffer.from(params.payload).toString()).toBe('payload');
    expect(params.contentType).toBe('application/gzip');
    expect(params.expires.expiresAt).toBe(2n ** 64n - 1n);
    const attributes = params.attributes as Record<string, { type: string; value: unknown }>;
    expect(attributes.compilationref).toMatchObject({ type: 'key', value: chainCompilation });
    expect(attributes.matchid).toMatchObject({ type: 'u64', value: 54163398n });
    expect(attributes.optimizerruns).toMatchObject({ type: 'i32', value: 200 });
    expect(attributes.optimizer).toMatchObject({ type: 'bool', value: true });
    expect(attributes.kind).toMatchObject({ type: 'str', value: 'verified_contract' });
    expect(Object.keys(attributes).some(name => name.startsWith('$'))).toBe(false);
  });

  test('an unmapped key reference, a lifetime or flags other than the seed\'s, and unknown types are refused', () => {
    expect(() => createParams(wire)).toThrow('has no chain key');
    expect(() => createParams({ ...wire, $expiresAt: '100' }, new Map([[seedCompilation, chainCompilation as `0x${string}`]]))).toThrow('lifetime');
    expect(() => createParams({ ...wire, $creationFlags: ['readonly'] })).toThrow('creation flags');
    expect(() => createParams({ ...wire, compilationref: undefined, extra: 'f64:1.5' })).toThrow('unsupported attribute');
    expect(() => createParams({ ...wire, compilationref: undefined, optimizer: 'bool:yes' })).toThrow('invalid bool');
  });

  test('packing keeps order and the calldata budget; an oversized entity travels alone', async () => {
    const item = (index: number, bytes: number) => ({ index, params: { payload: new Uint8Array(bytes).fill(index), contentType: 'application/octet-stream', attributes: {} } });
    const items = [item(0, 30_000), item(1, 30_000), item(2, 30_000), item(3, 120_000), item(4, 10_000), item(5, 10_000)];
    const batches = await pack(items, 100_000);
    expect(batches.map(batch => batch.items.map(entry => entry.index))).toEqual([[0, 1, 2], [3], [4, 5]]);
    for (const batch of batches) if (batch.items.length > 1) expect(batch.bytes).toBeLessThanOrEqual(100_000);
    expect(batches[1]!.bytes).toBeGreaterThan(120_000);
  });
});
