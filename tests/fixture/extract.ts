/**
 * Regenerates tests/fixture/parquet from a full Sourcify Parquet export: every revision of the listed matches and
 * every row they reference, with the export's own schema.
 *
 *   bun tests/fixture/extract.ts --parquet-dir DATA/parquet [--out tests/fixture/parquet]
 *
 * After regenerating, refresh tests/fixture/expected.json (see tests/pipeline.test.ts).
 */
import { DuckDBInstance } from '@duckdb/node-api';
import { mkdir, rm } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';

/** Chosen to cover each code path; the comments describe the export of 2026-10. */
const fixtureMatches = {
  solidity: [1, 32, 235, 452, 607, 673, 877, 881, 911, 990],
  largeSource: [21561], // 848 KB source file: blob entities
  twoRevisions: [468227], // two revisions with different verified contracts: latest selection
  vyper: [5888028],
  fe: [28469961],
  nonEthereum: [100], // chain 35443: never selected
  // Skipped with --skip-missing-references:
  yulWithoutAbi: [28759260],
  solidityWithoutMetadata: [23813307],
  missingBytecode: [15040941],
  missingDeployedContract: [54163146],
};

const { values } = parseArgs({ strict: true, options: { 'parquet-dir': { type: 'string' }, out: { type: 'string', default: join(import.meta.dir, 'parquet') } } });
if (!values['parquet-dir']) throw new Error('--parquet-dir is required');
const parquet = resolve(values['parquet-dir']), out = resolve(values.out);
const instance = await DuckDBInstance.create(':memory:', { threads: '8', memory_limit: '8GB' });
const db = await instance.connect();
const raw = (table: string) => `read_parquet('${join(parquet, table, '*.parquet')}',union_by_name=true)`;
const ids = Object.values(fixtureMatches).flat().join(',');
const tables: [string, string][] = [
  ['sourcify_matches', `SELECT * FROM ${raw('sourcify_matches')} WHERE id IN (${ids})`],
  ['verified_contracts', `SELECT * FROM ${raw('verified_contracts')} WHERE id IN (SELECT verified_contract_id FROM sourcify_matches)`],
  ['contract_deployments', `SELECT * FROM ${raw('contract_deployments')} WHERE id IN (SELECT deployment_id FROM verified_contracts)`],
  ['compiled_contracts', `SELECT * FROM ${raw('compiled_contracts')} WHERE id IN (SELECT compilation_id FROM verified_contracts)`],
  ['compiled_contracts_metadata', `SELECT * FROM ${raw('compiled_contracts_metadata')} WHERE compilation_id IN (SELECT id FROM compiled_contracts)`],
  ['contracts', `SELECT * FROM ${raw('contracts')} WHERE id IN (SELECT contract_id FROM contract_deployments)`],
  ['compiled_contracts_sources', `SELECT * FROM ${raw('compiled_contracts_sources')} WHERE compilation_id IN (SELECT id FROM compiled_contracts)`],
  ['sources', `SELECT * FROM ${raw('sources')} WHERE source_hash IN (SELECT source_hash FROM compiled_contracts_sources)`],
  ['code', `SELECT * FROM ${raw('code')} WHERE code_hash IN (SELECT creation_code_hash FROM compiled_contracts UNION SELECT runtime_code_hash FROM compiled_contracts UNION SELECT creation_code_hash FROM contracts UNION SELECT runtime_code_hash FROM contracts)`],
];
await rm(out, { recursive: true, force: true });
for (const [table, sql] of tables) {
  const started = performance.now();
  await db.run(`CREATE TABLE ${table} AS ${sql}`);
  await mkdir(join(out, table), { recursive: true });
  // Sorted rows and a single row group keep the files reproducible.
  await db.run(`COPY (SELECT * FROM ${table} ORDER BY ALL) TO '${join(out, table, 'fixture.parquet')}' (FORMAT parquet, COMPRESSION zstd)`);
  const count = (await db.runAndReadAll(`SELECT count(*)::INTEGER AS n FROM ${table}`)).getRowObjectsJson()[0].n;
  console.error(JSON.stringify({ table, rows: count, seconds: +((performance.now() - started) / 1000).toFixed(1) }));
}
