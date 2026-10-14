#!/usr/bin/env bun
/**
 * Small Arkiv helper CLI (Bun + @arkiv-network/sdk): inspect the target chain and try single entity writes before the
 * Postgres -> Arkiv sync is built on the same calls. Run `bun arkiv/cli.ts help`.
 *
 * Settings (environment):
 *   ARKIV_RPC_URL           RPC endpoint (default: the dev chain's public read-only endpoint)
 *   ARKIV_RPC_API_KEY       optional, sent only as the X-Api-Key header
 *   ARKIV_CHAIN_ID          optional; when set, the RPC must report this chain id
 *   ARKIV_PRIVATE_KEY_FILE  writer key, a 32-byte hex file (default ./arkiv-writer.key); never printed
 *
 * Writes are dry runs unless --execute is given: a dry run encodes the operation, resolves its expiry and estimates
 * gas (eth_estimateGas works on read-only endpoints); --execute signs and sends it, waits for the receipt and reads the
 * entity back.
 */
import { createWalletClient, ExpirationTime, resolveExpiry, type Expiry } from '@arkiv-network/sdk';
import { addr, bool, bytes32, dec, i32, key, str, u256, u64 } from '@arkiv-network/sdk/attr';
import { type Hex } from 'viem';
import { readFileSync } from 'node:fs';
import { parseArgs } from 'node:util';
import { ARKIV_ADDRESS, connect as connectChain, describeError, executeCalldata, sdkInternals, writerAccount } from './chain';
const USAGE = `Usage: bun arkiv/cli.ts <command> [options]

  info                         chain id, head block, writer address, balance, nonces, entity count
  get <key> [--payload]        one entity: metadata and attributes; --payload also prints the (decoded) payload
  query <expr> [--limit N]     raw arkiv_query, e.g. "kind = str('verified_contract') AND chainid = u64(1)"
  create [--text S | --file F] [--content-type T] [--attr name=type:value ...] [--expires permanent|<days>d|<block>]
         [--execute]           create one entity (default --expires permanent, i.e. block 2^64-1)
  delete <key> [--execute]     delete one entity the writer owns

Attribute types: str, u64, i32, u256, dec, bool, addr, key, bytes32 (e.g. --attr projectid=str:sourcify-sync-test).`;

const env = process.env;

const json = (value: unknown) => JSON.stringify(value, (_, v) => typeof v === 'bigint' ? v.toString() : v instanceof Uint8Array ? `<${v.length} bytes>` : v, 2);
const fail = (message: string): never => { console.error(`error: ${message}`); process.exit(1); };

const writer = () => writerAccount(env.ARKIV_PRIVATE_KEY_FILE || 'arkiv-writer.key');

const ATTRIBUTE_TYPES: Record<string, (value: string) => unknown> = {
  str, dec, addr: v => addr(v as Hex), key: v => key(v as Hex), bytes32: v => bytes32(v as Hex),
  u64: v => u64(BigInt(v)), u256: v => u256(BigInt(v)), i32: v => i32(Number(v)),
  bool: v => v === 'true' ? bool(true) : v === 'false' ? bool(false) : fail(`bool attribute must be true or false, got ${v}`),
};
function attributes(specs: string[]) {
  const result: Record<string, any> = {};
  for (const spec of specs) {
    const match = /^([^=]+)=([a-z0-9]+):(.*)$/s.exec(spec);
    if (!match) fail(`attribute must look like name=type:value, got ${spec}`);
    const [, name, type, value] = match!;
    if (!ATTRIBUTE_TYPES[type!]) fail(`unknown attribute type ${type}`);
    if (name! in result) fail(`attribute ${name} given twice`);
    result[name!] = ATTRIBUTE_TYPES[type!]!(value!);
  }
  return result;
}

function expiry(spec: string): Expiry {
  if (spec === 'permanent') return ExpirationTime.permanent();
  if (/^[1-9][0-9]*d$/.test(spec)) return ExpirationTime.fromDays(Number(spec.slice(0, -1)));
  if (/^[1-9][0-9]*$/.test(spec)) return ExpirationTime.atBlock(BigInt(spec));
  return fail(`--expires must be permanent, <days>d or a block number, got ${spec}`);
}

async function estimate(client: any, from: Hex, operation: unknown) {
  const data = await executeCalldata([operation]);
  try {
    const gas = await client.estimateGas({ account: from, to: ARKIV_ADDRESS, data, value: 0n });
    return { calldataBytes: (data.length - 2) / 2, gas, gasPrice: await client.getGasPrice() };
  } catch (error: any) { return fail(`the chain rejects this operation: ${error.shortMessage ?? error.message}`); }
}

async function readBack(client: any, entityKey: Hex) {
  for (let attempt = 1; ; attempt++) {
    try { return await client.getEntity(entityKey); }
    catch (error) { if (attempt >= 5) throw error; await Bun.sleep(1000); }
  }
}

