/**
 * The UDC (v4) writer of the Postgres -> Arkiv sync (docs/udc-proposal.md, "Migration > Processor"), on the UDC SDK
 * (@arkiv-network/sdk 0.9.0-next.2, installed as @arkiv-network/sdk-udc next to the v3 sync's 0.8 SDK).
 *
 * One batch's records become a v4 bundle (`arkiv-entity-processor process --format udc`); its entities carry bundle keys
 * (salt-0 keys of the bundle's own nonces). This module writes them to the chain:
 *
 *  - Dependencies are deduplicated against the chain by content hash: a source, code or compilation already there
 *    (`$owner`, `arkiv.schema` and `hash`) is reused, and its chunks are not written again.
 *  - A contract is looked up by address (`$owner`, `arkiv.schema`, `chainid = 1`, `address`): none -> created; the same
 *    content -> already synced; another version with a matchId at least the chain's -> **patched in place** (its key stays,
 *    every attribute is set, attributes the new version lacks are unset); an older matchId -> skipped; two -> skipped.
 *  - New entities go in four levels, each mined before the next is sent: chunks; sources and codes; compilations;
 *    contracts (creates and patches). Within a level no operation names another, so a level's transactions are
 *    independent; the keys come from the receipts (EntityCreated, salt 0) and every reference of the next level is
 *    rewritten from bundle keys to chain keys (key attributes, `sourcerefs`, chunk lists). A reverted transaction stops
 *    the batch before anything names what it would have created.
 *  - Every created or patched entity is read back by key and compared attribute by attribute.
 */
import { createPublicClient, createWalletClient, ENTITY_EVENTS_ABI, MAX_EXPIRES_AT } from '@arkiv-network/sdk-udc';
import { custom, decodeEventLog, encodeFunctionData, TransactionReceiptNotFoundError, type Hex } from 'viem';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const UDC_SDK_VERSION = '0.9.0-next.2';
export const ARKIV_ADDRESS = '0x4400000000000000000000000000000000000044';
export const KEY_BATCH = 8;
export const KINDS = ['contract', 'compilation', 'source', 'code', 'chunk'] as const;
export type Kind = (typeof KINDS)[number];
/** An attribute in the node's RPC encoding (bytes as 0x hex), with its indexed flag. */
export type Attr = { name: string; type: string; indexed: boolean; value: any };
/** One bundle entity: its bundle key and kind, attributes in name order. */
export type Line = { nonce: number; key: Hex; kind: Kind; attributes: Attr[] };
/** A contract as the chain holds it. */
export type ChainEntity = { key: Hex; owner?: string; creator?: string; expiresAt?: string; attributes: Attr[] };

let internals: Promise<any> | undefined;
/** createOperation, patchOperation, EXECUTE_ABI, encodeAbiAttribute, encodeTombstone of the pinned UDC SDK. */
export function udcInternals(): Promise<any> {
  return internals ??= (async () => {
    const root = dirname(fileURLToPath(import.meta.resolve('@arkiv-network/sdk-udc/package.json')));
    const { version } = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
    if (version !== UDC_SDK_VERSION) throw new Error(`UDC operation encoding is checked against @arkiv-network/sdk ${UDC_SDK_VERSION}, installed is ${version}`);
    const load = (path: string) => import(pathToFileURL(join(root, 'src', path)).href);
    const [codec, operations, values] = await Promise.all([load('schema/codec.ts'), load('entity/operations.ts'), load('schema/values.ts')]);
    return { ...codec, ...operations, values };
  })();
}

const lower = (h: string) => h.toLowerCase() as Hex;
const toHex = (bytes: Uint8Array) => `0x${Buffer.from(bytes).toString('hex')}` as Hex;
const fromHex = (hex: string) => new Uint8Array(Buffer.from(hex.slice(2), 'hex'));
const attr = (line: { attributes: Attr[] }, name: string) => line.attributes.find(a => a.name === name);

