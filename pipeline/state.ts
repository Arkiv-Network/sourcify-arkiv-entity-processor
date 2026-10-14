/**
 * $WORK_DIR/timings.json bookkeeping for pipeline/run.sh. Every call rewrites the file atomically (tmp + rename)
 * and prints one JSON progress line to stdout.
 *
 *   init COMMAND | start PHASE | end PHASE STATUS [MESSAGE] | forget PHASE | process-reusable | check-counts | finish STATUS [ERROR]
 */
import { createHash } from 'node:crypto';
import { closeSync, existsSync, fsyncSync, openSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { hostname } from 'node:os';
import { join } from 'node:path';

type Json = Record<string, any>;
const env = process.env;
const workDir = env.WORK_DIR!, file = join(workDir, 'timings.json');
const paths = { source: join(workDir, 'source'), bundle: join(workDir, 'bundle'), archive: join(workDir, 'archive', 'seed.tar.zst'), logs: join(workDir, 'logs') };
const now = new Date(), iso = now.toISOString();
const read = (path: string): Json | undefined => {
  try { return JSON.parse(readFileSync(path, 'utf8')); } catch (error: any) { if (error.code === 'ENOENT') return undefined; throw error; }
};
const state: Json = read(file) ?? { schema: 'arkiv-entity-processor.run.v1', createdAt: iso, status: 'running', phases: {}, invocations: [] };
const save = () => {
  state.updatedAt = iso;
  const descriptor = openSync(`${file}.tmp`, 'w');
  try { writeFileSync(descriptor, JSON.stringify(state, null, 2) + '\n'); fsyncSync(descriptor); } finally { closeSync(descriptor); }
  renameSync(`${file}.tmp`, file);
};
const emit = (line: Json) => console.log(JSON.stringify({ pipeline: line.event ?? line.phase, at: iso, ...line }));
const round = (value: number) => Math.round(value * 10) / 10;
const sha256 = (path: string) => createHash('sha256').update(readFileSync(path)).digest('hex');
const dirBytes = (path: string): number => !existsSync(path) ? 0 : readdirSync(path, { withFileTypes: true })
  .reduce((sum, entry) => sum + (entry.isDirectory() ? dirBytes(join(path, entry.name)) : statSync(join(path, entry.name)).size), 0);
const bool = (value: string | undefined) => value === '1';

function config(): Json {
  return {
    // The source database: the prepared source's, else the libpq settings (the password is never recorded).
    database: sourceManifest()?.database ?? { host: env.PGHOST ?? null, port: env.PGPORT || '5432', database: env.PGDATABASE ?? null, user: env.PGUSER ?? null, schema: env.PG_SCHEMA },
    prepareLimit: env.PREPARE_LIMIT || null, prepareThreads: env.PREPARE_THREADS, prepareMemoryLimit: env.PREPARE_MEMORY_LIMIT,
    sourceBatchSize: env.SOURCE_BATCH_SIZE, sourceMemoryLimit: env.SOURCE_MEMORY_LIMIT, contractLimit: env.CONTRACT_LIMIT || null,
    skipMissingReferences: bool(env.SKIP_MISSING_REFERENCES), maxSkipped: env.MAX_SKIPPED, processWorkers: env.PROCESS_WORKERS, compressionWorkers: env.COMPRESSION_WORKERS,
    entityOwner: env.ENTITY_OWNER, destinationChainId: env.DESTINATION_CHAIN_ID, progressInterval: env.PROGRESS_INTERVAL,
    ...(['prepare', 'run'].includes(env.PIPELINE_COMMAND!) ? {} : { packContents: env.PACK_CONTENTS, zstdLevel: env.ZSTD_LEVEL, r2Bucket: env.R2_BUCKET || null, r2Key: env.R2_KEY || null }),
  };
}
/** Settings that change the published bundle; a completed bundle is only reused under the same ones. */
const outputConfig = (c: Json) => ({ prepareLimit: c.prepareLimit, contractLimit: c.contractLimit, skipMissingReferences: c.skipMissingReferences,
  entityOwner: String(c.entityOwner).toLowerCase(), destinationChainId: c.destinationChainId,
  processorSha256: state.build.processorSha256, scriptsSha256: state.build.scriptsSha256 });
function build(): Json {
  const root = env.APP_DIR!;
  const scripts = ['source', 'pipeline'].flatMap(dir => readdirSync(join(root, dir)).filter(name => /\.(ts|sh)$/.test(name)).map(name => `${dir}/${name}`)).sort();
  const digest = createHash('sha256');
  for (const script of scripts) digest.update(`${sha256(join(root, script))}  ${script}\n`);
  return { revision: env.APP_REVISION || null, processorSha256: env.PROCESSOR_PATH ? sha256(env.PROCESSOR_PATH) : null,
    scriptsSha256: digest.digest('hex'), scripts: scripts.length, bun: Bun.version, hostname: hostname(), container: existsSync('/.dockerenv') };
}
/** Container cgroup v2 counters (container-wide). memory.peak is not used: it includes page cache. */
function cgroup(): Json | undefined {
  if (!existsSync('/.dockerenv')) return undefined;
  const value = (name: string) => { try { return readFileSync(`/sys/fs/cgroup/${name}`, 'utf8').trim(); } catch { return undefined; } };
  const usage = value('cpu.stat')?.match(/usage_usec (\d+)/)?.[1], kills = value('memory.events')?.match(/oom_kill (\d+)/)?.[1];
  return { cpuSeconds: usage === undefined ? undefined : Number(usage) / 1e6, oomKills: kills === undefined ? undefined : Number(kills), cpuMax: value('cpu.max'), memoryMax: value('memory.max') };
}
/** Peak container anonymous memory sampled by run.sh's monitor during the phase (every <= 5 s). */
function sampledPeak(phase: string): number | undefined {
  try { const bytes = Number(readFileSync(join(workDir, 'tmp', `memory-peak-${phase}`), 'utf8')); return bytes > 0 ? bytes : undefined; } catch { return undefined; }
}
const sourceManifest = () => read(join(paths.source, 'source-manifest.json'));
function records(): Json | undefined {
  const report = read(join(paths.bundle, 'report.json'));
  if (!report) return undefined;
  return { contracts: report.contracts, sourceSkipped: report.sourceSkippedCount ?? 0, entities: report.entities, uniquePayloads: report.uniquePayloads,
    payloadBytes: report.payloadBytes, parts: report.parts?.length, processSeconds: report.seconds, contractsPerSecond: report.contractsPerSecond };
}
function details(phase: string): Json {
  if (phase === 'prepare') {
    const manifest = sourceManifest();
    return manifest ? { selectedCount: manifest.selectedCount, firstMatchId: manifest.firstMatchId, lastMatchId: manifest.lastMatchId, databaseBytes: manifest.databaseBytes } : {};
  }
  if (phase === 'process') return { ...records(), bundleBytes: dirBytes(paths.bundle) };
  if (phase === 'verify') {
    const sums = existsSync(join(paths.bundle, 'SHA256SUMS')) ? readFileSync(join(paths.bundle, 'SHA256SUMS'), 'utf8').trim().split('\n').length : 0;
    return { checkedFiles: sums };
  }
  if (phase === 'pack') {
    const archive = read(`${paths.archive}.json`);
    return archive ? { contents: archive.contents ?? 'full', archiveFiles: archive.files, archiveBytes: archive.bytes, archiveSha256: archive.sha256 } : {};
  }
  if (phase === 'upload') {
    const result = read(join(paths.logs, 'upload-result.json'));
    return result ? { uri: result.uri, bytes: result.bytes, sha256: result.sha256, uploaded: result.uploaded, verifiedRemote: result.verifiedRemote } : {};
  }
  return {};
}

const [command, ...args] = process.argv.slice(2);
switch (command) {
  case 'init': {
    for (const phase of Object.values<Json>(state.phases)) if (phase.status === 'running') phase.status = 'interrupted';
    const last = state.invocations.at(-1);
    if (last && !last.finishedAt) last.status = 'interrupted';
    state.status = 'running'; delete state.error; delete state.finishedAt;
    // `publish` keeps the processing settings recorded by `run`; it only adds its own.
    const fresh = config();
    state.workDir = workDir; state.config = args[0] === 'publish' && state.config ? { ...state.config, packContents: fresh.packContents, zstdLevel: fresh.zstdLevel, r2Bucket: fresh.r2Bucket, r2Key: fresh.r2Key } : fresh;
    state.build = build(); state.limits = cgroup() && (({ cpuSeconds: _, ...limits }) => limits)(cgroup()!);
    state.invocations.push({ command: args[0], startedAt: iso, status: 'running', scriptsSha256: state.build.scriptsSha256, processorSha256: state.build.processorSha256 });
    save(); emit({ event: 'start', command: args[0], invocation: state.invocations.length, workDir, config: state.config, build: state.build, limits: state.limits });
    break;
  }
  case 'start': {
    const [name] = args, previous: Json | undefined = state.phases[name];
    const earlier = previous?.status === 'completed' ? (({ completedEarlier: _, ...entry }) => entry)(previous) : previous?.completedEarlier;
    rmSync(join(workDir, 'tmp', `memory-peak-${name}`), { force: true });
    state.phases[name] = { status: 'running', invocation: state.invocations.length, startedAt: iso, ...(earlier ? { completedEarlier: earlier } : {}), cpuSecondsAtStart: cgroup()?.cpuSeconds };
    save(); emit({ phase: name, status: 'started' });
    break;
  }
  case 'end': {
    const [name, status, message] = args, phase: Json = state.phases[name] ??= { startedAt: iso };
    const counters = cgroup();
    Object.assign(phase, { status, endedAt: iso, seconds: round((now.getTime() - Date.parse(phase.startedAt)) / 1000) }, message ? { message } : {});
    if (counters && phase.cpuSecondsAtStart !== undefined) Object.assign(phase, { cpuSeconds: round(counters.cpuSeconds - phase.cpuSecondsAtStart), sampledAnonMemoryPeakBytes: sampledPeak(name), containerOomKills: counters.oomKills });
    delete phase.cpuSecondsAtStart;
    if (status === 'completed' || status === 'reused') Object.assign(phase, details(name));
    if (name === 'process' && status === 'completed') phase.outputConfig = outputConfig(state.config);
    if (status === 'reused' && phase.completedEarlier?.outputConfig) phase.outputConfig = phase.completedEarlier.outputConfig;
    // A failed attempt keeps the earlier completion (e.g. a refused reuse of a completed bundle).
    if (status === 'completed') delete phase.completedEarlier;
    save();
    const { completedEarlier: _, outputConfig: __, endedAt: ___, startedAt: ____, ...summary } = phase;
    emit({ phase: name, ...summary });
    break;
  }
  case 'forget': {
    // A new attempt replaces the phase's output: an earlier completion no longer describes what is on disk.
    delete state.phases[args[0]]?.completedEarlier;
    save();
    break;
  }
  case 'process-reusable': {
    const recorded = state.phases.process?.outputConfig ?? state.phases.process?.completedEarlier?.outputConfig;
    const current = outputConfig(config());
    if (JSON.stringify(recorded) !== JSON.stringify(current)) {
      console.error(`The completed bundle was produced with ${JSON.stringify(recorded ?? 'unrecorded settings')}; the current settings are ${JSON.stringify(current)}.`);
      process.exit(1);
    }
    break;
  }
  case 'check-counts': {
    const report = read(join(paths.bundle, 'report.json')), manifest = sourceManifest();
    if (!report || report.status !== 'completed' || !manifest) { console.error('report.json or the source manifest is missing or incomplete'); process.exit(1); }
    const skipped = report.sourceSkippedCount ?? 0, limit = state.config.contractLimit;
    const ok = limit ? report.contracts === Number(limit) : report.contracts + skipped === manifest.selectedCount;
    const counts = { contracts: report.contracts, skipped, selectedCount: manifest.selectedCount, contractLimit: limit };
    if (!ok) { console.error(`Record counts do not add up: ${JSON.stringify(counts)}`); process.exit(1); }
    break;
  }
  case 'finish': {
    const [status, error] = args;
    state.status = status; state.finishedAt = iso;
    if (error) state.error = error;
    const result = records();
    if (result) state.records = { selectedCount: sourceManifest()?.selectedCount, ...result };
    const phaseSeconds = (phase: Json) => phase.status === 'completed' ? phase.seconds : phase.status === 'reused' ? phase.completedEarlier?.seconds ?? 0 : 0;
    state.phaseSecondsTotal = round(Object.values<Json>(state.phases).reduce((sum, phase) => sum + (phaseSeconds(phase) ?? 0), 0));
    const last = state.invocations.at(-1);
    if (last) Object.assign(last, { status, finishedAt: iso, seconds: round((now.getTime() - Date.parse(last.startedAt)) / 1000) });
    save();
    emit({ event: 'finish', status, ...(error ? { error } : {}), phases: Object.fromEntries(Object.entries<Json>(state.phases).map(([name, phase]) => [name, { status: phase.status, seconds: phase.seconds, invocation: phase.invocation }])),
      phaseSecondsTotal: state.phaseSecondsTotal, records: state.records });
    break;
  }
  default:
    console.error(`Unknown command ${command}`); process.exit(2);
}
