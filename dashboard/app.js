// Sourcify → Arkiv sync dashboard (ES module). Polls status.json (schema 2) and renders it with DOM APIs
// only (createElement / textContent, never innerHTML): error strings come from remote chains.
const D = '–', NB = ' '; // NB keeps a number and its unit on one line
const STALE_DATA = 120;    // s: generatedAt older than this → generator down?
const STALL_CHAIN = 60;    // s between the chain's last block and our query → chain stalled
const QUIET_MIRROR = 1800; // s without a new Ethereum match → mirror ingest quiet
const LOW_RUNWAY = 7;      // days of writer balance left
const MON = 'Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec'.split(' ');
const WDAY = 'Sun Mon Tue Wed Thu Fri Sat'.split(' ');
const NF = new Intl.NumberFormat('en-US');
// known sync states → severity level (the state name itself is always shown as text)
const STATES = {
  'caught-up': 'ok', syncing: 'info', starting: 'info', 'needs-seed': 'warn',
  retrying: 'warn', stopped: 'warn', failed: 'bad', stale: 'bad',
};
const GLYPH = { ok: '✓', info: '↻', warn: '!', bad: '✕' };

let data = null, fail = null, skew = 0, timer = 0, busy = false, width = 0, hooks = [];
const kept = new Set(); // open <details> / expanded errors, kept across re-renders
const $ = id => document.getElementById(id);

// ---- values & formatting ----
const obj = v => (v && typeof v === 'object' && !Array.isArray(v) ? v : {});
const arr = v => (Array.isArray(v) ? v : []);
const str = v => (v == null ? '' : String(v));
const clip = (s, n) => (s = str(s), s.length > n ? s.slice(0, n - 1) + '…' : s);
const num = v => (v == null || v === '' || !isFinite(+v) ? null : +v);
const ms = v => { const t = typeof v === 'string' ? Date.parse(v) : NaN; return isNaN(t) ? null : t; };
const now = () => Date.now() + skew;
const age = v => { const t = ms(v); return t == null ? null : (now() - t) / 1000; };
const pad = n => String(n).padStart(2, '0');
const int = v => ((v = num(v)) == null ? D : NF.format(Math.round(v)));
const fix = (v, d) => ((v = num(v)) == null ? D : v.toLocaleString('en-US', { minimumFractionDigits: d, maximumFractionDigits: d }));
const unit = (s, u) => (s === D ? D : s + u);
const eth = v => unit(fix(v, Math.abs(num(v)) < 10 ? 4 : 2), NB + 'ETH');
const hash = v => (num(v) == null ? D : '#' + int(v));
const plural = (v, w) => int(v) + ' ' + w + (num(v) === 1 ? '' : 's');

// 1234 → "1.2 k", 3.4e6 bytes → "3.4 MB", 9.3e9 gas → "9.3 Ggas"
function compact(v, u = '') {
  if ((v = num(v)) == null) return D;
  for (const [d, p] of [[1e12, 'T'], [1e9, 'G'], [1e6, 'M'], [1e3, 'k']]) {
    if (Math.abs(v) >= d * 0.9995) { const x = v / d; return x.toFixed(x >= 99.95 ? 0 : 1) + NB + p + u; }
  }
  return (v % 1 ? +v.toFixed(1) : v) + (u ? NB + u : '');
}
// seconds → "12 s", "4 m 05 s", "38 m", "3 h 05 m", "2 d 04 h"
function dur(s) {
  if ((s = num(s)) == null) return D;
  s = Math.max(0, Math.round(s));
  const m = Math.floor(s / 60), h = Math.floor(m / 60), d = Math.floor(h / 24);
  const u = (a, x, b, y) => a + NB + x + (y ? ' ' + pad(b) + NB + y : '');
  return s < 60 ? u(s, 's') : m < 10 ? u(m, 'm', s % 60, 's') : h < 1 ? u(m, 'm') : d < 2 ? u(h, 'h', m % 60, 'm') : u(d, 'd', h % 24, 'h');
}
// deliberately coarse duration for ETAs
const rough = s => (s < 120 ? '< 2 m' : s < 3600 ? Math.round(s / 60) + ' m' : s < 7200 ? '1 h ' + pad(Math.round(s / 60 - 60) % 60) + ' m'
  : s < 172800 ? Math.round(s / 3600) + ' h' : (s / 86400).toFixed(s < 864000 ? 1 : 0) + ' d');
