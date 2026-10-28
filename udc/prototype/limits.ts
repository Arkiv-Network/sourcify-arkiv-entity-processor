/**
 * Measures the node limits the UDC format depends on (docs/udc-proposal.md, "Measured limits").
 *
 *   bun limits.ts sizes OWNER                 # eth_estimateGas only: no transaction is sent
 *   bun limits.ts paging PREFIX               # writes small expiring probe entities, then pages them
 *   bun limits.ts patchorder PREFIX           # patches expiring probes, then checks result order and updatedAt
 *
 * `sizes` encodes creates with growing bytes values and attribute counts and asks the node to estimate
 * them (estimation runs the create's checks and discards the result); it also reports the calldata
 * and signed-transaction sizes, which bound what a reth pool (128 KiB) admits. `paging` writes 70
 * entities that expire after one day (ARKIV_KEY_FILE), whose `matchid` order differs from their
 * creation order, then checks page caps, cursors with and without `atBlock`, and result order.
 */
import { readFileSync } from "node:fs"
import { createPublicClient, createWalletClient, ExpirationTime, NO_SALT } from "@arkiv-network/sdk"
import { defineSchema, u64 } from "@arkiv-network/sdk/schema"
import { encodeAbiParameters, encodeFunctionData, http, parseAbiParameters, serializeTransaction, toHex, type Hex } from "viem"
import { privateKeyToAccount } from "viem/accounts"

const env = process.env
const url = env.ARKIV_RPC_URL ?? "https://alt-node-fast.arkiv-global.net/rpc"
const chainId = Number(env.ARKIV_CHAIN_ID ?? 7706815)
const chain = { id: chainId, name: `arkiv-${chainId}`, nativeCurrency: { name: "GLM", symbol: "GLM", decimals: 18 }, rpcUrls: { default: { http: [url] } } }
const transport = http(url, { retryCount: 0, timeout: 60_000 })
const pub = createPublicClient({ chain, transport, pollingInterval: 1000 })
const rpc = (method: string, params: unknown[]) => pub.request({ method, params } as any) as Promise<any>
const ARKIV = "0x4400000000000000000000000000000000000044"
const EXECUTE = [{ type: "function", name: "execute", stateMutability: "nonpayable", inputs: [{ name: "ops", type: "tuple[]", components: [{ name: "operation", type: "uint8" }, { name: "operationData", type: "bytes" }] }], outputs: [{ name: "keys", type: "bytes32[]" }] }] as const
const name32 = (s: string) => toHex(new TextEncoder().encode(s), { size: 32 })

type A = { name: string; typeId: number; value: Hex }
const bytesAttr = (name: string, n: number): A => ({ name, typeId: 7, value: toHex(new Uint8Array(n).map((_, i) => (i * 31 + 7) % 256)) })
const strAttr = (name: string, s: string, indexed = true): A => ({ name, typeId: (indexed ? 0x80 : 0) | 8, value: toHex(new TextEncoder().encode(s)) })
function createCalldata(attrs: A[], expiresAt = 2n ** 64n - 1n) {
  attrs.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))
  const data = encodeAbiParameters(parseAbiParameters("(uint128 salt, uint64 expiresAt, uint64 minLifetime, uint8 creationFlags, (bytes32 name, uint8 typeId, bytes value)[] attributes)"), [
    { salt: 0n, expiresAt, minLifetime: 0n, creationFlags: 0, attributes: attrs.map((a) => ({ name: name32(a.name), typeId: a.typeId, value: a.value })) },
  ])
  return encodeFunctionData({ abi: EXECUTE, functionName: "execute", args: [[{ operation: 1, operationData: data }]] })
}

async function sizes(owner: Hex) {
  const cases: [string, A[]][] = [
    ["bytes 120000", [bytesAttr("data", 120_000)]],
    ["bytes 131072 (128 KiB)", [bytesAttr("data", 131_072)]],
    ["bytes 131073", [bytesAttr("data", 131_073)]],
    ["2 bytes x 65536 (128 KiB total)", [bytesAttr("a", 65_536), bytesAttr("b", 65_536)]],
    ["2 bytes 65536 + 65537", [bytesAttr("a", 65_536), bytesAttr("b", 65_537)]],
    ["32 attributes", Array.from({ length: 32 }, (_, i) => strAttr(`a${String(i).padStart(2, "0")}`, "x"))],
    ["33 attributes", Array.from({ length: 33 }, (_, i) => strAttr(`a${String(i).padStart(2, "0")}`, "x"))],
    ["str 128 bytes", [strAttr("s", "x".repeat(128))]],
    ["str 129 bytes", [strAttr("s", "x".repeat(129))]],
    ["indexed bytes", [{ ...bytesAttr("data", 4), typeId: 0x87 }]],
    ["name 32 bytes", [strAttr("n".repeat(32), "x")]],
    ["24 indexed + 120000 bytes", [...Array.from({ length: 23 }, (_, i) => strAttr(`a${String(i).padStart(2, "0")}`, "value-of-attr")), bytesAttr("data", 120_000)]],
  ]
  for (const [label, attrs] of cases) {
    const data = createCalldata(attrs)
    const signedBytes = (serializeTransaction({ chainId, type: "eip1559", to: ARKIV, data, nonce: 1000, gas: 5_000_000n, maxFeePerGas: 2_000_000_000n, maxPriorityFeePerGas: 0n, value: 0n, r: `0x${"11".repeat(32)}`, s: `0x${"22".repeat(32)}`, yParity: 0 } as any).length - 2) / 2
    let outcome: string
    try {
      outcome = `ok gas ${BigInt(await rpc("eth_estimateGas", [{ from: owner, to: ARKIV, data }]))}`
    } catch (e: any) {
      outcome = `refused: ${(e.details ?? e.shortMessage ?? String(e)).slice(0, 140)}`
    }
    console.log(JSON.stringify({ case: label, calldataBytes: (data.length - 2) / 2, signedTxBytes: signedBytes, outcome }))
  }
}

