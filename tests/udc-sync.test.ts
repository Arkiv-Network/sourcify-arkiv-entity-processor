/**
 * The UDC (v4) writer of the sync (arkiv/udc.ts), offline: reading a v4 bundle, references and their rewriting to
 * chain keys, the plan (create, reuse, patch in place, already synced, skip) and operation encoding and packing. The
 * bundle is the SafeProxy record processed by the release binary; nothing here talks to a chain.
 */
import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { decodeAbiParameters, parseAbiParameters, type Hex } from 'viem';
import { chunkList, operation, packOperations, patchMutations, plan, readBundle, references, remap, sameAttributes, type ChainEntity, type Line, type Lookups } from '../arkiv/udc';

const root = resolve(import.meta.dir, '..');
const PREFIX = 'sourcify-udc-test.v4';
let scratch: string;
let lines: Line[];

beforeAll(() => {
  scratch = mkdtempSync(join(tmpdir(), 'entity-processor-udc-sync-'));
  const bundle = join(scratch, 'bundle');
  const result = Bun.spawnSync([join(root, 'target/release/arkiv-entity-processor'), 'process', '-i', join(root, 'docs/data/processor-input.ndjson'), '-o', bundle,
    '--format', 'udc', '--schema-prefix', PREFIX, '--owner', '0x3ffcb8c66ba2a35df36d73b213399220fd59f42f', '--require-source-complete'], { cwd: root });
  if (result.exitCode !== 0) throw new Error(result.stderr.toString());
  lines = readBundle(bundle, PREFIX);
});
afterAll(() => rmSync(scratch, { recursive: true, force: true }));

const chainKey = (n: number) => `0x${n.toString(16).padStart(64, '0')}` as Hex;
const empty = (): Lookups => ({ dependencies: new Map(), contracts: new Map() });
const contractLine = () => lines.find(l => l.kind === 'contract')!;
/** Every dependency of the bundle already on the chain, under chain keys 1, 2, ... */
function allDependenciesOnChain(): Lookups {
  const lookups = empty();
  lines.filter(l => l.kind !== 'contract' && l.kind !== 'chunk').forEach((l, i) => lookups.dependencies.set(l.key, [chainKey(i + 1)]));
  return lookups;
}
const keysOf = (lookups: Lookups) => new Map([...lookups.dependencies].map(([k, v]) => [k, v[0]!]));

describe('v4 bundle as the sync reads it', () => {
  test('six entities in creation order, dependencies first, bytes as hex', () => {
    // The on-chain creation code follows the compilation: the v3 split's order, dependencies before dependents.
    expect(lines.map(l => l.kind)).toEqual(['source', 'code', 'code', 'compilation', 'code', 'contract']);
    expect(lines.map(l => l.nonce)).toEqual([0, 1, 2, 3, 4, 5]);
    const data = contractLine().attributes.find(a => a.name === 'data')!;
    expect(data.type).toBe('bytes');
    expect(String(data.value).startsWith('0x01')).toBe(true); // raw DEFLATE
  });
  test('references name earlier lines of the right kind', () => {
    const byKey = new Map(lines.map(l => [l.key, l]));
    const comp = lines.find(l => l.kind === 'compilation')!;
    const refs = references(comp);
    expect(refs.filter(r => r.kind === 'source').length).toBe(1);
    expect(refs.filter(r => r.kind === 'code').length).toBe(2);
    for (const line of lines) for (const r of references(line)) {
      expect(byKey.get(r.key)?.kind).toBe(r.kind);
      expect(byKey.get(r.key)!.nonce).toBeLessThan(line.nonce);
    }
  });
  test('remapping rewrites key attributes and sourcerefs, and refuses an unmapped key', () => {
    const lookups = allDependenciesOnChain();
    const keys = keysOf(lookups);
    const comp = lines.find(l => l.kind === 'compilation')!;
    const mapped = remap(comp, keys);
    expect(references({ kind: 'compilation', attributes: mapped }).every(r => [...keys.values()].includes(r.key))).toBe(true);
    expect(mapped.find(a => a.name === 'hash')).toEqual(comp.attributes.find(a => a.name === 'hash')!);
    keys.delete(comp.key);
    expect(() => remap(contractLine(), keys)).toThrow('has no chain key');
  });
});

