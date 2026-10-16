#!/usr/bin/env bun
/**
 * Postgres -> Arkiv sync: writes the Ethereum matches a Sourcify mirror gained after a cursor (a matchId) to a live Arkiv
 * chain as the same entities a prefill seed holds, so the Arkiv-backed server reads both alike.
 *
 *   bun arkiv/sync.ts [--once | --batches N] [--start-after MATCH_ID] [--dry-run]
 *
 * One batch: prepare (source/prepare.ts --after-match-id, only the selected rows) -> records (source/stream.ts) ->
 * entities (arkiv-entity-processor process) -> Arkiv:
 *  1. dependencies (sourcefile, code, compilation, blob): one already on the chain (same owner, kind, hash, part) is
 *     reused, the others are created, packed into transactions;
 *  2. then each verified_contract, its compilationref pointing to the compilation's chain key. A contract whose
 *     address already has a verified_contract is not created again: the same entity counts as synced, a different one
 *     is skipped and logged (changed contracts are not updated yet, see issue #13).
 * Every created entity is read back and compared. The cursor (SYNC_DIR/state.json) advances only after a whole batch.
 *
 * A start checks the cursor against the chain (startCursor): the writer's highest verified_contract matchid there, from a
 * seed the chain was prefilled with or from syncing, less a margin of matches. Without state.json that is the cursor;
 * with one, it moves the cursor back only when the chain is behind it. When the writer has no contract near the mirror's
 * newest matches and there is no state.json, a continuous run waits for the chain to be seeded, or --start-after
 * MATCH_ID sets the first cursor. A continuous run retries a failed batch with backoff and keeps SYNC_DIR/status.json.
 * pipeline/sync-targets.sh runs one sync per chain, each with its own SYNC_DIR.
 *
 * The chain is the record of what was written: a rerun after any failure looks entities up again instead of creating
 * them twice. That needs a single writer whose transactions are all mined, so the sync takes SYNC_DIR/sync.lock and
 * starts only once the writer has no pending transaction. Never run another signer with the same key.
 *
 * The writer must be the seed's owner (ENTITY_OWNER, when set): the chain lookups only see the writer's own entities, so
 * a chain prefilled by another owner would get everything a second time, and the server reads one owner.
 *
 * Settings (environment): ARKIV_RPC_URL, ARKIV_RPC_API_KEY, ARKIV_CHAIN_ID, ARKIV_PRIVATE_KEY_FILE (see cli.ts), ENTITY_OWNER; PGHOST,
 * PGPORT, PGDATABASE, PGUSER, PGPASSWORD or PGPASSFILE (the mirror, SELECT only); SYNC_DIR (required, one per chain),
 * SYNC_BATCH (matches per batch, default 200), SYNC_POLL_SECONDS (default 60), SYNC_MAX_TX_BYTES (calldata per
 * transaction, default 120000), SYNC_MAX_GAS_PRICE_GWEI (default 2), SYNC_IN_FLIGHT
 * (unmined transactions at a time, default 4), SYNC_GAS (computed or estimate, see send()), ARKIV_PROCESSOR_BIN
 * (default ./target/release/arkiv-entity-processor).
 */
