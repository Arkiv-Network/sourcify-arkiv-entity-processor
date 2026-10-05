/** Thin DuckDB wrapper shared by prepare.ts and records.ts. */
import { DuckDBInstance, type DuckDBConnection } from '@duckdb/node-api';
import { mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';

export type Row = Record<string, any>;
export const literal = (value: string) => `'${value.replaceAll("'", "''")}'`;

export class Database {
  private constructor(public instance: DuckDBInstance, public connection: DuckDBConnection, private temporary: string) {}
  /** Opens `path` with its own spill directory below `spillParent`, removed again on close. */
  static async open(path: string, options: Record<string, string>, spillParent: string, spillPrefix: string) {
    const instance = await DuckDBInstance.create(path, options);
    try {
      const connection = await instance.connect();
      try {
        const temporary = await mkdtemp(join(spillParent, spillPrefix));
        const db = new Database(instance, connection, temporary);
        try { await connection.run(`SET temp_directory=${literal(temporary)}`); } catch (error) { await db.close(); throw error; }
        return db;
      } catch (error) { connection.closeSync(); throw error; }
    } catch (error) { instance.closeSync(); throw error; }
  }
  async rows(sql: string): Promise<Row[]> {
    return (await this.connection.runAndReadAll(sql)).getRowObjectsJson() as Row[];
  }
  async one(sql: string): Promise<Row> {
    const rows = await this.rows(sql);
    if (rows.length !== 1) throw new Error(`Expected one row, found ${rows.length}`);
    return rows[0];
  }
  async close() {
    this.connection.closeSync(); this.instance.closeSync();
    await rm(this.temporary, { recursive: true, force: true });
  }
}