const ago = v => { const a = age(v); return a == null ? D : a < 2 ? 'just now' : dur(a) + ' ago'; };
const mins = a => (a == null ? 'unknown' : a < 60 ? '< 1 min' : a < 5400 ? Math.floor(a / 60) + ' min' : dur(a));
const day = t => new Date(t).getUTCDate() + ' ' + MON[new Date(t).getUTCMonth()];
const hm = t => new Date(t).toISOString().slice(11, 16);
const stamp = (v, withDay = true) => { const t = ms(v); return t == null ? D : (withDay ? day(t) + ' ' : '') + hm(t) + ' UTC'; };
const hourLabel = (t, partial) => (t == null ? D : day(t) + ' ' + hm(t) + '–' + hm(t + 36e5) + ' UTC' + (partial ? ' (in progress)' : ''));
const seedDay = v => { const m = /^\d{4}(\d\d)(\d\d)$/.exec(str(v)); return m ? +m[2] + ' ' + MON[m[1] - 1] : clip(v, 10) || D; };
const short = a => (a = str(a), /^0x[0-9a-f]{40}$/i.test(a) ? a.slice(0, 6) + '…' + a.slice(-4) : clip(a, 14) || D);
const runway = d => (d == null ? D : d < 1 ? '<' + NB + '1' + NB + 'd' : d < 730 ? int(d) + NB + 'd' : (d < 3650 ? (d / 365).toFixed(1) : int(d / 365)) + NB + 'y');
// epoch ms → "today 21:40 UTC" / "tomorrow 08:40 UTC" / "Sat 18 Oct 08:40 UTC" (rounded to 5 min)
function when(t) {
  t = Math.round(t / 3e5) * 3e5;
  const diff = Math.floor(t / 864e5) - Math.floor(now() / 864e5);
  return (diff === 0 ? 'today' : diff === 1 ? 'tomorrow' : WDAY[new Date(t).getUTCDay()] + ' ' + day(t)) + ' ' + hm(t) + ' UTC';
}
function nextRun(at) { // a seed job's daily timer, "HH:MM" UTC
  const [hh, mm] = at.split(':').map(Number), n = now(), t = Math.floor(n / 864e5) * 864e5 + (hh * 60 + mm) * 6e4;
  return 'next run in ' + dur(((t > n ? t : t + 864e5) - n) / 1000) + ' (' + at + ' UTC)';
}

// ---- DOM helpers ----
function add(e, kids) {
  for (const k of kids.flat(9)) if (k != null && k !== false) e.append(k instanceof Node ? k : String(k));
  return e;
}
function h(tag, a, ...kids) {
  const e = document.createElement(tag);
  if (typeof a === 'string') e.className = a;
  else for (const k in a || {}) if (a[k] != null && a[k] !== false) e.setAttribute(k, a[k]);
  return add(e, kids);
}
function s(tag, a, ...kids) {
  const e = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const k in a || {}) if (a[k] != null) e.setAttribute(k, a[k]);
  return add(e, kids);
}
// text refreshed every second by tick()
function live(fn) { const e = h('span'); hooks.push(() => { e.textContent = fn(); }); return e; }
// stat tile: label, value, sub lines; cls adds "warn" / "alarm" / "nodata"
const tile = (label, value, subs, cls) => h('div', 'tile ' + (cls || ''),
  h('div', 'lbl', label), h('div', 'val', value), arr(subs).map(x => (x == null ? null : h('div', 'sub', x))));

// error box: clamped to 3 lines; a toggle appears once the text overflows
function errBox(key, label, msg, after) {
  if (!msg) return null;
  const k = 'err:' + key, p = h('p', null, clip(msg, 4000)), box = h('div', 'err', h('strong', null, label), p);
  const set = (b, on) => {
    box.classList.toggle('full', on);
    on ? kept.add(k) : kept.delete(k);
    b.textContent = on ? 'Show less' : 'Show all';
    b.setAttribute('aria-expanded', on);
  };
  after.push(() => {
    if (!kept.has(k) && p.scrollHeight <= p.clientHeight + 1) return;
    const b = box.appendChild(h('button', { type: 'button', class: 'toggle' }));
    set(b, kept.has(k));
    b.addEventListener('click', () => set(b, !kept.has(k)));
  });
  return box;
}

