/** Batched reconstruction of Sourcify API contract records from a prepared source (prepare.ts). */
import { readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Database, literal, type Row } from './database';
import { sourceSchema, type SourceManifest } from './prepare';
import { buildStandardJson } from './standard-json';
import { checksumAddress } from './api-values';

export interface Source { db: Database; manifest: SourceManifest; close(): Promise<void> }

// Batch lookups are literal IN-lists. Above max(2048, 0.1% of table rows) matching rows DuckDB would
// replace the index scan by a full table scan (e.g. sources/codes at batch sizes >= ~500).
const indexScans = 'SET index_scan_max_count=1000000';

/** Opens a completed prepared source read-only; DuckDB spill files go to $TMPDIR. */
export async function openSource(sourceDir: string, memoryLimit: string): Promise<Source> {
  if (!/^[1-9][0-9]*(MB|GB)$/i.test(memoryLimit)) throw new Error('Memory limit must be a positive whole MB or GB value');
  const manifest: SourceManifest = JSON.parse(await readFile(join(sourceDir, 'source-manifest.json'), 'utf8'));
  if (manifest.schema !== sourceSchema || manifest.status !== 'completed') throw new Error(`Source must have a completed ${sourceSchema} source-manifest.json; prepare it from PostgreSQL (prepare.ts) in a fresh source directory`);
  const db = await Database.open(join(sourceDir, 'subset.duckdb'), { access_mode: 'READ_ONLY', threads: '1', memory_limit: memoryLimit }, tmpdir(), 'sourcify-source-');
  try { await db.connection.run(indexScans); } catch (error) { await db.close(); throw error; }
  return { db, manifest, close: () => db.close() };
}

// Only columns read by assembleContract: every fetched column costs an index-scan row fetch.
const columns = {
  verifications: 'id,deployment_id,compilation_id,creation_transformations,creation_values,runtime_transformations,runtime_values',
  deployments: 'id,chain_id,block_number,transaction_index,contract_id,address,transaction_hex,deployer_hex',
  compilations: 'id,language,compiler,version,name,fully_qualified_name,compiler_settings,compilation_artifacts,creation_code_artifacts,runtime_code_artifacts,additional_input,creation_hash,runtime_hash',
};
const values = (rows: Row[], key: string) => [...new Set(rows.map(row => String(row[key])))].map(literal).join(',');
const keyed = (rows: Row[], key: string) => new Map(rows.map(row => [String(row[key]), row]));
const grouped = (rows: Row[], key: string) => {
  const map = new Map<string, Row[]>();
  for (const row of rows) { const id = String(row[key]); const group = map.get(id) ?? []; group.push(row); map.set(id, group); }
  return map;
};
/** The row for `key`, else an error that names content keys (hashes) but never an internal row ID: the reason
 * of a skipped record is published in the completion marker and the bundle report. */
function required(map: Map<string, Row>, key: unknown, label: string, internalKey = false): Row {
  const row = map.get(String(key));
  if (!row) throw new Error(internalKey ? `Missing ${label}` : `Missing ${label}: ${String(key)}`);
  return row;
}

/** Yields every selected match in ID order: an assembled record, or the error that prevented it.
 * Shared bodies are read once per batch; the next batch is queried while the current one is consumed.
 * The database's internal row IDs (verification, deployment, compilation, contract) only join rows here; they are
 * never part of a record. */
