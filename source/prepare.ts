/**
 * Sourcify PostgreSQL database -> prepared DuckDB source: materializes the Ethereum mainnet (chain 1) Sourcify matches
 * and every row they reference into SOURCE_DIR/subset.duckdb, indexed for batched lookups by stream.ts.
 *
 *   PGHOST=... PGPORT=... PGDATABASE=... PGUSER=... PGPASSWORD=... \
 *     bun source/prepare.ts --source-dir DIR (--all | --limit N [--after-match-id ID]) [--pg-schema public] [--threads 8] [--memory-limit 24GB]
 *
 * The database is a Sourcify mirror (e.g. the sourcify-mirror-node), read through DuckDB's postgres extension in a
 * read-only attachment; the connection comes from the libpq PG* environment variables (the password is never written).
 * Its internal row IDs only join the tables here: records (records.ts) carry API fields only.
 * Reuses a completed identical preparation and resumes an interrupted one; a different selection in SOURCE_DIR is
 * refused. Progress is JSON on stderr.
 *
 * With --after-match-id (incremental sync), at most N matches after that ID are selected, and only their rows are
 * read: each table is loaded with an explicit ID list instead of scanned. The derived tables are the same.
 */
import { mkdir, readFile, rename, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { Database, literal } from './database';

export interface SourceManifest {
  schema: 'sourcify-arkiv.import-source.v3'; status: 'preparing' | 'completed' | 'failed';
  /** Where the rows came from; the password is not recorded. */
  database: SourceDatabase; limit: number; all?: boolean; afterMatchId?: string; selectedCount?: number; firstMatchId?: string; lastMatchId?: string;
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

/** A few addresses have two matches (e.g. two deployment rows of one contract); the Sourcify API then returns either,
 * in its database's row order. A seed and the sync keep one per address, so the Arkiv server finds exactly one: the
 * better match (perfect 2, partial 1, none 0, creation + runtime), then the newer (higher) ID. Of the 90 such mainnet
 * addresses in October 2026, sourcify.dev returned this one for 83. `m` is a sourcify_matches row (SQL in both DBs). */
export const matchRank = (m: string) =>
  `(CASE ${m}.creation_match WHEN 'perfect' THEN 2 WHEN 'partial' THEN 1 ELSE 0 END)+(CASE ${m}.runtime_match WHEN 'perfect' THEN 2 WHEN 'partial' THEN 1 ELSE 0 END)`;

/** The libpq connection the postgres extension uses: the PG* environment variables (PGPASSWORD is not returned). */
export function sourceDatabase(env: Record<string, string | undefined>, schema = 'public'): SourceDatabase {
  for (const name of ['PGHOST', 'PGDATABASE', 'PGUSER']) if (!env[name]) throw new Error(`${name} is required (libpq environment variables PGHOST, PGPORT, PGDATABASE, PGUSER, PGPASSWORD)`);
  if (!/^[a-z_][a-z0-9_]*$/.test(schema)) throw new Error('--pg-schema must be a lowercase SQL identifier');
  return { host: env.PGHOST!, port: env.PGPORT || '5432', database: env.PGDATABASE!, user: env.PGUSER!, schema };
}

/** The newest Ethereum match ID at or below `atOrBelow` (default: the newest mirrored one), or the one `offset`
 * Ethereum matches before it; undefined when there is none. One query (an index scan backwards). It counts every
 * Ethereum match, also one a preparation leaves out (another match of the same address, see matchRank). */
export async function newestEthereumMatchId(database: SourceDatabase, options: { atOrBelow?: string; offset?: number } = {}): Promise<string | undefined> {
  const atOrBelow = options.atOrBelow ?? String(localMatchIdStart - 1n), offset = options.offset ?? 0;
  if (!/^(0|[1-9][0-9]{0,18})$/.test(atOrBelow) || BigInt(atOrBelow) >= localMatchIdStart) throw new Error(`Match ID must be a whole number below ${localMatchIdStart}`);
  if (!Number.isSafeInteger(offset) || offset < 0) throw new Error('Offset must be a whole number');
  const db = await Database.open(':memory:', { threads: '1', memory_limit: '256MB' }, tmpdir(), 'sourcify-newest-');
  try {
    await db.connection.run('INSTALL postgres');
    await db.connection.run('LOAD postgres');
    await db.connection.run("ATTACH '' AS pg (TYPE postgres, READ_ONLY)");
    const s = database.schema;
    const sql = `SELECT m.id FROM ${s}.sourcify_matches m JOIN ${s}.verified_contracts v ON v.id=m.verified_contract_id JOIN ${s}.contract_deployments d ON d.id=v.deployment_id ` +
      `WHERE d.chain_id=1 AND m.id<=${atOrBelow} ORDER BY m.id DESC LIMIT 1 OFFSET ${offset}`;
    const rows = await db.rows(`SELECT id::VARCHAR AS id FROM postgres_query('pg', ${literal(sql)})`);
    return rows[0]?.id;
  } finally { await db.close(); }
}

/** Materialize a bounded sample or all current Ethereum matches; reconstruction stays batched (records.ts).
 * Without `database`, only a completed preparation of the same selection is accepted (reused offline). */
export async function prepareSource(options: {
  limit?: number; all?: boolean; afterMatchId?: string; sourceDir: string; database?: SourceDatabase; memoryLimit?: string; threads?: number; onProgress?: (step: string) => void;
}): Promise<SourceManifest> {
  const { limit, afterMatchId } = options;
  if (options.all && limit !== undefined) throw new Error('Choose all or limit, not both');
  if (afterMatchId !== undefined && (options.all || !/^(0|[1-9][0-9]{0,18})$/.test(afterMatchId) || BigInt(afterMatchId) >= localMatchIdStart)) throw new Error(`After match ID must be a whole number below ${localMatchIdStart}, with a limit`);
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
    if (Boolean(existing.all) !== Boolean(options.all) || (!options.all && existing.limit !== limit) || existing.afterMatchId !== afterMatchId || (database && JSON.stringify(existing.database) !== JSON.stringify(database))) throw new Error('Source directory holds a different selection; use a fresh directory');
    if (existing.status === 'completed') return existing;
    // Its row lists are loaded once, not resumed: a later attempt could select newer matches than the tables it reuses.
    if (afterMatchId !== undefined) throw new Error('An incremental preparation is not resumed; use a fresh directory');
  }
  if (!database) throw new Error('No completed prepared source: preparing one reads the Sourcify database (set PGHOST, PGPORT, PGDATABASE, PGUSER, PGPASSWORD)');
  const manifest: SourceManifest = existing ?? { schema: sourceSchema, status: 'preparing', database, limit: limit ?? 0, ...(options.all ? {all: true} : {}), ...(afterMatchId !== undefined ? { afterMatchId } : {}), startedAt: new Date().toISOString(), steps: [] };
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
    if (afterMatchId === undefined) {
      for (const name of tableNames) await db.connection.run(`CREATE OR REPLACE TEMP VIEW raw_${name} AS SELECT * FROM pg.${database.schema}.${name}`);
    } else await loadIncrementalRows(db, database.schema, afterMatchId, limit!, options.onProgress);
    // Each step reads its own database snapshot. A writer that commits a match with all its rows in one transaction
    // (Sourcify, the mirror's sync) cannot make the selection inconsistent: the match IDs are fixed after the
    // deployments and verifications they are filtered by, and every later step only reads rows those reference.
    await run('Ethereum deployment candidates', 'CREATE TEMP TABLE ethereum_deployments AS SELECT id,address FROM raw_contract_deployments WHERE chain_id=1');
    await run('Ethereum verification candidates', 'CREATE TEMP TABLE ethereum_verifications AS SELECT v.id,d.address FROM raw_verified_contracts v JOIN ethereum_deployments d ON d.id=v.deployment_id');
    // Timestamps as UTC without a zone: DuckDB's Node client renders TIMESTAMPTZ in the process's local time zone.
    // One match per address (matchRank). Incrementally the rows are already chosen against the whole database.
    await run('Current Ethereum selection', `CREATE TABLE matches AS SELECT * REPLACE (timezone('UTC', created_at) AS created_at, timezone('UTC', updated_at) AS updated_at) FROM raw_sourcify_matches WHERE id IN ` +
      `(SELECT id FROM (SELECT m.id,row_number() OVER (PARTITION BY v.address ORDER BY ${matchRank('m')} DESC,m.id DESC) AS rank FROM raw_sourcify_matches m JOIN ethereum_verifications v ON v.id=m.verified_contract_id) WHERE rank=1) ` +
      `ORDER BY id${options.all ? '' : ` LIMIT ${limit}`}`);
    const local = await db.one(`SELECT count(*) AS count FROM matches WHERE id>=${localMatchIdStart}`);
    if (Number(local.count) > 0) throw new Error(`${local.count} selected matches have IDs from ${localMatchIdStart} on: they were created by the node itself, not mirrored from Sourcify, and have no Sourcify matchId`);
    const selected = await db.one('SELECT count(*) AS count,min(id)::VARCHAR AS first_id,max(id)::VARCHAR AS last_id FROM matches');
    manifest.selectedCount = Number(selected.count); manifest.firstMatchId = selected.first_id; manifest.lastMatchId = selected.last_id;
    // An incremental selection may be short or empty (the sync is caught up); a sample must be complete.
    if (!Number.isSafeInteger(manifest.selectedCount) || (manifest.selectedCount < 1 && afterMatchId === undefined)) throw new Error('No current Ethereum matches available');
    if (options.all) manifest.limit = manifest.selectedCount;
    await save();
    if (!options.all && afterMatchId === undefined && manifest.selectedCount !== limit) throw new Error(`Only ${manifest.selectedCount} current Ethereum matches available, requested ${limit}`);
    await run('Selected verification rows', 'CREATE TABLE verifications AS SELECT * FROM raw_verified_contracts WHERE id IN (SELECT verified_contract_id FROM matches)');
    await run('Selected deployments', `CREATE TABLE deployments AS SELECT * EXCLUDE(address,transaction_hash,deployer), '0x'||lower(hex(address)) AS address, '0x'||lower(hex(transaction_hash)) AS transaction_hex, '0x'||lower(hex(deployer)) AS deployer_hex FROM raw_contract_deployments WHERE id IN (SELECT deployment_id FROM verifications)`);
    await run('Selected compilations', 'CREATE TABLE compilations AS SELECT * EXCLUDE(creation_code_hash,runtime_code_hash),hex(creation_code_hash) AS creation_hash,hex(runtime_code_hash) AS runtime_hash FROM raw_compiled_contracts WHERE id IN (SELECT compilation_id FROM verifications)');
    await run('Selected signature references', 'CREATE TABLE signature_refs AS SELECT DISTINCT compilation_id,hex(signature_hash_32) AS hash,signature_type FROM raw_compiled_contracts_signatures WHERE compilation_id IN (SELECT id FROM compilations)');
    // The Sourcify API orders each signature group by signature text in the database's collation (ORDER BY
    // signatures.signature), which no byte or locale comparison here reproduces exactly: PostgreSQL ranks them.
    // Incrementally only the selected signatures are ranked: their relative order, all records use, is the same.
    const signatureOrder = `SELECT signature_hash_32,row_number() OVER (ORDER BY signature,signature_hash_32) AS signature_order FROM ${database.schema}.signatures` +
      (afterMatchId === undefined ? '' : ` WHERE signature_hash_32 IN ${await valueList(db, 'SELECT DISTINCT lower(hex(signature_hash_32)) AS v FROM raw_signatures', 'bytea')}`);
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
    for (const name of tableNames) await db.connection.run(`DROP ${afterMatchId === undefined ? 'VIEW' : 'TABLE'} raw_${name}`);
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

/** `(v1,v2,...)` for a PostgreSQL IN clause from the single column `v` of a DuckDB query, or `(NULL)` (matches
 * nothing) when it is empty. Values are digits (bigint), UUIDs or lowercase hex (bytea); anything else is refused. */
async function valueList(db: Database, sql: string, type: 'bigint' | 'uuid' | 'bytea') {
  const pattern = { bigint: /^[0-9]+$/, uuid: /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/, bytea: /^(?:[0-9a-f]{2})*$/ }[type];
  const values = (await db.rows(sql)).map(row => String(row.v));
  for (const value of values) if (!pattern.test(value)) throw new Error(`Unexpected ${type} value in an ID list`);
  if (!values.length) return '(NULL)';
  return `(${values.map(v => type === 'bigint' ? v : type === 'uuid' ? `'${v}'::uuid` : `'\\x${v}'::bytea`).join(',')})`;
}

/** Incremental selection: raw_<table> temp tables holding only the rows the selected matches reference, each read
 * with an explicit ID list (PostgreSQL uses its indexes), in reference order. */
async function loadIncrementalRows(db: Database, schema: string, afterMatchId: string, limit: number, onProgress?: (step: string) => void) {
  const load = async (table: string, where: string) => {
    onProgress?.(`Load ${table}`);
    await db.connection.run(`CREATE OR REPLACE TEMP TABLE raw_${table} AS SELECT * FROM postgres_query('pg', ${literal(`SELECT * FROM ${schema}.${table} WHERE ${where}`)})`);
  };
  const ids = (sql: string, type: 'bigint' | 'uuid' | 'bytea') => valueList(db, sql, type);
  // The next matches by ID whose deployment is on Ethereum mainnet, below the node's own (local) IDs, leaving out a
  // match when the database holds a better-ranked one for the same address (matchRank), wherever its ID is.
  const better = `SELECT 1 FROM ${schema}.contract_deployments d2 JOIN ${schema}.verified_contracts v2 ON v2.deployment_id=d2.id JOIN ${schema}.sourcify_matches m2 ON m2.verified_contract_id=v2.id ` +
    `WHERE d2.chain_id=1 AND d2.address=d.address AND m2.id<>m.id AND m2.id<${localMatchIdStart} AND (${matchRank('m2')}>${matchRank('m')} OR (${matchRank('m2')}=${matchRank('m')} AND m2.id>m.id))`;
  await load('sourcify_matches', `id IN (SELECT m.id FROM ${schema}.sourcify_matches m JOIN ${schema}.verified_contracts v ON v.id=m.verified_contract_id ` +
    `JOIN ${schema}.contract_deployments d ON d.id=v.deployment_id WHERE d.chain_id=1 AND m.id>${afterMatchId} AND m.id<${localMatchIdStart} AND NOT EXISTS (${better}) ORDER BY m.id LIMIT ${limit})`);
  await load('verified_contracts', `id IN ${await ids('SELECT verified_contract_id::VARCHAR AS v FROM raw_sourcify_matches', 'bigint')}`);
  await load('contract_deployments', `id IN ${await ids('SELECT DISTINCT deployment_id::VARCHAR AS v FROM raw_verified_contracts', 'uuid')}`);
  await load('compiled_contracts', `id IN ${await ids('SELECT DISTINCT compilation_id::VARCHAR AS v FROM raw_verified_contracts', 'uuid')}`);
  const compilations = await ids('SELECT id::VARCHAR AS v FROM raw_compiled_contracts', 'uuid');
  for (const table of ['compiled_contracts_metadata', 'compiled_contracts_sources', 'compiled_contracts_signatures']) await load(table, `compilation_id IN ${compilations}`);
  await load('contracts', `id IN ${await ids('SELECT DISTINCT contract_id::VARCHAR AS v FROM raw_contract_deployments', 'uuid')}`);
  await load('sources', `source_hash IN ${await ids('SELECT DISTINCT lower(hex(source_hash)) AS v FROM raw_compiled_contracts_sources', 'bytea')}`);
  await load('signatures', `signature_hash_32 IN ${await ids('SELECT DISTINCT lower(hex(signature_hash_32)) AS v FROM raw_compiled_contracts_signatures', 'bytea')}`);
  await load('code', `code_hash IN ${await ids(`SELECT DISTINCT lower(hex(h)) AS v FROM (SELECT creation_code_hash AS h FROM raw_compiled_contracts UNION ALL SELECT runtime_code_hash FROM raw_compiled_contracts
    UNION ALL SELECT creation_code_hash FROM raw_contracts UNION ALL SELECT runtime_code_hash FROM raw_contracts) WHERE h IS NOT NULL`, 'bytea')}`);
}

const usage = `bun source/prepare.ts --source-dir DIR (--all | --limit N [--after-match-id ID]) [--pg-schema public] [--threads 4] [--memory-limit 4GB]
Materializes all (or the first N, 1..100000, by match ID) Ethereum mainnet matches of a Sourcify PostgreSQL database
into SOURCE_DIR/subset.duckdb. The connection comes from PGHOST, PGPORT, PGDATABASE, PGUSER and PGPASSWORD; a role with
SELECT on these tables suffices: ${tableNames.join(', ')}. Matches with IDs from ${localMatchIdStart} on (created by
the node itself) are refused. Reuses a completed identical preparation and resumes an interrupted one; a different
selection is refused. --after-match-id ID selects at most N matches after ID instead (possibly none) and reads only
their rows; it is never resumed.`;

async function main() {
  const { values } = parseArgs({ strict: true, options: {
    'source-dir': { type: 'string' }, 'pg-schema': { type: 'string', default: 'public' }, limit: { type: 'string' }, all: { type: 'boolean', default: false }, 'after-match-id': { type: 'string' },
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
    ...(values.all ? { all: true } : { limit: Number(values.limit) }), afterMatchId: values['after-match-id'], sourceDir, database: process.env.PGHOST ? sourceDatabase(process.env, values['pg-schema']) : undefined,
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