async function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      execute: { type: 'boolean', default: false }, payload: { type: 'boolean', default: false },
      limit: { type: 'string', default: '10' }, text: { type: 'string' }, file: { type: 'string' },
      'content-type': { type: 'string' }, attr: { type: 'string', multiple: true, default: [] },
      expires: { type: 'string', default: 'permanent' }, help: { type: 'boolean', short: 'h' },
    },
  });
  const [command, argument] = positionals;
  if (!command || command === 'help' || values.help) return console.log(USAGE);
  const { chain, client, transport, rpcUrl } = await connectChain(env);

  if (command === 'info') {
    const account = env.ARKIV_PRIVATE_KEY_FILE || readable('arkiv-writer.key') ? writer() : undefined;
    const [block, count] = await Promise.all([client.getBlockNumber(), client.getEntityCount()]);
    const wallet = account && await Promise.all([
      client.getBalance({ address: account.address }), client.getTransactionCount({ address: account.address }),
      client.getTransactionCount({ address: account.address, blockTag: 'pending' }), client.getEntityNonce(account.address),
    ]);
    return console.log(json({ rpcUrl, chainId: chain.id, block, entityCount: count,
      ...(wallet ? { writer: account!.address, balanceWei: wallet[0], nonce: wallet[1], pendingNonce: wallet[2], entityNonce: wallet[3] } : {}) }));
  }

  if (command === 'get') {
    if (!/^0x[0-9a-fA-F]{64}$/.test(argument ?? '')) fail('get needs a 32-byte entity key');
    const entity = await client.getEntity(argument as Hex);
    const { payload, ...rest } = entity as any;
    const out: any = { ...rest, payloadBytes: payload?.length };
    if (values.payload && payload) {
      const gzip = rest.contentType === 'application/gzip' || rest.attributes?.contentencoding?.value === 'gzip';
      const bytes = gzip ? Bun.gunzipSync(payload) : payload;
      const text = new TextDecoder().decode(bytes);
      try { out.payload = JSON.parse(text); } catch { out.payload = text; }
    }
    return console.log(json(out));
  }

  if (command === 'query') {
    if (!argument) fail('query needs an expression');
    const limit = Number(values.limit);
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > 200) fail('--limit must be 1..200 (the node returns at most 200 per query)');
    const result = await client.request({ method: 'arkiv_query' as any, params: [argument, { limit, select: { key: true, owner: true, expiresAt: true, contentType: true, attributes: true } }] as any });
    return console.log(json(result));
  }

  if (command === 'create') {
    if ((values.text === undefined) === (values.file === undefined)) fail('create needs exactly one of --text or --file');
    const payload = values.text !== undefined ? new TextEncoder().encode(values.text) : new Uint8Array(readFileSync(values.file!));
    const contentType = values['content-type'] ?? (values.text !== undefined ? 'text/plain' : 'application/octet-stream');
    const attrs = attributes(values.attr as string[]);
    const expires = expiry(values.expires!);
    const account = writer();
    const currentBlock = await client.getBlockNumber();
    const resolved = resolveExpiry(expires, { currentBlock });
    const { encodeAttributes, createOperation } = await sdkInternals();
    // A random salt, as the SDK uses on --execute: keys are not predictable before the transaction is mined.
    const operation = createOperation({ salt: 1n, expiry: resolved, creationFlags: 0, attributes: encodeAttributes(attrs, { payload, contentType }) });
    const plan = { writer: account.address, payloadBytes: payload.length, contentType, attributes: attrs, expiresAtBlock: resolved.target, permanent: values.expires === 'permanent', ...(await estimate(client, account.address, operation)) };
    if (!values.execute) return console.log(json({ dryRun: true, ...plan }));
    const wallet = createWalletClient({ chain, transport, account });
    const created = await wallet.createEntity({ payload, contentType, attributes: attrs, expires });
    const entity = await readBack(client, created.entityKey);
    const matches = entity.owner?.toLowerCase() === account.address.toLowerCase() && entity.contentType === contentType &&
      Buffer.from(entity.payload ?? []).equals(Buffer.from(payload)) && BigInt(entity.expiresAt ?? 0n) >= resolved.target;
    console.log(json({ ...plan, entityKey: created.entityKey, txHash: created.txHash, expiresAt: entity.expiresAt, readBack: matches ? 'ok' : 'MISMATCH' }));
    if (!matches) process.exit(2);
    return;
  }

  if (command === 'delete') {
    if (!/^0x[0-9a-fA-F]{64}$/.test(argument ?? '')) fail('delete needs a 32-byte entity key');
    const account = writer();
    const entity = await client.getEntity(argument as Hex);
    if (entity.owner?.toLowerCase() !== account.address.toLowerCase()) fail(`entity is owned by ${entity.owner}, not the writer ${account.address}`);
    const { deleteOperation } = await sdkInternals();
    const plan = { writer: account.address, entityKey: argument, ...(await estimate(client, account.address, deleteOperation({ entityKey: argument as Hex }))) };
    if (!values.execute) return console.log(json({ dryRun: true, ...plan }));
    const { txHash } = await createWalletClient({ chain, transport, account }).deleteEntity({ entityKey: argument as Hex });
    return console.log(json({ ...plan, txHash, deleted: true }));
  }

  fail(`unknown command ${command}\n\n${USAGE}`);
}

function readable(file: string) { try { readFileSync(file); return true; } catch { return false; } }

await main().catch(error => fail(describeError(error)));
