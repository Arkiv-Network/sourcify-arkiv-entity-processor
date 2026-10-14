/**
 * Test PostgreSQL access: loads tests/fixture/schema.sql and fixture.sql into a fresh schema of the database named by
 * the libpq environment variables (PGHOST, PGPORT, PGDATABASE, PGUSER, PGPASSWORD), e.g. a disposable
 *
 *   docker run -d --rm -p 127.0.0.1:5432:5432 -e POSTGRES_PASSWORD=test postgres:15
 *   PGHOST=127.0.0.1 PGUSER=postgres PGPASSWORD=test PGDATABASE=postgres bun test tests
 *
 * The schema is dropped again by drop(); nothing else in that database is touched.
 */
import { DuckDBInstance } from '@duckdb/node-api';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { literal } from '../../source/database';
import { sourceDatabase, type SourceDatabase } from '../../source/prepare';

const schemaSql = readFileSync(join(import.meta.dir, 'schema.sql'), 'utf8');
export const fixtureSql = () => readFileSync(join(import.meta.dir, 'fixture.sql'), 'utf8');

export interface FixtureDatabase { database: SourceDatabase; execute(sql: string): Promise<void>; drop(): Promise<void> }

/** Creates schema `name` (default: a random test_* name) with the Sourcify tables, then runs `data` (SQL) in it. */
export async function createFixtureDatabase(data = fixtureSql(), name = `test_${process.pid}_${Math.random().toString(36).slice(2, 10)}`): Promise<FixtureDatabase> {
  if (!process.env.PGHOST) throw new Error('These tests need a PostgreSQL server: set PGHOST, PGPORT, PGDATABASE, PGUSER and PGPASSWORD (see tests/fixture/postgres.ts)');
  const database = sourceDatabase(process.env, name);
  const instance = await DuckDBInstance.create(':memory:');
  const connection = await instance.connect();
  const execute = async (sql: string) => { await connection.run(`CALL postgres_execute('pg', ${literal(`SET search_path TO ${name};\n${sql}`)})`); };
  try {
    await connection.run('INSTALL postgres');
    await connection.run('LOAD postgres');
    await connection.run("ATTACH '' AS pg (TYPE postgres)");
    await connection.run(`CALL postgres_execute('pg', ${literal(`CREATE SCHEMA ${name}`)})`);
    await execute(schemaSql);
    await execute(data);
  } catch (error) { connection.closeSync(); instance.closeSync(); throw error; }
  let dropped = false;
  return {
    database, execute,
    drop: async () => {
      if (dropped) return;
      dropped = true;
      try { await connection.run(`CALL postgres_execute('pg', ${literal(`DROP SCHEMA ${name} CASCADE`)})`); } finally { connection.closeSync(); instance.closeSync(); }
    },
  };
}