/** Reads a v4 bundle written by `process --format udc` (seed.json version 2), in creation order. */
export function readBundle(dir: string, prefix: string): Line[] {
  const seed = JSON.parse(readFileSync(join(dir, 'seed.json'), 'utf8'));
  if (seed.version !== 2 || seed.format !== 'sourcify-arkiv.udc-bundle.v1') throw new Error('not a v4 (UDC) bundle');
  if (seed.schemaPrefix !== prefix) throw new Error(`the bundle's schema prefix is ${seed.schemaPrefix}, not ${prefix}`);
  const lines: Line[] = [];
  for (const { path } of seed.entities) {
    for (const text of readFileSync(join(dir, path), 'utf8').split('\n')) {
      if (!text) continue;
      const raw = JSON.parse(text);
      const attributes: Attr[] = raw.attributes.map((a: any) => ({ name: a.name, type: a.type, indexed: a.indexed,
        value: a.type === 'bytes' ? toHex(Buffer.from(a.value, 'base64')) : a.value }));
      const tag = attributes.find(a => a.name === 'arkiv.schema')?.value as string | undefined;
      const kind = tag?.startsWith(`${prefix}.`) ? tag.slice(prefix.length + 1) : undefined;
      if (!kind || !(KINDS as readonly string[]).includes(kind)) throw new Error(`bundle entity ${raw.key} has arkiv.schema ${tag}`);
      if (raw.expiresAt !== '0xffffffffffffffff') throw new Error(`bundle entity ${raw.key} is not permanent`);
      lines.push({ nonce: raw.nonce, key: lower(raw.key), kind: kind as Kind, attributes });
    }
  }
  return lines;
}

const KEY_TARGET: Record<string, Kind> = { compilationref: 'compilation', creationcoderef: 'code', runtimecoderef: 'code' };
/** The chunk keys of a chunk list (codec 2: sha256 ‖ u32 length ‖ keys), or undefined. */
export function chunkList(value: Hex): { header: Hex; keys: Hex[] } | undefined {
  const raw = fromHex(value);
  if (raw[0] !== 2) return undefined;
  if (raw.length < 37 || (raw.length - 37) % 32) throw new Error('malformed chunk list');
  const keys: Hex[] = [];
  for (let i = 37; i < raw.length; i += 32) keys.push(toHex(raw.subarray(i, i + 32)));
  return { header: toHex(raw.subarray(0, 37)), keys };
}
/** Every key an entity names, with the kind it must have (key attributes, `sourcerefs`, chunk lists). */
export function references(line: { kind: Kind; attributes: Attr[] }): { key: Hex; kind: Kind }[] {
  const out: { key: Hex; kind: Kind }[] = [];
  for (const a of line.attributes) {
    if (a.type === 'key') {
      const kind = KEY_TARGET[a.name];
      if (!kind) throw new Error(`unknown key attribute ${a.name}`);
      out.push({ key: lower(a.value), kind });
    } else if (a.type === 'bytes' && line.kind !== 'chunk') {
      if (a.name === 'sourcerefs') {
        // Inline only (codec 0): a chunked one would hide its keys inside chunk data, under a digest.
        const raw = fromHex(a.value);
        if (raw[0] !== 0 || raw.length < 33 || (raw.length - 1) % 32) throw new Error('malformed sourcerefs');
        for (let i = 1; i < raw.length; i += 32) out.push({ key: toHex(raw.subarray(i, i + 32)), kind: 'source' });
        continue;
      }
      const list = chunkList(a.value);
      if (list) out.push(...list.keys.map(key => ({ key, kind: 'chunk' as Kind })));
    }
  }
  return out;
}
/** The attributes with every reference rewritten from bundle keys to chain keys; an unmapped one is refused. */
export function remap(line: { kind: Kind; attributes: Attr[] }, keys: Map<string, Hex>): Attr[] {
  const map = (key: string) => {
    const mapped = keys.get(lower(key));
    if (!mapped) throw new Error(`a ${line.kind} names ${key}, which has no chain key`);
    return mapped;
  };
  const swap = (raw: Uint8Array, from: number) => {
    const out = new Uint8Array(raw);
    for (let i = from; i < raw.length; i += 32) out.set(fromHex(map(toHex(raw.subarray(i, i + 32)))), i);
    return toHex(out);
  };
  return line.attributes.map(a => {
    if (a.type === 'key') return { ...a, value: map(a.value) };
    if (a.type !== 'bytes' || line.kind === 'chunk') return a;
    const raw = fromHex(a.value);
    if (a.name === 'sourcerefs') {
      if (raw[0] !== 0) throw new Error('sourcerefs must be inline (codec 0)');
      return { ...a, value: swap(raw, 1) };
    }
    if (raw[0] === 2) return { ...a, value: swap(raw, 37) };
    return a;
  });
}

