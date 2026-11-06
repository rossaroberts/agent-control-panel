// Collect parity: Node's machine.mjs collect and `fleetd snapshot --json` over the same made-up home folders.
// Both sides are normalized to { profile, sessionId, engine, state, live, stage, model, contextTokens, lastAt, said, doing }
// and diffed. The only differences allowed are the ruled ones in EXPECTED_DIVERGENCE, each matched on the INPUT condition
// of the case (its roster row, pid, file size), never on the answer. Anything else prints and exits 1.
// Run: node native/parity/collect-parity.mjs   (needs native/target/release/fleetd: cargo build --release -p fleetd)
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn } from 'node:child_process';
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
const EXPECTED_DIVERGENCE = [
  { id: 'R7', note: 'a pid that is set but dead (and not active) is not live in fleetd',
    when: (i) => i.engine === 'claude' && i.pid != null && !i.pidAlive && !i.active,
    fields: { state: (n, f) => ['working', 'needs-you', 'waiting', 'capped'].includes(n) && ['ended', 'done'].includes(f), live: (n, f) => n === true && f === false } },
  { id: 'R8', note: 'stopped/failed beside a live pid: fleetd is open (respawned), Node closed',
    when: (i) => i.engine === 'claude' && ['stopped', 'failed'].includes(i.rosterState) && i.pid != null && i.pidAlive,
    fields: { state: (n, f) => n === 'closed' && ['working', 'needs-you', 'waiting', 'capped'].includes(f), live: (n, f) => n === false && f === true } },
  { id: 'R9', note: 'a file over 384 KiB: fleetd keeps all it read, Node a window; fleetd knows a field Node lost',
    when: (i) => i.fileBytes > WINDOW,
    fields: Object.fromEntries(['stage', 'model', 'contextTokens', 'lastAt', 'said', 'doing'].map((k) => [k, (n, f) => n == null && f != null])) },
  { id: 'Task 6', note: 'an empty-string doing or model is null in fleetd',
    when: (i) => i.emptyStrings === true,
    fields: { doing: (n, f) => n === '' && f == null, model: (n, f) => n === '' && f == null, said: (n, f) => n === '' && f == null } },
];

// ---------------------------------------------------------------- building homes

const FIELDS = ['profile', 'sessionId', 'engine', 'state', 'live', 'stage', 'model', 'contextTokens', 'lastAt', 'said', 'doing'];
const sid = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const jobId = (n) => n.toString(16).padStart(8, '0');

/** A pid that certainly is not alive: a child that has exited and been reaped. */
function deadPid() {
  const r = execFileSync(process.execPath, ['-e', 'process.stdout.write(String(process.pid))']);
  return Number(String(r));
}