describe('plan', () => {
  test('an empty chain: everything is created, in four levels', () => {
    const p = plan(lines, empty());
    expect(p.levels.map(level => level.map(o => `${o.op} ${o.line.kind}`))).toEqual([[], ['create source', 'create code', 'create code', 'create code'], ['create compilation'], ['create contract']]);
    expect(p.reused).toBe(0);
  });
  test('dependencies on the chain are reused; only the contract is created, naming chain keys', () => {
    const lookups = allDependenciesOnChain();
    const p = plan(lines, lookups);
    expect(p.levels.map(l => l.length)).toEqual([0, 0, 0, 1]);
    expect(p.reused).toBe(5);
    expect(references({ kind: 'contract', attributes: remap(contractLine(), p.keys) }).map(r => r.key).every(k => BigInt(k) <= 5n)).toBe(true);
  });
  test('the same contract on the chain is already synced', () => {
    const lookups = allDependenciesOnChain();
    const chain: ChainEntity = { key: chainKey(99), attributes: remap(contractLine(), keysOf(lookups)) };
    lookups.contracts.set(contractLine().key, [chain]);
    const p = plan(lines, lookups);
    expect(p.alreadySynced).toBe(1);
    expect(p.levels.flat().length).toBe(0);
  });
  test('a re-verification (higher matchId) patches the chain\'s contract in place', () => {
    const lookups = allDependenciesOnChain();
    const current = remap(contractLine(), keysOf(lookups)).map(a => a.name === 'matchid' ? { ...a, value: '0x1' } : a);
    lookups.contracts.set(contractLine().key, [{ key: chainKey(99), attributes: current }]);
    // The new version has no proxy attributes: the patch unsets them.
    const reverified = lines.map(l => l.kind === 'contract' ? { ...l, attributes: l.attributes.filter(a => a.name !== 'isproxy' && a.name !== 'proxytype') } : l);
    const p = plan(reverified, lookups);
    expect(p.levels[3]!.map(o => o.op)).toEqual(['patch']);
    const op = p.levels[3]![0]!;
    if (op.op !== 'patch') throw new Error('patch expected');
    expect(op.target.key).toBe(chainKey(99));
    const { set, unset } = patchMutations(op.target.attributes, remap(op.line, p.keys));
    expect(unset).toEqual(['isproxy', 'proxytype']);
    expect(set.some(a => a.name === 'arkiv.schema')).toBe(false);
    expect(set.find(a => a.name === 'matchid')!.value).toBe(contractLine().attributes.find(a => a.name === 'matchid')!.value);
  });
  test('the same matchId with other content is a correction: patched as well', () => {
    const lookups = allDependenciesOnChain();
    const current = remap(contractLine(), keysOf(lookups)).map(a => a.name === 'name' ? { ...a, value: 'Renamed' } : a);
    lookups.contracts.set(contractLine().key, [{ key: chainKey(99), attributes: current }]);
    expect(plan(lines, lookups).levels[3]!.map(o => o.op)).toEqual(['patch']);
  });
  test('an older matchId than the chain\'s, or two contracts for the address, are skipped', () => {
    const lookups = allDependenciesOnChain();
    const newer = remap(contractLine(), keysOf(lookups)).map(a => a.name === 'matchid' ? { ...a, value: '0xffffffff' } : a);
    lookups.contracts.set(contractLine().key, [{ key: chainKey(99), attributes: newer }]);
    let p = plan(lines, lookups);
    expect(p.levels.flat().length).toBe(0);
    expect(p.skipped[0]!.reason).toContain('newer match');
    lookups.contracts.set(contractLine().key, [{ key: chainKey(98), attributes: newer }, { key: chainKey(99), attributes: newer }]);
    p = plan(lines, lookups);
    expect(p.skipped[0]!.reason).toContain('several');
  });
  test('a patch to a new compilation creates it first, in the level below', () => {
    const lookups = allDependenciesOnChain();
    const comp = lines.find(l => l.kind === 'compilation')!;
    lookups.dependencies.delete(comp.key);
    const current = remap(contractLine(), new Map([...keysOf(lookups), [comp.key, chainKey(77)]])).map(a => a.name === 'matchid' ? { ...a, value: '0x1' } : a);
    lookups.contracts.set(contractLine().key, [{ key: chainKey(99), attributes: current }]);
    const p = plan(lines, lookups);
    expect(p.levels.map(l => l.map(o => `${o.op} ${o.line.kind}`))).toEqual([[], [], ['create compilation'], ['patch contract']]);
  });
});