// ---- charts: inline SVG, sized to the container in px ----
function nice(v) {
  const p = 10 ** Math.floor(Math.log10(v));
  return p * [1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10].find(f => f * p >= v);
}
function barPath(x, y, w, ht) { // 4 px rounded data end, square at the baseline
  const r = Math.min(4, w / 2, ht), f = n => +n.toFixed(1);
  return `M${f(x)},${f(y + ht)}V${f(y + r)}Q${f(x)},${f(y)} ${f(x + r)},${f(y)}H${f(x + w - r)}` +
    `Q${f(x + w)},${f(y)} ${f(x + w)},${f(y + r)}V${f(y + ht)}Z`;
}
// rows: [{t, v, ref?}]; the last row is the hour in progress (drawn faded); ref = mirror inflow line
function bars(W, rows, o) {
  W = Math.max(160, Math.floor(W));
  const H = o.h, top = o.axis ? 8 : 1, bot = o.axis ? 18 : 1, right = o.axis ? 34 : 0;
  const n = rows.length, slot = (W - right) / n, bw = Math.max(1.5, Math.min(24, slot - 2));
  const max = nice(Math.max(o.axis ? 4 : 1, ...rows.map(r => Math.max(r.v || 0, r.ref || 0))));
  const base = H - bot, y = v => base - Math.min(1, Math.max(0, v) / max) * (base - top), f = n => n.toFixed(1);
  const svg = s('svg', { class: 'svg', width: W, height: H, viewBox: `0 0 ${W} ${H}`, role: 'img', 'aria-label': o.label });
  if (o.axis) for (const k of [0, 0.5, 1]) {
    const yy = Math.round(y(max * k)) + 0.5;
    svg.append(s('line', { class: k ? 'grid' : 'base', x1: 0, x2: W - right, y1: yy, y2: yy }));
    if (k) svg.append(s('text', { x: W, y: yy + 3.5, 'text-anchor': 'end' }, compact(max * k)));
  }
  rows.forEach((r, i) => {
    const x = i * slot, cx = x + slot / 2, last = i === n - 1;
    const g = svg.appendChild(s('g', { class: last ? 'partial' : null }, s('title', null, o.tip(r, last)),
      s('rect', { class: 'hit', x: f(x), y: 0, width: f(slot), height: base })));
    if (r.v > 0) g.append(s('path', { class: 'bar ' + o.cls, d: barPath(x + (slot - bw) / 2, y(r.v), bw, base - y(r.v)) }));
    if (o.axis && r.t != null && new Date(r.t).getUTCHours() % 6 === 0 && cx > 15 && cx < W - right - 15) {
      svg.append(s('text', { x: f(cx), y: H - 4, 'text-anchor': 'middle' }, hm(r.t)));
    }
  });
  // inflow line: complete hours solid, the hour in progress faded, a dot on the last complete hour
  const pts = rows.map((r, i) => (r.ref == null ? null : [(i + 0.5) * slot, y(r.ref)]));
  const line = (p, cls) => {
    p = p.filter(Boolean);
    if (p.length > 1) svg.append(s('polyline', { class: cls, points: p.map(q => f(q[0]) + ',' + f(q[1])).join(' ') }));
  };
  line(pts.slice(0, n - 1), 'ref');
  line(pts.slice(n - 2), 'ref partial');
  const q = pts[n - 2];
  if (q) svg.append(s('circle', { class: 'dot', cx: f(q[0]), cy: f(q[1]), r: 3.5 }));
  return svg;
}

