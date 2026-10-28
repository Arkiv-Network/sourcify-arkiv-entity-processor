/**
 * The UDC (v4) writer of the sync (arkiv/udc.ts), offline: reading a v4 bundle, references and their rewriting to
 * chain keys, the plan (create, reuse, patch in place, already synced, skip) and operation encoding and packing. The
 * bundle is the SafeProxy record processed by the release binary; nothing here talks to a chain.
 */
import { afterAll, beforeAll, describe, expect, test } from 'bun:test';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { ENTITY_EVENTS_ABI, predictEntityKey } from '@arkiv-network/sdk-udc';
import { decodeAbiParameters, decodeFunctionData, encodeAbiParameters, encodeEventTopics, parseAbiParameters, type Hex } from 'viem';
import { highestMatchId } from '../arkiv/sync';
import { busyRetryTransport, chunkList, dependencyMismatch, lookup, matchIdProbe, operation, packOperations, patchMutations, plan, readBundle, references, remap, sameAttributes, udcInternals, write, type Attr, type ChainEntity, type Line, type Lookups } from '../arkiv/udc';

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
const OWNER = '0x3ffcb8c66ba2a35df36d73b213399220fd59f42f';
/** A bundle line as the chain would hold it under `key` (its references are compared by kind only). */
const onChain = (line: Line, key: Hex): ChainEntity => ({ key, owner: OWNER, creator: OWNER, expiresAt: '0xffffffffffffffff', attributes: line.attributes });
/** Every dependency of the bundle already on the chain, under chain keys 1, 2, ... */
function allDependenciesOnChain(): Lookups {
  const lookups = empty();
  lines.filter(l => l.kind !== 'contract' && l.kind !== 'chunk').forEach((l, i) => lookups.dependencies.set(l.key, [onChain(l, chainKey(i + 1))]));
  return lookups;
}
const keysOf = (lookups: Lookups) => new Map([...lookups.dependencies].map(([k, v]) => [k, v[0]!.key]));

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
    const code: Line = { nonce: 3, key: chainKey(2000), kind: 'code', attributes: [{ name: 'arkiv.schema', type: 'str', indexed: true, value: `${PREFIX}.code` },
      { name: 'code', type: 'bytes', indexed: false, value: list([chainKey(1001), chainKey(1002)]) }, { name: 'hash', type: 'bytes32', indexed: true, value: chainKey(7) }] };
    const contract: Line = { nonce: 4, key: chainKey(2001), kind: 'contract', attributes: [{ name: 'address', type: 'addr', indexed: true, value: `0x${'12'.repeat(20)}` },
      { name: 'arkiv.schema', type: 'str', indexed: true, value: `${PREFIX}.contract` }, { name: 'matchid', type: 'u64', indexed: true, value: '0x5' },
      { name: 'runtimecoderef', type: 'key', indexed: false, value: code.key }] };
    const batch = [chunk(1), chunk(2), code, contract];
    expect(plan(batch, empty()).levels.map(l => l.length)).toEqual([2, 1, 0, 1]);
    const lookups = empty();
    lookups.dependencies.set(code.key, [onChain(code, chainKey(5))]);
    expect(plan(batch, lookups).levels.map(l => l.length)).toEqual([0, 0, 0, 1]);
    // Without a contract that is written, nothing is: dependencies of a skipped contract stay unwritten.
    expect(plan([chunk(1), chunk(2), code], empty()).levels.map(l => l.length)).toEqual([0, 0, 0, 0]);
    lookups.contracts.set(contract.key, [onChain({ ...contract, attributes: contract.attributes.map(a => a.name === 'matchid' ? { ...a, value: '0x9' } : a) }, chainKey(6))]);
    expect(plan(batch, { dependencies: new Map(), contracts: lookups.contracts }).levels.map(l => l.length)).toEqual([0, 0, 0, 0]);
  });
  test('a dependency found by hash is reused only when it is permanent, the owner\'s and the same content', () => {
    const source = lines.find(l => l.kind === 'source')!;
    const good = onChain(source, chainKey(5));
    const cases: [ChainEntity, string][] = [
      [{ ...good, expiresAt: '0x1000' }, 'not permanent'],
      [{ ...good, creator: `0x${'99'.repeat(20)}` }, 'another owner'],
      [{ ...good, attributes: good.attributes.map(a => a.name === 'content' ? { ...a, value: '0x0001' } : a) }, 'content differs'],
      [{ ...good, attributes: good.attributes.map(a => a.name === 'hash' ? { ...a, indexed: false } : a) }, 'hash differs'],
    ];
    const comp = lines.find(l => l.kind === 'compilation')!;
    for (const [bad, why] of cases) {
      expect(dependencyMismatch(source, bad, OWNER)).toBeDefined();
      // A new compilation names the source, so the source is needed.
      const lookups = allDependenciesOnChain();
      lookups.dependencies.delete(comp.key);
      lookups.dependencies.set(source.key, [bad]);
      const p = plan(lines, lookups, OWNER);
      expect(p.levels[1]!.map(o => o.line.key)).toContain(source.key);
      expect(p.mismatched.length).toBe(1);
      // A second, matching one is used instead.
      lookups.dependencies.set(source.key, [bad, good]);
      expect(plan(lines, lookups, OWNER).keys.get(source.key)).toBe(chainKey(5));
      void why;
    }
    expect(dependencyMismatch(source, good, OWNER)).toBeUndefined();
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

/** An in-memory UDC chain: `execute` calldata applied to a key-value store, receipts with the engine's events, and
 * `arkiv_query` for the queries the writer sends. Faults: revert the n-th transaction, drop its events, or store an
 * attribute differently from what was sent. */
class FakeChain {
  entities = new Map<string, { owner: string; attributes: Attr[] }>();
  order: string[] = [];
  receipts = new Map<string, any>();
  entityNonce = 0n;
  txNonce = 0;
  sent = 0;
  constructor(public owner: string, public faults: { revert?: number; dropEvents?: number; corrupt?: string } = {}) {}
  async request({ method, params }: { method: string; params: any[] }) {
    if (method !== 'arkiv_query') throw new Error(`unexpected ${method}`);
    const [text, options] = params;
    const limit = Number(BigInt(options.limit));
    let rows = this.order.map(k => ({ key: k, ...this.entities.get(k)! }));
    const keys = [...String(text).matchAll(/\$key = key\((0x[0-9a-f]+)\)/g)].map(m => m[1]);
    if (keys.length) rows = rows.filter(r => keys.includes(r.key));
    for (const [, name, type, value] of String(text).matchAll(/(?<![$\w.])([a-z][a-z.]*) = (str|bytes32|addr|u64)\(([^)]*)\)/g)) {
      const want = type === 'str' ? value!.replace(/^'|'$/g, '') : value!;
      rows = rows.filter(r => { const a = r.attributes.find(x => x.name === name); return a && (type === 'u64' ? BigInt(a.value) === BigInt(want) : String(a.value).toLowerCase() === want.toLowerCase()); });
    }
    const above = /matchid > u64\((\d+)\)/.exec(String(text));
    if (above) rows = rows.filter(r => BigInt(r.attributes.find(x => x.name === 'matchid')?.value ?? -1) > BigInt(above[1]!));
    const owner = /\$owner = addr\((0x[0-9a-f]+)\)/.exec(String(text));
    if (owner) rows = rows.filter(r => r.owner === owner[1]);
    const page = rows.slice(0, limit);
    return { blockNumber: '0x1', ...(rows.length > limit ? { cursor: 'more' } : {}), data: page.map(r => ({ key: r.key, owner: r.owner, creator: r.owner,
      expiresAt: '0xffffffffffffffff', attributeSchema: r.attributes.map(a => ({ name: a.name, type: a.type, indexed: a.indexed })),
      attributes: r.attributes.map(a => ({ name: a.name, type: a.type, value: a.value })) })) };
  }
  async getTransactionCount() { return this.txNonce; }
  async estimateGas() { return 100_000n; }
  async getGasPrice() { return 1n; }
  async getTransactionReceipt({ hash }: { hash: string }) { return this.receipts.get(hash); }
  wallet() {
    return { account: { address: this.owner }, sendTransaction: async ({ data }: { data: Hex }) => this.execute(data) };
  }
  async execute(data: Hex) {
    const { EXECUTE_ABI } = await udcInternals();
    const { args } = decodeFunctionData({ abi: EXECUTE_ABI, data }) as any;
    const hash = `0x${(++this.sent).toString(16).padStart(64, '0')}`;
    this.txNonce++;
    if (this.faults.revert === this.sent) { this.receipts.set(hash, { status: 'reverted', gasUsed: 1n, logs: [], blockNumber: 1n }); return hash; }
    const logs: any[] = [];
    for (const op of args[0]) {
      if (op.operation === 1) {
        const [create] = decodeAbiParameters(parseAbiParameters('(uint128 salt, uint64 expiresAt, uint64 minLifetime, uint8 creationFlags, (bytes32 name, uint8 typeId, bytes value)[] attributes)'), op.operationData) as any;
        const key = predictEntityKey({ owner: this.owner as Hex, nonce: this.entityNonce++, salt: create.salt, chainId: 7706815 });
        this.entities.set(key, { owner: this.owner, attributes: create.attributes.map(fromAbi) });
        this.order.push(key);
        logs.push(this.log('EntityCreated', { entityKey: key, owner: this.owner, expiresAt: create.expiresAt, creationFlags: 0 }));
      } else if (op.operation === 2) {
        const [patch] = decodeAbiParameters(parseAbiParameters('(bytes32 entityKey, (bytes32 name, uint8 typeId, bytes value)[] mutations)'), op.operationData) as any;
        const e = this.entities.get(patch.entityKey)!;
        for (const m of patch.mutations) {
          const name = nameOf(m.name);
          e.attributes = e.attributes.filter(a => a.name !== name);
          if (m.typeId !== 0) e.attributes.push(fromAbi(m));
        }
        e.attributes.sort((a, b) => (a.name < b.name ? -1 : 1));
        logs.push(this.log('EntityPatched', { entityKey: patch.entityKey, owner: this.owner }));
      } else throw new Error(`unexpected operation ${op.operation}`);
    }
    if (this.faults.corrupt) for (const e of this.entities.values()) for (const a of e.attributes) if (a.name === this.faults.corrupt) a.indexed = !a.indexed;
    this.receipts.set(hash, { status: 'success', gasUsed: 21_000n, logs: this.faults.dropEvents === this.sent ? logs.slice(1) : logs, blockNumber: 1n });
    return hash;
  }
  log(eventName: string, args: any) {
    const event = ENTITY_EVENTS_ABI.find(e => e.name === eventName)!;
    const data = encodeAbiParameters(event.inputs.filter((i: any) => !i.indexed), event.inputs.filter((i: any) => !i.indexed).map((i: any) => args[i.name]));
    return { address: '0x4400000000000000000000000000000000000044', topics: encodeEventTopics({ abi: ENTITY_EVENTS_ABI, eventName, args } as any), data };
  }
}
const nameOf = (hex: Hex) => Buffer.from(hex.slice(2), 'hex').toString().replace(/\0+$/, '');
const TYPES: Record<number, string> = { 1: 'bool', 2: 'i32', 3: 'u64', 6: 'bytes32', 7: 'bytes', 8: 'str', 9: 'addr', 10: 'key' };
function fromAbi(a: { name: Hex; typeId: number; value: Hex }): Attr {
  const type = TYPES[a.typeId & 0x7f]!, v = a.value;
  const value = type === 'u64' ? `0x${BigInt(v).toString(16)}` : type === 'bool' ? BigInt(v) === 1n : type === 'i32' ? Number(BigInt.asIntN(32, BigInt(v)))
    : type === 'str' ? Buffer.from(v.slice(2), 'hex').toString() : type === 'addr' ? `0x${v.slice(-40)}` : v.toLowerCase();
  return { name: nameOf(a.name), type, indexed: (a.typeId & 0x80) !== 0, value };
}
const context = (chain: FakeChain) => ({ client: chain, wallet: chain.wallet(), owner: OWNER, dryRun: false, maxTxBytes: 120_000, maxTxOps: 16,
  inFlight: 4, maxGasPrice: 10n, gas: 'estimate' as const, log: () => {} });

describe('writing to a chain (in memory)', () => {
  test('a batch is written in levels, read back, and found again as synced', async () => {
    const chain = new FakeChain(OWNER);
    const p = plan(lines, await lookup(chain, lines, OWNER, PREFIX), OWNER);
    const sent = await write(p, context(chain));
    expect(sent.created).toBe(6);
    expect(chain.entities.size).toBe(6);
    // Every reference on the chain names a chain entity of the right kind.
    for (const [key, e] of chain.entities) for (const r of references({ kind: kindOf(e), attributes: e.attributes })) {
      expect(chain.entities.has(r.key)).toBe(true);
      expect(kindOf(chain.entities.get(r.key)!)).toBe(r.kind);
      void key;
    }
    const again = plan(lines, await lookup(chain, lines, OWNER, PREFIX), OWNER);
    expect([again.alreadySynced, again.reused, again.levels.flat().length]).toEqual([1, 5, 0]);
  });
  test('a re-verification patches the contract in place: same key, new attributes, the missing ones gone', async () => {
    const chain = new FakeChain(OWNER);
    await write(plan(lines, await lookup(chain, lines, OWNER, PREFIX), OWNER), context(chain));
    const before = [...chain.entities].find(([, e]) => kindOf(e) === 'contract')![0];
    const reverified = lines.map(l => l.kind === 'contract' ? { ...l, attributes: l.attributes.filter(a => a.name !== 'isproxy' && a.name !== 'proxytype')
      .map(a => a.name === 'matchid' ? { ...a, value: '0x3938701' } : a) } : l);
    const p = plan(reverified, await lookup(chain, reverified, OWNER, PREFIX), OWNER);
    expect(p.levels[3]!.map(o => o.op)).toEqual(['patch']);
    const sent = await write(p, context(chain));
    expect([sent.created, sent.patched]).toEqual([0, 1]);
    const after = chain.entities.get(before)!;
    expect(after.attributes.find(a => a.name === 'matchid')!.value).toBe('0x3938701');
    expect(after.attributes.some(a => a.name === 'isproxy' || a.name === 'proxytype')).toBe(false);
    expect(chain.entities.size).toBe(6);
  });
  test('a reverted transaction stops the batch before anything names what it would have created', async () => {
    const chain = new FakeChain(OWNER, { revert: 1 });
    const p = plan(lines, await lookup(chain, lines, OWNER, PREFIX), OWNER);
    await expect(write(p, { ...context(chain), maxTxOps: 1, inFlight: 1 })).rejects.toThrow('reverted');
    expect([...chain.entities.values()].some(e => kindOf(e) === 'compilation' || kindOf(e) === 'contract')).toBe(false);
  });
  test('missing events and a different read-back fail the batch', async () => {
    let chain = new FakeChain(OWNER, { dropEvents: 1 });
    await expect(write(plan(lines, await lookup(chain, lines, OWNER, PREFIX), OWNER), context(chain))).rejects.toThrow('events');
    chain = new FakeChain(OWNER, { corrupt: 'hash' });
    await expect(write(plan(lines, await lookup(chain, lines, OWNER, PREFIX), OWNER), context(chain))).rejects.toThrow('read back differently');
  });
  test('the start-cursor probe accepts a cursor and finds the highest matchid', async () => {
    const chain = new FakeChain(OWNER);
    for (let i = 1; i <= 40; i++) {
      const key = chainKey(5000 + i);
      chain.entities.set(key, { owner: OWNER, attributes: [{ name: 'arkiv.schema', type: 'str', indexed: true, value: `${PREFIX}.contract` }, { name: 'matchid', type: 'u64', indexed: true, value: `0x${(i * 7).toString(16)}` }] });
      chain.order.push(key);
    }
    const probe = matchIdProbe(chain, OWNER, PREFIX);
    expect(await probe(0n)).toBeDefined();
    expect(await highestMatchId(probe, 300n)).toBe(280n);
  });
});
const kindOf = (e: { attributes: Attr[] }) => String(e.attributes.find(a => a.name === 'arkiv.schema')!.value).split('.').pop() as any;

describe('the busy-retry transport', () => {
  test('a busy node is asked again; a cost limit and other errors are not', async () => {
    let answers: Response[] = [];
    let calls = 0;
    const server = Bun.serve({ port: 0, fetch: () => { calls++; return answers.shift() ?? Response.json({ jsonrpc: '2.0', id: 1, result: '0x7598bf' }); } });
    try {
      const transport = busyRetryTransport(`http://127.0.0.1:${server.port}`)({ chain: undefined } as any);
      answers = [new Response('{"error":"busy"}', { status: 429 }), Response.json({ jsonrpc: '2.0', id: 1, error: { code: -32010, message: 'ServerBusy' } })];
      expect(await transport.request({ method: 'eth_chainId' })).toBe('0x7598bf');
      expect(calls).toBe(3);
      calls = 0;
      answers = Array.from({ length: 10 }, () => new Response('{"error":"ANON_COST_LIMITED"}', { status: 429 }));
      await expect(transport.request({ method: 'eth_chainId' })).rejects.toThrow('cost limit');
      expect(calls).toBe(1);
      calls = 0;
      answers = Array.from({ length: 10 }, () => new Response('{"error":"busy"}', { status: 429 }));
      await expect(transport.request({ method: 'eth_chainId' })).rejects.toThrow('429');
      expect(calls).toBe(6);
      answers = [Response.json({ jsonrpc: '2.0', id: 1, error: { code: -32000, message: 'nope' } })];
      await expect(transport.request({ method: 'eth_chainId' })).rejects.toThrow('nope');
    } finally { server.stop(true); }
  }, 20_000);
});