/** An attribute value in one spelling for comparison (the node returns addresses lowercase, u64 as quantities). */
function normal(a: Attr): string {
  const v = a.value;
  switch (a.type) {
    case 'u64': case 'u256': return BigInt(v).toString();
    case 'i32': return String(Number(v));
    case 'bool': return String(v);
    case 'str': return String(v);
    default: return String(v).toLowerCase();
  }
}
/** Two attribute sets are equal: names, types, indexed flags and values. */
export function sameAttributes(a: Attr[], b: Attr[]) {
  if (a.length !== b.length) return false;
  const byName = new Map(b.map(x => [x.name, x]));
  return a.every(x => { const y = byName.get(x.name); return !!y && y.type === x.type && y.indexed === x.indexed && normal(y) === normal(x); });
}
/** What a patch from `current` to `next` sets (every attribute of `next` but `arkiv.schema`) and unsets. */
export function patchMutations(current: Attr[], next: Attr[]) {
  const names = new Set(next.map(a => a.name));
  return { set: next.filter(a => a.name !== 'arkiv.schema'), unset: current.filter(a => !names.has(a.name)).map(a => a.name) };
}

export type Lookups = {
  /** The chain's source, code and compilation entities with a bundle line's hash, by bundle key. */
  dependencies: Map<string, ChainEntity[]>;
  /** The chain's contract entities for a bundle contract's address, by bundle key. */
  contracts: Map<string, ChainEntity[]>;
};
export type Operation =
  | { op: 'create'; line: Line }
  | { op: 'patch'; line: Line; target: ChainEntity };
export type Plan = {
  /** Creates (and, last, patches) by level: chunks; sources and codes; compilations; contracts. */
  levels: Operation[][];
  /** Bundle key -> chain key of what the chain already has. */
  keys: Map<string, Hex>;
  reused: number;
  alreadySynced: number;
  skipped: { matchId?: string; address?: string; reason: string }[];
  duplicates: { kind: Kind; hash: string }[];
  /** Dependencies found on the chain by hash but not reused: not permanent, of another creator, or other content. */
  mismatched: { kind: Kind; hash: string; key: Hex; reason: string }[];
};
const LEVEL: Record<Kind, number> = { chunk: 0, source: 1, code: 1, compilation: 2, contract: 3 };

/** Why a dependency found on the chain by hash cannot stand in for a bundle line, or undefined if it can: it must be
 * permanent, created by the owner, and hold the line's attributes (names, types, indexed flags, values), comparing
 * a chunk list by its digest and length and a reference by its kind only (its key is the chain's). */
export function dependencyMismatch(line: Line, found: ChainEntity, owner?: string): string | undefined {
  if (found.expiresAt !== undefined && BigInt(found.expiresAt) !== MAX_EXPIRES_AT) return 'not permanent';
  if (owner && ((found.owner && found.owner !== owner) || (found.creator && found.creator !== owner))) return 'another owner or creator';
  if (found.attributes.length !== line.attributes.length) return 'other attributes';
  const theirs = new Map(found.attributes.map(a => [a.name, a]));
  for (const a of line.attributes) {
    const b = theirs.get(a.name);
    if (!b || b.type !== a.type || b.indexed !== a.indexed) return `attribute ${a.name} differs`;
    if (a.type === 'key') continue;
    if (a.type === 'bytes' && line.kind !== 'chunk') {
      const ours = a.name === 'sourcerefs' ? undefined : chunkList(a.value), other = ours && chunkList(b.value);
      if (ours) {
        if (!other || other.header !== ours.header || other.keys.length !== ours.keys.length) return `attribute ${a.name} differs`;
        continue;
      }
      if (a.name === 'sourcerefs') {
        if (String(b.value).length !== String(a.value).length || !String(b.value).startsWith('0x00')) return 'sourcerefs differ';
        continue;
      }
    }
    if (normal(a) !== normal(b)) return `attribute ${a.name} differs`;
  }
  return undefined;
}

