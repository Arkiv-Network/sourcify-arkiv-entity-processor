#!/usr/bin/env bun
/**
 * Sync dashboard: keeps DASHBOARD_OUT_DIR/status.json (schema 1, like dashboard/sample-status.json) for the static page
 * in this directory (index.html, app.js, style.css, copied next to it on start), which nginx serves read-only at /sync/.
 *
 *   bun dashboard/status.ts [--once]
 *
 * Every 15 s, from what pipeline/sync-targets.sh keeps: each target (targets/NAME.env, winning over .env) and its
 * data/sync/NAME/{status.json, state.json, sync.lock, sync.jsonl (read incrementally)}; the daily seed's runs
 * (data/runs/status.jsonl); each target's chain (read-only RPC calls: head, gas price, the writer's balance and nonces).
 * The mirror database (SELECT only, scans bounded by ID) at most every 60 s, attached only for its queries. A source that
 * fails (an RPC, the database, a malformed file) sets its own part's `error` and the rest is written as usual; a slow one
 * only delays its own part. status.json is replaced atomically. --once writes it once and exits.
 *
 * Nothing secret is published or logged: of an RPC URL only its hostname, and every error text goes through sanitize()
 * with every RPC URL and key seen (target files, the syncs' status.json rpcUrl): no URL paths or queries, filesystem
 * paths, credentials; at most 300 characters. The mirror's errors only as their kind. Never rpcUrl, keys, key files or
 * pgpass; values read from files only in their expected form.
 *
 * Settings (environment; defaults are the container's mounts, see dashboard.compose.yaml): DASHBOARD_TARGETS_DIR
 * (/targets), DASHBOARD_ENV_FILE (/config/.env), DASHBOARD_SYNC_DIR (/sync), DASHBOARD_RUNS_DIR (/runs),
 * DASHBOARD_OUT_DIR (/out), MIRROR_PUBLIC_URL (https://sourcify-mirror.arkiv-global.net), APP_REVISION; PGHOST, PGPORT,
 * PGDATABASE, PGUSER, PGPASSWORD or PGPASSFILE (the mirror).
 */
import { createPublicClient, http, type Hex, type PublicClient } from 'viem';
import { randomBytes } from 'node:crypto';
import { chmod, mkdir, open, readdir, readFile, rename, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { describeError } from '../arkiv/chain';
import { Database, literal } from '../source/database';
import { localMatchIdStart } from '../source/prepare';

const HERE = dirname(import.meta.path);
const env = process.env;
const paths = {
  targets: resolve(env.DASHBOARD_TARGETS_DIR || '/targets'), envFile: resolve(env.DASHBOARD_ENV_FILE || '/config/.env'),
  sync: resolve(env.DASHBOARD_SYNC_DIR || '/sync'), runs: resolve(env.DASHBOARD_RUNS_DIR || '/runs'), out: resolve(env.DASHBOARD_OUT_DIR || '/out'),
};
const PUBLIC_URL = (env.MIRROR_PUBLIC_URL || 'https://sourcify-mirror.arkiv-global.net').replace(/\/+$/, '');
const STATIC_FILES = ['index.html', 'app.js', 'style.css'];
const REFRESH_SECONDS = 15, DATABASE_SECONDS = 60, RPC_TIMEOUT_MS = 10_000;
const HOUR = 3_600_000, DAY = 24 * HOUR;
// arkiv/sync.ts refreshes its lock every 30 s; one not refreshed for 2 minutes is taken over (its sync is gone).
const LOCK_STALE_MS = 120_000;
const TARGET_FILE = /^[a-z0-9][a-z0-9-]{0,39}\.env$/;
// Match IDs of every chain: about 65,000 a day (October 2026), so the hourly scan reaches back 600,000 (over a week).
const DAY_ID_SPAN = 600_000n;
// A cursor further back than this many IDs is not counted (behind: null) rather than scanned.
const BEHIND_ID_SPAN = 1_000_000n;
// A running sync rewrites status.json after each batch and each poll: one silent this long is hung.
const HUNG_MS = 30 * 60_000;
// A gap between two batches longer than this (the sync's longest retry wait is 15 minutes) starts a new run of them.
const RUN_GAP_MS = 20 * 60_000;
// The Ethereum matches listed from each cursor on: a cursor that moves on before the next query still resolves.
const NEXT_MATCHES = 1000;
const BLOCK_SPAN = 20n;
const MATCHES = 'FROM public.sourcify_matches m JOIN public.verified_contracts v ON v.id=m.verified_contract_id JOIN public.contract_deployments d ON d.id=v.deployment_id';
// libpq settings beyond PG*: an unreachable or slow database fails the query set instead of stalling it.
const CONNINFO = "connect_timeout=10 application_name=arkiv-sync-dashboard options='-c statement_timeout=15000'";

const json = (value: unknown) => JSON.stringify(value, (_, v) => typeof v === 'bigint' ? v.toString() : v);
const log = (event: string, fields: Record<string, unknown> = {}) => console.log(json({ dashboard: event, at: new Date().toISOString(), ...fields }));
const isId = (value: unknown): value is string => typeof value === 'string' && /^(0|[1-9][0-9]{0,18})$/.test(value) && BigInt(value) < localMatchIdStart;
const isAddress = (value: unknown): value is string => typeof value === 'string' && /^0x[0-9a-fA-F]{40}$/.test(value);
const text = (value: unknown) => typeof value === 'string' ? value : null;
const matching = (pattern: RegExp, value: unknown) => typeof value === 'string' && pattern.test(value) ? value : null;
const ISO_TIME = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}:\d{2})$/;
const isoText = (value: unknown) => matching(ISO_TIME, value);
const count =(value: unknown) => typeof value === 'number' && Number.isFinite(value) ? value : 0;
const fileError = (label: string, error: any) => `${label}: ${error?.code ?? error?.message ?? error}`;
const iso = (ms: number) => new Date(ms).toISOString();

