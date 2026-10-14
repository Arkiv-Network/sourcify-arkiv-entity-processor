/**
 * Prepared source -> NDJSON contract records on stdout, for `arkiv-entity-processor process --require-source-complete`.
 * Ends with the completion marker {"type":"source-complete","records":N,"lastMatchId":...[,"skipped":[...]]}.
 * Diagnostics and progress are JSON on stderr. Writes respect pipe backpressure, so memory stays bounded.
 */
import { once } from 'node:events';
import { resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { iterateRecords, openSource } from './records';

export interface Options {
  sourceDir: string; limit?: number; afterMatchId?: string; batchSize: number; memoryLimit: string;
  /** Skip (and report) records whose referenced rows are absent from the source, or that the processor cannot take. */
  skipMissingReferences: boolean;
  /** Abort once more than this many records were skipped; skipped records are kept for the marker. */
  maxSkipped: number;
}
export interface Skipped { matchId: string; reason: string }

/** Only reconstruction failures caused by referenced data absent from the source (see `required()` in records.ts). */
export const isMissingReference = (error: string) => error.startsWith('Missing ');
/** Reject malformed ABI values while preserving valid null ABIs (e.g. Yul). */
export const unsupportedRecord = (record: Record<string, any>) =>
  record.abi === null || Array.isArray(record.abi) ? undefined : `ABI is neither an array nor null (${record.compilation?.language ?? 'unknown'} compilation)`;

const usage = `bun source/stream.ts --source-dir DIR [--limit N] [--after-match-id ID] [--batch-size 250] [--memory-limit 6GB]
                     [--skip-missing-references [--max-skipped 10000]]
Writes the prepared Ethereum mainnet records of DIR (see prepare.ts) as NDJSON to stdout in match ID order, then a
source-complete marker. Any reconstruction error aborts, unless --skip-missing-references: records whose referenced
rows are absent from the source (or whose ABI is neither an array nor null) are then skipped, logged to stderr and listed in the
marker, at most --max-skipped of them.`;

export function parseOptions(args: string[]): Options | undefined {
  const { values } = parseArgs({ args, strict: true, options: {
    'source-dir': { type: 'string' }, limit: { type: 'string' }, 'after-match-id': { type: 'string' }, 'batch-size': { type: 'string', default: '250' },
    'memory-limit': { type: 'string', default: '6GB' }, 'skip-missing-references': { type: 'boolean', default: false },
    'max-skipped': { type: 'string', default: '10000' }, help: { type: 'boolean', default: false },
  } });
  if (values.help) return undefined;
  if (!values['source-dir']) throw new Error('--source-dir is required (see --help)');
  const options: Options = { sourceDir: resolve(values['source-dir']), batchSize: Number(values['batch-size']), memoryLimit: values['memory-limit'],
    skipMissingReferences: values['skip-missing-references'], maxSkipped: Number(values['max-skipped']),
    ...(values.limit === undefined ? {} : { limit: Number(values.limit) }), ...(values['after-match-id'] === undefined ? {} : { afterMatchId: values['after-match-id'] }) };
  if (options.limit !== undefined && (!Number.isSafeInteger(options.limit) || options.limit < 1)) throw new Error('--limit must be a positive integer');
  if (!Number.isSafeInteger(options.maxSkipped) || options.maxSkipped < 0 || options.maxSkipped > 1_000_000) throw new Error('--max-skipped must be 0..1000000');
  if (!Number.isSafeInteger(options.batchSize) || options.batchSize < 1 || options.batchSize > 1000) throw new Error('--batch-size must be 1..1000');
  if (options.afterMatchId !== undefined && !/^\d+$/.test(options.afterMatchId)) throw new Error('--after-match-id must be a nonnegative integer');
  return options;
}

export async function streamRecords(options: Options, write: (line: string) => Promise<void>): Promise<{ records: number; lastMatchId?: string; skipped?: Skipped[] }> {
  const source = await openSource(options.sourceDir, options.memoryLimit);
  const started = performance.now();
  let records = 0, lastMatchId: string | undefined, closed = false;
  const skipped: Skipped[] | undefined = options.skipMissingReferences ? [] : undefined;
  try {
    for await (const item of iterateRecords(source, { batchSize: options.batchSize, afterMatchId: options.afterMatchId })) {
      const unsupported = skipped && item.record ? unsupportedRecord(item.record) : undefined;
      if (item.error || !item.record || unsupported) {
        const reason = unsupported ?? item.error;
        if (!skipped || !reason || !(unsupported || isMissingReference(reason))) throw new Error(`Source match ${item.matchId}: ${item.error ?? 'missing record'}`);
        if (skipped.length >= options.maxSkipped) throw new Error(`More than ${options.maxSkipped} records skipped; last: source match ${item.matchId}: ${reason}`);
        skipped.push({ matchId: item.matchId, reason });
        console.error(JSON.stringify({ skippedMatchId: item.matchId, reason }));
        continue;
      }
      await write(JSON.stringify({ record: item.record }) + '\n');
      records++; lastMatchId = item.matchId;
      if (records % 1000 === 0) console.error(JSON.stringify({ sourceRecords: records, lastMatchId, ...(skipped ? { skipped: skipped.length } : {}), elapsedSeconds: (performance.now() - started) / 1000 }));
      if (options.limit !== undefined && records >= options.limit) break;
    }
    if (options.limit !== undefined && records !== options.limit) throw new Error(`Source exhausted after ${records} records; requested ${options.limit}`);
    await source.close(); closed = true;
    // `skipped` is only present with --skip-missing-references.
    await write(JSON.stringify({ type: 'source-complete', records, lastMatchId, ...(skipped ? { skipped } : {}) }) + '\n');
    console.error(JSON.stringify({ sourceComplete: true, records, lastMatchId, ...(skipped ? { skipped: skipped.length } : {}), elapsedSeconds: (performance.now() - started) / 1000 }));
    return { records, lastMatchId, ...(skipped ? { skipped } : {}) };
  } finally { if (!closed) await source.close(); }
}

if (import.meta.main) {
  try {
    const options = parseOptions(process.argv.slice(2));
    if (!options) console.error(usage);
    else {
      process.stdout.on('error', error => { console.error(String(error)); process.exitCode = 1; });
      await streamRecords(options, async line => { if (!process.stdout.write(line)) await once(process.stdout, 'drain'); });
    }
  } catch (error) { console.error(error instanceof Error ? error.message : String(error)); process.exitCode = 1; }
}