/** Decides, for one batch, what to create, reuse, patch and skip (no I/O). Creates only what a created or patched
 * contract reaches: the dependencies of a skipped contract are not written. */
export function plan(lines: Line[], lookups: Lookups, owner?: string): Plan {
  const keys = new Map<string, Hex>();
  const result: Plan = { levels: [[], [], [], []], keys, reused: 0, alreadySynced: 0, skipped: [], duplicates: [], mismatched: [] };
  const byKey = new Map(lines.map(l => [l.key, l]));
  // Dependencies first (bundle order: everything a line names comes before it).
  for (const line of lines) {
    if (line.kind === 'chunk' || line.kind === 'contract') continue;
    const found = lookups.dependencies.get(line.key) ?? [];
    const hash = attr(line, 'hash')!.value;
    if (found.length > 1) result.duplicates.push({ kind: line.kind, hash });
    const usable = found.find(f => {
      const reason = dependencyMismatch(line, f, owner);
      if (reason) result.mismatched.push({ kind: line.kind, hash, key: f.key, reason });
      return !reason;
    });
    if (usable) { keys.set(line.key, lower(usable.key)); result.reused++; }
  }
  const addresses = new Set<string>();
  for (const line of lines) {
    if (line.kind !== 'contract') continue;
    const address = String(attr(line, 'address')!.value).toLowerCase(), matchId = BigInt(attr(line, 'matchid')!.value).toString();
    if (addresses.has(address)) throw new Error(`batch holds two contracts for ${address}`);
    addresses.add(address);
    const skip = (reason: string) => result.skipped.push({ matchId, address, reason });
    const found = lookups.contracts.get(line.key) ?? [];
    if (found.length > 1) { skip('several contract entities for this address on the chain'); continue; }
    if (found.length === 0) { result.levels[3]!.push({ op: 'create', line }); continue; }
    const chain = found[0]!;
    // Chunks of an unchanged value are the chain's own: same digest and length, same split.
    for (const a of line.attributes) {
      const ours = a.type === 'bytes' ? chunkList(a.value) : undefined;
      const theirs = ours && attr(chain, a.name)?.type === 'bytes' ? chunkList(attr(chain, a.name)!.value) : undefined;
      if (ours && theirs && ours.header === lower(theirs.header) && ours.keys.length === theirs.keys.length)
        ours.keys.forEach((k, i) => keys.set(k, lower(theirs.keys[i]!)));
    }
    const mapped = references(line).every(r => keys.has(r.key));
    if (mapped && sameAttributes(remap(line, keys), chain.attributes)) { result.alreadySynced++; continue; }
    const theirs = BigInt(attr(chain, 'matchid')?.value ?? -1);
    if (BigInt(matchId) < theirs) { skip(`the chain holds a newer match (matchId ${theirs})`); continue; }
    result.levels[3]!.push({ op: 'patch', line, target: chain });
  }
  // What the contracts written reach, short of what the chain already has.
  const reach = new Set<string>();
  const visit = (line: Line) => {
    for (const r of references(line)) {
      if (keys.has(r.key) || reach.has(r.key)) continue;
      const target = byKey.get(r.key);
      if (!target) throw new Error(`${line.kind} ${line.key} names ${r.key}, which is neither in the batch nor on the chain`);
      if (target.kind !== r.kind) throw new Error(`${line.kind} ${line.key} names a ${target.kind} as a ${r.kind}`);
      reach.add(r.key);
      visit(target);
    }
  };
  for (const { line } of result.levels[3]!) visit(line);
  for (const line of lines) if (line.kind !== 'contract' && reach.has(line.key)) result.levels[LEVEL[line.kind]]!.push({ op: 'create', line });
  // Creates before patches in the contract level, each in bundle order.
  result.levels[3]!.sort((a, b) => (a.op === b.op ? a.line.nonce - b.line.nonce : a.op === 'create' ? -1 : 1));
  return result;
}

