/**
 * Parquet -> prepared DuckDB source: materializes the current Ethereum mainnet (chain 1) Sourcify matches and every
 * row they reference into SOURCE_DIR/subset.duckdb, indexed for batched lookups by stream.ts.
 *
 *   bun source/prepare.ts --parquet-dir DIR --source-dir DIR (--all | --limit N) [--threads 8] [--memory-limit 24GB]
 *
 * Reads the Parquet export read-only. Reuses a completed identical preparation and resumes an interrupted one;
 * a different selection in SOURCE_DIR is refused. Progress is JSON on stderr.
 */
import { mkdir, readFile, rename, stat, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { Database, literal } from './database';

export interface SourceManifest {
  schema: 'sourcify-arkiv.import-source.v1'; status: 'preparing' | 'completed' | 'failed';
  dataDir: string; limit: number; all?: boolean; selectedCount?: number; firstMatchId?: string; lastMatchId?: string;
  startedAt: string; finishedAt?: string; databaseBytes?: number; error?: string;
  /** Signature tables are never materialized; records omit `signatures` and `stdJsonInput`. */
  signatures?: false;
  steps: { name: string; seconds: number; sql: string }[];
  counts?: Record<string, string>;
}

/** Parquet tables read by the preparation, each a directory of *.parquet files below the Parquet directory. */
export const tableNames = ['contract_deployments', 'verified_contracts', 'sourcify_matches', 'compiled_contracts', 'compiled_contracts_metadata', 'contracts', 'compiled_contracts_sources', 'sources', 'code'];
const latestOrder = 'updated_at DESC NULLS LAST, created_at DESC NULLS LAST, verified_contract_id DESC';

/** Materialize a bounded sample or all current Ethereum matches; reconstruction stays batched (records.ts). */
export async function prepareSource(options: {
  limit?: number; all?: boolean; sourceDir: string; parquetDir: string; memoryLimit?: string; threads?: number; onProgress?: (step: string) => void;
}): Promise<SourceManifest> {
  const { limit } = options;
  if (options.all && limit !== undefined) throw new Error('Choose all or limit, not both');
  if (!options.all && (!Number.isSafeInteger(limit) || limit! < 1 || limit! > 100_000)) throw new Error('Limit must be between 1 and 100000');
  if (options.memoryLimit && !/^[1-9][0-9]*(?:MB|GB)$/i.test(options.memoryLimit)) throw new Error('Memory limit must be a positive whole MB or GB value');
  if (options.threads !== undefined && (!Number.isSafeInteger(options.threads) || options.threads < 1 || options.threads > 64)) throw new Error('Threads must be 1..64');
  const dataDir = resolve(options.parquetDir);
  const workDir = resolve(options.sourceDir);
  await mkdir(workDir, { recursive: true });
  const path = join(workDir, 'subset.duckdb');
  const manifestPath = join(workDir, 'source-manifest.json');
  let existing: SourceManifest | undefined;
  try { existing = JSON.parse(await readFile(manifestPath, 'utf8')); } catch (error: any) { if (error.code !== 'ENOENT') throw error; }
  if (existing) {
    if (Boolean(existing.all) !== Boolean(options.all) || (!options.all && existing.limit !== limit) || existing.dataDir !== dataDir) throw new Error('Source directory holds a different selection; use a fresh directory');
    if (existing.status === 'completed') return existing;
  }
  const manifest: SourceManifest = existing ?? { schema: 'sourcify-arkiv.import-source.v1', status: 'preparing', dataDir, limit: limit ?? 0, ...(options.all ? {all: true} : {}), signatures: false, startedAt: new Date().toISOString(), steps: [] };
  manifest.status = 'preparing'; delete manifest.error; delete manifest.finishedAt;
  const save = async () => { await writeFile(`${manifestPath}.tmp`, JSON.stringify(manifest, null, 2) + '\n'); await rename(`${manifestPath}.tmp`, manifestPath); };
  await save();
  const db = await Database.open(path, { threads: String(options.threads ?? 4), memory_limit: options.memoryLimit ?? '4GB' }, workDir, 'sourcify-spill-');
  const run = async (name: string, sql: string) => {
    options.onProgress?.(name);
    const started = performance.now();
    // DuckDB statements are atomic. A crash after commit but before the manifest save safely reuses that same table.
    const resumableSql = sql.replace(/^CREATE TABLE /, 'CREATE TABLE IF NOT EXISTS ').replace(/^CREATE INDEX /, 'CREATE INDEX IF NOT EXISTS ');
    await db.connection.run(resumableSql);
    manifest.steps = manifest.steps.filter(step => step.name !== name);
    manifest.steps.push({ name, seconds: (performance.now() - started) / 1000, sql });
    await save();
  };
  try {
    // Bulk materialization does not rely on physical row order: selection and reconstruction explicitly ORDER BY id.
    await db.connection.run('SET preserve_insertion_order=false');
    for (const name of tableNames) await db.connection.run(`CREATE OR REPLACE VIEW raw_${name} AS SELECT * FROM read_parquet(${literal(join(dataDir, name, '*.parquet'))},union_by_name=true)`);
    await run('Ethereum deployment candidates', 'CREATE TEMP TABLE ethereum_deployments AS SELECT id FROM raw_contract_deployments WHERE chain_id=1');
    await run('Ethereum verification candidates', 'CREATE TEMP TABLE ethereum_verifications AS SELECT id FROM raw_verified_contracts WHERE deployment_id IN (SELECT id FROM ethereum_deployments)');
    await run('Candidate match IDs', 'CREATE TEMP TABLE candidate_ids AS SELECT DISTINCT id FROM raw_sourcify_matches WHERE verified_contract_id IN (SELECT id FROM ethereum_verifications)');
    // Filter a mutable relationship only AFTER latest selection across every revision.
    await run('Current Ethereum selection', `CREATE TABLE matches AS SELECT * FROM (SELECT * FROM raw_sourcify_matches WHERE id IN (SELECT id FROM candidate_ids) QUALIFY row_number() OVER(PARTITION BY id ORDER BY ${latestOrder})=1) current_matches WHERE verified_contract_id IN (SELECT id FROM ethereum_verifications) ORDER BY id${options.all ? '' : ` LIMIT ${limit}`}`);
    const selected = await db.one('SELECT count(*) AS count,min(id)::VARCHAR AS first_id,max(id)::VARCHAR AS last_id FROM matches');
    manifest.selectedCount = Number(selected.count); manifest.firstMatchId = selected.first_id; manifest.lastMatchId = selected.last_id;
    if (!Number.isSafeInteger(manifest.selectedCount) || manifest.selectedCount < 1) throw new Error('No current Ethereum matches available');
    if (options.all) manifest.limit = manifest.selectedCount;
    await save();
    if (!options.all && manifest.selectedCount !== limit) throw new Error(`Only ${manifest.selectedCount} current Ethereum matches available, requested ${limit}`);
    await run('Selected verification rows', 'CREATE TABLE verifications AS SELECT * FROM raw_verified_contracts WHERE id IN (SELECT verified_contract_id FROM matches)');
    await run('Selected deployments', `CREATE TABLE deployments AS SELECT * EXCLUDE(address,transaction_hash,deployer), '0x'||lower(hex(address)) AS address, '0x'||lower(hex(transaction_hash)) AS transaction_hex, '0x'||lower(hex(deployer)) AS deployer_hex FROM raw_contract_deployments WHERE id IN (SELECT deployment_id FROM verifications)`);
    await run('Selected compilations', 'CREATE TABLE compilations AS SELECT * EXCLUDE(creation_code_hash,runtime_code_hash),hex(creation_code_hash) AS creation_hash,hex(runtime_code_hash) AS runtime_hash FROM raw_compiled_contracts WHERE id IN (SELECT compilation_id FROM verifications)');
    await run('Selected deployed contracts', 'CREATE TABLE contracts AS SELECT id,hex(creation_code_hash) AS creation_hash,hex(runtime_code_hash) AS runtime_hash FROM raw_contracts WHERE id IN (SELECT contract_id FROM deployments)');
    // compilation_id is unique in the export; the latest-row window (which must buffer every metadata body)
    // is only needed, and only run, when the selected rows actually contain duplicates.
    await run('Selected metadata', 'CREATE TABLE metadata AS SELECT compilation_id,metadata FROM raw_compiled_contracts_metadata WHERE compilation_id IN (SELECT id FROM compilations)');
    if (Number((await db.one('SELECT count(*)-count(DISTINCT compilation_id) AS duplicates FROM metadata')).duplicates) > 0) {
      await db.connection.run('DROP TABLE metadata');
      await run('Selected latest metadata', 'CREATE TABLE metadata AS SELECT compilation_id,metadata FROM raw_compiled_contracts_metadata WHERE compilation_id IN (SELECT id FROM compilations) QUALIFY row_number() OVER(PARTITION BY compilation_id ORDER BY created_at DESC)=1');
    }
    await run('Selected source references', 'CREATE TABLE source_refs AS SELECT compilation_id,path,hex(source_hash) AS hash FROM raw_compiled_contracts_sources WHERE compilation_id IN (SELECT id FROM compilations)');
    await run('Selected source bodies', 'CREATE TABLE sources AS SELECT hex(source_hash) AS hash,content FROM raw_sources WHERE source_hash IN (SELECT from_hex(hash) FROM source_refs)');
    await run('Selected code hashes', 'CREATE TEMP TABLE code_hashes AS SELECT creation_hash AS hash FROM compilations UNION SELECT runtime_hash FROM compilations UNION SELECT creation_hash FROM contracts UNION SELECT runtime_hash FROM contracts');
    await run('Selected bytecode bodies', `CREATE TABLE codes AS SELECT hex(code_hash) AS hash,'0x'||lower(hex(code)) AS bytecode FROM raw_code WHERE code_hash IN (SELECT from_hex(hash) FROM code_hashes WHERE hash IS NOT NULL)`);
    for (const name of ['ethereum_deployments', 'ethereum_verifications', 'candidate_ids', 'code_hashes']) await db.connection.run(`DROP TABLE ${name}`);
    for (const [table, key] of [['matches','id'],['verifications','id'],['deployments','id'],['compilations','id'],['contracts','id'],['metadata','compilation_id'],['source_refs','compilation_id'],['sources','hash'],['codes','hash']]) {
      await run(`Index ${table}`, `CREATE INDEX ${table}_lookup ON ${table}(${key})`);
    }
    // The derived database is self-contained; it has no view of unrelated chains.
    for (const name of tableNames) await db.connection.run(`DROP VIEW raw_${name}`);
    manifest.counts = {};
    for (const name of ['matches', 'verifications', 'deployments', 'compilations', 'contracts', 'metadata', 'source_refs', 'sources', 'codes']) {
      manifest.counts[name] = String((await db.one(`SELECT count(*) AS count FROM ${name}`)).count);
    }
    await run('Checkpoint derived database', 'CHECKPOINT');
    manifest.databaseBytes = (await stat(path)).size;
    manifest.status = 'completed'; manifest.finishedAt = new Date().toISOString(); await save();
    return manifest;
  } catch (error) {
    manifest.status = 'failed'; manifest.finishedAt = new Date().toISOString(); manifest.error = error instanceof Error ? error.message : String(error);
    await save(); throw error;
  } finally { await db.close(); }
}

const usage = `bun source/prepare.ts --parquet-dir DIR --source-dir DIR (--all | --limit N) [--threads 4] [--memory-limit 4GB]
Materializes all (or the first N, 1..100000, by match ID) current Ethereum mainnet matches from the Sourcify Parquet
export in DIR/<table>/*.parquet into SOURCE_DIR/subset.duckdb. Tables read: ${tableNames.join(', ')}.
Reuses a completed identical preparation and resumes an interrupted one; a different selection is refused.`;

async function main() {
  const { values } = parseArgs({ strict: true, options: {
    'parquet-dir': { type: 'string' }, 'source-dir': { type: 'string' }, limit: { type: 'string' }, all: { type: 'boolean', default: false },
    threads: { type: 'string', default: '4' }, 'memory-limit': { type: 'string', default: '4GB' }, help: { type: 'boolean', default: false },
  } });
  if (values.help) { console.error(usage); return; }
  if (!values['parquet-dir'] || !values['source-dir']) throw new Error('--parquet-dir and --source-dir are required (see --help)');
  if (values.all === (values.limit !== undefined)) throw new Error('Choose exactly one of --limit N or --all');
  const sourceDir = resolve(values['source-dir']);
  let reused = false;
  try { reused = JSON.parse(await readFile(join(sourceDir, 'source-manifest.json'), 'utf8')).status === 'completed'; } catch (error: any) { if (error.code !== 'ENOENT') throw error; }
  const started = performance.now();
  const elapsedSeconds = () => +((performance.now() - started) / 1000).toFixed(1);
  const manifest = await prepareSource({
    ...(values.all ? { all: true } : { limit: Number(values.limit) }), sourceDir, parquetDir: values['parquet-dir'],
    threads: Number(values.threads), memoryLimit: values['memory-limit'],
    onProgress: step => console.error(JSON.stringify({ sourcePreparation: step, elapsedSeconds: elapsedSeconds() })),
  });
  console.error(JSON.stringify({ prepared: join(sourceDir, 'subset.duckdb'), reused, selectedCount: manifest.selectedCount,
    firstMatchId: manifest.firstMatchId, lastMatchId: manifest.lastMatchId, databaseBytes: manifest.databaseBytes,
    steps: manifest.steps.map(step => ({ name: step.name, seconds: +step.seconds.toFixed(1) })), elapsedSeconds: elapsedSeconds() }));
}
if (import.meta.main) {
  try { await main(); } catch (error) { console.error(error instanceof Error ? error.message : String(error)); process.exitCode = 1; }
}