export async function* iterateRecords(source: Source, options: { batchSize?: number; afterMatchId?: string } = {}): AsyncGenerator<{ matchId: string; record?: Row; error?: string }> {
  const batchSize = options.batchSize ?? 250;
  if (!Number.isSafeInteger(batchSize) || batchSize < 1 || batchSize > 1000) throw new Error('Source batch size must be between 1 and 1000');
  const cursor = options.afterMatchId ?? '-1';
  if (!/^-?\d+$/.test(cursor)) throw new Error('Invalid match cursor');
  const { db } = source;
  // Set when the consumer stops early: an in-flight prefetch then issues no further queries.
  let aborted = false;
  const stopped = Symbol('aborted');
  const rows = async (sql: string): Promise<Row[]> => {
    if (aborted) throw stopped;
    return db.rows(sql);
  };
  const load = async (after: string) => {
    try { return await query(after); } catch (error) { if (error === stopped) return undefined; throw error; }
  };
  const query = async (after: string) => {
    const matches = await rows(`SELECT * FROM matches WHERE id>${literal(after)}::BIGINT ORDER BY id LIMIT ${batchSize}`);
    if (!matches.length) return undefined;
    const verificationRows = await rows(`SELECT ${columns.verifications} FROM verifications WHERE id IN (${values(matches, 'verified_contract_id')})`);
    const deploymentRows = verificationRows.length ? await rows(`SELECT ${columns.deployments} FROM deployments WHERE id IN (${values(verificationRows, 'deployment_id')})`) : [];
    const compilationRows = verificationRows.length ? await rows(`SELECT ${columns.compilations} FROM compilations WHERE id IN (${values(verificationRows, 'compilation_id')})`) : [];
    const contractRows = deploymentRows.length ? await rows(`SELECT * FROM contracts WHERE id IN (${values(deploymentRows, 'contract_id')})`) : [];
    const compilationIds = values(compilationRows, 'id');
    const signatureRefs = compilationIds ? await rows(`SELECT * FROM signature_refs WHERE compilation_id IN (${compilationIds}) ORDER BY signature_type,hash`) : [];
    const signatureRows = signatureRefs.length ? await rows(`SELECT * FROM signatures WHERE hash IN (${values(signatureRefs, 'hash')})`) : [];
    const metadataRows = compilationIds ? await rows(`SELECT * FROM metadata WHERE compilation_id IN (${compilationIds})`) : [];
    const sourceRefs = compilationIds ? await rows(`SELECT * FROM source_refs WHERE compilation_id IN (${compilationIds}) ORDER BY path`) : [];
    const sourceRows = sourceRefs.length ? await rows(`SELECT * FROM sources WHERE hash IN (${values(sourceRefs, 'hash')})`) : [];
    const hashes = [...new Set([...compilationRows, ...contractRows].flatMap(row => [row.creation_hash, row.runtime_hash]).filter(Boolean))];
    const codeRows = hashes.length ? await rows(`SELECT * FROM codes WHERE hash IN (${hashes.map(literal).join(',')})`) : [];
    return { matches, verifications: keyed(verificationRows, 'id'), deployments: keyed(deploymentRows, 'id'), compilations: keyed(compilationRows, 'id'), contracts: keyed(contractRows, 'id'),
      signatureRefs: grouped(signatureRefs, 'compilation_id'), signatures: keyed(signatureRows, 'hash'), metadata: grouped(metadataRows, 'compilation_id'), paths: grouped(sourceRefs, 'compilation_id'), bodies: keyed(sourceRows, 'hash'), codes: keyed(codeRows, 'hash') };
  };
  let pending: ReturnType<typeof load> | undefined = load(cursor);
  try {
    for (;;) {
      const batch = await pending;
      pending = undefined;
      if (!batch) return;
      const { matches, verifications, deployments, compilations, contracts, signatureRefs, signatures, metadata, paths, bodies, codes } = batch;
      // The next batch's queries run on DuckDB's thread while this batch is assembled and consumed.
      pending = load(String(matches.at(-1)!.id));
      pending.catch(() => {});
      for (const match of matches) {
        const matchId = String(match.id);
        // Let DuckDB completions advance the prefetch query chain between records.
        await new Promise(resolve => setImmediate(resolve));
        try {
          const verification = required(verifications, match.verified_contract_id, 'verification', true);
          const deployment = required(deployments, verification.deployment_id, 'deployment', true);
          if (String(deployment.chain_id) !== '1') throw new Error('Selected deployment is not Ethereum mainnet');
          const compilation = required(compilations, verification.compilation_id, 'compilation', true);
          const deployed = required(contracts, deployment.contract_id, 'deployed contract', true);
          const meta = metadata.get(String(compilation.id)) ?? [];
          const sourcePaths = paths.get(String(compilation.id)) ?? [];
          const sources = sourcePaths.length ? Object.fromEntries(sourcePaths.map(path => [path.path, { content: required(bodies, path.hash, `source ${path.path}`).content }])) : null;
          const signatureGroups: Row = { function: [], event: [], error: [] };
          for (const ref of signatureRefs.get(String(compilation.id)) ?? []) {
            if (!Object.hasOwn(signatureGroups, ref.signature_type)) throw new Error(`Unknown signature type: ${ref.signature_type}`);
            const signature = required(signatures, ref.hash, 'signature');
            signatureGroups[ref.signature_type].push({ order: Number(signature.signature_order), signature: { signature: signature.signature, signatureHash32: signature.hash32, signatureHash4: signature.hash4 } });
          }
          // Sourcify orders each signature group by signature text in the database's collation (prepare.ts).
          for (const [type, group] of Object.entries(signatureGroups) as [string, Row[]][]) {
            signatureGroups[type] = group.sort((a, b) => a.order - b.order).map(entry => entry.signature);
          }
          const code = (hash: string | null) => hash ? required(codes, hash, 'bytecode').bytecode : null;
          yield { matchId, record: assembleContract({ match, verification, deployment, compilation, deployed, metadataRows: meta, sources, code, signatures: signatureGroups }) };
        } catch (error) {
          yield { matchId, error: error instanceof Error ? error.message : String(error) };
        }
      }
    }
  } finally {
    // Never close the source under an in-flight prefetch query; stop it after its current query.
    aborted = true;
    await pending?.catch(() => {});
  }
}

