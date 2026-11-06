// Collect parity: Node's machine.mjs collect and `fleetd snapshot --json` over the same made-up home folders.
// Both sides are normalized to { profile, sessionId, engine, state, live, stage, model, contextTokens, lastAt, said, doing }
// and diffed. The only differences allowed are the ruled ones in EXPECTED_DIVERGENCE, each matched on the INPUT condition
// of the case (its roster row, pid, file size), never on the answer. Anything else prints and exits 1.
// Run: node native/parity/collect-parity.mjs   (needs native/target/release/fleetd: cargo build --release -p fleetd)
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..');
const MACHINE = path.join(ROOT, 'fleet', 'machine.mjs');
const FLEETD = process.env.FLEETD_BIN ?? path.join(ROOT, 'native', 'target', 'release', 'fleetd');
const TRANSCRIPTS = path.join(HERE, 'fixtures', 'transcripts');
const CODEX = path.join(HERE, 'fixtures', 'codex');
const WINDOW = 384 * 1024;

// ---------------------------------------------------------------- the ruled differences

/** Each entry: the ruling, the input condition (a case's `input`, never an output), the fields it may move, and what the
 *  two sides may say in them. Anything not matched here is a real mismatch. */
const LIVE_STATES = ['working', 'needs-you', 'waiting', 'capped'];
const EXPECTED_DIVERGENCE = [
  { id: 'R7', note: 'a pid that is set but dead (and not active) is not live in fleetd',
    when: (i) => i.engine === 'claude' && i.pid != null && !i.pidAlive && !i.active,
    fields: { state: (n, f) => LIVE_STATES.includes(n) && ['ended', 'done'].includes(f), live: (n, f) => n === true && f === false } },
  { id: 'R8', note: 'stopped/failed beside a live pid: fleetd is open (respawned; reason says so), Node closed',
    when: (i) => i.engine === 'claude' && ['stopped', 'failed'].includes(i.rosterState) && i.pid != null && i.pidAlive,
    fields: { state: (n, f) => n === 'closed' && LIVE_STATES.includes(f), live: (n, f) => n === false && f === true } },
  // The line that carries the value lies more than 384 KiB from the end of the file (the fixture says so: `marker.fromEnd`),
  // so Node's window lost it; fleetd read it while it followed the file, and its value must be the marker's own.
  { id: 'R9', note: 'the value is further than 384 KiB from the end of the file: Node lost it, fleetd keeps what it read',
    when: (i) => Boolean(i.marker) && i.marker.fromEnd > WINDOW,
    fields: Object.fromEntries(['model', 'contextTokens', 'said', 'doing', 'stage'].map((k) => [k, (n, f, i) => n == null && f === i.marker[k]])) },
  { id: 'Task 6', note: 'an empty-string model or doing is null in fleetd',
    when: (i) => i.emptyStrings === true,
    fields: { doing: (n, f) => n === '' && f == null, model: (n, f) => n === '' && f == null } },
];

// ---------------------------------------------------------------- building homes

const iso = (i) => new Date(Date.now() - 3_600_000 + i * 1000).toISOString();
const ask = (i, text) => ({ isSidechain: false, timestamp: iso(i), sessionId: 'x', type: 'user', message: { role: 'user', content: text } });
const reply = (i, o) => ({ isSidechain: false, timestamp: iso(i), sessionId: 'x', type: 'assistant', ...o.top,
  message: { role: 'assistant', model: o.model, stop_reason: 'tool_use', content: o.content, usage: o.usage } });
