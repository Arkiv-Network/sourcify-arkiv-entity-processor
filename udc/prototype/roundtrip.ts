/**
 * Writes the UDC prototype's entities (`arkiv-entity-processor udc-example` output) to a node in one
 * transaction, then reads them back the way a reader does and saves exactly what the node returned:
 *
 *   bun roundtrip.ts write ENTITIES.json OUT_DIR    # needs ARKIV_RPC_URL, ARKIV_CHAIN_ID, ARKIV_KEY_FILE
 *   bun roundtrip.ts read ADDRESS OWNER PREFIX OUT_DIR [abi,compilation]
 *   bun roundtrip.ts delete WRITE.json OUT_DIR       # removes what a write created
 *
 * `write` checks that the node assigns the keys the example predicted (salt 0 from the owner's entity
 * nonce) and records block, gas and rent. `read` runs the hydration read plan (docs/udc-proposal.md):
 * the contract by address, then its compilation and code by key (at most 8 keys per query, so the
 * node's planned path serves it), then sources and chunks by key. All reads are pinned to one block
 * when the node keeps history (ARKIV_PIN_BLOCK=1). It saves readback.json (the node's entities) and
 * reads.json (each query, its block, rows and time); `arkiv-entity-processor udc-hydrate` rebuilds
 * the contract from readback.json. The private key is read from a file and never printed.
 */
import { readFileSync, writeFileSync, mkdirSync } from "node:fs"
import { createPublicClient, createWalletClient, ExpirationTime, NO_SALT } from "@arkiv-network/sdk"
import { addr, bool, bytes, bytes32, defineSchema, i32, key, str, u64 } from "@arkiv-network/sdk/schema"
import { http, type Hex } from "viem"
import { privateKeyToAccount } from "viem/accounts"

const env = process.env
const url = env.ARKIV_RPC_URL ?? "https://alt-node-fast.arkiv-global.net/rpc"
const chainId = Number(env.ARKIV_CHAIN_ID ?? 7706815)
const chain = {
  id: chainId,
  name: `arkiv-${chainId}`,
  nativeCurrency: { name: "GLM", symbol: "GLM", decimals: 18 },
  rpcUrls: { default: { http: [url] } },
}
const transport = http(url, { retryCount: 0, timeout: 60_000 })
const pub = createPublicClient({ chain, transport, pollingInterval: 1000 })
const rpc = (method: string, params: unknown[]) => pub.request({ method, params } as any) as Promise<any>
const json = (v: unknown) => JSON.stringify(v, (_, x) => (typeof x === "bigint" ? x.toString() : x), 2) + "\n"

type Attr = { name: string; type: string; indexed: boolean; value: any }
type Entity = { key: Hex; owner: Hex; expiresAt: string; attributes: Attr[] }

const COLUMN: Record<string, () => any> = { str, bytes, key, addr, bytes32, u64, i32, bool }
const VALUE: Record<string, (v: any) => any> = {
  str: (v) => str(v),
  bytes: (v) => bytes(v),
  key: (v) => key(v),
  addr: (v) => addr(v),
  bytes32: (v) => bytes32(v),
  u64: (v) => u64(BigInt(v)),
  i32: (v) => i32(v),
  bool: (v) => bool(v),
}

/** One SDK schema per entity: its arkiv.schema tag plus its other attributes as columns. */
function create(e: Entity) {
  const tag = e.attributes.find((a) => a.name === "arkiv.schema")!.value as string
  const columns: Record<string, any> = {}
  const values: Record<string, any> = {}
  for (const a of e.attributes) {
    if (a.name === "arkiv.schema") continue
    const column = COLUMN[a.type]!()
    columns[a.name] = a.indexed ? column.indexed() : column
    values[a.name] = VALUE[a.type]!(a.value)
  }
  return { schema: defineSchema(tag, columns), values, expiration: ExpirationTime.permanent(), salt: NO_SALT }
}