import { createWalletClient, ENTITY_EVENTS_ABI, ExpirationTime, randomSalt, resolveExpiry } from '@arkiv-network/sdk';
import { addr, bool, bytes32, i32, key, str, u64 } from '@arkiv-network/sdk/attr';
import { decodeEventLog, TransactionReceiptNotFoundError, type Hex } from 'viem';
import { closeSync, openSync, readFileSync, rmSync, writeSync } from 'node:fs';
import { appendFile, mkdir, readdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { newestEthereumMatchId, prepareSource, sourceDatabase, type SourceDatabase, type SourceManifest } from '../source/prepare';
import { ARKIV_ADDRESS, connect, describeError, executeCalldata, sdkInternals, writerAccount } from './chain';

const APP_DIR = resolve(dirname(import.meta.path), '..');
const env = process.env;
const settings = {
  dir: env.SYNC_DIR ? resolve(env.SYNC_DIR) : '',
  batch: Number(env.SYNC_BATCH || 200),
  pollSeconds: Number(env.SYNC_POLL_SECONDS || 60),
  maxTxBytes: Number(env.SYNC_MAX_TX_BYTES || 120_000),
  maxGasPrice: BigInt(Math.round(Number(env.SYNC_MAX_GAS_PRICE_GWEI || 2) * 1e9)),
  inFlight: Number(env.SYNC_IN_FLIGHT || 4),
  gas: env.SYNC_GAS || 'computed',
  processor: resolve(APP_DIR, env.ARKIV_PROCESSOR_BIN || 'target/release/arkiv-entity-processor'),
  keyFile: resolve(APP_DIR, env.ARKIV_PRIVATE_KEY_FILE || 'arkiv-writer.key'),
};
const DEPENDENCY_KINDS = new Set(['sourcefile', 'code', 'compilation', 'blob']);
const READ_CONCURRENCY = 8;
const NEEDS_SEED_POLL_SECONDS = 600;
const RETRY_MAX_SECONDS = 900;

/** One line of a bundle's entities/*.jsonl: $-fields and "name": "type:value" attributes (see src/main.rs). */
type Wire = Record<string, any>;
type Found = { key: Hex; stagingid?: string; matchid?: string };
interface Plan { create: { wire: Wire; params: any }[]; reused: number; conflicts: { kind: string; seedKey: string; reason: string }[] }

const json = (value: unknown) => JSON.stringify(value, (_, v) => typeof v === 'bigint' ? v.toString() : v);
const log = (event: string, fields: Record<string, unknown> = {}) => console.log(json({ sync: event, at: new Date().toISOString(), ...fields }));

function typed(wire: Wire, name: string, type: string) {
  const value = wire[name];
  if (typeof value !== 'string' || !value.startsWith(`${type}:`)) throw new Error(`entity attribute ${name} is not ${type}`);
  return value.slice(type.length + 1);
}

const CONVERT: Record<string, (value: string) => unknown> = {
  str, addr: v => addr(v), key: v => key(v as Hex), bytes32: v => bytes32(v as Hex), u64: v => u64(BigInt(v)),
  i32: v => { if (!/^-?[0-9]+$/.test(v)) throw new Error(`invalid i32 ${v}`); return i32(Number(v)); },
  bool: v => { if (v !== 'true' && v !== 'false') throw new Error(`invalid bool ${v}`); return bool(v === 'true'); },
};

/** SDK create parameters for a bundle entity; key-typed attributes are replaced through `keys` (seed key -> chain key). */
export function createParams(wire: Wire, keys: Map<string, Hex> = new Map()) {
  if (wire.$expiresAt !== 'never' || !Array.isArray(wire.$creationFlags) || wire.$creationFlags.length) throw new Error('unexpected entity lifetime or creation flags');
  const attributes: Record<string, unknown> = {};
  for (const [name, value] of Object.entries(wire)) {
    if (name.startsWith('$')) continue;
    const separator = String(value).indexOf(':');
    const type = String(value).slice(0, separator), raw = String(value).slice(separator + 1);
    if (separator < 1 || !CONVERT[type]) throw new Error(`unsupported attribute ${name}=${value}`);
    let converted = raw;
    if (type === 'key') {
      const mapped = keys.get(raw.toLowerCase());
      if (!mapped) throw new Error(`${name} references ${raw}, which has no chain key`);
      converted = mapped;
    }
    attributes[name] = CONVERT[type]!(converted);
  }
  return { payload: new Uint8Array(Buffer.from(wire.$payload, 'base64')), contentType: wire.$contentType, attributes, expires: ExpirationTime.permanent() };
}

/** Greedy, order-preserving packing of creates into `execute` calldata of at most `maxBytes` each (an entity larger
 * than that alone goes in a transaction of its own). */
export async function pack<T extends { params: any }>(items: T[], maxBytes: number): Promise<{ items: T[]; bytes: number }[]> {
  const { encodeAttributes, createOperation } = await sdkInternals();
  const expiry = resolveExpiry(ExpirationTime.permanent(), { currentBlock: 0n });
  const operation = (p: any) => createOperation({ salt: 1n, expiry, creationFlags: 0, attributes: encodeAttributes(p.attributes, { payload: p.payload, contentType: p.contentType }) });
  const size = async (operations: unknown[]) => ((await executeCalldata(operations)).length - 2) / 2;
  const batches: { items: T[]; bytes: number }[] = [];
  let current: T[] = [], operations: unknown[] = [];
  for (const item of items) {
    const op = operation(item.params);
    if (current.length && await size([...operations, op]) > maxBytes) { batches.push({ items: current, bytes: await size(operations) }); current = []; operations = []; }
    current.push(item); operations.push(op);
  }
  if (current.length) batches.push({ items: current, bytes: await size(operations) });
  return batches;
}

async function pool<T, R>(items: T[], limit: number, work: (item: T) => Promise<R>): Promise<R[]> {
  const results: R[] = new Array(items.length);
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (next < items.length) { const index = next++; results[index] = await work(items[index]!); }
  }));
  return results;
}