async function paging(prefix: string) {
  const account = privateKeyToAccount(readFileSync(env.ARKIV_KEY_FILE!, "utf8").trim() as Hex)
  const wallet = createWalletClient({ chain, transport, account, pollingInterval: 1000 })
  const Probe = defineSchema(`${prefix}.probe`, { seq: u64().indexed(), matchid: u64().indexed() })
  const run = BigInt(Date.now())
  const N = 70
  // matchid order is the reverse of creation order in each group of ten.
  const rows = Array.from({ length: N }, (_, i) => ({ seq: BigInt(i), matchid: run * 1000n + BigInt(Math.floor(i / 10) * 10 + (9 - (i % 10))) }))
  let block = 0n
  for (let i = 0; i < N; i += 14) {
    const r = await wallet.executeBatch({
      creates: rows.slice(i, i + 14).map((v) => ({ schema: Probe, values: { seq: u64(v.seq), matchid: u64(v.matchid) }, expiration: ExpirationTime.fromBlocks(43_200), salt: NO_SALT })) as any,
    })
    const receipt: any = await rpc("eth_getTransactionReceipt", [r.txHash])
    block = BigInt(receipt.blockNumber)
    console.log(JSON.stringify({ tx: r.txHash, block: Number(block), created: r.createdEntities.length, rentFeeWei: BigInt(receipt.rentFee ?? 0).toString() }))
  }
  const scope = `$owner = addr(${account.address.toLowerCase()}) AND arkiv.schema = str('${prefix}.probe') AND matchid >= u64(${run * 1000n})`
  const q = async (label: string, opts: any) => {
    try {
      const r = await rpc("arkiv_query", [scope, { select: { key: true, attributes: { seq: true, matchid: true } }, ...opts }])
      const seqs = r.data.map((e: any) => Number(BigInt(e.attributes.find((a: any) => a.name === "seq").value)))
      console.log(JSON.stringify({ label, block: r.blockNumber, rows: r.data.length, cursor: r.cursor ? `${String(r.cursor).slice(0, 16)}…` : null, firstSeqs: seqs.slice(0, 12) }))
      return r
    } catch (e: any) {
      console.log(JSON.stringify({ label, error: (e.details ?? e.shortMessage ?? String(e)).slice(0, 160) }))
      return null
    }
  }
  const atBlock = `0x${block.toString(16)}`
  for (const limit of [64, 65, 100, 200]) await q(`limit ${limit}`, { limit, atBlock })
  const p1 = await q("page 1 (limit 30, atBlock)", { limit: 30, atBlock })
  if (p1?.cursor) {
    await q("page 2 with cursor + atBlock", { limit: 30, atBlock, cursor: p1.cursor })
    await q("page 2 with cursor, no atBlock", { limit: 30, cursor: p1.cursor })
  }
  const p0 = await q("page 1 (limit 30, head)", { limit: 30 })
  if (p0?.cursor) await q("page 2 with cursor + head atBlock", { limit: 30, cursor: p0.cursor, atBlock: p0.blockNumber })
  // Keyset paging on matchid: does the node return ascending matchid or creation order?
  const order = await rpc("arkiv_query", [scope, { select: { attributes: { seq: true, matchid: true } }, limit: 20, atBlock }])
  const pairs = order.data.map((e: any) => e.attributes.map((a: any) => Number(BigInt(a.value) % 1000n)))
  console.log(JSON.stringify({ label: "order of first 20 (matchid%1000, seq)", pairs }))
  const count = await rpc("arkiv_getEntityCount", [{ query: scope, block: Number(block) }])
  console.log(JSON.stringify({ label: "count", count }))
}

/**
 * Where a patched entity sorts. Creates six probes in two transactions (matchid ascending with creation),
 * then patches the first and the fourth to the highest matchids, as a re-verification patches a contract
 * in place. Reports, before and after the patch: result order, createdAt and updatedAt, the order of a
 * `matchid` range query, the pre-patch view through `atBlock`, and a matchid-window walk with limit 2.
 */