describe('chunks', () => {
  const chunk = (n: number): Line => ({ nonce: n, key: chainKey(1000 + n), kind: 'chunk', attributes: [{ name: 'arkiv.schema', type: 'str', indexed: true, value: `${PREFIX}.chunk` }, { name: 'data', type: 'bytes', indexed: false, value: '0xabcd' }] });
  const list = (keys: Hex[]) => `0x02${'11'.repeat(32)}00000004${keys.map(k => k.slice(2)).join('')}` as Hex;
  test('a chunk list names its chunks; a reused parent\'s chunks are not written again', () => {
    expect(chunkList(list([chainKey(1001), chainKey(1002)]))!.keys).toEqual([chainKey(1001), chainKey(1002)]);
    const source: Line = { nonce: 3, key: chainKey(2000), kind: 'source', attributes: [{ name: 'arkiv.schema', type: 'str', indexed: true, value: `${PREFIX}.source` },
      { name: 'content', type: 'bytes', indexed: false, value: list([chainKey(1001), chainKey(1002)]) }, { name: 'hash', type: 'bytes32', indexed: true, value: chainKey(7) }] };
    const batch = [chunk(1), chunk(2), source];
    expect(plan(batch, empty()).levels.map(l => l.length)).toEqual([2, 1, 0, 0]);
    const lookups = empty();
    lookups.dependencies.set(source.key, [chainKey(5)]);
    expect(plan(batch, lookups).levels.map(l => l.length)).toEqual([0, 0, 0, 0]);
  });
  test('an unchanged chunked contract value keeps the chain\'s chunks', () => {
    const base = contractLine();
    const contract: Line = { ...base, nonce: 9, attributes: base.attributes.map(a => a.name === 'data' ? { ...a, value: list([chainKey(1001), chainKey(1002)]) } : a) };
    const lookups = allDependenciesOnChain();
    const onChain = remap({ ...contract, attributes: contract.attributes.map(a => a.name === 'data' ? { ...a, value: list([chainKey(3001), chainKey(3002)]) } : a) },
      new Map([...keysOf(lookups), [chainKey(3001), chainKey(3001)], [chainKey(3002), chainKey(3002)]]));
    lookups.contracts.set(contract.key, [{ key: chainKey(99), attributes: onChain }]);
    const p = plan([...lines.filter(l => l.kind !== 'contract'), chunk(1), chunk(2), contract], lookups);
    expect(p.alreadySynced).toBe(1);
    expect(p.levels.flat().length).toBe(0);
    expect(p.keys.get(chainKey(1001))).toBe(chainKey(3001));
  });
});

describe('operations', () => {
  const CREATE = parseAbiParameters('(uint128 salt, uint64 expiresAt, uint64 minLifetime, uint8 creationFlags, (bytes32 name, uint8 typeId, bytes value)[] attributes)');
  const PATCH = parseAbiParameters('(bytes32 entityKey, (bytes32 name, uint8 typeId, bytes value)[] mutations)');
  const name = (hex: Hex) => Buffer.from(hex.slice(2), 'hex').toString().replace(/\0+$/, '');
  test('a create is permanent, salt 0, attributes in name order with their indexed flags', async () => {
    const line = lines.find(l => l.kind === 'source')!;
    const op = await operation({ op: 'create', attributes: line.attributes });
    expect(op.operation).toBe(1);
    const [decoded] = decodeAbiParameters(CREATE, op.operationData) as any;
    expect(decoded.salt).toBe(0n);
    expect(decoded.expiresAt).toBe(2n ** 64n - 1n);
    expect(decoded.attributes.map((a: any) => name(a.name))).toEqual(['arkiv.schema', 'content', 'hash']);
    expect(decoded.attributes.map((a: any) => a.typeId)).toEqual([0x88, 0x07, 0x86]);
  });
  test('a patch sets and unsets (tombstones) in name order', async () => {
    const op = await operation({ op: 'patch', key: chainKey(5), set: [{ name: 'matchid', type: 'u64', indexed: true, value: '0x2a' }, { name: 'data', type: 'bytes', indexed: false, value: '0x0001' }], unset: ['isproxy'] });
    expect(op.operation).toBe(2);
    const [decoded] = decodeAbiParameters(PATCH, op.operationData) as any;
    expect(decoded.entityKey).toBe(chainKey(5));
    expect(decoded.mutations.map((m: any) => [name(m.name), m.typeId])).toEqual([['data', 7], ['isproxy', 0], ['matchid', 0x83]]);
  });
  test('packing keeps order, the calldata budget and the operation cap', async () => {
    const items = await Promise.all(Array.from({ length: 20 }, async (_, i) => ({ i, operation: await operation({ op: 'create', attributes: [{ name: 'data', type: 'bytes', indexed: false, value: `0x${'ab'.repeat(i === 5 ? 50_000 : 1_000)}` }] }) })));
    const batches = await packOperations(items, 30_000, 4);
    expect(batches.flatMap(b => b.items.map(x => x.i))).toEqual(items.map(x => x.i));
    for (const b of batches) {
      expect(b.items.length).toBeLessThanOrEqual(4);
      if (b.items.length > 1) expect(b.bytes).toBeLessThanOrEqual(30_000);
    }
    expect(batches.find(b => b.items.some(x => x.i === 5))!.items.length).toBe(1);
  });
  test('attribute comparison follows the node\'s spellings', () => {
    expect(sameAttributes([{ name: 'a', type: 'u64', indexed: true, value: '0x10' }], [{ name: 'a', type: 'u64', indexed: true, value: '0x10' }])).toBe(true);
    expect(sameAttributes([{ name: 'a', type: 'addr', indexed: true, value: '0xABcd' }], [{ name: 'a', type: 'addr', indexed: true, value: '0xabcd' }])).toBe(true);
    expect(sameAttributes([{ name: 'a', type: 'u64', indexed: true, value: '0x10' }], [{ name: 'a', type: 'u64', indexed: false, value: '0x10' }])).toBe(false);
  });
});