/** ABI attributes of a create, in name order. */
export async function encodeAttributes(attributes: Attr[]) {
  const { encodeAbiAttribute, values } = await udcInternals();
  const value = (a: Attr) => {
    switch (a.type) {
      case 'u64': return values.u64(BigInt(a.value));
      case 'i32': return values.i32(Number(a.value));
      case 'bool': return values.bool(Boolean(a.value));
      case 'str': return values.str(String(a.value));
      case 'addr': return values.addr(a.value);
      case 'bytes32': return values.bytes32(a.value);
      case 'key': return values.key(a.value);
      case 'bytes': return values.bytes(a.value);
      default: throw new Error(`unsupported attribute type ${a.type}`);
    }
  };
  return [...attributes].sort((x, y) => Buffer.compare(Buffer.from(x.name), Buffer.from(y.name))).map(a => encodeAbiAttribute(a.name, value(a), a.indexed));
}
/** The `execute` operation for a create (permanent, salt 0, no flags) or a patch (set and unset, in name order). */
export async function operation(item: { op: 'create'; attributes: Attr[] } | { op: 'patch'; key: Hex; set: Attr[]; unset: string[] }) {
  const { createOperation, patchOperation, encodeTombstone } = await udcInternals();
  if (item.op === 'create') return createOperation({ salt: 0n, expiry: { expiresAt: MAX_EXPIRES_AT, minLifetime: 0n }, creationFlags: 0, attributes: await encodeAttributes(item.attributes) });
  const mutations = [...await encodeAttributes(item.set), ...item.unset.map(name => encodeTombstone(name))]
    .sort((x: any, y: any) => (x.name < y.name ? -1 : x.name > y.name ? 1 : 0));
  if (!mutations.length) throw new Error('empty patch');
  return patchOperation({ entityKey: item.key, mutations });
}
export async function calldata(operations: unknown[]): Promise<Hex> {
  const { EXECUTE_ABI } = await udcInternals();
  return encodeFunctionData({ abi: EXECUTE_ABI, functionName: 'execute', args: [operations] });
}
/** Greedy, order-preserving packing into `execute` calls of at most `maxBytes` calldata and `maxOps` operations (an
 * operation larger than that travels alone). */
export async function packOperations<T extends { operation: unknown }>(items: T[], maxBytes: number, maxOps: number) {
  const size = async (ops: unknown[]) => ((await calldata(ops)).length - 2) / 2;
  const batches: { items: T[]; bytes: number }[] = [];
  let current: T[] = [];
  for (const item of items) {
    if (current.length && (current.length >= maxOps || await size([...current, item].map(i => i.operation)) > maxBytes)) {
      batches.push({ items: current, bytes: await size(current.map(i => i.operation)) }); current = [];
    }
    current.push(item);
  }
  if (current.length) batches.push({ items: current, bytes: await size(current.map(i => i.operation)) });
  return batches;
}

/** Whether a JSON-RPC answer is our node's "busy, ask again" (HTTP 429, code -32010 ServerBusy). Jalebi's anonymous
 * cost limit is also a 429, but its window resets in minutes: not busy. */
export const isBusy = (status: number, body: any) =>
  (status === 429 && !JSON.stringify(body ?? '').includes('COST_LIMITED')) || body?.error?.code === -32010;
/** JSON-RPC over HTTP that asks again while the node answers ServerBusy (our node does so with about eight requests in
 * flight): six attempts, 100, 200, 400, 800 and 1,600 ms apart, the policy of read-arkiv and the server too. Any other
 * error is the caller's. */