async function patchorder(prefix: string) {
  const account = privateKeyToAccount(readFileSync(env.ARKIV_KEY_FILE!, "utf8").trim() as Hex)
  const wallet = createWalletClient({ chain, transport, account, pollingInterval: 1000 })
  const Probe = defineSchema(`${prefix}.patchprobe`, { seq: u64().indexed(), matchid: u64().indexed() })
  const base = BigInt(Date.now()) * 1000n
  const keys: Hex[] = []
  for (const group of [[0, 1, 2], [3, 4, 5]]) {
    const r = await wallet.executeBatch({
      creates: group.map((i) => ({ schema: Probe, values: { seq: u64(BigInt(i)), matchid: u64(base + BigInt(i) * 10n) }, expiration: ExpirationTime.fromBlocks(43_200), salt: NO_SALT })) as any,
    })
    const receipt: any = await rpc("eth_getTransactionReceipt", [r.txHash])
    keys.push(...r.createdEntities)
    console.log(JSON.stringify({ step: "create", tx: r.txHash, block: Number(BigInt(receipt.blockNumber)), keys: r.createdEntities }))
  }
  const before = BigInt(await rpc("eth_blockNumber", []))
  const p = await wallet.executeBatch({
    patches: [
      { schema: Probe, entityKey: keys[0]!, set: { matchid: u64(base + 900n) } },
      { schema: Probe, entityKey: keys[3]!, set: { matchid: u64(base + 950n) } },
    ] as any,
  })
  const pr: any = await rpc("eth_getTransactionReceipt", [p.txHash])
  const patchBlock = BigInt(pr.blockNumber)
  console.log(JSON.stringify({ step: "patch", tx: p.txHash, block: Number(patchBlock), patched: p.patchedEntities, status: pr.status }))
  const scope = `$owner = addr(${account.address.toLowerCase()}) AND arkiv.schema = str('${prefix}.patchprobe')`
  const rows = async (label: string, q: string, opts: any) => {
    const r = await rpc("arkiv_query", [q, { select: { key: true, createdAt: true, updatedAt: true, attributes: { seq: true, matchid: true } }, ...opts }])
    const out = r.data.map((e: any) => {
      const a = Object.fromEntries(e.attributes.map((x: any) => [x.name, BigInt(x.value)]))
      return { seq: Number(a.seq), matchid: Number(a.matchid - base), createdAt: Number(BigInt(e.createdAt)), updatedAt: Number(BigInt(e.updatedAt)) }
    })
    console.log(JSON.stringify({ step: label, block: Number(BigInt(r.blockNumber)), rows: out, cursor: r.cursor ? "yes" : null }))
    return out
  }
  const at = `0x${patchBlock.toString(16)}`
  await rpc("arkiv_query", [scope, { limit: 1, atBlock: at }])
  await rows("order after patch (atBlock = patch block)", `${scope} AND matchid >= u64(${base})`, { limit: 100, atBlock: at })
  await rows("order before patch (atBlock = before)", `${scope} AND matchid >= u64(${base})`, { limit: 100, atBlock: `0x${before.toString(16)}` })
  await rows("range matchid > base+15 after patch", `${scope} AND matchid > u64(${base + 15n})`, { limit: 100, atBlock: at })
  // The matchid-window walk (docs/udc-proposal.md, paging) with limit 2: a page with fewer than `limit`
  // rows is complete for its window, whatever order the node returns.
  const limit = 2
  const walk: number[] = []
  let after = base - 1n
  for (let guard = 0; guard < 50; guard++) {
    let high: bigint | undefined
    let got: any[] = []
    for (;;) {
      const bound = high === undefined ? "" : ` AND matchid <= u64(${high})`
      got = await rows(`window (${after - base}, ${high === undefined ? "inf" : high - base}]`, `${scope} AND matchid > u64(${after})${bound}`, { limit, atBlock: at })
      if (got.length < limit) break
      const sorted = got.map((g) => BigInt(g.matchid) + base).sort((a, b) => (a < b ? -1 : 1))
      high = sorted[0]! // the smallest of a full page bounds a window holding fewer rows
    }
    got.sort((a, b) => a.matchid - b.matchid)
    walk.push(...got.map((g) => g.matchid))
    if (high === undefined) break
    after = high
  }
  console.log(JSON.stringify({ step: "window walk (matchid - base, ascending)", walk }))
  const count = await rpc("arkiv_getEntityCount", [{ query: scope, block: Number(patchBlock) }])
  console.log(JSON.stringify({ step: "count", count, expiresAfterBlocks: 43_200 }))
}

const [mode, arg] = process.argv.slice(2)
if (mode === "sizes") await sizes(arg as Hex)
else if (mode === "paging") await paging(arg!)
else if (mode === "patchorder") await patchorder(arg!)
else throw new Error("usage: bun limits.ts sizes OWNER | paging PREFIX | patchorder PREFIX")
