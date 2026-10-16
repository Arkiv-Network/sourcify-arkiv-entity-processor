/**
 * The Postgres -> Arkiv sync's offline parts: the incremental source selection (prepare.ts --after-match-id) against
 * the PostgreSQL fixture (see tests/fixture/postgres.ts), the bundle-entity conversion and transaction packing of
 * arkiv/sync.ts, and how a start finds its cursor (against a simulated chain). Nothing here talks to an Arkiv chain.
 */
import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { existsSync, mkdtempSync, readFileSync, rmSync, utimesSync, writeFileSync } from 'node:fs';
import { hostname } from 'node:os';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { localMatchIdStart, newestEthereumMatchId, prepareSource } from '../source/prepare';
import { blockTime, createParams, highestMatchId, lock, pack, startCursor } from '../arkiv/sync';
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

  test('the newest Ethereum match, and the one N matches before a given ID', async () => {
    const ids = full.map(item => item.matchId);
    expect(await newestEthereumMatchId(fixture.database)).toBe(ids.at(-1));
    expect(await newestEthereumMatchId(fixture.database, { atOrBelow: ids[6]!, offset: 2 })).toBe(ids[4]);
    expect(await newestEthereumMatchId(fixture.database, { atOrBelow: String(BigInt(ids[6]!) + 1n), offset: 0 })).toBe(ids[6]);
    expect(await newestEthereumMatchId(fixture.database, { atOrBelow: ids[1]!, offset: 5 })).toBeUndefined();
    await expect(newestEthereumMatchId(fixture.database, { atOrBelow: String(localMatchIdStart) })).rejects.toThrow('below');
  }, 30_000);

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

describe('start cursor', () => {
  /** A chain whose writer has verified_contract entities with these matchids; the node answers `matchid > X` with the
   * two highest (as it does) or the two lowest. Records each X asked about. */
  const chain = (matchIds: number[], order: 'highest' | 'lowest' = 'highest') => {
    const asked: bigint[] = [];
    const sorted = matchIds.map(BigInt).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    const probe = async (after: bigint) => {
      asked.push(after);
      const above = sorted.filter(id => id > after);
      const returned = order === 'highest' ? above.slice(-2) : above.slice(0, 2);
      return returned.length ? returned.reduce((a, b) => (a > b ? a : b)) : undefined;
    };
    return { probe, asked };
  };
  const range = (from: number, to: number, step = 1) => Array.from({ length: Math.floor((to - from) / step) + 1 }, (_, i) => from + i * step);

  test('finds the writer\'s highest matchid below or above the mirror\'s newest, asking only near the top', async () => {
    for (const order of ['highest', 'lowest'] as const) {
      // A seed up to 56,195,245 with holes, then nothing synced yet; the mirror is 33,000 IDs further.
      const seeded = chain([...range(56_000_000, 56_195_000, 7), 56_195_245], order);
      expect(await highestMatchId(seeded.probe, 56_228_537n)).toBe(56_195_245n);
      // Never above anything far below: the node's time grows with the entities above X.
      expect(seeded.asked.every(after => after >= 56_228_537n - 65_536n)).toBe(true);
      if (order === 'highest') expect(seeded.asked.length).toBeLessThan(12);
      // Synced past the mirror's newest match (another mirror ahead of this one).
      expect(await highestMatchId(chain([...range(100, 5_000, 3), 5_003], order).probe, 4_000n)).toBe(5_003n);
      // Caught up exactly.
      expect(await highestMatchId(chain([1, 2, 3, 900], order).probe, 900n)).toBe(900n);
    }
  });

  test('nothing within the window, or nothing at all, is undefined; matchids beyond 2^40 are refused', async () => {
    const far = chain([1_000, 2_000]);
    expect(await highestMatchId(far.probe, 50_000_000n)).toBeUndefined();
    expect(far.asked.every(after => after >= 50_000_000n - 2n ** 21n)).toBe(true);
    expect(await highestMatchId(chain([]).probe, 1_000n)).toBeUndefined();
    expect(await highestMatchId(chain([5]).probe, 1_000n)).toBe(5n);
    await expect(highestMatchId(chain([2 ** 41]).probe, 1_000n)).rejects.toThrow('2^40');
  });

  test('state.json is kept when the chain reached it; otherwise the chain\'s progress less a margin', () => {
    // No state file: from the chain (a seeded chain, or one synced before), else wait for a seed.
    expect(startCursor(undefined, '56195245', '56170000')).toEqual({ cursor: '56170000', from: 'chain' });
    expect(startCursor(undefined, undefined, undefined)).toEqual({ from: 'needs-seed' });
    // The chain holds the cursor, possibly part of the next batch: the cursor stays.
    expect(startCursor('56228394', '56228394', '56211171')).toEqual({ cursor: '56228394', from: 'state' });
    expect(startCursor('56228394', '56228500', '56211200')).toEqual({ cursor: '56228394', from: 'state' });
    // The chain is far ahead (prefilled again from a newer seed), or behind (reset, an older seed): from the chain.
    expect(startCursor('56000000', '56228394', '56211171')).toEqual({ cursor: '56211171', from: 'chain' });
    expect(startCursor('56228394', '56195245', '56170000')).toEqual({ cursor: '56170000', from: 'chain-behind-state' });
    // Nothing of the writer near the top: the state file is all there is.
    expect(startCursor('50000000', undefined, undefined)).toEqual({ cursor: '50000000', from: 'state-unchecked' });
  });
});

describe('sync lock', () => {
  test('held by a live sync elsewhere it is refused; a dead process on this host, or no refresh for 2 minutes, is taken over', async () => {
    const path = join(scratch, 'sync.lock');
    const holdBy = (holder: string, secondsAgo = 0) => { writeFileSync(path, holder); const t = new Date(Date.now() - secondsAgo * 1000); utimesSync(path, t, t); };
    // Another container's sync, refreshed recently: PIDs say nothing across containers.
    holdBy('8@another-container', 20);
    await expect(lock(path, false)).rejects.toThrow('another sync (8@another-container)');
    // The same holder after 2 minutes without a refresh (its container stopped); the old PID-only format alike.
    for (const holder of ['8@another-container', '8']) {
      holdBy(holder, 180);
      const release = await lock(path, false);
      expect(readFileSync(path, 'utf8')).toBe(`${process.pid}@${hostname()}`);
      release();
      expect(existsSync(path)).toBe(false);
    }
    holdBy('8', 20);
    await expect(lock(path, false)).rejects.toThrow('another sync');
    // A process gone from this host, at once.
    holdBy(`999999999@${hostname()}`);
    (await lock(path, false))();
    // Released only by its holder: a lock taken over meanwhile stays.
    const release = await lock(path, false);
    holdBy('9@another-container');
    release();
    expect(readFileSync(path, 'utf8')).toBe('9@another-container');
    rmSync(path);
  });
});

describe('block time for the log', () => {
  test('a block the node does not serve yet is asked again, then left out instead of failing the batch', async () => {
    let calls = 0;
    const late = { getBlock: async () => { if (++calls < 3) throw new Error('Block at number "7" could not be found.'); return { timestamp: 1_760_000_000n }; } };
    expect(await blockTime(late, 7n, 3, 1)).toBe('2025-10-09T08:53:20.000Z');
    expect(calls).toBe(3);
    const missing = { getBlock: async () => { throw new Error('Block at number "8" could not be found.'); } };
    expect(await blockTime(missing, 8n, 3, 1)).toBeUndefined();
  });
});