export function busyRetryTransport(rpcUrl: string, apiKey?: string, attempts = 6) {
  let id = 0;
  // viem's own retries stay off: a busy answer is asked again here, and anything else (a sent transaction included)
  // is the caller's to judge.
  return custom({
    async request({ method, params }: { method: string; params?: unknown }) {
      for (let attempt = 1; ; attempt++) {
        const response = await fetch(rpcUrl, { method: 'POST', signal: AbortSignal.timeout(30_000),
          headers: { 'content-type': 'application/json', ...(apiKey ? { 'X-Api-Key': apiKey } : {}) },
          body: JSON.stringify({ jsonrpc: '2.0', id: ++id, method, params: params ?? [] }) });
        const body: any = await response.json().catch(() => undefined);
        if (isBusy(response.status, body) && attempt < attempts) { await Bun.sleep(100 * 2 ** (attempt - 1)); continue; }
        if (response.status === 429) throw new Error(`${method}: HTTP 429${JSON.stringify(body ?? '').includes('COST_LIMITED') ? ', the anonymous cost limit (set ARKIV_RPC_API_KEY)' : ' (busy)'}`);
        if (body?.error) throw Object.assign(new Error(body.error.message ?? `${method} failed`), { code: body.error.code, data: body.error.data });
        if (!response.ok || !body || !('result' in body)) throw new Error(`${method}: HTTP ${response.status}`);
        return body.result;
      }
    },
  }, { retryCount: 0 });
}
/** The SDK client pair for a chain (read and write). */
export function udcClients(rpcUrl: string, chainId: number, account: any, apiKey?: string) {
  const transport = busyRetryTransport(rpcUrl, apiKey);
  const chain = { id: chainId, name: `arkiv-${chainId}`, nativeCurrency: { name: 'GLM', symbol: 'GLM', decimals: 18 }, rpcUrls: { default: { http: [rpcUrl] } } } as any;
  return { client: createPublicClient({ chain, transport, pollingInterval: 1000 }), wallet: createWalletClient({ chain, transport, account, pollingInterval: 1000 }) };
}
const SELECT_ALL = { key: true, owner: true, creator: true, expiresAt: true, attributeSchema: true, attributes: true };
/** An `arkiv_query` row as a ChainEntity (indexed flags from attributeSchema). */
export function chainEntity(row: any): ChainEntity {
  const flags = new Map<string, boolean>((row.attributeSchema ?? []).map((s: any) => [s.name, !!s.indexed]));
  return { key: lower(row.key), owner: row.owner?.toLowerCase(), creator: row.creator?.toLowerCase(), expiresAt: row.expiresAt,
    attributes: (row.attributes ?? []).map((a: any) => ({ name: a.name, type: a.type, indexed: a.indexed ?? flags.get(a.name) ?? false, value: a.value })) };
}
/** One `arkiv_query`. A continuation cursor is an error unless `more` is set: then the rows are a first page (enough
 * for "two or more"), and `more` (the result) says that there are others. */
export async function query(client: any, text: string, limit: number, select: any = SELECT_ALL, options: { allowMore?: boolean } = {}) {
  const result: any = await client.request({ method: 'arkiv_query', params: [text, { limit: `0x${limit.toString(16)}`, select }] });
  if (result.cursor && !options.allowMore) throw new Error(`query continues past its limit: ${text.slice(0, 120)}`);
  return (result.data ?? []).map(chainEntity) as ChainEntity[];
}
/** Entities by key, at most KEY_BATCH per query; every key must be found. A key just written may reach a node behind a
 * load balancer a little after its receipt, so a missing one is asked again (`attempts` times, a second apart). */
export async function byKeys(client: any, keys: Hex[], attempts = 1, delayMs = 1000) {
  const out = new Map<string, ChainEntity>();
  for (let i = 0; i < keys.length; i += KEY_BATCH) {
    const batch = keys.slice(i, i + KEY_BATCH);
    for (let attempt = 1; ; attempt++) {
      const missing = batch.filter(k => !out.has(lower(k)));
      for (const e of await query(client, missing.map(k => `$key = key(${k})`).join(' OR '), missing.length)) out.set(e.key, e);
      const still = batch.filter(k => !out.has(lower(k)));
      if (!still.length) break;
      if (attempt >= attempts) throw new Error(`entity ${still[0]} is not on the chain`);
      await Bun.sleep(delayMs);
    }
  }
  return out;
}

async function pool<T, R>(items: T[], limit: number, work: (item: T) => Promise<R>): Promise<R[]> {
  const results: R[] = new Array(items.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (next < items.length) { const index = next++; results[index] = await work(items[index]!); }
  }));
  return results;
}
/** The chain's dependencies with the bundle's hashes, and contracts at the bundle's addresses (at most two of each:
 * two contracts are ambiguous, and a second dependency is a fallback when the first does not match). */