const filler = (i) => ({ isSidechain: false, timestamp: iso(i), sessionId: 'x', type: 'user',
  message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_f', content: 'x'.repeat(1000) }] } });
const MARKER = { model: 'marker-model-1', contextTokens: 4242, said: 'marker said text', doing: 'Read marker.ts', stage: 'planning' };
/** A model and a tool with empty-string names: Node says '', fleetd null (Task 6). */
const emptyStringsLines = () => [ask(0, 'Tidy the notes'), reply(1, { top: { requestedModel: '' }, model: '',
  content: [{ type: 'tool_use', id: 'toolu_e', name: '', input: {} }], usage: { input_tokens: 10, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 } })];
/** The value-carrying reply first, then tool results after it. */
const markerLines = (fillers) => [ask(0, 'Mark the far line'), reply(1, { top: { requestedModel: MARKER.model }, model: MARKER.model,
  content: [{ type: 'text', text: MARKER.said }, { type: 'tool_use', id: 'toolu_m', name: 'Read', input: { file_path: '/nonexistent-fleet/marker.ts' } }],
  usage: { input_tokens: 4000, cache_creation_input_tokens: 200, cache_read_input_tokens: 42 } }),
  ...Array.from({ length: fillers }, (_, k) => filler(2 + k))];
const FAR = 420; // over 400 KiB of tool results

// `job_id` (proto 3) and `title`, `epic`, `archived`, `account` (proto 4) are Node-less here (Node has them in fleet.mjs, not machine.mjs): not compared.
const FIELDS = ['profile', 'sessionId', 'engine', 'state', 'live', 'stage', 'model', 'contextTokens', 'lastAt', 'said', 'doing'];
const sid = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const jobId = (n) => n.toString(16).padStart(8, '0');

/** A pid that certainly is not alive: a child that has exited and been reaped. */
function deadPid() {
  const r = execFileSync(process.execPath, ['-e', 'process.stdout.write(String(process.pid))']);
  return Number(String(r));
}

class Home {
  constructor(name, { min = 0, max = Infinity } = {}) {
    this.name = name;
    this.min = min;
    this.max = max;
    this.dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), `collect-parity-${name}-`))); // real path: lsof names files by it
    this.cases = new Map(); // `${engine}/${profile}/${sessionId}` -> input condition
    this.kids = [];
    this.growth = null; // text appended to a transcript while fleetd runs
    this.ready = []; // promises: each held-open codex has its file open
    this.profiles = new Map();
    fs.mkdirSync(path.join(this.dir, '.local', 'bin'), { recursive: true });
  }
  withProfile(id) { this.profile(id); return this; }
  profile(id, { plain = false } = {}) {
    const dir = path.join(this.dir, plain ? '.claude' : `.claude-${id}`);
    fs.mkdirSync(path.join(dir, 'fleet-status'), { recursive: true });
    fs.mkdirSync(path.join(dir, 'fleet-state'), { recursive: true });
    fs.mkdirSync(path.join(dir, 'projects'), { recursive: true });
    fs.writeFileSync(path.join(dir, 'settings.json'), '{}');
    const p = { id: plain ? 'default' : id, dir, rows: [], active: [] };
    this.profiles.set(p.id, p);
    return p;
  }
  /** A Claude session: a roster row, a copy of a transcript fixture, and optionally a hook file, a status line, a job record. */
  claude(p, n, { row = {}, transcript = null, lines = null, hook = null, status = null, active = false, job = null, mtimeAgoMs = 3_600_000, tags = [], emptyStrings = false, grow = 0 } = {}) {
    const cwd = path.join(this.dir, `repo-${n}`);
    const r = { id: jobId(n), sessionId: sid(n), name: `case-${n}`, kind: 'interactive', cwd, startedAt: Date.now() - 600_000, ...row };
    p.rows.push(r);
    if (active) p.active.push({ id: r.id, sessionId: r.sessionId });
    let fileBytes = 0, marker = null;
    if (transcript || lines) {
      const dest = path.join(p.dir, 'projects', cwd.replace(/[^A-Za-z0-9]/g, '-'));
      fs.mkdirSync(dest, { recursive: true });
      const f = path.join(dest, `${r.sessionId}.jsonl`);
      if (lines) {
        const text = lines.map((l) => JSON.stringify(l) + '\n');
        fs.writeFileSync(f, text.join(''));
        // R9's fixture: the marker reply is followed by `grow` more tool results that are appended while fleetd runs, and the
        // proof that Node's window loses it is the distance from the marker line's start to the end of the file.
        if (grow) {
          const more = Array.from({ length: grow }, (_, k) => filler(100 + k));
          const add = more.map((l) => JSON.stringify(l) + '\n').join('');
          marker = { ...MARKER, fromEnd: text.slice(1).join('').length + add.length };
          this.growth = { file: f, text: add, lastAt: Date.parse(more.at(-1).timestamp) };
        }
      } else fs.copyFileSync(path.join(TRANSCRIPTS, transcript), f);
      const t = new Date(Date.now() - mtimeAgoMs);
      fs.utimesSync(f, t, t);
      fileBytes = fs.statSync(f).size;
    }
    if (hook) fs.writeFileSync(path.join(p.dir, 'fleet-state', `${r.sessionId}.json`), JSON.stringify({ at: Date.now() - (hook.agoMs ?? 0), ...hook, agoMs: undefined }));
    if (status) fs.writeFileSync(path.join(p.dir, 'fleet-status', `${r.sessionId}.json`), JSON.stringify({ at: Date.now() + 86_400_000, session_id: r.sessionId, model: { id: 'claude-fixture-1' },
      context_window: { context_window_size: 1_000_000, current_usage: status } }));
    if (job) {
      const d = path.join(p.dir, 'jobs', r.id);
      fs.mkdirSync(d, { recursive: true });
      fs.writeFileSync(path.join(d, 'state.json'), JSON.stringify({ updatedAt: new Date(Date.now() - job.agoMs).toISOString() }));
    }
    const pid = r.pid ?? null;
    this.cases.set(`claude/${p.id}/${r.sessionId}`, { engine: 'claude', rosterState: r.state ?? null, pid, pidAlive: pid != null && pid === process.pid, active, fileBytes, emptyStrings, marker, tags });
    return r;
  }
  /** The Codex home: every rollout fixture copied under today's folder, with the times given by name (default: written an hour ago). */
  codex({ recent = [], stale = [], held = [] } = {}) {
    const c = path.join(this.dir, '.codex');
    fs.mkdirSync(c, { recursive: true });
    fs.copyFileSync(path.join(CODEX, 'auth.json'), path.join(c, 'auth.json'));
    const d = new Date();
    const day = path.join(c, 'sessions', String(d.getFullYear()), String(d.getMonth() + 1).padStart(2, '0'), String(d.getDate()).padStart(2, '0'));
    fs.mkdirSync(day, { recursive: true });
    const agoOf = (n) => (recent.some((k) => n.includes(k)) ? 30_000 : stale.some((k) => n.includes(k)) ? 4 * 86_400_000 : 3_600_000);
    const files = {};
    for (const dd of fs.readdirSync(path.join(CODEX, 'sessions', '2026', '11'))) {
      for (const n of fs.readdirSync(path.join(CODEX, 'sessions', '2026', '11', dd))) {
        const f = path.join(day, n);
        fs.copyFileSync(path.join(CODEX, 'sessions', '2026', '11', dd, n), f);
        const t = new Date(Date.now() - agoOf(n));
        fs.utimesSync(f, t, t);
        files[n] = f;
        // The thread id is the file's own session_meta id (or the file name without one); its input condition is the size.
        let id = path.basename(n, '.jsonl');
        try { id = JSON.parse(fs.readFileSync(f, 'utf8').split('\n')[0]).payload?.id ?? id; } catch {}
        this.cases.set(`codex/codex/${id}`, { engine: 'codex', fileBytes: fs.statSync(f).size, file: n,
          tags: [...(n.includes('-working') ? ['codex-live'] : []), ...(n.includes('-old') ? ['codex-closed'] : [])] });
      }
    }
    // A running `codex` that holds a rollout open: node under the name `codex` (a hard link, else a copy), since fleetd's macOS
    // scan names a process by its program file, as Node's does by argv[0].
    for (const k of held) {
      const f = Object.entries(files).find(([n]) => n.includes(k))[1];
      const exe = path.join(this.dir, '.codex-root', 'bin', 'codex');
      fs.mkdirSync(path.dirname(exe), { recursive: true });
      try { fs.symlinkSync(path.join(path.dirname(process.execPath), '..', 'lib'), path.join(this.dir, '.codex-root', 'lib')); } catch {} // where node finds libnode
      if (!fs.existsSync(exe)) { try { fs.linkSync(process.execPath, exe); } catch { fs.copyFileSync(process.execPath, exe); fs.chmodSync(exe, 0o755); } }
      const kid = spawn(exe, ['-e', `require('node:fs').openSync(${JSON.stringify(f)}, 'r'); console.log('open'); setTimeout(() => {}, 120000)`], { stdio: ['ignore', 'pipe', 'pipe'] });
      this.kids.push(kid);
      this.ready.push(new Promise((resolve, reject) => {
        let err = '';
        kid.once('error', (e) => reject(new Error(`the held-open codex did not start: ${e.message}`)));
        kid.once('exit', (code) => reject(new Error(`the held-open codex exited (${code}): ${err.slice(0, 300)}`)));
        kid.stderr.on('data', (d) => { err += d; });
        kid.stdout.once('data', () => resolve());
      }));
    }
    return files;
  }
  /** The stand-in `claude`: `agents --json --all` is every row, `agents --json` what runs, `logs` says the job is there. */
  finish() {
    for (const p of this.profiles.values()) {
      fs.writeFileSync(path.join(p.dir, '.parity-roster.json'), JSON.stringify(p.rows));
      fs.writeFileSync(path.join(p.dir, '.parity-active.json'), JSON.stringify(p.active));
    }
    const script = `#!/bin/sh
D="\${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
case "$1" in
  agents) case "$*" in *--all*) cat "$D/.parity-roster.json";; *) cat "$D/.parity-active.json";; esac;;
  auth) echo '{"loggedIn":true,"email":"me@example.test"}';;
  logs) echo "log of $2";;
  *) exit 1;;
esac
`;
    fs.writeFileSync(path.join(this.dir, '.local', 'bin', 'claude'), script, { mode: 0o755 });
  }
  /** Waits until every held-open codex has its file open; fails if one does not start within 10 s. */
  async settle() {
    const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error('a held-open codex did not open its file in 10 s')), 10_000).unref());
    await Promise.race([Promise.all(this.ready), timeout]);
  }
  cleanup() {
    for (const k of this.kids) try { k.kill('SIGKILL'); } catch {}
    try { fs.rmSync(this.dir, { recursive: true, force: true }); } catch {}
  }
}