// ---- assessment ----
// seconds between the chain's last block and when we looked (independent of the generator's age)
function chainLag(c) {
  const head = ms(c.headAt);
  return head == null ? null : ((ms(c.queriedAt) ?? ms(data.generatedAt) ?? now()) - head) / 1000;
}
function assess(t) { // → [level, short note]
  const st = str(t.state), c = obj(t.chain), behind = num(t.behind), lv = Object.hasOwn(STATES, st) ? STATES[st] : undefined, bad = [];
  if (c.error && c.head == null) bad.push('chain RPC unreachable');
  else if (chainLag(c) > STALL_CHAIN) bad.push('chain stalled');
  if (st === 'failed') bad.push('sync failed');
  if (st === 'stale') bad.push('sync not running');
  if (bad.length) return ['bad', bad.join(', ')];
  if (!lv) return ['warn', st ? 'unknown state “' + clip(st, 24) + '”' : 'no state reported'];
  if (lv === 'warn') return ['warn', st.replace(/-/g, ' ')];
  if (num(c.runwayDays) != null && c.runwayDays < LOW_RUNWAY) return ['warn', 'writer balance low'];
  if (st === 'starting') return ['info', 'starting'];
  if (behind > 100) return num(t.etaSeconds) == null ? ['warn', 'falling behind'] : ['info', 'catching up'];
  return ['ok', 'in sync'];
}
const targets = () => arr(data && data.targets).filter(t => t && typeof t === 'object');
const seeds = () => arr(data && data.seeds).filter(x => x && typeof x === 'object');
const seedName = x => str(x.label) || str(x.format) || D;
const runLevel = r => (r === 'completed' ? 'ok' : r === 'failed' ? 'bad' : 'info');
function verdict() {
  if (!data) return fail ? ['bad', 'Status unavailable'] : ['info', 'Loading status…'];
  const a = age(data.generatedAt);
  if (!(a <= STALE_DATA)) return ['warn', 'No fresh status for ' + mins(a) + (fail ? ': connection lost' : ': generator down?')];
  const ts = targets(), issues = [], busyT = [], fine = [];
  for (const t of ts) {
    const [lv, note] = assess(t);
    (lv === 'ok' ? fine : lv === 'info' ? busyT : issues).push({ lv, note, t, name: str(t.name) || 'unnamed' });
  }
  const m = obj(data.mirror), q = age(m.newestMatchAt);
  if (m.error) issues.push({ lv: 'warn', name: 'mirror', note: 'database error' });
  else if (q > QUIET_MIRROR) issues.push({ lv: 'warn', name: 'mirror', note: 'no new matches for ' + dur(q) });
  for (const x of seeds()) if (x.schedule && obj(x.latest).result === 'failed') issues.push({ lv: 'warn', name: 'daily seed ' + seedName(x), note: 'failed' });
  if (issues.length) {
    issues.sort((x, y) => (y.lv === 'bad') - (x.lv === 'bad'));
    const list = issues.slice(0, 3).map(i => i.name + ': ' + i.note).join(' · ');
    return [issues[0].lv, issues.length === 1 ? list : issues.length + ' issues · ' + list + (issues.length > 3 ? ' · +' + (issues.length - 3) + ' more' : '')];
  }
  if (!ts.length) return ['info', 'No sync targets configured'];
  if (!busyT.length) return ['ok', ts.length === 1 ? fine[0].name + ' in sync' : (ts.length === 2 ? 'Both' : 'All ' + ts.length) + ' chains in sync'];
  const b = busyT[0], eta = num(b.t.etaSeconds);
  return ['info', (busyT.length > 1 ? busyT.length + ' chains catching up' : b.name + ' is ' + b.note + (b.note === 'catching up' && eta ? ' · ≈ ' + rough(eta) + ' left' : '')) +
    (fine.length ? ' · ' + (fine.length === 1 ? fine[0].name : fine.length + ' chains') + ' in sync' : '')];
}

// ---- sections ----
function mirrorCard(m, after) {
  const rows = arr(m.hourly).map(b => (b = obj(b), { t: ms(b.hour), v: num(b.matches) }));
  const spark = h('div', 'spark'), quiet = age(m.newestMatchAt) > QUIET_MIRROR;
  if (rows.length) after.push(() => spark.append(bars(spark.clientWidth, rows, {
    h: 40, cls: 'b-in', label: 'New Ethereum matches per hour, last 24 hours',
    tip: (r, last) => hourLabel(r.t, last) + '\n' + int(r.v) + ' new Ethereum matches',
  })));
  return h('section', { class: 'card', 'aria-labelledby': 'mh' },
    h('div', 'thead', h('div', null, h('h2', { id: 'mh' }, 'Sourcify mirror'),
      h('p', 'meta', 'Ethereum (chain 1) matches in the local Postgres mirror'))),
    errBox('mirror', 'Mirror database error', m.error, after),
    h('div', 'tiles wide',
      tile(quiet ? 'Mirror quiet' : 'Newest Ethereum match', hash(m.newestMatchId), [
        live(() => (m.newestMatchAt ? 'added ' + ago(m.newestMatchAt) : D)),
        live(() => 'database checked ' + ago(m.queriedAt))], quiet && 'warn'),
      tile('Inflow · last hour', unit(int(m.matchesLastHour), '/h'), [
        int(m.matchesLast24h) + ' in 24\u00a0h · peak ' + int(rows.length ? Math.max(...rows.map(r => r.v || 0)) : null) + '/h', spark])));
}