export async function lookup(client: any, lines: Line[], owner: string, prefix: string, concurrency = 8): Promise<Lookups> {
  const dependencies = new Map<string, ChainEntity[]>(), contracts = new Map<string, ChainEntity[]>();
  const scope = (kind: Kind) => `$owner = addr(${owner}) AND arkiv.schema = str('${prefix}.${kind}')`;
  await pool(lines.filter(l => l.kind !== 'chunk'), concurrency, async line => {
    if (line.kind === 'contract') {
      contracts.set(line.key, await query(client, `${scope('contract')} AND chainid = u64(1) AND address = addr(${String(attr(line, 'address')!.value).toLowerCase()})`, 2, SELECT_ALL, { allowMore: true }));
    } else {
      dependencies.set(line.key, await query(client, `${scope(line.kind)} AND hash = bytes32(${attr(line, 'hash')!.value})`, 2, SELECT_ALL, { allowMore: true }));
    }
  });
  return { dependencies, contracts };
}

export type SendContext = {
  client: any; wallet: any; owner: string; dryRun: boolean; maxTxBytes: number; maxTxOps: number; inFlight: number;
  maxGasPrice: bigint; gas: 'estimate' | 'computed'; log: (event: string, fields?: Record<string, unknown>) => void;
};
export type Sent = { transactions: number; gasUsed: bigint; rentFee: bigint; calldataBytes: number; created: number; patched: number };

/** Writes a plan level by level; fills `plan.keys` with the chain keys of what it creates. */
export async function write(p: Plan, context: SendContext): Promise<Sent> {
  const out: Sent = { transactions: 0, gasUsed: 0n, rentFee: 0n, calldataBytes: 0, created: 0, patched: 0 };
  for (const [level, operations] of p.levels.entries()) {
    if (!operations.length) continue;
    const items = await Promise.all(operations.map(async o => {
      const attributes = remap(o.line, p.keys);
      if (o.op === 'create') return { o, attributes, operation: await operation({ op: 'create', attributes }) };
      const { set, unset } = patchMutations(o.target.attributes, attributes);
      return { o, attributes, operation: await operation({ op: 'patch', key: o.target.key, set, unset }) };
    }));
    const batches = await packOperations(items, context.maxTxBytes, context.maxTxOps);
    if (context.dryRun) {
      // Nothing is sent: placeholder keys let the next level be planned too.
      items.forEach((item, i) => { if (item.o.op === 'create') p.keys.set(item.o.line.key, `0x${'ee'.repeat(31)}${(level * 64 + (i % 64)).toString(16).padStart(2, '0')}` as Hex); });
      out.transactions += batches.length; out.calldataBytes += batches.reduce((s, b) => s + b.bytes, 0);
      context.log('dry-run', { level, operations: items.length, transactions: batches.length });
      continue;
    }
    const keys = await sendLevel(batches, context, out);
    items.forEach((item, i) => { if (item.o.op === 'create') p.keys.set(item.o.line.key, keys[i]!); });
    // Read back what this level wrote before anything names it.
    const written = items.map((item, i) => ({ key: item.o.op === 'create' ? keys[i]! : item.o.target.key, attributes: item.attributes }));
    const found = await byKeys(context.client, written.map(w => w.key), 5);
    for (const w of written) {
      const e = found.get(lower(w.key))!;
      const problems: string[] = [];
      if (e.owner !== context.owner || e.creator !== context.owner) problems.push('owner');
      if (BigInt(e.expiresAt ?? 0) !== MAX_EXPIRES_AT) problems.push('expiresAt');
      if (!sameAttributes(w.attributes, e.attributes)) problems.push('attributes');
      if (problems.length) throw new Error(`entity ${w.key} read back differently: ${problems.join(', ')}`);
    }
    out.created += items.filter(i => i.o.op === 'create').length;
    out.patched += items.filter(i => i.o.op === 'patch').length;
  }
  return out;
}

/** Sends one level's transactions, up to `inFlight` unmined, and returns the chain key of each create, in item order
 * (undefined for a patch). A reverted transaction or an unexpected event stops the level. */