// ---------------------------------------------------------------- the homes

const hookAt = (e, extra = {}, agoMs = 0) => ({ e, agoMs, ...extra });
const useful = { input_tokens: 1000, cache_creation_input_tokens: 200, cache_read_input_tokens: 300 };

function homes() {
  const out = [];
  const live = process.pid; // a pid that is alive: this process
  const dead = deadPid();

  // 1. Every Claude state, in one profile.
  {
    const h = new Home('states', { min: 18 }), p = h.profile('work');
    h.claude(p, 1, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'running.jsonl', status: useful, tags: ['working', 'status-context'] });
    h.claude(p, 2, { row: { pid: live, state: 'blocked', status: 'waiting', waitingFor: 'permission prompt' }, transcript: 'turn-over.jsonl', tags: ['roster-ask'] });
    h.claude(p, 3, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl', hook: hookAt('PermissionRequest', { t: 'Bash' }), tags: ['hook-ask'] });
    h.claude(p, 4, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl', tags: ['waiting'] });
    h.claude(p, 5, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'limit.jsonl', tags: ['capped'] });
    h.claude(p, 6, { row: { state: 'done' }, transcript: 'turn-over.jsonl', tags: ['done'] });
    h.claude(p, 7, { row: { state: 'stopped' }, transcript: 'workdir-a.jsonl', tags: ['ended'] });
    h.claude(p, 8, { row: { state: 'failed' } });
    h.claude(p, 9, { row: { pid: live, state: 'stopped', status: 'idle' }, transcript: 'turn-over.jsonl' }); // respawned (R8)
    h.claude(p, 10, { row: { pid: dead, state: 'working', status: 'busy' }, transcript: 'running.jsonl' }); // dead pid (R7)
    h.claude(p, 11, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl', hook: hookAt('UserPromptSubmit', {}, 300_000) }); // hook working, idle a long while
    h.claude(p, 12, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'workdir-b.jsonl', hook: hookAt('PostToolUse', { t: 'Edit' }) });
    h.claude(p, 13, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl', hook: hookAt('SessionEnd', { r: 'other' }) });
    h.claude(p, 14, { row: { state: 'blocked', status: 'idle' }, active: true, job: { agoMs: 60_000 } }); // a job at its startup question
    h.claude(p, 15, { row: { state: 'running', status: 'busy' }, active: true, job: { agoMs: 3_600_000 } }); // listed as running, record long cold
    h.claude(p, 16, { row: { pid: live, state: 'working' }, transcript: 'break-ask.jsonl' }); // no status: the state word decides
    h.claude(p, 17, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'empty-ask.jsonl' });
    h.claude(p, 18, { row: { pid: live, state: 'working', status: 'busy' }, lines: emptyStringsLines(), emptyStrings: true }); // Task 6
    out.push(h);
  }
  // 2. Transcripts at their limits: the head cut, the big window, the early break, a value beyond the window.
  {
    const h = new Home('windows', { min: 7 }), p = h.profile('big');
    h.claude(p, 1, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'big.jsonl' });
    h.claude(p, 2, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'head-cut.jsonl' });
    h.claude(p, 3, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'break-uses.jsonl', status: { input_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 } });
    h.claude(p, 6, { row: { pid: live, state: 'working', status: 'busy' }, lines: markerLines(FAR) }); // over the window from the start: both lose it
    // R9: the marker is read while the file is small, then 400 KiB is appended; fleetd keeps it, Node's window loses it.
    h.claude(p, 7, { row: { pid: live, state: 'working', status: 'busy' }, lines: markerLines(3), grow: FAR });
    // The default profile and a third one: profiles are discovered, not configured.
    const d = h.profile('default', { plain: true });
    h.claude(d, 4, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'running.jsonl' });
    const e = h.profile('enterprise');
    h.claude(e, 5, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl' });
    out.push(h);
  }
  // 3. Codex: every rollout fixture; some written just now (open by the ten minutes), one held open by a process, some cold.
  {
    const h = new Home('codex', { min: 15 });
    h.codex({ recent: ['working', 'finished', 'asking.', 'aborted', 'answered', 'exec', 'big'], stale: ['asking-stale', 'old'], held: ['hidden-done'] });
    out.push(h);
  }
  // 4. Nothing at all: a profile that has no sessions yields none on either side.
  out.push(new Home('empty', { min: 0, max: 0 }).withProfile('idle'));
  return out;
}