// the daily seed jobs: where each one's newest seed is (R2 object and pointer) and which format and build it is
function seedCard(list, after) {
  return h('section', { class: 'card', 'aria-labelledby': 'sh' },
    h('div', 'thead', h('div', null, h('h2', { id: 'sh' }, 'Daily seed'),
      h('p', 'meta', 'Prefill seed bundles exported from the mirror and published to R2'))),
    list.length ? list.map(x => seedBlock(x, after)) : h('p', 'small', 'No seed runs yet.'));
}
function seedBlock(x, after) {
  const L = obj(x.latest), P = obj(x.published), at = /^\d\d:\d\d$/.test(str(x.schedule)) ? str(x.schedule) : null;
  const lv = runLevel(L.result), runs = arr(x.recent).slice(0, 7).map(obj), name = seedName(x);
  const code = (v, full) => (str(v) ? h('code', full ? { title: full } : null, str(v)) : D);
  const ver = num(P.seedVersion), rev = str(P.revision), sha = str(P.sha256), owner = str(P.owner);
  const rows = [
    ['Object', code(P.key && (P.bucket ? 's3://' + P.bucket + '/' : '') + P.key)],
    ['Pointer', code(P.pointer)],
    ['Format', [code(P.bundleFormat), ver == null ? '' : ' · seed.json v' + ver, P.schemaPrefix ? [' · entities ', code(P.schemaPrefix + '.*')] : '']],
    ['Arkiv chain', [num(P.chainId) == null ? D : str(P.chainId), owner ? [' · owner ', code(short(owner), owner)] : '']],
    ['Dataset', num(P.contracts) == null ? D : 'up to match ' + hash(P.lastMatchId) + ' · ' + int(P.contracts) + ' contracts · ' + int(P.entities) + ' entities'],
    ['Archive', [compact(P.bytes, 'B'), sha ? [' · SHA-256 ', code(sha.slice(0, 12) + '…', sha)] : '']],
    ['Built', ['processor ', code(rev.slice(0, 7), rev), ' · published ', stamp(P.publishedAt)]],
  ];
  return h('div', { class: 'seed' + (at && L.result === 'failed' ? ' alarm' : ''), 'aria-label': 'Daily seed ' + name },
    h('div', 'shead', h('h3', null, name), L.day || L.result ? h('span', 'pill lv-' + lv, seedDay(L.day) + ' · ' + (str(L.result) || D)) : null),
    h('p', 'small', L.startedAt ? 'last run took ' + dur(L.seconds) + ' · started ' + stamp(L.startedAt) : 'no runs yet',
      at ? [' · ', live(() => nextRun(at))] : ' · no timer on this host' + (P.day ? ': the seed below stays as published' : '')),
    errBox('seed:' + str(x.format), 'Seed files unreadable', x.error, after),
    P.day ? h('dl', 'seedkv', rows.map(([k, v]) => [h('dt', null, k), h('dd', null, v)])) : h('p', 'small', 'Nothing published yet.'),
    runs.length ? h('div', { class: 'runs', 'aria-label': 'Recent ' + name + ' seed runs, newest first' }, runs.map(r => h('span', {
      class: 'run lv-' + runLevel(r.result),
      title: seedDay(r.day) + ' · ' + (str(r.result) || D) + ' · started ' + stamp(r.startedAt) + ' · took ' + dur(r.seconds),
    }, GLYPH[runLevel(r.result)] + ' ' + seedDay(r.day)))) : null);
}