async function main() {
  const { values } = parseArgs({ strict: true, options: {
    once: { type: 'boolean', default: false }, batches: { type: 'string' }, 'start-after': { type: 'string' },
    'dry-run': { type: 'boolean', default: false }, help: { type: 'boolean', short: 'h', default: false },
  } });
  if (values.help) { console.log(readFileSync(import.meta.path, 'utf8').split('*/')[0]!.replace(/^#!.*\n\/\*\*\n/, '').replace(/^ \* ?/gm, '')); return; }
  if (!settings.dir) throw new Error('SYNC_DIR is required: the directory of this chain\'s cursor and logs');
  if (!Number.isSafeInteger(settings.batch) || settings.batch < 1 || settings.batch > 5000) throw new Error('SYNC_BATCH must be 1..5000');
  if (!Number.isSafeInteger(settings.maxTxBytes) || settings.maxTxBytes < 10_000) throw new Error('SYNC_MAX_TX_BYTES must be at least 10000');
  if (!Number.isSafeInteger(settings.inFlight) || settings.inFlight < 1 || settings.inFlight > 16) throw new Error('SYNC_IN_FLIGHT must be 1..16');
  if (!['computed', 'estimate'].includes(settings.gas)) throw new Error('SYNC_GAS must be computed or estimate');
  const maxBatches = values.once ? 1 : values.batches !== undefined ? Number(values.batches) : Infinity;
  if (maxBatches !== Infinity && (!Number.isSafeInteger(maxBatches) || maxBatches < 1)) throw new Error('--batches must be a positive whole number');
  if (values['start-after'] !== undefined && !/^(0|[1-9][0-9]*)$/.test(values['start-after'])) throw new Error('--start-after must be a match ID');
  // A continuous run waits out what can pass (a chain not seeded yet, a failed batch) instead of exiting.
  const continuous = maxBatches === Infinity && !values['dry-run'];
  const database = sourceDatabase(env);
  const account = writerAccount(settings.keyFile);
  const owner = account.address.toLowerCase();
  if (env.ENTITY_OWNER && env.ENTITY_OWNER.toLowerCase() !== owner) throw new Error(`the writer key is for ${account.address}, but ENTITY_OWNER (the seeds' owner) is ${env.ENTITY_OWNER}`);
  const { chain, client, transport, rpcUrl } = await connect(env);
  // Receipts are polled every second (viem's default is 4 s, two blocks here).
  const wallet = createWalletClient({ chain, transport, account, pollingInterval: 1000 });
  await mkdir(settings.dir, { recursive: true });
  const release = lock(join(settings.dir, 'sync.lock'));
  // What a continuous run is doing, for pipeline/sync-targets.sh ls.
  let status: Record<string, unknown> = {};
  const report = async (fields: Record<string, unknown>) => {
    if (!continuous) return;
    status = { ...status, ...fields, at: new Date().toISOString() };
    const path = join(settings.dir, 'status.json');
    await writeFile(`${path}.tmp`, JSON.stringify(status, (_, v) => typeof v === 'bigint' ? v.toString() : v, 2) + '\n'); await rename(`${path}.tmp`, path);
  };
  try {
    log('start', { rpcUrl, chainId: chain.id, writer: account.address, dir: settings.dir, batch: settings.batch, dryRun: values['dry-run'] });
    await report({ status: 'starting', rpcUrl, chainId: chain.id, writer: account.address });

    // Queries return at most two rows: more than one entity for a lookup is ambiguous for the server too.
    const find = async (selector: string): Promise<Found[]> => {
      const result: any = await client.request({ method: 'arkiv_query' as any, params: [`projectid = str('sourcify') AND $owner = addr(${owner}) AND ${selector}`, { limit: 2, select: { key: true, attributes: true } }] as any });
      return (result.data ?? []).map((entity: any) => {
        const attribute = (name: string) => entity.attributes?.find((a: any) => a.name === name)?.value;
        return { key: entity.key.toLowerCase(), stagingid: attribute('stagingid')?.toLowerCase(), matchid: attribute('matchid') !== undefined ? BigInt(attribute('matchid')).toString() : undefined };
      });
    };

    const statePath = join(settings.dir, 'state.json');
    let state: { lastMatchId: string; updatedAt?: string } | undefined;
    try { state = JSON.parse(await readFile(statePath, 'utf8')); } catch (error: any) { if (error.code !== 'ENOENT') throw error; }
    let cursor: string | undefined;
    if (values['start-after'] !== undefined) {
      if (state) throw new Error(`--start-after given, but ${statePath} already holds cursor ${state.lastMatchId}; remove it to restart deliberately`);
      cursor = values['start-after'];
      log('cursor', { lastMatchId: cursor, from: 'argument' });
    }
    while (cursor === undefined) {
      const start = await startingPoint(state?.lastMatchId, database, find);
      await report({ status: start.cursor === undefined ? 'needs-seed' : 'starting', start });
      if ((cursor = start.cursor) !== undefined) break;
      if (!continuous) throw new Error('No cursor: the writer has no verified_contract near the mirror\'s newest matches; prefill the chain with a seed or pass --start-after MATCH_ID');
      log('needs-seed', { retryInSeconds: NEEDS_SEED_POLL_SECONDS });
      await Bun.sleep(NEEDS_SEED_POLL_SECONDS * 1000);
    }
    const saveState = async (lastMatchId: string) => {
      cursor = lastMatchId;
      await writeFile(`${statePath}.tmp`, JSON.stringify({ lastMatchId, updatedAt: new Date().toISOString() }, null, 2) + '\n'); await rename(`${statePath}.tmp`, statePath);
    };
    if (!values['dry-run']) await saveState(cursor);
    await report({ lastMatchId: cursor });

    for (let done = 0, failures = 0; done < maxBatches;) {
      let result: Awaited<ReturnType<typeof syncBatch>>;
      try {
        if (!values['dry-run']) await waitForSettledNonce(client, account.address);
        result = await syncBatch({ after: cursor, owner, chainId: chain.id, database, client, wallet, find, dryRun: values['dry-run'] });
      } catch (error) {
        if (!continuous) throw error;
        // The cursor did not move, and the rerun looks up whatever the failed attempt wrote.
        const seconds = Math.min(RETRY_MAX_SECONDS, settings.pollSeconds * 2 ** failures++);
        log('retry', { error: describeError(error), failures, inSeconds: seconds });
        await report({ status: 'retrying', error: describeError(error), failures });
        await Bun.sleep(seconds * 1000); continue;
      }
      failures = 0;
      if (result.selected === 0) {
        log('caught-up', { lastMatchId: cursor });
        await report({ status: 'caught-up', error: undefined, failures: undefined });
        if (!continuous) break;
        await Bun.sleep(settings.pollSeconds * 1000); continue;
      }
      if (!values['dry-run']) await saveState(result.lastMatchId);
      await appendFile(join(settings.dir, 'sync.jsonl'), json({ at: new Date().toISOString(), ...result, skippedContracts: result.skippedContracts.length }) + '\n');
      if (result.skippedContracts.length) await appendFile(join(settings.dir, 'skipped.jsonl'), result.skippedContracts.map(s => json({ at: new Date().toISOString(), ...s })).join('\n') + '\n');
      log('batch', { ...result, skippedContracts: result.skippedContracts.length });
      await report({ status: 'syncing', lastMatchId: cursor, lastBatchAt: new Date().toISOString(), error: undefined, failures: undefined });
      done++;
      if (values['dry-run']) break;
    }
  } catch (error) {
    await report({ status: 'failed', error: describeError(error) }).catch(() => {});
    throw error;
  } finally { release(); }
}

/** Where a start continues (startCursor), from state.json's cursor, the mirror's newest match and the chain. */
async function startingPoint(state: string | undefined, database: SourceDatabase, find: (selector: string) => Promise<Found[]>) {
  const newest = await newestEthereumMatchId(database);
  const chain = await highestMatchId(async after => {
    let highest: bigint | undefined;
    for (const { matchid } of await find(`kind = str('verified_contract') AND matchid > u64(${after})`)) {
      if (matchid === undefined) throw new Error('a verified_contract entity on the chain has no matchid');
      if (highest === undefined || BigInt(matchid) > highest) highest = BigInt(matchid);
    }
    return highest;
  }, BigInt(newest ?? state ?? 0));
  // Back from the chain's progress: twice a batch, at least 1,000 Ethereum matches (SYNC_BATCH may have been larger).
  const stepBack = chain === undefined ? undefined : await newestEthereumMatchId(database, { atOrBelow: String(chain), offset: Math.max(2 * settings.batch, 1000) }) ?? '0';
  const start = { mirrorNewest: newest, state, chain: chain?.toString(), ...startCursor(state, chain?.toString(), stepBack) };
  log('progress', start);
  return start;
}

/** Where a start continues. `chain` is the writer's highest verified_contract matchid: batches run in order and the
 * cursor moves only after a whole one, so only the batch in flight when a sync stopped can have holes below it, and
 * `stepBack` (enough matches before `chain`) is safe. state.json's cursor stays when the chain has reached it. Matches
 * done again cost only lookups: what is on the chain is found and counted as synced. */
export function startCursor(state: string | undefined, chain: string | undefined, stepBack: string | undefined): { cursor?: string; from: string } {
  if (chain === undefined || stepBack === undefined) return state === undefined ? { from: 'needs-seed' } : { cursor: state, from: 'state-unchecked' };
  if (state === undefined) return { cursor: stepBack, from: 'chain' };
  if (BigInt(chain) < BigInt(state)) return { cursor: stepBack, from: 'chain-behind-state' };
  return BigInt(stepBack) > BigInt(state) ? { cursor: stepBack, from: 'chain' } : { cursor: state, from: 'state' };
}

async function syncBatch(context: { after: string; owner: string; chainId: number; database: any; client: any; wallet: any; find: (s: string) => Promise<Found[]>; dryRun: boolean }) {
  const { after, owner, client, wallet, find } = context;
  const started = performance.now();
  const phases: Record<string, number> = {};
  let mark = started;
  const phase = (name: string) => { const now = performance.now(); phases[name] = +((now - mark) / 1000).toFixed(1); mark = now; };
  const work = join(settings.dir, 'work', `after-${after}`);
  await rm(work, { recursive: true, force: true });
  await mkdir(work, { recursive: true });
  const manifest: SourceManifest = await prepareSource({ sourceDir: join(work, 'source'), database: context.database, afterMatchId: after, limit: settings.batch, memoryLimit: '1GB', threads: 2 });
  const result = { firstMatchId: manifest.firstMatchId, lastMatchId: manifest.lastMatchId!, selected: manifest.selectedCount ?? 0, entities: 0, created: 0, reused: 0,
    contracts: 0, alreadySynced: 0, transactions: 0, gasUsed: 0n, calldataBytes: 0, seconds: 0, phases, skippedContracts: [] as { matchId?: string; address?: string; reason: string }[] };
  phase('prepare');
  if (!result.selected) { await rm(work, { recursive: true, force: true }); return result; }

  // Records, then entities. Records whose rows are missing are skipped by stream.ts and listed in its marker.
  const records = join(work, 'records.ndjson');
  await run([process.execPath, '--no-env-file', join(APP_DIR, 'source/stream.ts'), '--source-dir', join(work, 'source'), '--memory-limit', '1GB', '--skip-missing-references'], { stdout: records, stderr: join(work, 'stream.log') });
  phase('records');
  const marker = JSON.parse((await readFile(records, 'utf8')).trimEnd().split('\n').at(-1)!);
  for (const skipped of marker.skipped ?? []) result.skippedContracts.push({ matchId: skipped.matchId, reason: `record: ${skipped.reason}` });
  if (marker.records === 0) { await rm(work, { recursive: true, force: true }); return result; }
  const bundle = join(work, 'bundle');
  await run([settings.processor, 'process', '-i', records, '-o', bundle, '--owner', owner, '--destination-chain-id', String(context.chainId), '--require-source-complete'], { stderr: join(work, 'process.log') });
  phase('process');
  const wires: Wire[] = [];
  for (const part of (await readdir(join(bundle, 'entities'))).sort()) {
    for (const line of (await readFile(join(bundle, 'entities', part), 'utf8')).split('\n')) if (line) wires.push(JSON.parse(line));
  }
  result.entities = wires.length;
  const kindOf = (wire: Wire) => typed(wire, 'kind', 'str');
  const dependencies = wires.filter(wire => DEPENDENCY_KINDS.has(kindOf(wire)));
  const contracts = wires.filter(wire => kindOf(wire) === 'verified_contract');
  if (dependencies.length + contracts.length !== wires.length) throw new Error('bundle holds an unknown entity kind');
  // prepare keeps one match per address (matchRank): two here would become two entities the server cannot choose from.
  const addresses = new Set(contracts.map(wire => typed(wire, 'address', 'addr').toLowerCase()));
  if (addresses.size !== contracts.length) throw new Error('batch holds two contracts for one address');

  // 1. Dependencies: reuse what the chain has (found by the same lookup the server uses), create the rest.
  const keys = new Map<string, Hex>(); // seed key (bundle $key) -> chain key
  const unusable = new Set<string>();   // seed keys of compilations that cannot be referenced (see below)
  const lookups = await pool(dependencies, READ_CONCURRENCY, async wire => {
    const kind = kindOf(wire);
    const selector = `kind = str('${kind}') AND hash = bytes32(${typed(wire, 'hash', 'bytes32')})` + (kind === 'blob' ? ` AND part = i32(${typed(wire, 'part', 'i32')})` : '');
    return { wire, kind, found: await find(selector) };
  });
  phase('lookupDependencies');
  const toCreate: { wire: Wire; params: any }[] = [];
  for (const { wire, kind, found } of lookups) {
    const seedKey = String(wire.$key).toLowerCase(), stagingId = typed(wire, 'stagingid', 'bytes32').toLowerCase();
    if (found.length === 0) { toCreate.push({ wire, params: createParams(wire) }); continue; }
    result.reused++;
    keys.set(seedKey, found[0]!.key);
    // The server resolves a compilationref through the referenced entity's stagingid: only an identical one will do.
    // Two entities for one lookup are ambiguous to the server, whatever this sync writes.
    if (found.length > 1 || found[0]!.stagingid !== stagingId) {
      if (kind === 'compilation') unusable.add(seedKey);
      log('dependency-conflict', { kind, hash: typed(wire, 'hash', 'bytes32'), reason: found.length > 1 ? 'ambiguous on chain' : 'different staging ID on chain' });
    }
  }
  const sent = await send(toCreate, context);
  phase('createDependencies');
  for (const [index, item] of toCreate.entries()) keys.set(String(item.wire.$key).toLowerCase(), sent.keys[index]!);
  Object.assign(result, { created: sent.keys.length, transactions: sent.transactions, gasUsed: sent.gasUsed, calldataBytes: sent.calldataBytes });

  // 2. Contracts, after every dependency is on the chain: the verified_contract is what makes a contract readable.
  const contractLookups = await pool(contracts, READ_CONCURRENCY, async wire => ({ wire, found: await find(`kind = str('verified_contract') AND chainid = u64(1) AND address = addr(${typed(wire, 'address', 'addr').toLowerCase()})`) }));
  phase('lookupContracts');
  const contractCreates: { wire: Wire; params: any }[] = [];
  for (const { wire, found } of contractLookups) {
    const matchId = typed(wire, 'matchid', 'u64'), address = typed(wire, 'address', 'addr');
    const skip = (reason: string) => result.skippedContracts.push({ matchId, address, reason });
    if (found.length > 1) { skip('several verified_contract entities for this address on the chain'); continue; }
    if (found.length === 1) {
      if (found[0]!.stagingid === typed(wire, 'stagingid', 'bytes32').toLowerCase()) result.alreadySynced++;
      else skip(`address already has a different verified_contract (matchId ${found[0]!.matchid ?? '?'}); updates are not synced yet`);
      continue;
    }
    if (unusable.has(typed(wire, 'compilationref', 'key').toLowerCase())) { skip('its compilation exists on the chain with different content'); continue; }
    contractCreates.push({ wire, params: createParams(wire, keys) });
  }
  const sentContracts = await send(contractCreates, context);
  phase('createContracts');
  result.contracts = sentContracts.keys.length;
  result.created += sentContracts.keys.length; result.transactions += sentContracts.transactions;
  result.gasUsed += sentContracts.gasUsed; result.calldataBytes += sentContracts.calldataBytes;
  result.seconds = +((performance.now() - started) / 1000).toFixed(1);
  if (!context.dryRun) await rm(work, { recursive: true, force: true });
  return result;
}

/** Creates entities in packed transactions, up to SYNC_IN_FLIGHT of them unmined at a time (the creates in one call are
 * independent: none references another), then reads every created entity back. Returns the chain keys in item order.
 * Any failure stops sending; transactions already sent still get mined, which the next run's lookups account for. */
async function send(items: { wire: Wire; params: any }[], context: { client: any; wallet: any; owner: string; dryRun: boolean }) {
  const { client, wallet } = context;
  const out = { keys: [] as Hex[], transactions: 0, gasUsed: 0n, calldataBytes: 0 };
  if (!items.length) return out;
  const batches = await pack(items, settings.maxTxBytes);
  if (context.dryRun) {
    // Nothing is created: a placeholder key lets the contracts that would reference these be planned too.
    out.keys = items.map(() => `0x${'ee'.repeat(32)}` as Hex);
    out.transactions = batches.length; out.calldataBytes = batches.reduce((sum, b) => sum + b.bytes, 0);
    log('dry-run', { creates: items.length, transactions: batches.length, calldataBytes: out.calldataBytes });
    return out;
  }
  const { encodeAttributes, createOperation } = await sdkInternals();
  const expiry = resolveExpiry(ExpirationTime.permanent(), { currentBlock: 0n });
  const keys: Hex[][] = new Array(batches.length);
  // The sync is the writer's only signer and starts with nothing pending, so nonces simply count up from here.
  let nonce: number = await client.getTransactionCount({ address: wallet.account.address, blockTag: 'pending' });

  const timing = { encode: 0, gasPrice: 0, estimate: 0, sendRaw: 0, receiptWait: 0 };
  const timed = async <T>(name: keyof typeof timing, work: () => Promise<T>) => { const t = performance.now(); try { return await work(); } finally { timing[name] += (performance.now() - t) / 1000; } };
  // Encoding and gas estimation do not depend on the nonce, so the next transactions are prepared while earlier ones
  // are sent and mined. SYNC_GAS=estimate simulates each one (a create the chain would refuse fails before signing),
  // but the node estimates slowly (~2 s each); the default computes the limit from the calldata size instead: creates
  // used at most 38 gas per calldata byte, and only gas used is charged. A transaction that ran out would revert as a
  // whole, stopping the sync; a rerun reuses whatever is on the chain.
  const prepared: Promise<{ data: Hex; gas: bigint }>[] = [];
  const prepare = (index: number) => prepared[index] ??= (async () => {
    const data = await timed('encode', () => executeCalldata(batches[index]!.items.map(({ params }) => createOperation({ salt: randomSalt(), expiry, creationFlags: 0,
      attributes: encodeAttributes(params.attributes, { payload: params.payload, contentType: params.contentType }) }))));
    const bytes = BigInt((data.length - 2) / 2);
    const gas: bigint = settings.gas === 'computed' ? bytes * 50n + 100_000n
      : await timed('estimate', () => client.estimateGas({ account: wallet.account.address, to: ARKIV_ADDRESS, data, value: 0n })) * 12n / 10n + 1n;
    return { data, gas };
  })();
  const submit = async (index: number) => {
    for (let ahead = index; ahead < Math.min(batches.length, index + settings.inFlight); ahead++) prepare(ahead).catch(() => {});
    const { data, gas } = await prepare(index);
    const gasPrice: bigint = await timed('gasPrice', () => client.getGasPrice());
    if (gasPrice > settings.maxGasPrice) throw new Error(`gas price ${gasPrice} exceeds SYNC_MAX_GAS_PRICE_GWEI`);
    return timed('sendRaw', () => wallet.sendTransaction({ to: ARKIV_ADDRESS, data, value: 0n, nonce: nonce++, gas, gasPrice, type: 'legacy' }) as Promise<Hex>);
  };
  const verifications: Promise<unknown>[] = [];
  const settle = async (index: number, batch: { wire: Wire; params: any }[], bytes: number, txHash: Hex, sentAt: number) => {
    const receipt = await timed('receiptWait', () => receiptOf(client, txHash));
    if (receipt.status !== 'success') throw new Error(`transaction ${txHash} reverted (gas used ${receipt.gasUsed}; with SYNC_GAS=computed it may have run out: rerun with SYNC_GAS=estimate)`);
    const created = receipt.logs.filter((entry: any) => entry.address.toLowerCase() === ARKIV_ADDRESS).flatMap((entry: any) => {
      try {
        const event: any = decodeEventLog({ abi: ENTITY_EVENTS_ABI, data: entry.data, topics: entry.topics });
        return event.eventName === 'EntityCreated' ? [event.args] : [];
      } catch { return []; }
    });
    if (created.length !== batch.length || created.some((event: any) => event.owner.toLowerCase() !== context.owner)) throw new Error(`transaction ${txHash} created ${created.length} entities (or for another owner), expected ${batch.length}`);
    keys[index] = created.map((event: any) => event.entityKey.toLowerCase() as Hex);
    out.transactions++; out.gasUsed += receipt.gasUsed; out.calldataBytes += bytes;
    const verification = pool(batch.map((item, i) => ({ item, key: keys[index]![i]! })), READ_CONCURRENCY, ({ item, key }) => verify(client, key, item, context.owner));
    verification.catch(() => {});
    verifications.push(verification);
    const minedAt = Number((await client.getBlock({ blockNumber: receipt.blockNumber })).timestamp) * 1000;
    log('transaction', { txHash, creates: batch.length, calldataBytes: bytes, gasUsed: receipt.gasUsed, block: receipt.blockNumber, sentAt: new Date(sentAt).toISOString(), minedAt: new Date(minedAt).toISOString() });
  };
  const window: Promise<void>[] = [];
  try {
    for (const [index, { items: batch, bytes }] of batches.entries()) {
      if (window.length >= settings.inFlight) await window.shift()!;
      const txHash = await submit(index);
      const sentAt = Date.now();
      const settled = settle(index, batch, bytes, txHash, sentAt);
      settled.catch(() => {}); // awaited below, in order
      window.push(settled);
    }
    while (window.length) await window.shift()!;
    await Promise.all(verifications);
    log('send-timing', Object.fromEntries(Object.entries(timing).map(([k, v]) => [k, +v.toFixed(1)])));
  } catch (error) {
    await Promise.allSettled([...window, ...verifications, ...prepared.filter(Boolean)]);
    throw error;
  }
  out.keys = keys.flat();
  return out;
}

/** Polls for a receipt every 500 ms (viem's waitForTransactionReceipt noticed blocks 5-10 s late here). */
async function receiptOf(client: any, hash: Hex, timeoutMs = 300_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try { return await client.getTransactionReceipt({ hash }); }
    catch (error) { if (!(error instanceof TransactionReceiptNotFoundError)) throw error; }
    if (Date.now() > deadline) throw new Error(`transaction ${hash} not mined after ${timeoutMs / 1000}s; it may still be: the next start waits for it`);
    await Bun.sleep(500);
  }
}

/** The created entity, read back, must be exactly what was sent. */
async function verify(client: any, entityKey: Hex, item: { wire: Wire; params: any }, owner: string) {
  let entity: any;
  for (let attempt = 1; ; attempt++) {
    try { entity = await client.getEntity(entityKey); break; } catch (error) { if (attempt >= 5) throw error; await Bun.sleep(1000); }
  }
  const problems: string[] = [];
  if (entity.owner?.toLowerCase() !== owner || entity.creator?.toLowerCase() !== owner) problems.push('owner');
  if (entity.contentType !== item.params.contentType) problems.push('contentType');
  if (!Buffer.from(entity.payload ?? []).equals(Buffer.from(item.params.payload))) problems.push('payload');
  if (BigInt(entity.expiresAt ?? 0n) !== 2n ** 64n - 1n) problems.push('expiresAt');
  for (const name of Object.keys(item.params.attributes)) {
    const sent = item.params.attributes[name], got = entity.attributes?.[name];
    const norm = (value: any) => typeof value === 'string' ? value.toLowerCase() : String(value);
    if (!got || got.type !== sent.type || norm(got.value) !== norm(sent.value)) problems.push(`attribute ${name}`);
  }
  if (Object.keys(entity.attributes ?? {}).length !== Object.keys(item.params.attributes).length) problems.push('attribute count');
  if (problems.length) throw new Error(`entity ${entityKey} read back differently: ${problems.join(', ')}`);
}

/** All of the writer's transactions are mined (or none was sent): only then are chain lookups complete. */
async function waitForSettledNonce(client: any, address: Hex, timeoutSeconds = 600) {
  const deadline = Date.now() + timeoutSeconds * 1000;
  for (;;) {
    const [latest, pending] = await Promise.all([client.getTransactionCount({ address, blockTag: 'latest' }), client.getTransactionCount({ address, blockTag: 'pending' })]);
    if (latest === pending) return;
    if (Date.now() > deadline) throw new Error(`writer still has ${pending - latest} pending transaction(s) after ${timeoutSeconds}s; inspect before syncing`);
    log('waiting-for-pending', { latest, pending });
    await Bun.sleep(5000);
  }
}

/** The highest matchid of the writer's verified_contract entities (`probe(X)` returns the highest of those the node
 * returns for `matchid > X`), or undefined when it has none within `maxWindow` IDs below `newest`, the mirror's newest
 * Ethereum match. The node answers `matchid > X` in time that grows with the entities above X, of every owner (about
 * 30,000 a second; with an upper bound too, or `matchid > 0` on a prefilled chain, it times out), so the search only
 * asks above values near the top: a window below `newest` doubles from 1,024 IDs, then it bisects. Asking above the
 * best matchid so far ends it at once when the node returns the highest ones first, as it does. */
export async function highestMatchId(probe: (after: bigint) => Promise<bigint | undefined>, newest: bigint, maxWindow = 2n ** 21n) {
  let high = 2n ** 40n; // nothing above high
  if (await probe(high) !== undefined) throw new Error('matchIds beyond 2^40 on the chain');
  let low = await probe(newest); // one the writer has
  if (low === undefined) {
    high = newest;
    for (let window = 1024n; low === undefined; window *= 2n) {
      if (window > maxWindow || high === 0n) return undefined;
      const after = newest > window ? newest - window : 0n;
      if ((low = await probe(after)) === undefined) high = after;
    }
  }
  for (;;) {
    const above = await probe(low);
    if (above === undefined) return low;
    low = above;
    if (high - low > 1n) {
      const middle = (low + high) / 2n, found = await probe(middle);
      if (found === undefined) high = middle; else low = found;
    }
  }
}

/** A PID lock file; a stale one (its process gone) is taken over. */
function lock(path: string) {
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      const fd = openSync(path, 'wx'); writeSync(fd, String(process.pid)); closeSync(fd);
      return () => rmSync(path, { force: true });
    } catch (error: any) {
      if (error.code !== 'EEXIST') throw error;
      const pid = Number(readFileSync(path, 'utf8'));
      let alive = false;
      try { process.kill(pid, 0); alive = true; } catch (e: any) { alive = e.code === 'EPERM'; }
      if (alive && pid !== process.pid) throw new Error(`another sync (pid ${pid}) holds ${path}`);
      rmSync(path, { force: true }); // stale: its process is gone; take it over

    }
  }
  throw new Error(`cannot take ${path}`);
}

/** Runs a command to completion; stdout/stderr to files. Fails with the command's exit code and its log's tail. */
async function run(command: string[], files: { stdout?: string; stderr: string }) {
  const stdout = files.stdout ? Bun.file(files.stdout) : 'ignore';
  const proc = Bun.spawn(command, { cwd: APP_DIR, stdout, stderr: Bun.file(files.stderr), env: { ...env } });
  const code = await proc.exited;
  if (code !== 0) {
    const tail = (await readFile(files.stderr, 'utf8').catch(() => '')).trim().split('\n').slice(-3).join(' | ');
    throw new Error(`${command[1]?.endsWith('stream.ts') ? 'stream.ts' : 'arkiv-entity-processor'} exited ${code}: ${tail}`);
  }
}

if (import.meta.main) {
  try { await main(); } catch (error) { log('failed', { error: describeError(error) }); process.exitCode = 1; }
}