// ---------------------------------------------------------------- running both sides

const SPAWN = { maxBuffer: 64 << 20, timeout: 60_000, encoding: 'utf8' };
/** Runs a program; on a failure (or output that is not JSON) throws with the program's own stderr. */
function runJson(label, cmd, args, opts) {
  const r = spawnSync(cmd, args, { ...SPAWN, ...opts });
  if (r.error || r.status !== 0) throw new Error(`${label} failed (${r.error?.message ?? `exit ${r.status}`}):\n${r.stderr}`);
  try { return JSON.parse(r.stdout.trim().split('\n').at(-1)); } catch (e) { throw new Error(`${label} printed no JSON (${e.message}):\n${r.stdout.slice(0, 400)}\n${r.stderr}`); }
}

/** fleetd as the watcher runs it: `serve` reads the home, the growth is appended, and the first snapshot a client gets afterwards is
 *  the board once the daemon has followed the file to its new end. Polls (up to 15 s); the daemon is stopped at the end. */
async function fleetdGrown(h) {
  const sockDir = fs.mkdtempSync('/tmp/fp-'); // a socket path must stay under 104 bytes
  const env = { HOME: h.dir, FLEETD_HOME: h.dir, FLEETD_MACHINE: 'parity', FLEETD_SOCK: path.join(sockDir, 's'), PATH: '/usr/bin:/bin:/usr/sbin:/sbin' };
  const srv = spawn(FLEETD, ['serve'], { env, stdio: ['ignore', 'ignore', 'pipe'] });
  let err = '';
  srv.stderr.on('data', (d) => { err += d; });
  const watchOnce = () => {
    const r = spawnSync(FLEETD, ['watch'], { env, encoding: 'utf8', timeout: 1500, killSignal: 'SIGKILL' });
    const line = String(r.stdout ?? '').split('\n')[0];
    try { return JSON.parse(line); } catch { return null; }
  };
  const until = async (ok) => {
    for (let i = 0; i < 150; i++) {
      const snap = watchOnce();
      if (snap && ok(snap)) return snap;
      await new Promise((r) => setTimeout(r, 100));
    }
    throw new Error(`fleetd serve never reached the expected board:\n${err}`);
  };
  try {
    await until((snap) => snap.sessions.some((s) => s.model === MARKER.model));
    fs.appendFileSync(h.growth.file, h.growth.text);
    return await until((snap) => snap.sessions.some((s) => s.last_at === h.growth.lastAt));
  } finally { srv.kill('SIGKILL'); fs.rmSync(sockDir, { recursive: true, force: true }); }
}