async function write(entitiesFile: string, out: string) {
  const entities: Entity[] = JSON.parse(readFileSync(entitiesFile, "utf8"))
  const keyText = readFileSync(env.ARKIV_KEY_FILE!, "utf8").trim() as Hex
  const account = privateKeyToAccount(keyText)
  const wallet = createWalletClient({ chain, transport, account, pollingInterval: 1000 })
  if (entities.some((e) => e.owner.toLowerCase() !== account.address.toLowerCase()))
    throw new Error("the example was generated for another owner")
  const nonce = await pub.getEntityNonce(account.address)
  const predicted = await pub.predictEntityKeys({ owner: account.address, salts: entities.map(() => NO_SALT) })
  const mismatch = predicted.findIndex((p, i) => p.key !== entities[i]!.key)
  if (mismatch >= 0) throw new Error(`entity nonce ${nonce}: the example's key ${mismatch} is not the next key`)
  const balanceBefore = await pub.getBalance({ address: account.address })
  const started = Date.now()
  const result = await wallet.executeBatch({ creates: entities.map(create) as any })
  const receipt: any = await rpc("eth_getTransactionReceipt", [result.txHash])
  const balanceAfter = await pub.getBalance({ address: account.address, blockNumber: BigInt(receipt.blockNumber) })
  const tx: any = await rpc("eth_getTransactionByHash", [result.txHash])
  const record = {
    rpc: new URL(url).host,
    chainId,
    owner: account.address.toLowerCase(),
    firstNonce: nonce,
    txHash: result.txHash,
    block: Number(BigInt(receipt.blockNumber)),
    status: receipt.status,
    calldataBytes: (tx.input.length - 2) / 2,
    gasUsed: Number(BigInt(receipt.gasUsed)),
    effectiveGasPriceWei: BigInt(receipt.effectiveGasPrice).toString(),
    rentFeeWei: BigInt(receipt.rentFee ?? 0).toString(),
    balanceSpentWei: (balanceBefore - balanceAfter).toString(),
    created: result.createdEntities,
    keysAsPredicted: result.createdEntities.every((k, i) => k === entities[i]!.key),
    ms: Date.now() - started,
  }
  mkdirSync(out, { recursive: true })
  writeFileSync(`${out}/write.json`, json(record))
  console.log(json(record))
  if (!record.keysAsPredicted || record.status !== "0x1") process.exit(1)
}

const SELECT_ALL = { key: true, owner: true, creator: true, expiresAt: true, attributeSchema: true, attributes: true }
const hexBytes = (h: string) => Buffer.from(h.slice(2), "hex")
const keyList = (raw: Buffer, offset: number) => {
  const keys: Hex[] = []
  for (let i = offset; i < raw.length; i += 32) keys.push(`0x${raw.subarray(i, i + 32).toString("hex")}`)
  return keys
}
/** Chunk keys of a bytes value that is a chunk manifest (codec 2). */
const chunks = (value: string) => {
  const raw = hexBytes(value)
  return raw[0] === 2 ? keyList(raw, 37) : []
}

