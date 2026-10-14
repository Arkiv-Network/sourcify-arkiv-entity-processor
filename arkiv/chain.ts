/** Arkiv connection pieces shared by the helper CLI (cli.ts) and the Postgres -> Arkiv sync (sync.ts). */
import { createPublicClient } from '@arkiv-network/sdk';
import { defineChain, encodeFunctionData, http, type Hex } from 'viem';
import { privateKeyToAccount } from 'viem/accounts';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const DEFAULT_RPC_URL = 'https://sourcify-prefill.arkiv-global.net/rpc';
export const ARKIV_ADDRESS = '0x4400000000000000000000000000000000000044';
// Encoding without sending uses the SDK's internal operation encoders, which only this exact version is checked against.
export const SDK_VERSION = '0.8.1-dev.1';

export function rpcSettings(env: Record<string, string | undefined>) {
  const rpcUrl = env.ARKIV_RPC_URL || DEFAULT_RPC_URL;
  const transport = http(rpcUrl, { timeout: 30_000, retryCount: 2, fetchOptions: env.ARKIV_RPC_API_KEY ? { headers: { 'X-Api-Key': env.ARKIV_RPC_API_KEY } } : undefined });
  return { rpcUrl, transport };
}

/** A public client for the RPC's own chain; with ARKIV_CHAIN_ID set, a different chain is refused. */
export async function connect(env: Record<string, string | undefined>) {
  const { rpcUrl, transport } = rpcSettings(env);
  const id = await createPublicClient({ transport }).getChainId();
  if (env.ARKIV_CHAIN_ID && Number(env.ARKIV_CHAIN_ID) !== id) throw new Error(`RPC reports chain ${id}, ARKIV_CHAIN_ID is ${env.ARKIV_CHAIN_ID}`);
  const chain = defineChain({ id, name: `arkiv-${id}`, nativeCurrency: { name: 'Ether', symbol: 'ETH', decimals: 18 }, rpcUrls: { default: { http: [rpcUrl] } } });
  return { rpcUrl, transport, chain, client: createPublicClient({ chain, transport, pollingInterval: 1000 }) };
}

/** The writer account from a file holding a 32-byte hex private key (never printed). */
export function writerAccount(file: string) {
  let secret: string;
  try { secret = readFileSync(file, 'utf8').trim(); } catch { throw new Error(`cannot read the writer key file ${file}`); }
  if (!/^(0x)?[0-9a-fA-F]{64}$/.test(secret)) throw new Error(`${file} does not hold a 32-byte hex private key`);
  return privateKeyToAccount((secret.startsWith('0x') ? secret : `0x${secret}`) as Hex);
}

let internals: Promise<any> | undefined;
/** encodeAttributes, createOperation, deleteOperation, EXECUTE_ABI, ... of the pinned SDK. */
export function sdkInternals(): Promise<any> {
  return internals ??= (async () => {
    const root = dirname(fileURLToPath(import.meta.resolve('@arkiv-network/sdk/package.json')));
    const { version } = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
    if (version !== SDK_VERSION) throw new Error(`operation encoding is checked against @arkiv-network/sdk ${SDK_VERSION}, installed is ${version}`);
    const load = (path: string) => import(pathToFileURL(join(root, 'src', path)).href);
    const [attr, operations] = await Promise.all([load('attr/attributes.ts'), load('entity/operations.ts')]);
    return { ...attr, ...operations };
  })();
}

/** Calldata of one `execute` call with these operations. */
export async function executeCalldata(operations: unknown[]): Promise<Hex> {
  const { EXECUTE_ABI } = await sdkInternals();
  return encodeFunctionData({ abi: EXECUTE_ABI, functionName: 'execute', args: [operations] });
}

/** viem and the SDK wrap RPC errors several times; the innermost one names the actual cause. */
export function describeError(error: any) {
  const messages: string[] = [];
  for (let current = error; current; current = current.cause) {
    const message = current.details ?? current.shortMessage ?? current.message;
    if (message && !messages.includes(message)) messages.push(message);
  }
  return messages.length ? messages.join(': ') : String(error);
}
