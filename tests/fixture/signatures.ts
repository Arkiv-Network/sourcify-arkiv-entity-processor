/** Regenerate synthetic signature coverage added to the real contract fixture.
 * These rows exercise exported table handling; they are not a live API parity oracle.
 * Full fixture extraction instead copies actual export signatures (extract.ts).
 */
import { DuckDBInstance } from '@duckdb/node-api';
import { keccak_256 } from '@noble/hashes/sha3';
import { mkdir } from 'node:fs/promises';
import { join } from 'node:path';
const root = join(import.meta.dir, 'parquet');
const instance = await DuckDBInstance.create(':memory:');
const db = await instance.connect();
const rows = (await db.runAndReadAll(`SELECT v.compilation_id FROM read_parquet('${root}/verified_contracts/*.parquet') v JOIN read_parquet('${root}/sourcify_matches/*.parquet') m ON m.verified_contract_id=v.id WHERE m.id=1`)).getRowObjectsJson();
const id = String(rows[0].compilation_id);
await db.run('CREATE TABLE signatures(signature_hash_32 BLOB,signature_hash_4 BLOB,signature VARCHAR)');
await db.run('CREATE TABLE compiled_contracts_signatures(compilation_id VARCHAR,signature_hash_32 BLOB,signature_type VARCHAR)');
for (const [type, signature] of [
  ['function', 'transfer(address,uint256)'],
  ['function', 'allowance(address,address)'], // Hash order differs from API signature-text order.
  ['event', 'Transfer(address,address,uint256)'],
  ['error', 'Error(string)'],
]) {
  const hash = Buffer.from(keccak_256(new TextEncoder().encode(signature))).toString('hex');
  await db.run(`INSERT INTO signatures VALUES(from_hex('${hash}'),from_hex('${hash.slice(0,8)}'),'${signature}')`);
  // Duplicate exported relationship rows must not duplicate API signature entries.
  await db.run(`INSERT INTO compiled_contracts_signatures VALUES('${id}',from_hex('${hash}'),'${type}'),('${id}',from_hex('${hash}'),'${type}')`);
}
for (const table of ['signatures','compiled_contracts_signatures']) {
  await mkdir(join(root,table), {recursive:true});
  await db.run(`COPY (SELECT * FROM ${table} ORDER BY ALL) TO '${root}/${table}/fixture.parquet' (FORMAT parquet, COMPRESSION zstd)`);
}
db.closeSync(); instance.closeSync();