const json = (value: any, fallback: any = null) => value == null ? fallback : JSON.parse(value);
const integer = (value: any) => value == null ? null : String(value);
const status = (value: any) => value === 'perfect' ? 'exact_match' : value === 'partial' ? 'match' : value;
const languages: Row = { solidity: 'Solidity', vyper: 'Vyper', yul: 'Yul', fe: 'Fe' };

/** One Ethereum mainnet contract record in the shape the processor reads (Sourcify API v2 field names).
 * Signature groups come from the exported tables, including empty groups. */
export function assembleContract({ match, verification, deployment, compilation, deployed, metadataRows, sources, code, signatures }: {
  match: Row; verification: Row; deployment: Row; compilation: Row; deployed: Row; metadataRows: Row[]; sources: Row | null;
  code: (hash: string | null) => string | null; signatures: Row;
}) {
  const artifacts = json(compilation.compilation_artifacts, {});
  const creationMatch = status(match.creation_match), runtimeMatch = status(match.runtime_match);
  const nonempty = (value: string | null) => value === '0x' ? null : value;
  const bytecode = (kind: 'creation' | 'runtime') => {
    const artifacts = json(compilation[`${kind}_code_artifacts`], {});
    return {
      onchainBytecode: nonempty(code(deployed[`${kind}_hash`])), recompiledBytecode: nonempty(code(compilation[`${kind}_hash`])),
      sourceMap: artifacts.sourceMap ?? null, linkReferences: artifacts.linkReferences ?? null, cborAuxdata: artifacts.cborAuxdata ?? null,
      ...(kind === 'runtime' ? { immutableReferences: artifacts.immutableReferences ?? null } : {}),
      transformations: json(verification[`${kind}_transformations`]), transformationValues: json(verification[`${kind}_values`]),
    };
  };
  const apiCompilation = { language: languages[compilation.language.toLowerCase()] ?? compilation.language, compiler: compilation.compiler, compilerVersion: compilation.version, compilerSettings: json(compilation.compiler_settings), name: compilation.name, fullyQualifiedName: compilation.fully_qualified_name };
  const creationBytecode = bytecode('creation'), runtimeBytecode = bytecode('runtime');
  const additionalInput = json(compilation.additional_input);
  const createdAt = String(match.created_at);
  // prepare.ts stores UTC timestamps without a zone; anything else would render a shifted verifiedAt.
  if (!/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d(\.\d+)?$/.test(createdAt)) throw new Error(`Unexpected match timestamp ${createdAt}`);
  return {
    chainId: '1', address: checksumAddress(deployment.address), matchId: String(match.id), creationMatch, runtimeMatch,
    match: [creationMatch, runtimeMatch].includes('exact_match') ? 'exact_match' : [creationMatch, runtimeMatch].includes('match') ? 'match' : null,
    verifiedAt: createdAt.replace(' ', 'T').slice(0, 19) + 'Z',
    compilation: apiCompilation,
    deployment: { transactionHash: nonempty(deployment.transaction_hex), blockNumber: integer(deployment.block_number), transactionIndex: integer(deployment.transaction_index), deployer: checksumAddress(nonempty(deployment.deployer_hex)) },
    sources, creationBytecode, runtimeBytecode,
    metadata: compilation.language.toLowerCase() === 'solidity' ? json(metadataRows[0]?.metadata) : null,
    abi: artifacts.abi ?? null, userdoc: artifacts.userdoc ?? null, devdoc: artifacts.devdoc ?? null,
    storageLayout: artifacts.storageLayout ?? null, transientStorageLayout: artifacts.transientStorageLayout ?? null,
    sourceIds: artifacts.sources ?? null, additionalInput,
    ...buildStandardJson({ compilation: apiCompilation, sources, metadata: metadataRows[0]?.metadata ?? null, creationBytecode, runtimeBytecode, artifacts, additionalInput }),
    signatures,
  };
}