const nodeCollect = (h) => runJson('node collect', process.execPath, ['--input-type=module', '-'],
  { input: fs.readFileSync(MACHINE), env: { HOME: h.dir, PATH: '/usr/bin:/bin:/usr/sbin:/sbin', FLEET_DO: 'collect' } });
const fleetdSnapshot = (h) => runJson('fleetd snapshot', FLEETD, ['snapshot', '--json'],
  { env: { HOME: h.dir, FLEETD_HOME: h.dir, FLEETD_MACHINE: 'parity', PATH: '/usr/bin:/bin:/usr/sbin:/sbin' } });

const NODE_STATE = (s) => (s.state === 'ready' ? 'waiting' : s.state);
const nz = (v) => (v === undefined ? null : v);

/** The sessions of a side by key, and how many it listed (a duplicate key or an unknown engine shows as a difference). */
function normalizeNode(r) {
  const out = new Map();
  let raw = 0;
  for (const a of r.accounts) {
    if (a.engine === 'muse') continue;
    for (const s of a.sessions ?? []) {
      raw++;
      if (a.engine !== 'claude' && a.engine !== 'codex') continue;
      // A Codex rollout with no session_meta has no sessionId: Node's `id` (the file's name) is its key, as fleetd's.
      const sessionId = s.sessionId ?? s.id;
      out.set(`${a.engine}/${a.id}/${sessionId}`, { profile: a.id, sessionId, engine: a.engine, state: NODE_STATE(s), live: s.live, stage: nz(s.stage),
        model: nz(s.model), contextTokens: nz(s.context?.tokens), lastAt: nz(s.lastAt), said: nz(s.said), doing: nz(s.doing) });
    }
  }
  return { map: out, raw };
}

