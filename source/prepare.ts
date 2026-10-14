/**
 * Sourcify PostgreSQL database -> prepared DuckDB source: materializes the Ethereum mainnet (chain 1) Sourcify matches
 * and every row they reference into SOURCE_DIR/subset.duckdb, indexed for batched lookups by stream.ts.
 *
 *   PGHOST=... PGPORT=... PGDATABASE=... PGUSER=... PGPASSWORD=... \
 *     bun source/prepare.ts --source-dir DIR (--all | --limit N) [--pg-schema public] [--threads 8] [--memory-limit 24GB]
 *
 * The database is a Sourcify mirror (e.g. the sourcify-mirror-node), read through DuckDB's postgres extension in a
 * read-only attachment; the connection comes from the libpq PG* environment variables (the password is never written).
 * Its internal row IDs only join the tables here: records (records.ts) carry API fields only.
 * Reuses a completed identical preparation and resumes an interrupted one; a different selection in SOURCE_DIR is
 * refused. Progress is JSON on stderr.
 */
import { mkdir, readFile, rename, stat, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { Database, literal } from './database';

export interface SourceManifest {
  schema: 'sourcify-arkiv.import-source.v3'; status: 'preparing' | 'completed' | 'failed';
  /** Where the rows came from; the password is not recorded. */
  database: SourceDatabase; limit: number; all?: boolean; selectedCount?: number; firstMatchId?: string; lastMatchId?: string;
  startedAt: string; finishedAt?: string; databaseBytes?: number; error?: string;
  steps: { name: string; seconds: number; sql: string }[];
  counts?: Record<string, string>;
}

export interface SourceDatabase { host: string; port: string; database: string; user: string; schema: string }
export const sourceSchema = 'sourcify-arkiv.import-source.v3';

/** Sourcify tables read by the preparation (SELECT only). */
export const tableNames = ['contract_deployments', 'verified_contracts', 'sourcify_matches', 'compiled_contracts', 'compiled_contracts_metadata', 'contracts', 'compiled_contracts_sources', 'sources', 'code', 'compiled_contracts_signatures', 'signatures'];
/** Match IDs from here on are the database's own (rows a node created itself, e.g. through its verify API), not the
 * upstream Sourcify matchIds a mirror carries; the sourcify-mirror-node starts its sequences here. */
export const localMatchIdStart = 1_000_000_000_000n;

/** The libpq connection the postgres extension uses: the PG* environment variables (PGPASSWORD is not returned). */
export function sourceDatabase(env: Record<string, string | undefined>, schema = 'public'): SourceDatabase {
  for (const name of ['PGHOST', 'PGDATABASE', 'PGUSER']) if (!env[name]) throw new Error(`${name} is required (libpq environment variables PGHOST, PGPORT, PGDATABASE, PGUSER, PGPASSWORD)`);
  if (!/^[a-z_][a-z0-9_]*$/.test(schema)) throw new Error('--pg-schema must be a lowercase SQL identifier');
  return { host: env.PGHOST!, port: env.PGPORT || '5432', database: env.PGDATABASE!, user: env.PGUSER!, schema };
}

/** Materialize a bounded sample or all current Ethereum matches; reconstruction stays batched (records.ts).
 * Without `database`, only a completed preparation of the same selection is accepted (reused offline). */
export async function prepareSource(options: {
  limit?: number; all?: boolean; sourceDir: string; database?: SourceDatabase; memoryLimit?: string; threads?: number; onProgress?: (step: string) => void;
}): Promise<SourceManifest> {
  const { limit } = options;
  if (options.all && limit !== undefined) throw new Error('Choose all or limit, not both');
  if (!options.all && (!Number.isSafeInteger(limit) || limit! < 1 || limit! > 100_000)) throw new Error('Limit must be between 1 and 100000');
  if (options.memoryLimit && !/^[1-9][0-9]*(?:MB|GB)$/i.test(options.memoryLimit)) throw new Error('Memory limit must be a positive whole MB or GB value');
  if (options.threads !== undefined && (!Number.isSafeInteger(options.threads) || options.threads < 1 || options.threads > 64)) throw new Error('Threads must be 1..64');
  const { database } = options;
  const workDir = resolve(options.sourceDir);
  await mkdir(workDir, { recursive: true });
  const path = join(workDir, 'subset.duckdb');
  const manifestPath = join(workDir, 'source-manifest.json');
  let existing: SourceManifest | undefined;
  try { existing = JSON.parse(await readFile(manifestPath, 'utf8')); } catch (error: any) { if (error.code !== 'ENOENT') throw error; }
  if (existing) {
    if (existing.schema !== sourceSchema) throw new Error('Prepared source format is outdated (not prepared from PostgreSQL); regenerate in a fresh source directory');
    if (Boolean(existing.all) !== Boolean(options.all) || (!options.all && existing.limit !== limit) || (database && JSON.stringify(existing.database) !== JSON.stringify(database))) throw new Error('Source directory holds a different selection; use a fresh directory');
    if (existing.status === 'completed') return existing;
  }
  if (!database) throw new Error('No completed prepared source: preparing one reads the Sourcify database (set PGHOST, PGPORT, PGDATABASE, PGUSER, PGPASSWORD)');
  const manifest: SourceManifest = existing ?? { schema: sourceSchema, status: 'preparing', database, limit: limit ?? 0, ...(options.all ? {all: true} : {}), startedAt: new Date().toISOString(), steps: [] };
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
    // Timestamps (verifiedAt) are read and written in UTC; numeric block numbers stay exact text, not DOUBLE.
    await db.connection.run("SET TimeZone='UTC'");
    await db.connection.run('INSTALL postgres');
    await db.connection.run('LOAD postgres');
    await db.connection.run('SET pg_numeric_as_varchar=true');
    // Empty connection string: libpq reads PGHOST, PGPORT, PGDATABASE, PGUSER and PGPASSWORD.
    await db.connection.run("ATTACH IF NOT EXISTS '' AS pg (TYPE postgres, READ_ONLY)");
    for (const name of tableNames) await db.connection.run(`CREATE OR REPLACE TEMP VIEW raw_${name} AS SELECT * FROM pg.${database.schema}.${name}`);
    // Each step reads its own database snapshot. A writer that commits a match with all its rows in one transaction
    // (Sourcify, the mirror's sync) cannot make the selection inconsistent: the match IDs are fixed after the
    // deployments and verifications they are filtered by, and every later step only reads rows those reference.
    await run('Ethereum deployment candidates', 'CREATE TEMP TABLE ethereum_deployments AS SELECT id FROM raw_contract_deployments WHERE chain_id=1');
    await run('Ethereum verification candidates', 'CREATE TEMP TABLE ethereum_verifications AS SELECT id FROM raw_verified_contracts WHERE deployment_id IN (SELECT id FROM ethereum_deployments)');
    // Timestamps as UTC without a zone: DuckDB's Node client renders TIMESTAMPTZ in the process's local time zone.
    await run('Current Ethereum selection', `CREATE TABLE matches AS SELECT * REPLACE (timezone('UTC', created_at) AS created_at, timezone('UTC', updated_at) AS updated_at) FROM raw_sourcify_matches WHERE verified_contract_id IN (SELECT id FROM ethereum_verifications) ORDER BY id${options.all ? '' : ` LIMIT ${limit}`}`);
    const local = await db.one(`SELECT count(*) AS count FROM matches WHERE id>=${localMatchIdStart}`);
    if (Number(local.count) > 0) throw new Error(`${local.count} selected matches have IDs from ${localMatchIdStart} on: they were created by the node itself, not mirrored from Sourcify, and have no Sourcify matchId`);
    const selected = await db.one('SELECT count(*) AS count,min(id)::VARCHAR AS first_id,max(id)::VARCHAR AS last_id FROM matches');
    manifest.selectedCount = Number(selected.count); manifest.firstMatchId = selected.first_id; manifest.lastMatchId = selected.last_id;
    if (!Number.isSafeInteger(manifest.selectedCount) || manifest.selectedCount < 1) throw new Error('No current Ethereum matches available');
    if (options.all) manifest.limit = manifest.selectedCount;
    await save();
    if (!options.all && manifest.selectedCount !== limit) throw new Error(`Only ${manifest.selectedCount} current Ethereum matches available, requested ${limit}`);
    await run('Selected verification rows', 'CREATE TABLE verifications AS SELECT * FROM raw_verified_contracts WHERE id IN (SELECT verified_contract_id FROM matches)');
    await run('Selected deployments', `CREATE TABLE deployments AS SELECT * EXCLUDE(address,transaction_hash,deployer), '0x'||lower(hex(address)) AS address, '0x'||lower(hex(transaction_hash)) AS transaction_hex, '0x'||lower(hex(deployer)) AS deployer_hex FROM raw_contract_deployments WHERE id IN (SELECT deployment_id FROM verifications)`);
    await run('Selected compilations', 'CREATE TABLE compilations AS SELECT * EXCLUDE(creation_code_hash,runtime_code_hash),hex(creation_code_hash) AS creation_hash,hex(runtime_code_hash) AS runtime_hash FROM raw_compiled_contracts WHERE id IN (SELECT compilation_id FROM verifications)');
    await run('Selected signature references', 'CREATE TABLE signature_refs AS SELECT DISTINCT compilation_id,hex(signature_hash_32) AS hash,signature_type FROM raw_compiled_contracts_signatures WHERE compilation_id IN (SELECT id FROM compilations)');
    // The Sourcify API orders each signature group by signature text in the database's collation (ORDER BY
    // signatures.signature), which no byte or locale comparison here reproduces exactly: PostgreSQL ranks them.
    const signatureOrder = `SELECT signature_hash_32,row_number() OVER (ORDER BY signature,signature_hash_32) AS signature_order FROM ${database.schema}.signatures`;
    await run('Signature collation order', `CREATE TEMP TABLE signature_order AS SELECT * FROM postgres_query('pg',${literal(signatureOrder)}) WHERE signature_hash_32 IN (SELECT from_hex(hash) FROM signature_refs)`);
    await run('Selected signature bodies', "CREATE TABLE signatures AS SELECT hex(s.signature_hash_32) AS hash,s.signature,'0x'||lower(hex(s.signature_hash_32)) AS hash32,'0x'||lower(hex(s.signature_hash_4)) AS hash4,o.signature_order FROM raw_signatures s JOIN signature_order o ON o.signature_hash_32=s.signature_hash_32");
    await run('Selected deployed contracts', 'CREATE TABLE contracts AS SELECT id,hex(creation_code_hash) AS creation_hash,hex(runtime_code_hash) AS runtime_hash FROM raw_contracts WHERE id IN (SELECT contract_id FROM deployments)');
    // metadata is a json column: its stored text, which the Sourcify API returns inside stdJsonOutput.
    await run('Selected metadata', 'CREATE TABLE metadata AS SELECT compilation_id,metadata FROM raw_compiled_contracts_metadata WHERE compilation_id IN (SELECT id FROM compilations)');
    await run('Selected source references', 'CREATE TABLE source_refs AS SELECT compilation_id,path,hex(source_hash) AS hash FROM raw_compiled_contracts_sources WHERE compilation_id IN (SELECT id FROM compilations)');
    await run('Selected source bodies', 'CREATE TABLE sources AS SELECT hex(source_hash) AS hash,content FROM raw_sources WHERE source_hash IN (SELECT from_hex(hash) FROM source_refs)');
    await run('Selected code hashes', 'CREATE TEMP TABLE code_hashes AS SELECT creation_hash AS hash FROM compilations UNION SELECT runtime_hash FROM compilations UNION SELECT creation_hash FROM contracts UNION SELECT runtime_hash FROM contracts');
    await run('Selected bytecode bodies', `CREATE TABLE codes AS SELECT hex(code_hash) AS hash,'0x'||lower(hex(code)) AS bytecode FROM raw_code WHERE code_hash IN (SELECT from_hex(hash) FROM code_hashes WHERE hash IS NOT NULL)`);
    for (const name of ['ethereum_deployments', 'ethereum_verifications', 'signature_order', 'code_hashes']) await db.connection.run(`DROP TABLE ${name}`);
    for (const [table, key] of [['matches','id'],['verifications','id'],['deployments','id'],['compilations','id'],['contracts','id'],['metadata','compilation_id'],['source_refs','compilation_id'],['sources','hash'],['codes','hash'],['signature_refs','compilation_id'],['signatures','hash']]) {
      await run(`Index ${table}`, `CREATE INDEX ${table}_lookup ON ${table}(${key})`);
    }
    // The derived database is self-contained; it has no view of unrelated chains or the source database.
    for (const name of tableNames) await db.connection.run(`DROP VIEW raw_${name}`);
    await db.connection.run('DETACH pg');
    manifest.counts = {};
    for (const name of ['matches', 'verifications', 'deployments', 'compilations', 'contracts', 'metadata', 'source_refs', 'sources', 'codes', 'signature_refs', 'signatures']) {
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

const usage = `bun source/prepare.ts --source-dir DIR (--all | --limit N) [--pg-schema public] [--threads 4] [--memory-limit 4GB]
Materializes all (or the first N, 1..100000, by match ID) Ethereum mainnet matches of a Sourcify PostgreSQL database
into SOURCE_DIR/subset.duckdb. The connection comes from PGHOST, PGPORT, PGDATABASE, PGUSER and PGPASSWORD; a role with
SELECT on these tables suffices: ${tableNames.join(', ')}. Matches with IDs from ${localMatchIdStart} on (created by
the node itself) are refused. Reuses a completed identical preparation and resumes an interrupted one; a different
selection is refused.`;

async function main() {
  const { values } = parseArgs({ strict: true, options: {
    'source-dir': { type: 'string' }, 'pg-schema': { type: 'string', default: 'public' }, limit: { type: 'string' }, all: { type: 'boolean', default: false },
    threads: { type: 'string', default: '4' }, 'memory-limit': { type: 'string', default: '4GB' }, help: { type: 'boolean', default: false },
  } });
  if (values.help) { console.error(usage); return; }
  if (!values['source-dir']) throw new Error('--source-dir is required (see --help)');
  if (values.all === (values.limit !== undefined)) throw new Error('Choose exactly one of --limit N or --all');
  const sourceDir = resolve(values['source-dir']);
  let reused = false;
  try { reused = JSON.parse(await readFile(join(sourceDir, 'source-manifest.json'), 'utf8')).status === 'completed'; } catch (error: any) { if (error.code !== 'ENOENT') throw error; }
  const started = performance.now();
  const elapsedSeconds = () => +((performance.now() - started) / 1000).toFixed(1);
  const manifest = await prepareSource({
    ...(values.all ? { all: true } : { limit: Number(values.limit) }), sourceDir, database: process.env.PGHOST ? sourceDatabase(process.env, values['pg-schema']) : undefined,
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