class Home {
  constructor(name) {
    this.name = name;
    this.dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), `collect-parity-${name}-`))); // real path: lsof names files by it
    this.cases = new Map(); // `${engine}/${profile}/${sessionId}` -> input condition
    this.kids = [];
    this.profiles = new Map();
    fs.mkdirSync(path.join(this.dir, '.local', 'bin'), { recursive: true });
  }
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
  claude(p, n, { row = {}, transcript = null, hook = null, status = null, active = false, job = null, mtimeAgoMs = 3_600_000 } = {}) {
    const cwd = path.join(this.dir, `repo-${n}`);
    const r = { id: jobId(n), sessionId: sid(n), name: `case-${n}`, kind: 'interactive', cwd, startedAt: Date.now() - 600_000, ...row };
    p.rows.push(r);
    if (active) p.active.push({ id: r.id, sessionId: r.sessionId });
    let fileBytes = 0, emptyStrings = false;
    if (transcript) {
      const dest = path.join(p.dir, 'projects', cwd.replace(/[^A-Za-z0-9]/g, '-'));
      fs.mkdirSync(dest, { recursive: true });
      const f = path.join(dest, `${r.sessionId}.jsonl`);
      fs.copyFileSync(path.join(TRANSCRIPTS, transcript), f);
      const t = new Date(Date.now() - mtimeAgoMs);
      fs.utimesSync(f, t, t);
      fileBytes = fs.statSync(f).size;
    }
    if (hook) fs.writeFileSync(path.join(p.dir, 'fleet-state', `${r.sessionId}.json`), JSON.stringify({ at: Date.now() - (hook.agoMs ?? 0), ...hook, agoMs: undefined }));
    if (status) fs.writeFileSync(path.join(p.dir, 'fleet-status', `${r.sessionId}.json`), JSON.stringify({ at: Date.now(), session_id: r.sessionId, model: { id: 'claude-fixture-1' },
      context_window: { context_window_size: 1_000_000, current_usage: status }, ...{} }));
    if (job) {
      const d = path.join(p.dir, 'jobs', r.id);
      fs.mkdirSync(d, { recursive: true });
      fs.writeFileSync(path.join(d, 'state.json'), JSON.stringify({ updatedAt: new Date(Date.now() - job.agoMs).toISOString() }));
    }
    if (transcript === 'empty-ask.jsonl') emptyStrings = true; // see the fixture: its asks are empty strings
    const pid = r.pid ?? null;
    this.cases.set(`claude/${p.id}/${r.sessionId}`, { engine: 'claude', rosterState: r.state ?? null, pid, pidAlive: pid != null && pid === process.pid, active, fileBytes, emptyStrings });
    return r;
  }
  /** The Codex home: every rollout fixture copied under today's folder, with the times given by name (default: written an hour ago). */
  codex({ recent = [], stale = [], held = [] } = {}) {
    const c = path.join(this.dir, '.codex');
    fs.copyFileSync(path.join(CODEX, 'auth.json'), (fs.mkdirSync(c, { recursive: true }), path.join(c, 'auth.json')));
    const d = new Date();
    const day = path.join(c, 'sessions', String(d.getFullYear()), String(d.getMonth() + 1).padStart(2, '0'), String(d.getDate()).padStart(2, '0'));
    fs.mkdirSync(day, { recursive: true });
    const names = [];
    for (const dd of fs.readdirSync(path.join(CODEX, 'sessions', '2026', '11'))) for (const n of fs.readdirSync(path.join(CODEX, 'sessions', '2026', '11', dd))) names.push(n);
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
        this.cases.set(`codex/codex/${id}`, { engine: 'codex', fileBytes: fs.statSync(f).size, file: n });
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
      this.kids.push(spawn(exe, ['-e', `require('node:fs').openSync(${JSON.stringify(f)}, 'r'); setTimeout(() => {}, 120000)`], { stdio: 'ignore' }));
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
  async settle() { await new Promise((r) => setTimeout(r, 400)); } // the held-open processes get their fds
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
    const h = new Home('states'), p = h.profile('work');
    h.claude(p, 1, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'running.jsonl', status: useful });
    h.claude(p, 2, { row: { pid: live, state: 'blocked', status: 'waiting', waitingFor: 'permission prompt' }, transcript: 'turn-over.jsonl' });
    h.claude(p, 3, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl', hook: hookAt('PermissionRequest', { t: 'Bash' }) });
    h.claude(p, 4, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl' });
    h.claude(p, 5, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'limit.jsonl' });
    h.claude(p, 6, { row: { state: 'done' }, transcript: 'turn-over.jsonl' });
    h.claude(p, 7, { row: { state: 'stopped' }, transcript: 'workdir-a.jsonl' });
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
    out.push(h);
  }
  // 2. Transcripts at their limits: the head cut, the big window, the early break.
  {
    const h = new Home('windows'), p = h.profile('big');
    h.claude(p, 1, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'big.jsonl' });
    h.claude(p, 2, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'head-cut.jsonl' });
    h.claude(p, 3, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'break-uses.jsonl', status: { input_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 } });
    // The default profile and a third one: profiles are discovered, not configured.
    const d = h.profile('default', { plain: true });
    h.claude(d, 4, { row: { pid: live, state: 'working', status: 'busy' }, transcript: 'running.jsonl' });
    const e = h.profile('enterprise');
    h.claude(e, 5, { row: { pid: live, state: 'working', status: 'idle' }, transcript: 'turn-over.jsonl' });
    out.push(h);
  }
  // 3. Codex: every rollout fixture; some written just now (open by the ten minutes), some held open by a process, some cold.
  {
    const h = new Home('codex');
    h.codex({ recent: ['working', 'finished', 'asking.', 'aborted', 'answered', 'exec', 'big'], stale: ['asking-stale', 'old'], held: ['hidden-done'] });
    out.push(h);
  }
  // 4. Nothing at all, and a profile that has no sessions.
  {
    const h = new Home('empty'), p = h.profile('idle');
    void p;
    out.push(h);
  }
  return out;
}

// ---------------------------------------------------------------- running both sides

function nodeCollect(h) {
  const out = execFileSync(process.execPath, ['--input-type=module', '-'], {
    input: fs.readFileSync(MACHINE), maxBuffer: 64 << 20, timeout: 60_000,
    env: { HOME: h.dir, PATH: '/usr/bin:/bin:/usr/sbin:/sbin', FLEET_DO: 'collect' } });
  return JSON.parse(String(out));
}