async function read(address: string, owner: string, prefix: string, out: string, fields?: string) {
  const pin = env.ARKIV_PIN_BLOCK === "1"
  const head = BigInt(await rpc("eth_blockNumber", []))
  const atBlock = pin ? `0x${head.toString(16)}` : undefined
  const reads: any[] = []
  const entities = new Map<string, any>()
  const query = async (what: string, q: string, select: any, limit: number) => {
    const started = Date.now()
    const r = await rpc("arkiv_query", [q, { select, limit, ...(atBlock ? { atBlock } : {}) }])
    reads.push({ what, query: q, select, limit, block: Number(BigInt(r.blockNumber)), rows: r.data.length, cursor: r.cursor ?? null, ms: Date.now() - started })
    if (r.cursor) throw new Error(`${what}: unexpected continuation`)
    for (const e of r.data) entities.set(e.key, e)
    return r.data as any[]
  }
  const attrs = (e: any) => Object.fromEntries(e.attributes.map((a: any) => [a.name, a.value]))
  const byKeys = async (what: string, keys: string[], select: any) => {
    const want = [...new Set(keys)].filter((k) => !entities.has(k))
    for (let i = 0; i < want.length; i += 8) {
      const batch = want.slice(i, i + 8)
      const rows = await query(what, batch.map((k) => `$key = key(${k})`).join(" OR "), select, batch.length)
      if (rows.length !== batch.length) throw new Error(`${what}: ${batch.length - rows.length} missing`)
    }
  }
  const scope = `$owner = addr(${owner.toLowerCase()}) AND arkiv.schema = str('${prefix}.contract')`
  const found = await query("contract", `${scope} AND address = addr(${address.toLowerCase()})`, SELECT_ALL, 2)
  if (found.length !== 1) throw new Error(`${found.length} contracts`)
  const c = attrs(found[0])
  const hot = fields === "abi,compilation"
  // Hot path: only the compilation's manifest, abi and compilation attributes.
  const compSelect = hot
    ? { ...SELECT_ALL, attributes: { "arkiv.schema": true, hash: true, manifest: true, abi: true, compilation: true } }
    : SELECT_ALL
  if (hot) await byKeys("compilation", [c.compilationref], compSelect)
  else await byKeys("compilation+code", [c.compilationref, c.creationcoderef, c.runtimecoderef].filter(Boolean), SELECT_ALL)
  const comp = attrs(entities.get(c.compilationref))
  const next: string[] = []
  for (const value of Object.values(comp)) if (typeof value === "string" && value.startsWith("0x02")) next.push(...chunks(value))
  if (!hot) {
    for (const k of [comp.creationcoderef, comp.runtimecoderef]) if (k) next.push(k)
    if (comp.sourcerefs) next.push(...keyList(hexBytes(comp.sourcerefs), 1))
  }
  await byKeys("source+code+chunk", next, SELECT_ALL)
  // Chunks of the dependencies just read.
  const more: string[] = []
  for (const e of entities.values()) for (const a of e.attributes) if (a.type === "bytes" && a.value.startsWith("0x02")) more.push(...chunks(a.value))
  await byKeys("chunk", more, SELECT_ALL)
  mkdirSync(out, { recursive: true })
  writeFileSync(`${out}/readback.json`, json([...entities.values()]))
  writeFileSync(`${out}/reads.json`, json({ rpc: new URL(url).host, pinned: pin, head: Number(head), fields: fields ?? "all", queries: reads }))
  console.log(json({ entities: entities.size, queries: reads.length, ms: reads.reduce((s, r) => s + r.ms, 0) }))
}

/** Deletes the entities a previous `write` created (its write.json), in one transaction. */
async function remove(writeFile: string, out: string) {
  const { created } = JSON.parse(readFileSync(writeFile, "utf8")) as { created: Hex[] }
  const account = privateKeyToAccount(readFileSync(env.ARKIV_KEY_FILE!, "utf8").trim() as Hex)
  const wallet = createWalletClient({ chain, transport, account, pollingInterval: 1000 })
  const result = await wallet.executeBatch({ deletes: created.map((entityKey) => ({ entityKey })) })
  const receipt: any = await rpc("eth_getTransactionReceipt", [result.txHash])
  const record = { txHash: result.txHash, block: Number(BigInt(receipt.blockNumber)), status: receipt.status, deleted: result.deletedEntities }
  mkdirSync(out, { recursive: true })
  writeFileSync(`${out}/delete.json`, json(record))
  console.log(json(record))
}

const [mode, ...args] = process.argv.slice(2)
if (mode === "write") await write(args[0]!, args[1]!)
else if (mode === "delete") await remove(args[0]!, args[1]!)
else if (mode === "read") await read(args[0]!, args[1]!, args[2]!, args[3]!, args[4])
else throw new Error("usage: bun roundtrip.ts write ENTITIES OUT | read ADDRESS OWNER PREFIX OUT [abi,compilation] | delete WRITE.json OUT")