function normalizeFleetd(r) {
  const out = new Map();
  let raw = 0;
  for (const s of r.sessions) {
    if (s.engine === 'muse') continue;
    raw++;
    if (s.engine !== 'claude' && s.engine !== 'codex') continue;
    out.set(`${s.engine}/${s.key.profile}/${s.key.session_id}`, { profile: s.key.profile, sessionId: s.key.session_id, engine: s.engine, state: s.state, live: s.live,
      stage: nz(s.stage), model: nz(s.model), contextTokens: nz(s.context_tokens), lastAt: nz(s.last_at), said: nz(s.said), doing: nz(s.doing), reason: s.reason });
  }
  return { map: out, raw };
}

/** Whether the two states are the same session state under the name mapping (Node's closed is fleetd's ended or done). */
const sameState = (n, f) => n === f || (n === 'closed' && (f === 'ended' || f === 'done'));

function diffSession(node, fleet, input) {
  const unexpected = [], expected = [];
  for (const k of FIELDS) {
    const n = node[k], f = fleet[k];
    if (k === 'state' ? sameState(n, f) : JSON.stringify(n) === JSON.stringify(f)) continue;
    const rule = EXPECTED_DIVERGENCE.find((d) => input && d.when(input) && d.fields[k]?.(n, f, input));
    if (rule) expected.push({ k, rule: rule.id });
    else unexpected.push({ k, node: n, fleetd: f });
  }
  // Ruling R8: the respawned row's reason says so.
  if (input && EXPECTED_DIVERGENCE[1].when(input) && !String(fleet.reason).startsWith('respawned; ')) unexpected.push({ k: 'reason', node: '(any)', fleetd: fleet.reason });
  return { unexpected, expected };
}

// ---------------------------------------------------------------- coverage: the fixtures reached the states asked for