// how far behind, ETA, backlog meter and net catch-up rate
function hero(t, m) {
  const behind = num(t.behind), eta = num(t.etaSeconds), r = obj(t.rates), tot = obj(t.totals), done = num(tot.matches);
  const rate = num(r.matchesLastHour) ?? num(r.contractsLastHour), inflow = num(m.matchesLastHour);
  const [main, sub, cls] = behind === 0 ? ['✓ In sync', 'cursor at the newest match', 'ok-t']
    : !(behind > 0) ? [D] : eta == null ? ['No ETA', 'not gaining on the mirror inflow', 'warn-t']
    : ['≈ ' + rough(eta) + ' to go', eta < 3e7 && when((ms(data.generatedAt) ?? now()) + eta * 1e3)];
  const pct = behind != null && done != null && done + behind > 0 ? done / (done + behind) : null, p = Math.floor(pct * 100);
  const fill = h('span');
  fill.style.width = pct == null ? '0' : Math.max(1, pct * 100).toFixed(1) + '%';
  const net = rate - inflow, flow = rate == null || inflow == null ? '' : (behind > 0 ? (net < 0 ? '−' : '+') + int(Math.abs(net)) +
    '/h net · ' : '') + int(rate) + '/h synced vs ' + int(inflow) + '/h new';
  return h('div', 'hero',
    h('div', 'behind', h('span', 'big', int(behind)), h('span', 'unit', plural(behind, 'contract').replace(/^\S+ /, '') + ' behind')),
    h('div', 'eta', h('strong', cls || '', main), sub ? h('span', null, sub) : null),
    h('div', { class: behind === 0 ? 'meter done' : 'meter', role: 'img', 'aria-label': pct == null ? 'Backlog progress unknown' : p + ' % of the backlog synced' }, fill),
    h('div', 'hfoot', h('span', null, pct == null ? '' : (behind === 0 ? int(done) + ' matches' : (pct < 0.01 ? '< 1' : p) + ' % of backlog · ' + int(done)) +
      ' synced since ' + stamp(tot.since)), h('span', null, flow)));
}

function chartFig(t, m, after) {
  const inflow = new Map(arr(m.hourly).map(b => [ms(obj(b).hour), num(obj(b).matches)]));
  const rows = arr(t.hourly).map(b => (b = obj(b), { t: ms(b.hour), v: num(b.contracts), ref: inflow.get(ms(b.hour)) ?? null, tx: b.transactions, bytes: b.calldataBytes }));
  const plot = h('div', 'plot', rows.length ? null : h('p', 'none', 'No hourly data yet.'));
  if (rows.length) after.push(() => plot.append(bars(plot.clientWidth, rows, {
    h: 128, axis: true, cls: 'b-sync', label: 'Contracts synced per hour (bars) and new mirror matches per hour (line), last 24 hours',
    tip: (r, last) => hourLabel(r.t, last) + '\n' + int(r.v) + ' contracts synced · ' + int(r.tx) + ' tx · ' + compact(r.bytes, 'B') + '\n' + int(r.ref) + ' new in the mirror',
  })));
  return h('figure', 'chart', h('figcaption', null, h('span', null, 'Contracts per hour · last 24\u00a0h (UTC)'),
    h('span', 'legend', h('span', 'k k-sync', 'synced'), h('span', 'k k-in', 'new in mirror'))), plot);
}