/** KEY=value lines of an env file, the last of a key winning, one pair of surrounding quotes removed (as env_value in
 * pipeline/sync-targets.sh). */
export function parseEnv(content: string): Record<string, string> {
  const values: Record<string, string> = {};
  for (const line of content.split(/\r?\n/)) {
    const match = /^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(line);
    if (match) values[match[1]!] = match[2]!.replace(/^"(.*)"$|^'(.*)'$/, '$1$2');
  }
  return values;
}

/** A target's setting: its own file's value, else .env's (as `setting` in pipeline/sync-targets.sh: empty is unset). */
export const settingOf = (own: Record<string, string>, shared: Record<string, string>) => (key: string) => own[key] || shared[key] || undefined;

/** A target's sync settings with arkiv/sync.ts's defaults (`get`: settingOf). */
export function targetSettings(get: (key: string) => string | undefined) {
  const number = (key: string, fallback: number) => { const value = get(key); return !value ? fallback : Number.isFinite(Number(value)) ? Number(value) : null; };
  return { batch: number('SYNC_BATCH', 200), inFlight: number('SYNC_IN_FLIGHT', 4), maxTxBytes: number('SYNC_MAX_TX_BYTES', 120_000),
    batchDelaySeconds: number('SYNC_BATCH_DELAY_SECONDS', 0), pollSeconds: number('SYNC_POLL_SECONDS', 60) };
}