async function sendLevel(batches: { items: { o: Operation; operation: unknown }[]; bytes: number }[], context: SendContext, out: Sent) {
  const { client, wallet } = context;
  const results: (Hex | undefined)[][] = new Array(batches.length);
  let nonce: number = await client.getTransactionCount({ address: wallet.account.address, blockTag: 'pending' });
  const settle = async (index: number, txHash: Hex, bytes: number) => {
    const receipt = await receiptOf(client, txHash);
    if (receipt.status !== 'success') throw new Error(`transaction ${txHash} reverted (gas used ${receipt.gasUsed})`);
    const events = receipt.logs.filter((e: any) => e.address.toLowerCase() === ARKIV_ADDRESS).flatMap((e: any) => {
      try { return [decodeEventLog({ abi: ENTITY_EVENTS_ABI, data: e.data, topics: e.topics }) as any]; } catch { return []; }
    });
    const batch = batches[index]!.items;
    if (events.length !== batch.length) throw new Error(`transaction ${txHash} emitted ${events.length} events for ${batch.length} operations`);
    results[index] = batch.map(({ o }, i) => {
      const event = events[i]!;
      if (String(event.args.owner).toLowerCase() !== context.owner) throw new Error(`transaction ${txHash}: an entity of another owner`);
      if (o.op === 'create') { if (event.eventName !== 'EntityCreated') throw new Error(`transaction ${txHash}: expected EntityCreated, got ${event.eventName}`); return lower(event.args.entityKey); }
      if (event.eventName !== 'EntityPatched' || lower(event.args.entityKey) !== o.target.key) throw new Error(`transaction ${txHash}: expected EntityPatched of ${o.target.key}`);
      return undefined;
    });
    out.transactions++; out.gasUsed += receipt.gasUsed; out.calldataBytes += bytes; out.rentFee += BigInt((receipt as any).rentFee ?? 0);
    context.log('transaction', { txHash, operations: batch.length, calldataBytes: bytes, gasUsed: receipt.gasUsed, rentFee: BigInt((receipt as any).rentFee ?? 0), block: receipt.blockNumber });
  };
  const window: Promise<void>[] = [];
  try {
    for (const [index, batch] of batches.entries()) {
      if (window.length >= context.inFlight) await window.shift()!;
      const data = await calldata(batch.items.map(i => i.operation));
      const bytes = BigInt((data.length - 2) / 2);
      const gas: bigint = context.gas === 'computed' ? bytes * 40n + BigInt(batch.items.length) * 150_000n + 200_000n
        : await client.estimateGas({ account: wallet.account.address, to: ARKIV_ADDRESS, data, value: 0n }) * 13n / 10n + 50_000n;
      const gasPrice: bigint = await client.getGasPrice();
      if (gasPrice > context.maxGasPrice) throw new Error(`gas price ${gasPrice} exceeds SYNC_MAX_GAS_PRICE_GWEI`);
      const txHash: Hex = await wallet.sendTransaction({ to: ARKIV_ADDRESS, data, value: 0n, nonce: nonce++, gas, gasPrice, type: 'legacy' });
      const settled = settle(index, txHash, batch.bytes);
      settled.catch(() => {});
      window.push(settled);
    }
    while (window.length) await window.shift()!;
  } catch (error) {
    await Promise.allSettled(window);
    throw error;
  }
  return results.flat();
}

async function receiptOf(client: any, hash: Hex, timeoutMs = 300_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try { return await client.getTransactionReceipt({ hash }); }
    catch (error) { if (!(error instanceof TransactionReceiptNotFoundError)) throw error; }
    if (Date.now() > deadline) throw new Error(`transaction ${hash} not mined after ${timeoutMs / 1000}s; it may still be: the next start waits for it`);
    await Bun.sleep(500);
  }
}

/** The writer's highest contract matchid above `after` (two rows per probe, like the v3 sync's). */
export function matchIdProbe(client: any, owner: string, prefix: string) {
  return async (after: bigint) => {
    let highest: bigint | undefined;
    for (const e of await query(client, `$owner = addr(${owner}) AND arkiv.schema = str('${prefix}.contract') AND matchid > u64(${after})`, 2, { key: true, attributes: { matchid: true } }, { allowMore: true })) {
      const m = e.attributes.find(a => a.name === 'matchid');
      if (!m) throw new Error('a contract entity on the chain has no matchid');
      if (highest === undefined || BigInt(m.value) > highest) highest = BigInt(m.value);
    }
    return highest;
  };
}