// totals since the sync's first batch, last batch, writer nonce: collapsed by default
function totals(t, key) {
  const x = obj(t.totals), lb = obj(t.lastBatch), c = obj(t.chain), pend = c.pendingNonce - c.nonce;
  const cr = num(x.entitiesCreated), ru = num(x.entitiesReused), all = cr + ru;
  const kv = (k, v, sub) => h('div', null, h('dt', null, k), h('dd', null, v, sub && h('small', null, sub)));
  const d = h('details', 'totals',
    h('summary', null, x.since ? 'Totals since ' + stamp(x.since) : 'Totals', num(x.contracts) != null && h('span', 'muted', ' · ' + int(x.contracts) + ' contracts')),
    h('dl', 'kv',
      kv('Contracts', int(x.contracts), int(x.matches) + ' matches'),
      kv('Already on chain', int(x.alreadySynced), int(x.skipped) + ' skipped'),
      kv('Entity reuse', all > 0 ? Math.round(100 * ru / all) + ' %' : D, compact(cr) + ' created · ' + compact(ru) + ' reused'),
      kv('Transactions', int(x.transactions), int(x.batches) + ' batches'),
      kv('Gas used', compact(x.gasUsed, 'gas')),
      kv('Calldata', compact(x.calldataBytes, 'B'))),
    h('p', 'small', !lb.at ? 'No batches yet.' : 'Last batch ' + stamp(lb.at) + ': ' + plural(lb.contracts, 'contract') + ' · ' + int(lb.transactions) +
      ' tx · ' + compact(lb.calldataBytes, 'B') + ' · ' + unit(fix(lb.seconds, 1), NB + 's') + ' · ' + int(lb.created) + ' entities created, ' + int(lb.reused) + ' reused'),
    h('p', 'small', 'Status reported ', live(() => ago(t.statusAt)), ' · failures ' + int(t.failures) + ' · writer nonce ' + int(c.nonce) +
      (pend > 0 ? ' (' + int(pend) + ' pending)' : '') + (num(c.gasPriceGwei) != null ? ' · gas price ' + +(+c.gasPriceGwei).toPrecision(3) + ' gwei' : '')));
  d.open = kept.has(key);
  d.addEventListener('toggle', () => { d.open ? kept.add(key) : kept.delete(key); });
  return d;
}