const escape = (value: string) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const decoded = (value: string) => { try { return decodeURIComponent(value); } catch { return value; } };
// An absolute (or ~/, ./) path: after a start, a space, quote, bracket, = : , or ;.
const FILE_PATH = /(^|[\s'"`(\[{=:,;])(?:~|\.{1,2})?\/(?!\/)[^\s'"`()[\]{}<>,;]+/g;
// What the generic passes see of a text, once its secrets are masked: they stay linear and fast on any input.
const SANITIZE_INPUT_CHARS = 16 * 1024;

/** An error text as it may be published: each RPC URL in `secrets` (with or without its scheme) as its hostname, the
 * parts of those URLs that may be secret (path segments and query values of 6 characters or more, each of their
 * `_`/`-`-separated parts of 12 or more, the credentials) and the `values` masked wherever they appear, all before the
 * text is cut; any other URL as scheme://host; no filesystem paths, database user names or passwords, markup or line
 * breaks; at most `max` characters. */
export function sanitize(input: unknown, secrets: { urls?: Iterable<string | undefined>; values?: Iterable<string | undefined> } = {}, max = 300): string {
  let out = String(input ?? '');
  const hidden = new Set<string>();
  const hide = (value: string | undefined, min: number) => { if (value) for (const form of [value, decoded(value)]) if (form.length >= min) hidden.add(form); };
  for (const value of secrets.values ?? []) hide(value, 4);
  for (const url of secrets.urls ?? []) {
    if (!url) continue;
    let parsed: URL;
    try { parsed = new URL(url); } catch { hide(url, 4); continue; }
    for (const form of [url, url.replace(/^[a-z][a-z0-9+.-]{0,31}:\/\//i, '')]) out = out.replace(new RegExp(`${escape(form.replace(/\/+$/, ''))}/?`, 'gi'), parsed.hostname);
    for (const part of [...parsed.pathname.split('/'), ...parsed.searchParams.values(), parsed.hash.slice(1)]) {
      hide(part, 6);
      for (const piece of part.split(/[_-]/)) hide(piece, 12);
    }
    hide(parsed.username, 4); hide(parsed.password, 4);
  }
  for (const value of [...hidden].sort((a, b) => b.length - a.length)) out = out.replace(new RegExp(escape(value), 'gi'), '…');
  out = out.slice(0, SANITIZE_INPUT_CHARS).replace(/<[^<>]{0,300}>/g, ' ')
    .replace(/\b([a-z][a-z0-9+.-]{0,31}:\/\/)(?:[^\s/?#@'"`<>\\]*@)?([^\s/?#'"`<>\\]*)[^\s'"`<>\\]*/gi, '$1$2')
    .replace(FILE_PATH, '$1[path]')
    .replace(/\b(password|passwd|passfile|user)(\s*=\s*)('[^']{0,200}'|"[^"]{0,200}"|\S+)/gi, '$1$2…')
    .replace(/\b(user|role)\s+"[^"]{0,200}"/gi, '$1 "…"')
    .replace(/\\[nrt]/g, ' ').replace(/\s+/g, ' ').trim();
  return out.length > max ? `${out.slice(0, max - 1).trimEnd()}…` : out;
}

/** Every RPC URL and key seen since the start (target files, .env, the syncs' status.json rpcUrl), masked in every text
 * published or logged: also after a target file became unreadable, or while a sync still runs with a rotated token. */
const secrets = { urls: new Set<string>(), values: new Set<string>(env.PGPASSWORD ? [env.PGPASSWORD] : []) };
const clean = (input: unknown) => sanitize(input, secrets);

/** A mirror database failure as published: what kind, never libpq's text (addresses, ports, database and user names). */
export function databaseError(error: unknown) {
  const message = describeError(error);
  const kind = /connection refused/i.test(message) ? 'connection refused'
    : /timeout|timed out/i.test(message) ? 'timeout'
    : /authentication|password/i.test(message) ? 'authentication failed'
    : /translate host name|name or service not known|nodename nor servname|could not resolve/i.test(message) ? 'host not found'
    : /no route to host|unreachable/i.test(message) ? 'unreachable'
    : /permission denied/i.test(message) ? 'permission denied'
    : /unable to connect|could not connect|connection to server|server closed the connection|terminating connection|connection .*(lost|closed)/i.test(message) ? 'connection failed'
    : undefined;
  return kind ? `mirror database unavailable (${kind})` : 'mirror database query failed';
}

/** An RPC failure in a few words: viem's summary, the HTTP status, the innermost cause (still to be sanitized). */
const rpcError = (error: any) => [error?.shortMessage, error?.status && `HTTP ${error.status}`, describeError(error)]
  .filter(Boolean).map(part => String(part).replace(/\.$/, '')).filter((part, index, all) => all.indexOf(part) === index).join(': ');

export interface Batch {
  t: number; at: string; matchId: string | null; selected: number; contracts: number; alreadySynced: number; created: number; reused: number;
  transactions: number; gasUsed: bigint; calldataBytes: number; seconds: number; skipped: number;
}

/** One sync.jsonl line (see arkiv/sync.ts) as a batch; undefined for a malformed line and for a dry run's: marked
 * `dryRun`, or (lines from before the mark) planned transactions without sending any (gasUsed "0"). */
export function parseBatch(line: string): Batch | undefined {
  let raw: any;
  try { raw = JSON.parse(line); } catch { return undefined; }
  const t = Date.parse(raw?.at);
  if (!Number.isFinite(t) || !['string', 'number'].includes(typeof raw.gasUsed)) return undefined;
  let gasUsed: bigint;
  try { gasUsed = BigInt(raw.gasUsed); } catch { return undefined; }
  const transactions = count(raw.transactions);
  if (raw.dryRun === true || (gasUsed === 0n && transactions > 0)) return undefined;
  return { t, at: iso(t), matchId: isId(raw.lastMatchId) ? raw.lastMatchId : null, selected: count(raw.selected), contracts: count(raw.contracts),
    alreadySynced: count(raw.alreadySynced), created: count(raw.created), reused: count(raw.reused), transactions, gasUsed,
    calldataBytes: count(raw.calldataBytes), seconds: count(raw.seconds), skipped: count(raw.skippedContracts) };
}

/** The 24 hour starts (ms) up to the current hour's, oldest first. */
export const hourStarts = (now: number) => Array.from({ length: 24 }, (_, i) => (Math.floor(now / HOUR) - 23 + i) * HOUR);

/** sync.jsonl aggregated as it grows: totals of every batch, the last one, and the last 24 h (hourly, rates). */
export class SyncLog {
  totals = { since: null as string | null, batches: 0, matches: 0, contracts: 0, alreadySynced: 0, skipped: 0, entitiesCreated: 0, entitiesReused: 0, transactions: 0, gasUsed: 0n, calldataBytes: 0 };
  last?: Batch;
  ignored = 0;
  private recent: Batch[] = [];

  add(line: string, now = Date.now()) {
    if (!line.trim()) return;
    const batch = parseBatch(line);
    if (!batch) { this.ignored++; return; }
    const totals = this.totals;
    totals.since ??= batch.at; totals.batches++; totals.matches += batch.selected; totals.contracts += batch.contracts;
    totals.alreadySynced += batch.alreadySynced; totals.skipped += batch.skipped; totals.entitiesCreated += batch.created;
    totals.entitiesReused += batch.reused; totals.transactions += batch.transactions; totals.gasUsed += batch.gasUsed; totals.calldataBytes += batch.calldataBytes;
    this.last = batch;
    if (batch.t > now - DAY) this.recent.push(batch);
  }

  summary(now = Date.now()) {
    this.recent = this.recent.filter(batch => batch.t > now - DAY);
    const starts = hourStarts(now);
    const hourly = starts.map(start => ({ hour: iso(start), contracts: 0, matches: 0, transactions: 0, gasUsed: 0n, calldataBytes: 0 }));
    const hour = { contracts: 0, matches: 0, seconds: 0 };
    let contractsLast24h = 0, gasUsedLast24h = 0n;
    for (const batch of this.recent) {
      const bucket = hourly[Math.floor((batch.t - starts[0]!) / HOUR)];
      if (bucket) {
        bucket.contracts += batch.contracts; bucket.matches += batch.selected; bucket.transactions += batch.transactions;
        bucket.gasUsed += batch.gasUsed; bucket.calldataBytes += batch.calldataBytes;
      }
      contractsLast24h += batch.contracts; gasUsedLast24h += batch.gasUsed;
      if (batch.t > now - HOUR) { hour.contracts += batch.contracts; hour.matches += batch.selected; hour.seconds += batch.seconds; }
    }
    // The rate covers the last hour, or less when the current run of batches (a sync started, or resumed after a gap)
    // began within it: back from the last batch while the one before it is near.
    let runStart = now - HOUR;
    for (let i = this.recent.length - 1; i >= 0; i--) {
      const batch = this.recent[i]!, start = batch.t - batch.seconds * 1000, before = this.recent[i - 1];
      if (start <= now - HOUR) break;
      if (!before || start - before.t > RUN_GAP_MS) { runStart = start; break; }
    }
    const spanMs = Math.max(60_000, now - runStart);
    const last = this.last;
    return {
      totals: { ...this.totals, gasUsed: String(this.totals.gasUsed) },
      hourly: hourly.map(bucket => ({ ...bucket, gasUsed: String(bucket.gasUsed) })),
      rates: { contractsLastHour: hour.contracts, matchesLastHour: hour.matches, contractsLast24h, secondsPerContract: hour.contracts ? +(hour.seconds / hour.contracts).toFixed(2) : null },
      lastBatch: last ? { at: last.at, matchId: last.matchId, contracts: last.contracts, alreadySynced: last.alreadySynced, created: last.created, reused: last.reused,
        transactions: last.transactions, calldataBytes: last.calldataBytes, seconds: last.seconds } : null,
      gasUsedLast24h, matchesPerHour: Math.round(hour.matches * HOUR / spanMs),
    };
  }
}

/** sync.jsonl, read incrementally: each update reads only what was appended (complete lines); a file replaced or
 * truncated is read again from the start, a removed one leaves an empty log. */
export class SyncFile {
  log = new SyncLog();
  private offset = 0;
  private inode = -1;
  private rest = Buffer.alloc(0);
  constructor(readonly path: string, private chunkBytes = 1 << 20) {}

  async update(now = Date.now()) {
    let info;
    try { info = await stat(this.path); } catch (error: any) { if (error.code !== 'ENOENT') throw error; if (this.inode !== -1) this.reset(-1); return; }
    if (info.ino !== this.inode || info.size < this.offset) this.reset(info.ino);
    if (info.size === this.offset) return;
    const file = await open(this.path, 'r');
    try {
      const buffer = Buffer.alloc(Math.min(this.chunkBytes, info.size - this.offset));
      while (this.offset < info.size) {
        const { bytesRead } = await file.read(buffer, 0, Math.min(buffer.length, info.size - this.offset), this.offset);
        if (!bytesRead) break;
        this.offset += bytesRead;
        const data = Buffer.concat([this.rest, buffer.subarray(0, bytesRead)]);
        const end = data.lastIndexOf(10);
        if (end >= 0) for (const line of data.subarray(0, end).toString('utf8').split('\n')) this.log.add(line, now);
        this.rest = Buffer.from(data.subarray(end + 1));
        // No line is that long: whatever this is, it is dropped.
        if (this.rest.length > 1 << 20) { this.rest = Buffer.alloc(0); this.log.ignored++; }
      }
    } finally { await file.close(); }
  }

  private reset(inode: number) { this.log = new SyncLog(); this.offset = 0; this.inode = inode; this.rest = Buffer.alloc(0); }
}

/** The state shown for a target: status.json's status, but `stale` when its sync is not alive (`live`: sync.lock was
 * refreshed less than 2 minutes ago) though the status says it runs, or is hung: syncing, caught up or starting, but
 * status.json not rewritten for `quietMs` (its lock refreshes on a timer of its own). Without a status.json, `starting`
 * while the lock is live (a first start, or a one-off run, which writes none), else `stopped` (`stale` with a stale lock). */
export function syncState(status: string | undefined, lockAgeMs: number | undefined, statusAgeMs?: number, quietMs = HUNG_MS) {
  const live = lockAgeMs !== undefined && lockAgeMs < LOCK_STALE_MS;
  if (status === undefined) return { live, state: live ? 'starting' : lockAgeMs === undefined ? 'stopped' : 'stale' };
  const known = /^[a-z-]{1,20}$/.test(status) ? status : 'unknown';
  const hung = statusAgeMs !== undefined && statusAgeMs > quietMs && ['syncing', 'caught-up', 'starting'].includes(known);
  return { live, state: hung || (!live && known !== 'stopped' && known !== 'failed') ? 'stale' : known };
}

/** What a database query told about one target's cursor; `error` (databaseError) when its own queries failed. */
export interface CursorQuery { cursor: string; behind: number | null; next: { id: string; address: string }[]; error?: string }

/** What the last database query says about `cursor`: its contract's address and the Ethereum matches after it. The
 * query listed the matches from its own cursor on, so a cursor that moved on since (the sync advances between two
 * queries) still resolves; one that moved back does not, until the next query. */
export function cursorView(known: CursorQuery | undefined, cursor: string | undefined): { address: string | null; behind: number | null } {
  if (!known || known.error || cursor === undefined || BigInt(cursor) < BigInt(known.cursor)) return { address: null, behind: null };
  const from = BigInt(known.cursor), to = BigInt(cursor);
  const passed = known.next.filter(match => BigInt(match.id) > from && BigInt(match.id) <= to).length;
  return { address: known.next.find(match => match.id === cursor)?.address ?? null, behind: known.behind === null ? null : Math.max(0, known.behind - passed) };
}

/** Seconds until a target has synced the `behind` matches, at the matches it processed over the last hour less the
 * mirror's inflow over the last hour: 0 when nothing is left, the poll interval for a caught-up sync (it takes the
 * rest at its next poll), null when it does not gain on the mirror or a figure is unknown. */
export function etaSeconds(behind: number | null, matchesPerHour: number, inflowPerHour: number | null, state: string, pollSeconds: number | null) {
  if (behind === null) return null;
  if (behind <= 0) return 0;
  if (state === 'caught-up') return pollSeconds;
  if (inflowPerHour === null) return null;
  const net = matchesPerHour - inflowPerHour;
  return net > 0 ? Math.round(behind / net * 3600) : null;
}

/** wei as ETH with `decimals` decimals (rounded half up). */
export function formatEth(wei: bigint, decimals = 4) {
  const unit = 10n ** BigInt(18 - decimals);
  const digits = ((wei < 0n ? -wei : wei) + unit / 2n) / unit;
  const padded = digits.toString().padStart(decimals + 1, '0');
  return `${wei < 0n && digits ? '-' : ''}${padded.slice(0, -decimals)}.${padded.slice(-decimals)}`;
}

/** Days a balance lasts at `spentPerDay` (wei); null when nothing is spent. Whole days from 10 on. */
export function runwayDays(balance: bigint, spentPerDay: bigint) {
  if (spentPerDay <= 0n) return null;
  const days = Number(balance * 1000n / spentPerDay) / 1000;
  return days >= 10 ? Math.round(days) : +days.toFixed(1);
}

/** The daily seed's runs (data/runs/status.jsonl lines): the last, and the last 5 newest first without their key. Only
 * values of the expected form are published (else null); a run's error text never is. */
export function seedRuns(content: string) {
  const runs = content.split('\n').flatMap(line => { try { const run = JSON.parse(line); return run && typeof run === 'object' ? [run] : []; } catch { return []; } });
  const pick = (run: any) => ({ day: matching(/^\d{8}$/, run.day), result: matching(/^[a-z-]{1,20}$/, run.result), startedAt: isoText(run.startedAt),
    seconds: typeof run.seconds === 'number' && Number.isFinite(run.seconds) ? run.seconds : null });
  const last = runs.at(-1);
  return { latest: last ? { ...pick(last), key: matching(/^[A-Za-z0-9._/-]{1,200}$/, last.key) } : null, recent: runs.slice(-5).reverse().map(pick) };
}

/** A periodic source: started when due (every `everyMs` at most), never twice at once; its last result stays. */
class Job<T> {
  value?: T;
  error?: unknown;
  private running?: Promise<void>;
  private started = -Infinity;
  constructor(private everyMs: number) {}
  /** Starts `work` when due; resolves once the run in flight, if any, is done. */
  run(work: () => Promise<T>, now: number) {
    if (!this.running && now - this.started >= this.everyMs) {
      this.started = now;
      this.running = work().then(value => { this.value = value; this.error = undefined; }, error => { this.error = error; }).finally(() => { this.running = undefined; });
    }
    return this.running ?? Promise.resolve();
  }
}

interface MirrorReading {
  newestMatchId: string | null; newestMatchAt: string | null; matchesLastHour: number; matchesLast24h: number;
  hourly: { hour: string; matches: number }[]; queriedAt: string; cursors: Record<string, CursorQuery>;
}

/** The mirror database through one DuckDB instance (its postgres extension, libpq's PG* settings), attached for each
 * query set and detached after: no connection stays open in between. `timings`: the last set's queries, in ms. */
export class Mirror {
  private db?: Database;
  timings: Record<string, number> = {};

  /** The newest Ethereum match, the last 24 h by hour, and per target (name -> cursor) what cursorView needs. */
  async query(cursors: Record<string, string>): Promise<MirrorReading> {
    const db = this.db ??= await Mirror.open();
    this.timings = {};
    const rows = async (label: string, sql: string) => {
      const started = performance.now();
      try { return await db.rows(`SELECT * FROM postgres_query('pg', ${literal(sql)})`); } finally { this.timings[label] = Math.round(performance.now() - started); }
    };
    let failed = false;
    try {
      await db.connection.run('DETACH DATABASE IF EXISTS pg');
      await db.connection.run(`ATTACH ${literal(CONNINFO)} AS pg (TYPE postgres, READ_ONLY)`);
      try {
        const [newest] = await rows('newest', `SELECT m.id::text AS id, to_char(m.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS at ${MATCHES} ` +
          `WHERE d.chain_id=1 AND m.id<${localMatchIdStart} ORDER BY m.id DESC LIMIT 1`);
        const reading: MirrorReading = { newestMatchId: newest?.id ?? null, newestMatchAt: newest?.at ?? null, matchesLastHour: 0, matchesLast24h: 0,
          hourly: hourStarts(Date.now()).map(start => ({ hour: iso(start), matches: 0 })), queriedAt: '', cursors: {} };
        if (isId(newest?.id)) {
          const top = BigInt(newest.id);
          // The oldest group, the hour before the 24 shown, counts only towards matchesLast24h.
          for (const group of await rows('hourly', `SELECT to_char(date_trunc('hour', m.created_at AT TIME ZONE 'UTC'), 'YYYY-MM-DD"T"HH24:00:00.000"Z"') AS hour, count(*)::int AS matches, ` +
            `(count(*) FILTER (WHERE m.created_at > now() - interval '1 hour'))::int AS last_hour ${MATCHES} ` +
            `WHERE d.chain_id=1 AND m.id>${top - DAY_ID_SPAN} AND m.id<=${top} AND m.created_at > now() - interval '24 hours' GROUP BY 1`)) {
            reading.matchesLast24h += group.matches; reading.matchesLastHour += group.last_hour;
            const bucket = reading.hourly.find(b => b.hour === group.hour);
            if (bucket) bucket.matches = group.matches;
          }
          // Each target's queries on their own: one that fails (a statement timeout) leaves only its cursor unknown.
          const known = new Map<string, CursorQuery>();
          for (const [name, cursor] of Object.entries(cursors)) {
            if (!known.has(cursor)) {
              const after = BigInt(cursor);
              try {
                const next = await rows(`next:${name}`, `SELECT m.id::text AS id, '0x'||encode(d.address,'hex') AS address ${MATCHES} ` +
                  `WHERE d.chain_id=1 AND m.id>=${after} AND m.id<=${top} ORDER BY m.id LIMIT ${NEXT_MATCHES + 1}`);
                const behind = after >= top ? 0 : top - after > BEHIND_ID_SPAN ? null
                  : (await rows(`behind:${name}`, `SELECT count(*)::int AS behind ${MATCHES} WHERE d.chain_id=1 AND m.id>${after} AND m.id<=${top}`))[0]!.behind;
                known.set(cursor, { cursor, behind, next: next.map(row => ({ id: String(row.id), address: String(row.address) })) });
              } catch (error) { known.set(cursor, { cursor, behind: null, next: [], error: databaseError(error) }); failed = true; }
            }
            reading.cursors[name] = known.get(cursor)!;
          }
        }
        reading.queriedAt = new Date().toISOString();
        return reading;
      } finally { await db.connection.run('DETACH DATABASE IF EXISTS pg').catch(() => {}); }
    } catch (error) { failed = true; throw error; }
    finally {
      // Opened again for the next set after any failure: nothing of a broken connection is reused.
      if (failed) { this.db = undefined; await db.close().catch(() => {}); }
    }
  }

  async close() { await this.db?.close(); this.db = undefined; }

  private static async open() {
    const db = await Database.open(':memory:', { threads: '1', memory_limit: '64MB' }, tmpdir(), 'arkiv-dashboard-');
    try { await db.connection.run('INSTALL postgres'); await db.connection.run('LOAD postgres'); } catch (error) { await db.close(); throw error; }
    return db;
  }
}

interface ChainReading {
  head: number | null; headAt: string | null; blockSeconds: number | null; gasPrice: bigint | null; balance: bigint | null;
  nonce: number | null; pendingNonce: number | null; queriedAt: string; error: string | null;
}

/** Read-only calls, 10 s each: a failed one leaves its fields null and sets `error` (not sanitized yet). */
async function readChain(client: PublicClient, writer: string | null): Promise<ChainReading> {
  const errors: unknown[] = [];
  const get = async <T>(call: () => Promise<T>): Promise<T | null> => { try { return await call(); } catch (error) { errors.push(error); return null; } };
  const address = writer?.toLowerCase() as Hex | undefined;
  const [head, gasPrice, balance, nonce, pendingNonce] = await Promise.all([
    get(() => client.getBlock({ blockTag: 'latest' })), get(() => client.getGasPrice()),
    address ? get(() => client.getBalance({ address })) : null,
    address ? get(() => client.getTransactionCount({ address, blockTag: 'latest' })) : null,
    address ? get(() => client.getTransactionCount({ address, blockTag: 'pending' })) : null,
  ]);
  const span = head ? (head.number < BLOCK_SPAN ? head.number : BLOCK_SPAN) : 0n;
  const earlier = head && span ? await get(() => client.getBlock({ blockNumber: head.number - span })) : null;
  return {
    head: head ? Number(head.number) : null, headAt: head ? iso(Number(head.timestamp) * 1000) : null,
    blockSeconds: head && earlier ? +(Number(head.timestamp - earlier.timestamp) / Number(span)).toFixed(2) : null,
    gasPrice, balance, nonce, pendingNonce, queriedAt: new Date().toISOString(), error: errors.length ? rpcError(errors[0]) : null,
  };
}

/** What is kept of a target between refreshes. */
interface Tracked { sync: SyncFile; chain: Job<ChainReading>; client?: { key: string; client: PublicClient } }
const tracked = new Map<string, Tracked>();

async function readJson(path: string): Promise<Record<string, any> | undefined> {
  let content: string;
  try { content = await readFile(path, 'utf8'); } catch (error: any) { if (error.code === 'ENOENT') return undefined; throw error; }
  const value = JSON.parse(content);
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('not a JSON object');
  return value;
}

/** The last `bytes` of a file, from its first complete line on. */
async function readTail(path: string, bytes: number) {
  const file = await open(path, 'r');
  try {
    const { size } = await file.stat();
    const start = Math.max(0, size - bytes), buffer = Buffer.alloc(size - start);
    await file.read(buffer, 0, buffer.length, start);
    const content = buffer.toString('utf8');
    return start ? content.slice(content.indexOf('\n') + 1) : content;
  } finally { await file.close(); }
}

/** A target's files (its settings, data/sync/NAME) and sync.jsonl's new lines; what cannot be read is listed in `errors`. */
async function readTarget(name: string, shared: Record<string, string>, now: number) {
  const kept: Tracked = tracked.get(name) ?? { sync: new SyncFile(join(paths.sync, name, 'sync.jsonl')), chain: new Job((REFRESH_SECONDS - 1) * 1000) };
  tracked.set(name, kept);
  const errors: string[] = [];
  const attempt = async <T>(label: string, work: () => Promise<T>) => { try { return await work(); } catch (error) { errors.push(fileError(label, error)); return undefined; } };
  const own = await attempt(`targets/${name}.env`, async () => parseEnv(await readFile(join(paths.targets, `${name}.env`), 'utf8'))) ?? {};
  const get = settingOf(own, shared);
  const dir = join(paths.sync, name);
  const [status, state, lockAge] = await Promise.all([
    attempt('status.json', () => readJson(join(dir, 'status.json'))),
    attempt('state.json', () => readJson(join(dir, 'state.json'))),
    attempt('sync.lock', async () => { try { return now - (await stat(join(dir, 'sync.lock'))).mtimeMs; } catch (error: any) { if (error.code === 'ENOENT') return undefined; throw error; } }),
    attempt('sync.jsonl', () => kept.sync.update(now)),
  ]);
  if (state && !isId(state.lastMatchId)) errors.push('state.json: no valid lastMatchId');
  const rpcUrl = get('ARKIV_RPC_URL'), apiKey = get('ARKIV_RPC_API_KEY');
  // The running sync's URL may differ from the file's (a rotated token) or be all there is (an unreadable file).
  for (const url of [rpcUrl, text(status?.rpcUrl)]) if (url) secrets.urls.add(url);
  if (apiKey) secrets.values.add(apiKey);
  let rpcHost: string | null = null;
  try { if (rpcUrl) rpcHost = new URL(rpcUrl).hostname; } catch { errors.push('ARKIV_RPC_URL is invalid'); }
  if (rpcHost) {
    const key = `${rpcUrl}\n${apiKey ?? ''}`;
    // One retry: a node closing an idle keep-alive connection is common and not worth an error.
    if (kept.client?.key !== key) kept.client = { key, client: createPublicClient({ transport: http(rpcUrl, { timeout: RPC_TIMEOUT_MS, retryCount: 1, fetchOptions: apiKey ? { headers: { 'X-Api-Key': apiKey } } : undefined }) }) };
  } else kept.client = undefined;
  const chainId = Number(get('ARKIV_CHAIN_ID') ?? status?.chainId);
  return {
    name, kept, status, errors, lockAge: lockAge as number | undefined, rpcHost, chainId: Number.isSafeInteger(chainId) ? chainId : null, statusAt: isoText(status?.at),
    settings: targetSettings(get), cursor: isId(state?.lastMatchId) ? state!.lastMatchId as string : undefined, cursorAt: isoText(state?.updatedAt),
    writer: isAddress(status?.writer) ? status!.writer as string : isAddress(get('ENTITY_OWNER')) ? get('ENTITY_OWNER')! : null,
  };
}

/** One refresh: reads every source (waiting at most `waitMs` for the database and the RPCs, which otherwise land in a
 * later refresh), writes status.json, and returns what failed (sanitized), for the log. */
async function refresh(mirror: Mirror, mirrorJob: Job<MirrorReading>, started: number, waitMs: number) {
  const problems: Record<string, string> = {}; // raw, cleaned on return
  let shared: Record<string, string> = {};
  try { shared = parseEnv(await readFile(paths.envFile, 'utf8')); } catch (error: any) { if (error.code !== 'ENOENT') problems.env = fileError('.env', error); }
  for (const url of [shared.ARKIV_RPC_URL]) if (url) secrets.urls.add(url);
  if (shared.ARKIV_RPC_API_KEY) secrets.values.add(shared.ARKIV_RPC_API_KEY);
  let names: string[] = [];
  try { names = (await readdir(paths.targets)).filter(file => TARGET_FILE.test(file)).map(file => file.slice(0, -4)).sort(); }
  catch (error) { problems.targets = fileError('targets', error); }
  for (const name of tracked.keys()) if (!names.includes(name)) tracked.delete(name);
  const targets = await Promise.all(names.map(name => readTarget(name, shared, started)));

  const cursors = Object.fromEntries(targets.flatMap(t => t.cursor ? [[t.name, t.cursor]] : []));
  const runs = [mirrorJob.run(() => mirror.query(cursors), started),
    ...targets.flatMap(t => t.kept.client ? [t.kept.chain.run(() => readChain(t.kept.client!.client, t.writer), started)] : [])];
  const seed = readTail(join(paths.runs, 'status.jsonl'), 64 * 1024).then(content => ({ ...seedRuns(content), error: null as string | null }),
    (error: any) => ({ latest: null, recent: [], error: error.code === 'ENOENT' ? null : clean(fileError('status.jsonl', error)) }));
  await Promise.race([Promise.all(runs), new Promise(done => setTimeout(done, waitMs).unref())]);

  const reading = mirrorJob.value;
  // Published: only the kind of failure. Logged: libpq's text too (DuckDB adds the connection string, CONNINFO: noise).
  const mirrorError = mirrorJob.error === undefined ? null : databaseError(mirrorJob.error);
  if (mirrorError) problems.mirror = `${mirrorError}: ${describeError(mirrorJob.error).replaceAll(` at "${CONNINFO}"`, '')}`;
  const now = Date.now();
  const output = {
    schema: 1, generatedAt: iso(now), revision: env.APP_REVISION ?? '', refreshSeconds: REFRESH_SECONDS,
    mirror: { publicUrl: PUBLIC_URL, newestMatchId: reading?.newestMatchId ?? null, newestMatchAt: reading?.newestMatchAt ?? null,
      matchesLastHour: reading?.matchesLastHour ?? null, matchesLast24h: reading?.matchesLast24h ?? null, hourly: reading?.hourly ?? null,
      queriedAt: reading?.queriedAt ?? null, error: mirrorError },
    seed: await seed,
    targets: targets.map(t => {
      const statusAge = t.statusAt ? started - Date.parse(t.statusAt) : undefined;
      const { state, live } = syncState(typeof t.status?.status === 'string' ? t.status.status : undefined, t.lockAge, statusAge, HUNG_MS + (t.settings.batchDelaySeconds ?? 0) * 1000);
      const summary = t.kept.sync.log.summary(started);
      const query = reading?.cursors[t.name];
      if (query?.error) t.errors.push(query.error);
      const { address, behind } = cursorView(query, t.cursor);
      const chain = t.kept.client ? t.kept.chain.value : undefined;
      const spent = chain?.gasPrice != null ? summary.gasUsedLast24h * chain.gasPrice : null;
      if (t.errors.length) problems[t.name] = t.errors.join('; ');
      if (chain?.error) problems[`${t.name}.chain`] = chain.error;
      const error = [text(t.status?.error), ...t.errors].filter(Boolean).join('; ');
      return {
        name: t.name, chainId: t.chainId, rpcHost: t.rpcHost, writer: t.writer, state, statusAt: t.statusAt, live,
        error: error ? clean(error) : null, failures: count(t.status?.failures),
        cursor: { matchId: t.cursor ?? null, at: t.cursor ? t.cursorAt : null, address, contractUrl: address ? `${PUBLIC_URL}/v2/contract/1/${address}` : null },
        behind, etaSeconds: etaSeconds(behind, summary.matchesPerHour, reading?.matchesLastHour ?? null, state, t.settings.pollSeconds),
        settings: t.settings, rates: summary.rates, hourly: summary.hourly, totals: summary.totals, lastBatch: summary.lastBatch,
        chain: {
          head: chain?.head ?? null, headAt: chain?.headAt ?? null, blockSeconds: chain?.blockSeconds ?? null,
          gasPriceGwei: chain?.gasPrice != null ? +(Number(chain.gasPrice) / 1e9).toPrecision(6) : null,
          balanceEth: chain?.balance != null ? formatEth(chain.balance) : null, nonce: chain?.nonce ?? null, pendingNonce: chain?.pendingNonce ?? null,
          spentEthLast24h: spent !== null ? formatEth(spent) : null, runwayDays: chain?.balance != null && spent !== null ? runwayDays(chain.balance, spent) : null,
          queriedAt: chain?.queriedAt ?? null, error: chain?.error ? clean(chain.error) : t.kept.client ? null : 'no RPC URL',
        },
      };
    }),
  };
  await writePublic(join(paths.out, 'status.json'), JSON.stringify(output) + '\n');
  return Object.fromEntries(Object.entries(problems).map(([part, problem]) => [part, clean(problem)]));
}

/** Replaces a published file atomically, mode 644, through a temporary file of its own (no two writers interleave). */
async function writePublic(path: string, content: string | Uint8Array) {
  const temporary = `${path}.${process.pid}.${randomBytes(4).toString('hex')}.tmp`;
  try { await writeFile(temporary, content, { mode: 0o644 }); await chmod(temporary, 0o644); await rename(temporary, path); }
  catch (error) { await rm(temporary, { force: true }).catch(() => {}); throw error; }
}

async function main() {
  const { values } = parseArgs({ strict: true, options: { once: { type: 'boolean', default: false }, help: { type: 'boolean', short: 'h', default: false } } });
  if (values.help) { console.log((await readFile(import.meta.path, 'utf8')).split('*/')[0]!.replace(/^#!.*\n\/\*\*\n/, '').replace(/^ \* ?/gm, '')); return; }
  // A directory that cannot be written is logged, here and by every refresh's attempt, rather than restarted over.
  const copied: string[] = [];
  try {
    await mkdir(paths.out, { recursive: true, mode: 0o755 });
    for (const file of STATIC_FILES) {
      let content: Buffer;
      try { content = await readFile(join(HERE, file)); } catch (error: any) { if (error.code === 'ENOENT') continue; throw error; }
      await writePublic(join(paths.out, file), content); copied.push(file);
    }
  } catch (error) { log('copy-failed', { error: clean(describeError(error)) }); }
  log('start', { copied, refreshSeconds: REFRESH_SECONDS, databaseSeconds: DATABASE_SECONDS, once: values.once });
  const mirror = new Mirror(), mirrorJob = new Job<MirrorReading>(DATABASE_SECONDS * 1000);
  // A failure is logged when it starts or changes, and once when all is well again.
  let reported = '{}';
  for (let first = true; ; first = false) {
    const started = Date.now();
    let problems: Record<string, string>;
    try { problems = await refresh(mirror, mirrorJob, started, first ? 60_000 : (REFRESH_SECONDS - 3) * 1000); }
    catch (error) { problems = { output: clean(describeError(error)) }; }
    const current = json(problems);
    if (current !== reported || (values.once && current !== '{}')) log(current === '{}' ? 'ok' : 'errors', current === '{}' ? {} : { errors: problems });
    reported = current;
    if (values.once) {
      log('written', { targets: tracked.size, mirrorQueryMs: mirror.timings });
      await mirror.close();
      if (problems.output) process.exitCode = 1;
      return;
    }
    await Bun.sleep(Math.max(1000, started + REFRESH_SECONDS * 1000 - Date.now()));
  }
}

if (import.meta.main) {
  try { await main(); } catch (error) { log('failed', { error: clean(describeError(error)) }); process.exitCode = 1; }
}