/** Each must be met at least once on each side, separately: a gate that compared two sides that both saw nothing proves nothing. */
const COVERAGE = [
  ['needs-you via roster waitingFor', 'roster-ask', (s) => s.state === 'needs-you'],
  ['needs-you via hook PermissionRequest', 'hook-ask', (s) => s.state === 'needs-you'],
  ['working', 'working', (s) => s.state === 'working'],
  ['waiting (Node ready)', 'waiting', (s) => s.state === 'waiting'],
  ['capped', 'capped', (s) => s.state === 'capped'],
  ['closed (Node closed, fleetd ended)', 'ended', (s, side) => (side === 'node' ? s.state === 'closed' : s.state === 'ended') && !s.live],
  ['done (fleetd done, Node closed)', 'done', (s, side) => (side === 'node' ? s.state === 'closed' : s.state === 'done') && !s.live],
  ['a live codex', 'codex-live', (s) => s.live === true],
  ['a closed codex', 'codex-closed', (s) => s.live === false],
  ['context tokens from a status-line snapshot', 'status-context', (s) => s.contextTokens === 1500],
];

function coverage(side, seen) {
  const miss = [];
  for (const [name, tag, ok] of COVERAGE) if (!seen.some(({ s, input }) => input?.tags?.includes(tag) && ok(s, side))) miss.push(name);
  return miss;
}

// ---------------------------------------------------------------- main

if (!fs.existsSync(FLEETD)) { console.error(`parity: ${FLEETD} not built (cargo build --release -p fleetd)`); process.exit(2); }

let nHomes = 0, nSessions = 0, bad = 0;
const fires = new Map(EXPECTED_DIVERGENCE.map((d) => [d.id, 0]));
const seen = { node: [], fleetd: [] };
const fail = (msg) => { bad++; console.log(msg); };
for (const h of homes()) {
  try {
    h.finish();
    await h.settle();
    // fleetd first: a grown file is appended to while it runs, and Node reads the file as it ends up.
    const fd = normalizeFleetd(h.growth ? await fleetdGrown(h) : fleetdSnapshot(h)), nd = normalizeNode(nodeCollect(h));
    nHomes++;
    // Each side's listing must be whole: nothing dropped for an unknown engine or a key used twice.
    for (const [side, x] of [['node', nd], ['fleetd', fd]]) {
      if (x.map.size !== x.raw) fail(`[${h.name}] ${side} listed ${x.raw} sessions, ${x.map.size} distinct known keys`);
      if (x.map.size < h.min || x.map.size > h.max) fail(`[${h.name}] ${side} has ${x.map.size} sessions, expected ${h.min}..${h.max}`);
    }
    for (const key of new Set([...nd.map.keys(), ...fd.map.keys()])) {
      const n = nd.map.get(key), f = fd.map.get(key), input = h.cases.get(key);
      if (!n || !f) { fail(`[${h.name}] ${key}: only in ${n ? 'node' : 'fleetd'}`); continue; }
      nSessions++;
      seen.node.push({ s: n, input });
      seen.fleetd.push({ s: f, input });
      if (false) console.log(JSON.stringify([n, f]));
      if (process.argv.includes('--show')) console.log(`[${h.name}] ${key.slice(0, 60).padEnd(60)} node=${n.state}/${n.live} fleetd=${f.state}/${f.live} ${input?.file ?? ''}`);
      const { unexpected, expected } = diffSession(n, f, input);
      for (const e of expected) fires.set(e.rule, fires.get(e.rule) + 1);
      if (unexpected.length) {
        bad += unexpected.length;
        console.log(`[${h.name}] ${key} input=${JSON.stringify(input)}`);
        for (const u of unexpected) console.log(`    ${u.k}: node=${JSON.stringify(u.node)} fleetd=${JSON.stringify(u.fleetd)}`);
      }
    }
  } catch (e) {
    fail(`[${h.name}] ${e.message}`);
  } finally { h.cleanup(); }
}
for (const side of ['node', 'fleetd']) for (const name of coverage(side, seen[side])) fail(`coverage: ${side} side never saw: ${name}`);
for (const [id, n] of fires) if (n === 0) fail(`rule ${id} never fired: its fixture does not reach the case it allows`);
const rules = [...fires].map(([k, v]) => `${k} x${v}`).join(', ');
console.log(`parity: ${nHomes} homes, ${nSessions} sessions, ${bad} unexpected differences (ruled differences fired: ${rules})`);
process.exit(bad ? 1 : 0);