function targetCard(t, m, after) {
  const name = str(t.name) || 'unnamed', id = 'tn-' + name.replace(/\W/g, '_'), st = str(t.state) || 'unknown';
  const c = obj(t.chain), r = obj(t.rates), cur = obj(t.cursor), set = obj(t.settings), spc = num(r.secondsPerContract);
  const down = !!c.error && c.head == null, stalled = chainLag(c) > STALL_CHAIN, run = num(c.runwayDays), low = run < LOW_RUNWAY && run != null;
  const url = /^https?:\/\/[^\s"<>]+\/v2\/contract\//i.test(str(cur.contractUrl)) ? cur.contractUrl : null;
  const fails = num(t.failures), sec = v => unit(int(v), NB + 's');
  return h('article', { class: 'card target lv-' + assess(t)[0], 'aria-labelledby': id },
    h('div', 'thead',
      h('div', null, h('h2', { id }, name), h('p', 'meta', 'chain ' + (str(t.chainId) || D) + ' · ' + (str(t.rpcHost) || D))),
      h('span', 'pill st-' + (Object.hasOwn(STATES, st) ? st : 'unknown'), clip(st.replace(/-/g, ' '), 24))),
    st === 'stale' && h('div', 'err note', h('strong', null, 'Sync process not responding'),
      h('p', null, 'Its lock file is no longer refreshed. Last status ', live(() => ago(t.statusAt)), '.')),
    errBox(name + ':sync', 'Sync error' + (fails > 0 ? ' · ' + plural(fails, 'failure') : ''), t.error, after),
    errBox(name + ':rpc', 'Chain RPC error', c.error, after),
    hero(t, m),
    chartFig(t, m, after),
    h('div', 'tiles',
      tile(stalled ? 'Chain stalled' : down ? 'Chain unreachable' : 'Last block',
        [h('span', { class: 'beat', 'aria-hidden': 'true' }), live(() => ago(c.headAt))],
        ['head ' + int(c.head) + (num(c.blockSeconds) == null ? '' : ' · ' + fix(c.blockSeconds, 1) + NB + 's/block')],
        stalled || down ? 'alarm' : ms(c.headAt) == null && 'nodata'),
      tile('Synced last hour', unit(int(r.contractsLastHour), '/h'),
        [(spc == null ? D : fix(spc, spc < 10 ? 1 : 0)) + NB + 's per contract', int(r.contractsLast24h) + ' in the last 24\u00a0h']),
      tile(low ? 'Writer balance low' : 'Writer balance', eth(c.balanceEth), [
        [h('span', { class: 'mono', title: str(t.writer) || null }, short(t.writer)), ' · runway' + NB + runway(run)],
        'spent ' + eth(c.spentEthLast24h) + ' in 24\u00a0h',
        c.pendingNonce > c.nonce ? int(c.pendingNonce - c.nonce) + ' tx pending' : null,
      ], low && 'warn'),
      tile('Last synced', url ? h('a', { class: 'mono', href: url, rel: 'noopener', target: '_blank', title: str(cur.address) }, short(cur.address))
        : h('span', 'mono', short(cur.address)), [['match ' + hash(cur.matchId) + ' · ', live(() => ago(cur.at))]])),
    totals(t, name),
    h('p', 'settings', 'Settings: batch ' + int(set.batch) + ' · in flight ' + int(set.inFlight) + ' · max tx ' + compact(set.maxTxBytes, 'B') +
      ' · pause ' + sec(set.batchDelaySeconds) + ' · poll ' + sec(set.pollSeconds)));
}

// ---- render, tick, poll ----
const empty = (title, text) => h('section', 'card empty', h('h2', null, title), h('p', null, text));
const refresh = () => Math.min(300, Math.max(5, num(data && data.refreshSeconds) || 15));
function render() {
  hooks = [];
  const app = $('app'), after = [], m = obj(data.mirror), ts = targets();
  app.replaceChildren(mirrorCard(m, after), seedCard(seeds(), after), ts.length
    ? h('section', { class: 'targets', 'aria-label': 'Sync targets' }, ts.map(t => targetCard(t, m, after)))
    : empty('No sync targets', 'No Arkiv chain is configured for syncing yet.'));
  width = app.clientWidth;
  after.forEach(f => f()); // charts and error toggles need the laid-out width
  $('meta').textContent = 'revision ' + (str(data.revision).slice(0, 7) || D) + ' · generated ' + stamp(data.generatedAt) + ' · refresh every ' + refresh() + ' s';
}

function tick() {
  hooks.forEach(f => f());
  const a = data ? age(data.generatedAt) : null, fresh = a <= STALE_DATA, [lv, txt] = verdict();
  $('tick').textContent = data ? 'updated ' + ago(data.generatedAt) : fail ? 'no data' : 'connecting…';
  $('tdot').className = 'tdot lv-' + (!data ? 'bad' : fail || !fresh ? 'warn' : 'ok');
  document.body.classList.toggle('stale-data', !!data && !fresh);
  $('verdict').className = 'verdict lv-' + lv;
  $('vi').textContent = GLYPH[lv];
  if ($('vtext').textContent !== txt) $('vtext').textContent = txt;
  const b = !data ? '' : fail ? 'Connection lost (' + clip(fail, 60) + ') · showing data from ' + mins(a) + ' ago · retrying every ' + refresh() + ' s'
    : !fresh ? 'Status not refreshed for ' + mins(a) + ': the status generator may be down. Showing the last data.'
    : num(data.schema) !== 2 ? 'Unexpected status schema “' + clip(data.schema, 12) + '” (this page expects 2): some values may be missing.' : '';
  const el = $('banner');
  if (el.textContent !== b) el.textContent = b;
  el.hidden = !b;
}

async function poll() {
  if (busy) return;
  busy = true;
  clearTimeout(timer);
  try {
    const res = await fetch('status.json', { cache: 'no-store' });
    if (!res.ok) throw new Error('HTTP ' + res.status);
    const d = await res.json();
    if (!d || typeof d !== 'object' || Array.isArray(d)) throw new Error('unexpected JSON');
    const srv = Date.parse(res.headers.get('Date') || ''); // correct a badly set client clock
    skew = Math.abs(srv - Date.now()) > 5000 ? srv - Date.now() : 0;
    data = d;
    fail = null;
  } catch (e) {
    fail = str((e && e.message) || 'fetch failed');
  }
  try {
    if (!data) $('app').replaceChildren(empty('Status unavailable', 'Could not load status.json (' + clip(fail, 80) + '). Retrying every ' + refresh() + ' s.'));
    else if (!fail) render(); // on failure keep showing the last good data
  } catch (e) { console.error('render failed', e); }
  tick();
  busy = false;
  timer = setTimeout(poll, refresh() * 1000);
}

let rz = 0;
addEventListener('resize', () => {
  clearTimeout(rz);
  rz = setTimeout(() => { if (data && $('app').clientWidth !== width) render(); tick(); }, 150);
});
document.addEventListener('visibilitychange', () => { if (!document.hidden) poll(); });
setInterval(tick, 1000);
poll();