function fleetdSnapshot(h) {
  const out = execFileSync(FLEETD, ['snapshot', '--json'], {
    maxBuffer: 64 << 20, timeout: 60_000, stdio: ['ignore', 'pipe', 'ignore'],
    env: { HOME: h.dir, FLEETD_HOME: h.dir, FLEETD_MACHINE: 'parity', PATH: '/usr/bin:/bin:/usr/sbin:/sbin' } });
  return JSON.parse(String(out).trim().split('\n').at(-1));
}

const NODE_STATE = (s) => (s.state === 'ready' ? 'waiting' : s.state === 'closed' ? 'closed' : s.state);
const nz = (v) => (v === undefined ? null : v);

function normalizeNode(r) {
  const out = new Map();
  for (const a of r.accounts) {
    if (a.engine !== 'claude' && a.engine !== 'codex') continue;
    for (const s of a.sessions ?? []) {
      // A Codex rollout with no session_meta has no sessionId: Node's `id` (the file's name) is its key, as fleetd's.
      const sessionId = s.sessionId ?? s.id;
      out.set(`${a.engine}/${a.id}/${sessionId}`, { profile: a.id, sessionId, engine: a.engine, state: NODE_STATE(s), live: s.live, stage: nz(s.stage),
        model: nz(s.model), contextTokens: nz(s.context?.tokens), lastAt: nz(s.lastAt), said: nz(s.said), doing: nz(s.doing) });
    }
  }
  return out;
}

function normalizeFleetd(r) {
  const out = new Map();
  for (const s of r.sessions) {
    if (s.engine === 'muse') continue;
    const key = `${s.engine}/${s.key.profile}/${s.key.session_id}`;
    out.set(key, { profile: s.key.profile, sessionId: s.key.session_id, engine: s.engine, state: s.state, live: s.live, stage: nz(s.stage), model: nz(s.model),
      contextTokens: nz(s.context_tokens), lastAt: nz(s.last_at), said: nz(s.said), doing: nz(s.doing) });
  }
  return out;
}

/** Whether the two states are the same session state under the name mapping (Node's closed is fleetd's ended or done). */
const sameState = (n, f) => n === f || (n === 'closed' && (f === 'ended' || f === 'done'));

function diffSession(key, node, fleet, input) {
  const unexpected = [], expected = [];
  for (const k of FIELDS) {
    const n = node[k], f = fleet[k];
    if (k === 'state' ? sameState(n, f) : JSON.stringify(n) === JSON.stringify(f)) continue;
    const rule = EXPECTED_DIVERGENCE.find((d) => input && d.when(input) && d.fields[k]?.(n, f));
    if (rule) expected.push({ k, n, f, rule: rule.id });
    else unexpected.push({ k, node: n, fleetd: f });
  }
  return { unexpected, expected };
}

// ---------------------------------------------------------------- main

if (!fs.existsSync(FLEETD)) { console.error(`parity: ${FLEETD} not built (cargo build --release -p fleetd)`); process.exit(2); }

let nHomes = 0, nSessions = 0, nExpected = 0, bad = 0;
const seenRules = new Map();
for (const h of homes()) {
  try {
    h.finish();
    await h.settle();
    const node = normalizeNode(nodeCollect(h)), fleet = normalizeFleetd(fleetdSnapshot(h));
    nHomes++;
    for (const key of new Set([...node.keys(), ...fleet.keys()])) {
      const n = node.get(key), f = fleet.get(key);
      if (!n || !f) { bad++; console.log(`[${h.name}] ${key}: only in ${n ? 'node' : 'fleetd'}`); continue; }
      nSessions++;
      if (process.argv.includes('--show')) console.log(`[${h.name}] ${key.slice(0, 60).padEnd(60)} node=${n.state}/${n.live} fleetd=${f.state}/${f.live} ${h.cases.get(key)?.file ?? ""}`);
      const { unexpected, expected } = diffSession(key, n, f, h.cases.get(key));
      for (const e of expected) { nExpected++; seenRules.set(e.rule, (seenRules.get(e.rule) ?? 0) + 1); }
      if (unexpected.length) {
        bad += unexpected.length;
        console.log(`[${h.name}] ${key} input=${JSON.stringify(h.cases.get(key))}`);
        for (const u of unexpected) console.log(`    ${u.k}: node=${JSON.stringify(u.node)} fleetd=${JSON.stringify(u.fleetd)}`);
      }
    }
  } finally { h.cleanup(); }
}
const rules = [...seenRules].map(([k, v]) => `${k}x${v}`).join(' ');
console.log(`parity: ${nHomes} homes, ${nSessions} sessions, ${bad} unexpected differences (${nExpected} ruled differences: ${rules || 'none'})`);
process.exit(bad ? 1 : 0);
