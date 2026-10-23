// Fleet: what one machine can tell about its agents, and how it starts one. No dependencies, Node 20+, macOS or Linux.
//
// The panel imports it for this Mac. For another machine it sends this same file over SSH on stdin
// (`node --input-type=module -`), with FLEET_DO=collect|launch and FLEET_ARGS (base64 JSON), so nothing is installed there
// and both ends always run the same code. It prints one JSON object.
//
// collect: every Claude Code profile (~/.claude, ~/.claude-<name>) with its account, plan, limits and sessions; Codex
//          (~/.codex) with its account, limits and recent sessions; the fleet kit's capped accounts and live dispatches.
// launch:  `claude --bg` on a profile, `codex exec` or `muse exec` detached, or a fleet dispatch, each through argv with no shell.
//
// Limits: Claude Code shows an account's plan limits only to its status line, so fleet/statusline.mjs keeps the newest
// snapshot per session in <profile>/fleet-status/. Codex writes its own into each session's log (token_count events).

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFile, spawn } from 'node:child_process';

const HOME = os.homedir();
const DAY = 86_400_000;
// A session's log is read from its end: transcripts grow to hundreds of megabytes.
const TAIL = 384 * 1024;
const RECENT_CODEX_MS = Number(process.env.FLEET_CODEX_RECENT_MS ?? 7 * DAY);
const CODEX_LIVE_MS = 10 * 60_000;

/** A program in the usual places, since sudo and launchd give children a bare PATH. */
function bin(name) {
  for (const d of [path.join(HOME, '.local', 'bin'), '/opt/homebrew/bin', '/usr/local/bin', '/usr/bin']) {
    const f = path.join(d, name);
    try { fs.accessSync(f, fs.constants.X_OK); return f; } catch {}
  }
  return name;
}
const childPath = [path.join(HOME, '.local', 'bin'), '/opt/homebrew/bin', '/usr/local/bin', process.env.PATH ?? '/usr/bin:/bin'].join(':');
// A Claude Code session that starts this must not hand its own markers to the agents it reads or starts.
const baseEnv = () => ({ ...Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE') && !k.startsWith('FLEET_'))), PATH: childPath });

const run = (cmd, args, opts = {}) => new Promise((resolve) =>
  execFile(cmd, args, { timeout: 8000, maxBuffer: 8_000_000, env: baseEnv(), ...opts },
    (err, stdout, stderr) => resolve({ err, stdout: String(stdout ?? ''), stderr: String(stderr ?? '') })));

const readJson = (f, max = 8 << 20) => { try { if (fs.statSync(f).size > max) return null; return JSON.parse(fs.readFileSync(f, 'utf8')); } catch { return null; } };
function tail(file, bytes = TAIL) {
  let fd;
  try {
    fd = fs.openSync(file, 'r');
    const size = fs.fstatSync(fd).size, start = Math.max(0, size - bytes), buf = Buffer.alloc(size - start);
    fs.readSync(fd, buf, 0, buf.length, start);
    const lines = buf.toString('utf8').split('\n');
    if (start > 0) lines.shift(); // the first is cut
    return lines.filter(Boolean);
  } catch { return []; } finally { if (fd !== undefined) fs.closeSync(fd); }
}
function head(file, bytes = 64 * 1024) {
  let fd;
  try {
    fd = fs.openSync(file, 'r');
    const buf = Buffer.alloc(bytes), n = fs.readSync(fd, buf, 0, bytes, 0);
    return buf.subarray(0, n).toString('utf8').split('\n')[0];
  } catch { return ''; } finally { if (fd !== undefined) fs.closeSync(fd); }
}
const parse = (l) => { try { return JSON.parse(l); } catch { return null; } };
/** The first lines of a file, up to `bytes`. */
function headLines(file, bytes = 256 * 1024) {
  let fd;
  try {
    fd = fs.openSync(file, 'r');
    const buf = Buffer.alloc(bytes), n = fs.readSync(fd, buf, 0, bytes, 0);
    const lines = buf.subarray(0, n).toString('utf8').split('\n');
    if (n === bytes) lines.pop(); // the last is cut
    return lines.filter(Boolean);
  } catch { return []; } finally { if (fd !== undefined) fs.closeSync(fd); }
}
/** A tool call in plain words: its own description when it has one (Bash and Agent calls do), else what it does to what.
 *  Null for the bookkeeping calls nobody needs to see. */
function stepOf(name, x = {}) {
  const base = (f) => (typeof f === 'string' ? path.basename(f) : '');
  if (/^mcp__ccd_turn__|^ToolSearch$/.test(name)) return null;
  if (typeof x.description === 'string' && x.description.trim()) return oneLine(x.description, 200);
  const mcp = /^mcp__([^_]+(?:_[^_]+)*)__(.+)$/.exec(name);
  const said = {
    Read: () => `Read ${base(x.file_path)}`, Edit: () => `Edit ${base(x.file_path)}`, MultiEdit: () => `Edit ${base(x.file_path)}`,
    Write: () => `Write ${base(x.file_path)}`, NotebookEdit: () => `Edit ${base(x.notebook_path)}`,
    Grep: () => `Search for ${x.pattern ?? ''}`, Glob: () => `Find ${x.pattern ?? ''}`,
    WebFetch: () => { try { return `Read ${new URL(x.url).host}`; } catch { return 'Read a web page'; } },
    WebSearch: () => `Search the web for ${x.query ?? ''}`,
    Bash: () => `Run ${String(x.command ?? '').split('\n')[0]}`,
    AskUserQuestion: () => `Asks: ${x.questions?.[0]?.question ?? ''}`, ExitPlanMode: () => 'Plan ready for you',
    TodoWrite: () => 'Update its task list', TaskCreate: () => `Add task: ${x.subject ?? x.title ?? ''}`, TaskUpdate: () => 'Update its task list',
    Skill: () => `Use ${x.skill ?? 'a skill'}`, Agent: () => `Start an agent: ${x.prompt ?? ''}`, Task: () => `Start an agent: ${x.prompt ?? ''}`,
  }[name];
  return oneLine(said ? said() : mcp ? `${mcp[2].replace(/[_-]+/g, ' ')} (${mcp[1].replace(/^(plugin_|claude_ai_)/, '').replace(/_/g, ' ')})` : name, 200);
}

/** What you typed in a Claude Code user line, or null for tool results, commands and what Claude Code sent in your name. */
function askOf(d) {
  if (d?.type !== 'user' || d.isMeta || d.isSidechain || !d.message) return null;
  const c = d.message.content;
  const text = typeof c === 'string' ? c : Array.isArray(c) && !c.some((x) => x.type === 'tool_result') ? c.filter((x) => x.type === 'text').map((x) => x.text).join('\n') : '';
  const t = text.trim();
  if (!t || t.startsWith('<') || t.startsWith('[Request interrupted') || t.startsWith('Caveat:')) return null;
  return t.slice(0, 4000);
}
const keepLines = (v, n) => (typeof v === 'string' ? v.trim().slice(0, n) : null);
const oneLine = (v, n = 220) => (typeof v === 'string' ? v.replace(/\s+/g, ' ').trim().slice(0, n) : null);
const tilde = (p) => (typeof p === 'string' && p.startsWith(HOME) ? '~' + p.slice(HOME.length) : p);
const mtime = (f) => { try { return fs.statSync(f).mtimeMs; } catch { return 0; } };

// ---------------------------------------------------------------- Claude Code

function claudeProfiles() {
  let names = [];
  try { names = fs.readdirSync(HOME).filter((n) => n === '.claude' || /^\.claude-[\w.-]{1,40}$/.test(n)).sort(); } catch {}
  return names.flatMap((n) => {
    const dir = path.join(HOME, n);
    try { if (!fs.statSync(dir).isDirectory()) return []; } catch { return []; }
    if (!['settings.json', '.claude.json', 'projects'].some((f) => fs.existsSync(path.join(dir, f)))) return [];
    const plain = n === '.claude';
    return [{ id: plain ? 'default' : n.slice('.claude-'.length), dir, plain, config: plain ? path.join(HOME, '.claude.json') : path.join(dir, '.claude.json') }];
  });
}
const profileEnv = (p) => (p.plain ? baseEnv() : { ...baseEnv(), CLAUDE_CONFIG_DIR: p.dir });

/** Where a session's transcript is: projects/<its folder with every other character a dash>/<id>.jsonl, else a scan. */
function transcriptOf(p, sessionId, cwd) {
  const root = path.join(p.dir, 'projects');
  if (typeof cwd === 'string') {
    const f = path.join(root, cwd.replace(/[^A-Za-z0-9]/g, '-'), sessionId + '.jsonl');
    if (fs.existsSync(f)) return f;
  }
  try { for (const d of fs.readdirSync(root)) { const f = path.join(root, d, sessionId + '.jsonl'); if (fs.existsSync(f)) return f; } } catch {}
  return null;
}

const LIMIT_HIT = /(hit|reached) your .{0,40}limit/i;

// A session's context window comes from its status-line snapshot. Without one, the window another session on the same model
// reported (the model name alone doesn't say: Opus 5.5 runs on 1M without a [1m] in its name), else a guess.
const baseModel = (m) => (typeof m === 'string' ? m.replace(/\[1m\]$/, '') : null);
function windowOf(model, tokens, learned) {
  const known = learned.get(baseModel(model));
  if (known) return { size: known, guessed: false };
  if ((typeof model === 'string' && model.endsWith('[1m]')) || tokens > 200_000) return { size: 1_000_000, guessed: true };
  return { size: 200_000, guessed: true };
}

/** From the end of a transcript: the context its last reply carried, its model, and what it last did or said. */
function readTranscript(file) {
  const out = { model: null, contextTokens: null, lastAt: null, doing: null, said: null, limitHit: undefined, turnOver: null, writtenAt: mtime(file),
                firstAsk: null, lastAsk: null };
  for (const l of headLines(file)) if ((out.firstAsk = oneLine(askOf(parse(l)), 500))) break;
  const lines = tail(file);
  for (let i = lines.length - 1; i >= 0; i--) {
    const d = parse(lines[i]);
    if (!d || d.isSidechain) continue;
    if (!out.lastAt && d.timestamp) out.lastAt = Date.parse(d.timestamp) || null;
    out.lastAsk ??= oneLine(askOf(d), 500);
    // The last word in the conversation: a reply that ended its turn, or anything else (a tool call, a tool's result).
    if (out.turnOver === null && (d.type === 'assistant' || d.type === 'user') && d.message) {
      out.turnOver = d.type === 'assistant' && (['end_turn', 'stop_sequence'].includes(d.message.stop_reason) || d.message.model === '<synthetic>');
    }
    if (d.type !== 'assistant' || !d.message) continue;
    // Claude Code writes a limit stop as a reply of its own ("You've hit your weekly limit · resets …"), with no usage.
    const synthetic = d.message.model === '<synthetic>';
    if (synthetic && out.limitHit === undefined) {
      const text = (d.message.content ?? []).find?.((c) => c.type === 'text')?.text ?? '';
      out.limitHit = LIMIT_HIT.test(text) ? { text: oneLine(text, 200), at: Date.parse(d.timestamp) || null } : null;
    }
    const u = synthetic ? null : d.message.usage;
    if (u && out.contextTokens === null) {
      out.contextTokens = (u.input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0);
      out.model = [d.requestedModel, d.message.model].find((m) => typeof m === 'string') ?? null;
      out.effort = typeof d.effort === 'string' ? d.effort : null;
    }
    for (const c of Array.isArray(d.message.content) ? [...d.message.content].reverse() : []) {
      if (!out.doing && c.type === 'tool_use') out.doing = stepOf(c.name, c.input ?? {});
      if (!out.said && c.type === 'text' && c.text?.trim()) out.said = oneLine(c.text, 400);
    }
    if (out.contextTokens !== null && (out.doing || out.said) && out.lastAsk) break;
  }
  out.lastAsk ??= out.firstAsk;
  return out;
}

// ---------------------------------------------------------------- how much of the weekly limit a session used
// Claude Code reports usage per account, not per session, so a session's share is estimated from its own transcript: each
// reply's tokens since the account's weekly window began, weighted the way usage is priced (a cache read is cheap, output
// and the bigger models dear). The page splits the account's real weekly figure across its sessions by these units.
// Transcripts run to hundreds of megabytes, so each one is read once from the window's start and then only its new lines,
// with where it got to kept in ~/.fleet-panel/usage.json.
const WINDOW_MS = 7 * DAY;
const USAGE_FILE = path.join(HOME, '.fleet-panel', 'usage.json');
const unitsOf = (u, model) => ((u.input_tokens ?? 0) + 1.25 * (u.cache_creation_input_tokens ?? 0) + 0.1 * (u.cache_read_input_tokens ?? 0)
  + 5 * (u.output_tokens ?? 0)) * (/haiku/.test(model ?? '') ? 1 : /sonnet/.test(model ?? '') ? 3 : 5);
function readAt(fd, at, n) { const b = Buffer.alloc(n); const got = fs.readSync(fd, b, 0, n, at); return b.subarray(0, got).toString('utf8'); }
/** The first byte at or after the first line written at or after `since` (transcripts are in time order). */
function offsetSince(fd, size, since) {
  let lo = 0, hi = size;
  while (hi - lo > 256 * 1024) {
    const mid = Math.floor((lo + hi) / 2), chunk = readAt(fd, mid, 64 * 1024);
    const ts = chunk.split('\n').slice(1).map((l) => Date.parse(/"timestamp":"([^"]+)"/.exec(l)?.[1] ?? '')).find((t) => !Number.isNaN(t));
    if (ts === undefined || ts < since) lo = mid; else hi = mid;
  }
  return lo;
}
function usageSince(file, since, cache) {
  let fd;
  try {
    fd = fs.openSync(file, 'r');
    const size = fs.fstatSync(fd).size;
    let c = cache[file];
    if (!c || c.since !== since || c.offset > size) c = { since, offset: offsetSince(fd, size, since), units: 0 };
    for (let at = c.offset; at < size;) {
      const n = Math.min(4 << 20, size - at), text = readAt(fd, at, n), cut = at + n < size ? text.lastIndexOf('\n') + 1 : text.length;
      if (cut === 0) break; // one line longer than the chunk: leave it for next time
      for (const l of text.slice(0, cut).split('\n')) {
        if (!l.includes('"usage"') || !l.includes('"assistant"')) continue;
        const d = parse(l);
        if (d?.type !== 'assistant' || !d.message?.usage || d.message.model === '<synthetic>' || !(Date.parse(d.timestamp) >= since)) continue;
        c.units += unitsOf(d.message.usage, d.message.model);
      }
      at += Buffer.byteLength(text.slice(0, cut));
      c.offset = at;
    }
    cache[file] = { ...c, seen: Date.now() };
    return Math.round(c.units);
  } catch { return null; } finally { if (fd !== undefined) fs.closeSync(fd); }
}

/** A live session's own record (`<profile>/sessions/<pid>.json`): its kind, where it was started from, and its claude.ai id
 *  (`bridgeSessionId`) when Remote Control is on for it. */
function sessionRecord(p, pid) {
  if (!Number.isInteger(pid)) return null;
  const d = readJson(path.join(p.dir, 'sessions', `${pid}.json`), 1 << 20);
  return d && d.pid === pid ? d : null;
}
/** A claude.ai session id as its page names it: session_<id>, whether it was recorded as session_ or cse_. */
const remoteId = (v) => { const m = /^(?:session|cse)_(\w+)$/.exec(String(v ?? '')); return m ? `session_${m[1]}` : null; };
function reachOf(r, rec) {
  const id = remoteId(rec?.bridgeSessionId);
  if (r.kind === 'background' || rec?.kind === 'bg') return { via: 'background', remote: id };
  if (id) return { via: 'remote', remote: id };
  if (rec?.entrypoint === 'claude-desktop') return { via: 'app', remote: null };
  return { via: r.pid ? 'terminal' : null, remote: null };
}

/** The newest status-line snapshot per session, and the account's newest limits among them. */
function statusSnapshots(p, learned) {
  const dir = path.join(p.dir, 'fleet-status'), bySession = new Map();
  let limits = null;
  try {
    for (const n of fs.readdirSync(dir)) {
      if (!n.endsWith('.json') || n.startsWith('.')) continue;
      const d = readJson(path.join(dir, n), 65536);
      if (!d?.session_id) continue;
      bySession.set(d.session_id, d);
      if (d.rate_limits && (!limits || d.at > limits.at)) limits = { at: d.at, ...d.rate_limits };
      const size = d.context_window?.context_window_size, m = baseModel(d.model?.id);
      if (m && size > 0) learned.set(m, Math.max(learned.get(m) ?? 0, size));
    }
  } catch {}
  return { bySession, limits };
}

/**
 * What a session is doing, from its roster row (`claude agents`) and its transcript's end (`t`). A session whose process
 * is alive is open: working, needing you, stopped on a limit, or ready at its prompt. One with no process is closed. The roster's "busy" also covers a background task the session started (a dev server, a watch), so a busy
 * session whose turn is over and whose transcript has been still for a minute reads ready.
 */
export function sessionState(r, t, now = Date.now()) {
  // Open means a process: a background job without one (blocked, stopped, or ended) only keeps its record. One whose turn is
  // done but whose process still runs waits for your next message.
  if (r.state === 'stopped' || r.state === 'failed' || !r.pid) return { state: 'closed', live: false };
  const live = true;
  if (r.status === 'waiting' || r.waitingFor || r.state === 'blocked') return { state: 'needs-you', live };
  if (t?.limitHit && t.limitHit.at >= (t.lastAt ?? 0) - 120_000) return { state: 'capped', live };
  const busy = r.status === 'busy' || r.state === 'working' || r.state === 'running';
  if (busy && !(t?.turnOver && now - (t.writtenAt ?? now) > 60_000)) return { state: 'working', live };
  return { state: live ? 'ready' : 'closed', live };
}

async function claudeProfile(p, learned, usage) {
  const env = profileEnv(p), claude = bin('claude');
  const [auth, roster] = await Promise.all([
    run(claude, ['auth', 'status', '--json'], { env, timeout: 8000 }),
    run(claude, ['agents', '--json', '--all'], { env, timeout: 8000 }),
  ]);
  const cfg = readJson(p.config)?.oauthAccount ?? null;
  let a = null;
  try { a = JSON.parse(auth.stdout); } catch {}
  const account = { email: a?.email ?? cfg?.emailAddress ?? null, org: a?.orgName ?? cfg?.organizationName ?? null,
                    plan: a?.subscriptionType ?? null, loggedIn: a ? Boolean(a.loggedIn) : Boolean(cfg) };
  const { bySession, limits } = statusSnapshots(p, learned);
  // The account's current weekly window: from its status line when it has one, else the last seven days (to the hour, so
  // what was read stays good for an hour).
  const resets = (limits?.seven_day?.resets_at ?? 0) * 1000;
  const since = resets > Date.now() ? resets - WINDOW_MS : Math.floor((Date.now() - WINDOW_MS) / 3600_000) * 3600_000;
  let list = [], error = null;
  if (roster.err) error = oneLine(roster.stderr || roster.err.message, 200);
  else { try { list = JSON.parse(roster.stdout); } catch { error = 'claude agents --json returned non-JSON'; } }
  const sessions = list.filter((r) => r?.sessionId).map((r) => {
    const file = transcriptOf(p, r.sessionId, r.cwd);
    const t = file ? readTranscript(file) : {};
    const sl = bySession.get(r.sessionId);
    const fresh = typeof sl?.context_window?.used_percentage === 'number' && sl.at >= (t.lastAt ?? 0) - 60_000;
    const w = sl?.context_window?.context_window_size ? { size: sl.context_window.context_window_size, guessed: false } : windowOf(t.model, t.contextTokens ?? 0, learned);
    const size = w.size;
    const pct = fresh ? sl.context_window.used_percentage : t.contextTokens != null ? (100 * t.contextTokens) / size : null;
    const { state, live } = sessionState(r, t);
    const { via, remote } = reachOf(r, sessionRecord(p, r.pid));
    const units = file && (t.writtenAt ?? 0) >= since ? usageSince(file, since, usage) : 0;
    return { engine: 'claude', id: r.id ?? null, sessionId: r.sessionId, name: r.name ?? null, kind: r.kind ?? null,
             state, live, rawState: r.state ?? r.status ?? null, waitingFor: r.waitingFor ?? null, pid: r.pid ?? null,
             cwd: tilde(r.cwd ?? null), startedAt: r.startedAt ?? null, lastAt: t.lastAt ?? null,
             model: t.model ?? null, context: pct == null ? null : { pct: Math.round(pct * 10) / 10, tokens: t.contextTokens ?? null, size, guessed: !fresh && w.guessed },
             doing: t.doing ?? null, said: t.said ?? null, limitHit: t.limitHit ?? null, firstAsk: t.firstAsk ?? null, lastAsk: t.lastAsk ?? null,
             via, web: remote ? `https://claude.ai/code/${remote}` : null, effort: t.effort ?? null, unitsWeek: units };
  });
  // The newest limit stop any session met: the account-wide cap when its status line never reported.
  const limitHit = sessions.map((s) => s.limitHit).filter((h) => h?.at).sort((a, b) => b.at - a.at)[0] ?? null;
  return { engine: 'claude', id: p.id, dir: tilde(p.dir), account, limits, limitHit, sessions, error, windowSince: since };
}

// ---------------------------------------------------------------- processes
// What the rosters don't list: Remote Control bridges (`claude remote-control`), fleet workers (`run-agent`), headless
// runs (`claude -p`, `codex exec`, `muse`), and which Codex logs a running Codex holds open. Linux reads /proc; macOS
// asks ps and lsof. FLEET_PROC_SCAN=0 turns it off.

const etimeMs = (e) => { // [[dd-]hh:]mm:ss
  const m = /^(?:(\d+)-)?(?:(\d+):)?(\d+):(\d+)$/.exec(String(e).trim());
  return m ? (((Number(m[1] ?? 0) * 24 + Number(m[2] ?? 0)) * 60 + Number(m[3])) * 60 + Number(m[4])) * 1000 : null;
};
const ENGINE = (args) => {
  const exe = path.basename(args[0] ?? '');
  if (args.some((a) => /(^|\/)run-agent$/.test(a))) return 'fleet';
  if (exe === 'claude' || /\/claude\/versions\/[\d.]+$/.test(args[0]) || /MacOS\/claude$/.test(args[0] ?? '')) return 'claude';
  if (exe === 'codex' || exe === 'codex-exec') return 'codex';
  if (exe === 'muse') return 'muse';
  return null;
};

async function processes() {
  if (process.env.FLEET_PROC_SCAN === '0') return null;
  const list = [];
  if (process.platform === 'linux') {
    let ids = [];
    try { ids = fs.readdirSync('/proc').filter((d) => /^\d+$/.test(d)); } catch { return null; }
    for (const d of ids) {
      try {
        const args = fs.readFileSync(`/proc/${d}/cmdline`, 'utf8').split('\0').filter(Boolean);
        const engine = ENGINE(args);
        if (!engine) continue;
        const stat = fs.readFileSync(`/proc/${d}/stat`, 'utf8');
        const ppid = Number(stat.slice(stat.lastIndexOf(')') + 2).split(' ')[1]);
        let env = {}, cwd = null, open = [];
        try { env = Object.fromEntries(fs.readFileSync(`/proc/${d}/environ`, 'utf8').split('\0').filter((x) => /^(CLAUDE_CONFIG_DIR|CODEX_HOME)=/.test(x)).map((x) => x.split(/=(.*)/s).slice(0, 2))); } catch {}
        try { cwd = fs.readlinkSync(`/proc/${d}/cwd`); } catch {}
        if (engine === 'codex') try { open = fs.readdirSync(`/proc/${d}/fd`).map((f) => { try { return fs.readlinkSync(`/proc/${d}/fd/${f}`); } catch { return ''; } }).filter((f) => f.endsWith('.jsonl')); } catch {}
        list.push({ pid: Number(d), ppid, args, engine, env, cwd, open, since: fs.statSync(`/proc/${d}`).mtimeMs });
      } catch {}
    }
  } else {
    const r = await run('ps', ['-axww', '-o', 'pid=,ppid=,etime=,args=']);
    if (r.err) return null;
    for (const line of r.stdout.split('\n')) {
      const m = /^\s*(\d+)\s+(\d+)\s+(\S+)\s+(.*)$/.exec(line);
      if (!m) continue;
      // macOS ps can't split args that contain spaces: an app path with spaces is the one case that matters here
      const args = m[4].startsWith('/') && m[4].includes('.app/Contents/MacOS/') ? [m[4].slice(0, m[4].indexOf(' --') > 0 ? m[4].indexOf(' --') : undefined), ...m[4].slice(m[4].indexOf(' --') + 1).split(' ')] : m[4].split(' ');
      const engine = ENGINE(args);
      if (engine) list.push({ pid: Number(m[1]), ppid: Number(m[2]), args, engine, env: {}, cwd: null, open: [], since: Date.now() - (etimeMs(m[3]) ?? 0) });
    }
    const codex = list.filter((x) => x.engine === 'codex').map((x) => x.pid);
    if (codex.length) {
      const o = await run('lsof', ['-nP', '-p', codex.join(','), '-Fpn']);
      let pid = null;
      for (const l of o.stdout.split('\n')) {
        if (l[0] === 'p') pid = Number(l.slice(1));
        else if (l[0] === 'n' && l.endsWith('.jsonl')) list.find((x) => x.pid === pid)?.open.push(l.slice(1));
      }
    }
  }
  return list;
}

const profileOfDir = (dir) => (!dir ? 'default' : path.basename(dir).replace(/^\.claude-?/, '') || 'default');

/** The scan, sorted: bridges per profile, fleet workers, and agent processes no roster or log accounts for. */
function sortProcesses(list, rosterPids, codexOpen) {
  const byPid = new Map(list.map((x) => [x.pid, x]));
  const under = (x, test) => { for (let p = byPid.get(x.ppid), i = 0; p && i < 12; p = byPid.get(p.ppid), i++) if (test(p)) return p; return null; };
  const bridges = [], workers = [], others = [];
  for (const x of list) {
    if (x.engine === 'claude' && x.args[1] === 'remote-control') {
      const i = x.args.indexOf('--name');
      bridges.push({ pid: x.pid, name: i > 0 ? oneLine(x.args[i + 1], 60) : 'remote-control', profile: profileOfDir(x.env.CLAUDE_CONFIG_DIR), since: x.since,
                     cwd: tilde(x.cwd), sessions: list.filter((c) => c.ppid === x.pid).length });
      continue;
    }
    if (x.engine === 'fleet') {
      const i = x.args.findIndex((a) => /(^|\/)run-agent$/.test(a));
      workers.push({ pid: x.pid, engine: oneLine(x.args[i + 1], 30), task: oneLine(x.args[i + 2], 200), since: x.since, cwd: tilde(x.cwd) });
      continue;
    }
    // this read's own calls, and Claude Code's own background machinery (the job supervisor, a job's terminal host, a
    // warmed-up spare), which are no agents
    if (x.ppid === process.pid || ['agents', 'auth', 'attach', 'stop', 'rm', 'daemon', 'bg-pty-host', 'bg-spare', '--version'].includes(x.args[1])) continue;
    if (x.args.includes('--json-schema') && x.args.includes('--no-session-persistence')) continue; // the panel's own titles and World's builds
    if (rosterPids.has(x.pid) || under(x, (p) => rosterPids.has(p.pid) || p.engine === 'fleet')) continue; // listed, or a part of one
    if (x.args.includes('--sdk-url') || x.args[1] === 'app-server' || x.args.includes('mcp-server')) continue; // a bridge's session (in the roster), Codex's desktop backend
    if (x.engine === 'codex' && x.open.some((f) => codexOpen.has(f))) continue;
    if (under(x, (p) => p.engine === x.engine)) continue; // a helper of another agent process
    const prompt = x.args.includes('-p') || x.args.includes('--print') || x.args[1] === 'exec';
    others.push({ pid: x.pid, engine: x.engine, kind: prompt ? 'headless' : 'process', since: x.since, cwd: tilde(x.cwd),
                  profile: x.engine === 'claude' ? profileOfDir(x.env.CLAUDE_CONFIG_DIR) : null, args: oneLine(x.args.slice(1).join(' '), 160) });
  }
  return { bridges, workers, others };
}

// ---------------------------------------------------------------- Codex

function codexHomes() {
  let names = [];
  try { names = fs.readdirSync(HOME).filter((n) => n === '.codex' || /^\.codex-[\w.-]{1,40}$/.test(n)).sort(); } catch {}
  return names.map((n) => path.join(HOME, n)).filter((d) => { try { return fs.statSync(d).isDirectory() && fs.existsSync(path.join(d, 'sessions')); } catch { return false; } });
}

/** Who a Codex home is signed in as: the email and plan claims of its id token, and nothing else from that file. */
function codexAccount(home) {
  const a = readJson(path.join(home, 'auth.json'), 1 << 20);
  if (!a) return { email: null, plan: null, loggedIn: false };
  if (a.OPENAI_API_KEY && !a.tokens) return { email: null, plan: 'api key', loggedIn: true };
  try {
    const part = String(a.tokens?.id_token ?? '').split('.')[1];
    const c = JSON.parse(Buffer.from(part, 'base64url').toString('utf8'));
    return { email: c.email ?? null, plan: c['https://api.openai.com/auth']?.chatgpt_plan_type ?? null, loggedIn: true };
  } catch { return { email: null, plan: null, loggedIn: Boolean(a.tokens) }; }
}

/** What a Codex log line says, for `role` user or assistant: an event (user_message, agent_message) or a response item
 *  (a message with input_text or output_text). Null for its own instruction blocks (AGENTS.md, page context). */
function codexText(d, role) {
  const p = d?.payload ?? {};
  let t = null;
  if (role === 'user' && p.type === 'user_message' && typeof p.message === 'string') t = p.message;
  else if (role === 'assistant' && p.type === 'agent_message' && typeof p.message === 'string') t = p.message;
  else if (d?.type === 'response_item' && p.type === 'message' && p.role === role && Array.isArray(p.content)) t = p.content.filter((c) => /text/.test(c?.type ?? '')).map((c) => c.text).join('\n');
  t = typeof t === 'string' ? t.trim() : '';
  return !t || t.startsWith('<') || t.startsWith('# AGENTS.md') ? null : t.slice(0, 6000);
}

/** Rollouts written to in the last RECENT_CODEX_MS: sessions/YYYY/MM/DD/rollout-*.jsonl, newest days first. */
function recentRollouts(home, ms = RECENT_CODEX_MS) {
  const out = [], since = Date.now() - ms;
  for (let back = 0; back <= Math.ceil(ms / DAY) + 1; back++) {
    const d = new Date(Date.now() - back * DAY);
    const dir = path.join(home, 'sessions', String(d.getFullYear()), String(d.getMonth() + 1).padStart(2, '0'), String(d.getDate()).padStart(2, '0'));
    let names = [];
    try { names = fs.readdirSync(dir); } catch { continue; }
    for (const n of names) if (n.endsWith('.jsonl')) { const f = path.join(dir, n), m = mtime(f); if (m >= since) out.push({ f, m }); }
  }
  return out.sort((a, b) => b.m - a.m).slice(0, 200);
}
/** A thread you started: Codex also logs the agents it starts and its reviewer's passes, each as a thread of its own. */
const yours = (f) => { const t = parse(head(f))?.payload?.thread_source; return t == null || t === 'user'; };

function readRollout(f, m, open) {
  const meta = parse(head(f))?.payload ?? {};
  const userText = (d) => oneLine(codexText(d, 'user'), 500);
  let firstAsk = null, lastAsk = null;
  for (const l of headLines(f)) if ((firstAsk = userText(parse(l)))) break;
  let tokens = null, limits = null, doing = null, said = null, done = null, lastAt = null, model = null, effort = null;
  const lines = tail(f);
  for (let i = lines.length - 1; i >= 0; i--) {
    const d = parse(lines[i]);
    if (!d) continue;
    lastAt ??= Date.parse(d.timestamp) || null;
    lastAsk ??= userText(d);
    const p = d.payload ?? {};
    if (d.type === 'turn_context' && !model) { model = p.model ?? null; effort = p.effort ?? null; }
    if (p.type === 'token_count' && !tokens) { tokens = p.info ?? null; limits = p.rate_limits ?? null; }
    if (p.type === 'task_complete' && done === null) { done = true; said ??= oneLine(p.last_agent_message, 400); }
    if ((p.type === 'task_started' || p.type === 'user_message') && done === null) done = false;
    if (p.type === 'function_call' && !doing) {
      let x = {};
      try { x = JSON.parse(p.arguments ?? '{}'); } catch {}
      const cmd = Array.isArray(x.command) ? x.command.join(' ') : x.cmd ?? x.command;
      doing = oneLine(cmd ? `Run ${String(cmd).split('\n')[0]}` : p.name === 'apply_patch' ? 'Edit files' : p.name, 200);
    }
    if (!said) said = oneLine(codexText(d, 'assistant'), 400);
    if (tokens && done !== null && (doing || said) && model && lastAsk) break;
  }
  const size = tokens?.model_context_window ?? null, used = tokens?.last_token_usage?.input_tokens ?? null;
  // Open while a running Codex holds its log; without a process scan, while it was written in the last ten minutes.
  const live = open ? open.has(f) : Date.now() - m < CODEX_LIVE_MS;
  return {
    session: { engine: 'codex', id: meta.id ?? path.basename(f, '.jsonl'), sessionId: meta.id ?? null,
               name: oneLine(meta.originator, 60), kind: meta.source ?? null, live,
               state: !live ? 'closed' : done === false ? 'working' : 'ready',
               cwd: tilde(meta.cwd ?? null), startedAt: Date.parse(meta.timestamp) || null, lastAt: lastAt ?? m, model,
               context: size && used != null ? { pct: Math.round((1000 * used) / size) / 10, tokens: used, size } : null,
               doing, said, firstAsk, lastAsk: lastAsk ?? firstAsk, effort,
               unitsWeek: tokens?.total_token_usage ? Math.round(unitsOf({ input_tokens: (tokens.total_token_usage.input_tokens ?? 0) - (tokens.total_token_usage.cached_input_tokens ?? 0),
                 cache_read_input_tokens: tokens.total_token_usage.cached_input_tokens ?? 0, output_tokens: tokens.total_token_usage.output_tokens ?? 0 }, 'codex')) : null },
    limits: limits && { at: lastAt ?? m, ...limits },
  };
}

function codexHome(home, open) {
  // Every log a running Codex holds open, however old, and the rest of the last day's.
  const recent = recentRollouts(home).filter((r) => yours(r.f)).slice(0, 60), seen = new Set(recent.map((r) => r.f));
  for (const f of open ?? []) if (!seen.has(f) && f.startsWith(path.join(home, 'sessions') + path.sep)) recent.push({ f, m: mtime(f) });
  const read = recent.map(({ f, m }) => readRollout(f, m, open));
  // Limits are the account's, so a quiet day still has them: the newest session that reported any, up to two weeks back.
  let limits = read.map((r) => r.limits).filter(Boolean).sort((a, b) => b.at - a.at)[0] ?? null;
  if (!limits) for (const { f, m } of recentRollouts(home, 14 * DAY).slice(0, 5)) if ((limits = readRollout(f, m, open).limits)) break;
  return { engine: 'codex', id: path.basename(home).slice(1), dir: tilde(home), account: codexAccount(home), limits,
           sessions: read.map((r) => r.session), error: null };
}

// ---------------------------------------------------------------- Muse (Meta's Muse Code)

/** Muse where its CLI is installed and signed in: no limits it will tell, so it lists its running processes. */
function museHome() {
  const exe = bin('muse');
  if (!path.isAbsolute(exe) || !fs.existsSync(path.join(HOME, '.config', 'muse', 'auth.json'))) return null;
  return { engine: 'muse', id: 'muse', dir: '~/.config/muse', account: { email: null, plan: 'Muse Code', loggedIn: true }, limits: null, sessions: [], error: null };
}

// ---------------------------------------------------------------- the fleet kit (run-agent, fleet-workers, fleet-usage)

function fleetDir() {
  for (const d of [process.env.FLEET_KIT, path.join(HOME, 'dev', 'fleet'), path.join(HOME, 'fleet')]) if (d && fs.existsSync(path.join(d, 'run-agent'))) return d;
  return null;
}
function fleetState() {
  const kit = fleetDir();
  if (!kit) return null;
  const home = process.env.FLEET_HOME ?? path.join(HOME, '.fleet'), now = Date.now();
  const capped = [];
  try {
    for (const n of fs.readdirSync(path.join(home, 'limits'))) {
      if (!n.endsWith('.capped')) continue;
      const [until, at, msg] = fs.readFileSync(path.join(home, 'limits', n), 'utf8').trim().split('\t');
      if (Number(until) * 1000 > now) capped.push({ account: n.slice(0, -'.capped'.length), until: Number(until) * 1000, at: Number(at) * 1000, msg: oneLine(msg, 200) });
    }
  } catch {}
  const dispatches = [];
  try {
    const dir = path.join(home, 'dispatch');
    for (const n of fs.readdirSync(dir)) {
      if (!n.endsWith('.pid')) continue;
      const key = n.slice(0, -4), pid = Number(fs.readFileSync(path.join(dir, n), 'utf8').trim());
      let alive = false;
      try { process.kill(pid, 0); alive = true; } catch {}
      const log = path.join(dir, key + '.log'), logAt = mtime(log);
      if (!alive && now - logAt > DAY) continue;
      dispatches.push({ key, pid, alive, logAt, last: oneLine(tail(log, 4096).at(-1) ?? null, 220) });
    }
  } catch {}
  return { kit: tilde(kit), capped, dispatches: dispatches.sort((a, b) => b.logAt - a.logAt).slice(0, 30) };
}

// ---------------------------------------------------------------- the machine itself

/** Load, memory, disk, the dev servers listening, and the database stacks and containers running. */
async function machineStatus() {
  const out = { cpus: os.cpus().length, load: Math.round(os.loadavg()[0] * 10) / 10, memPct: null, diskPct: null, dev: [], stacks: [], containers: null };
  if (process.platform === 'linux') {
    try {
      const mi = Object.fromEntries(fs.readFileSync('/proc/meminfo', 'utf8').split('\n').map((l) => l.split(/:\s+/)).filter((x) => x.length === 2).map(([k, v]) => [k, parseInt(v, 10)]));
      out.memPct = Math.round(100 * (1 - mi.MemAvailable / mi.MemTotal));
    } catch {}
  } else {
    const v = await run('vm_stat', []);
    const page = Number(/page size of (\d+)/.exec(v.stdout)?.[1] ?? 16384), n = (k) => Number(new RegExp(`${k}:\\s+(\\d+)`).exec(v.stdout)?.[1] ?? 0);
    const used = (n('Pages active') + n('Pages wired down') + n('Pages occupied by compressor')) * page;
    if (used) out.memPct = Math.round((100 * used) / os.totalmem());
  }
  const df = await run('df', ['-k', HOME]);
  out.diskPct = Number(/(\d+)%/.exec(df.stdout.split('\n')[1] ?? '')?.[1] ?? NaN) || null;

  // Rootless Docker answers on the user's own socket; Docker Desktop and OrbStack on the default one.
  const sock = process.env.XDG_RUNTIME_DIR && path.join(process.env.XDG_RUNTIME_DIR, 'docker.sock');
  const env = sock && fs.existsSync(sock) ? { ...baseEnv(), DOCKER_HOST: `unix://${sock}` } : baseEnv();
  const dk = await run(bin('docker'), ['ps', '--format', '{{.Names}}'], { env, timeout: 6000 });
  if (!dk.err) {
    const names = dk.stdout.split('\n').filter(Boolean);
    out.containers = names.length;
    out.stacks = names.map((n) => /^supabase_db_(.+)$/.exec(n)?.[1]).filter(Boolean).sort();
  }

  // Dev servers: node, next, vite, bun or deno listening on a port, other than this panel's own process.
  const DEV = /^(node|next-server|next|vite|bun|deno)/;
  const seen = new Set();
  const add = (pid, port, cmd) => {
    if (pid === process.pid || !DEV.test(cmd) || seen.has(port)) return;
    seen.add(port);
    let cwd = null;
    try { cwd = tilde(fs.readlinkSync(`/proc/${pid}/cwd`)); } catch {}
    out.dev.push({ port, pid, cwd });
  };
  if (process.platform === 'linux') {
    const ss = await run('ss', ['-ltnpH']);
    for (const l of ss.stdout.split('\n')) {
      const port = Number(/:(\d+)\s/.exec(l.split(/\s+/).slice(3, 4).join(' ') + ' ')?.[1]);
      const m = /users:\(\("([^"]+)",pid=(\d+)/.exec(l);
      if (port && m) add(Number(m[2]), port, m[1]);
    }
  } else {
    const ls = await run('lsof', ['-nP', '-iTCP', '-sTCP:LISTEN', '-Fpcn']);
    let pid = null, cmd = '';
    for (const l of ls.stdout.split('\n')) {
      if (l[0] === 'p') pid = Number(l.slice(1)); else if (l[0] === 'c') cmd = l.slice(1);
      else if (l[0] === 'n') { const port = Number(/:(\d+)$/.exec(l)?.[1]); if (port) add(pid, port, cmd); }
    }
    if (out.dev.length) {
      const cw = await run('lsof', ['-a', '-d', 'cwd', '-p', out.dev.map((d) => d.pid).join(','), '-Fpn']);
      let p = null;
      for (const l of cw.stdout.split('\n')) { if (l[0] === 'p') p = Number(l.slice(1)); else if (l[0] === 'n') { const d = out.dev.find((x) => x.pid === p); if (d) d.cwd = tilde(l.slice(1)); } }
    }
  }
  out.dev.sort((a, b) => a.port - b.port);
  return out;
}

// ---------------------------------------------------------------- collect

export async function collect() {
  const profiles = claudeProfiles(), learned = new Map();
  for (const p of profiles) statusSnapshots(p, learned);
  let usage = {};
  try { usage = JSON.parse(fs.readFileSync(USAGE_FILE, 'utf8')); } catch {}
  const [procs, claude, status] = await Promise.all([
    processes().catch(() => null),
    Promise.all(profiles.map((p) => claudeProfile(p, learned, usage).catch((e) => ({ engine: 'claude', id: p.id, dir: tilde(p.dir), error: String(e?.message ?? e).slice(0, 200), sessions: [] })))),
    machineStatus().catch(() => null),
  ]);
  // Keep where each transcript was read to; forget ones not seen for a day.
  for (const [k, v] of Object.entries(usage)) if (Date.now() - (v.seen ?? 0) > DAY) delete usage[k];
  try { fs.mkdirSync(path.dirname(USAGE_FILE), { recursive: true }); fs.writeFileSync(USAGE_FILE + '.tmp', JSON.stringify(usage)); fs.renameSync(USAGE_FILE + '.tmp', USAGE_FILE); } catch {}
  const open = procs ? new Set(procs.flatMap((x) => x.open)) : null;
  const codex = codexHomes().map((h) => { try { return codexHome(h, open); } catch (e) { return { engine: 'codex', id: path.basename(h).slice(1), dir: tilde(h), error: String(e?.message ?? e).slice(0, 200), sessions: [] }; } });
  const muse = museHome();
  const rosterPids = new Set(claude.flatMap((a) => (a.sessions ?? []).map((x) => x.pid).filter(Boolean)));
  const sorted = procs ? sortProcesses(procs, rosterPids, open) : { bridges: [], workers: [], others: [] };
  // A bridge belongs to its profile's account.
  for (const a of claude) a.bridges = sorted.bridges.filter((b) => b.profile === a.id);
  // How each session is reached. A Remote Control session runs `--sdk-url …/sessions/cse_<id>`; its page on claude.ai is
  // /code/session_<id>, where you can type to it.
  const byPid = new Map((procs ?? []).map((x) => [x.pid, x]));
  for (const a of claude) for (const x of a.sessions ?? []) {
    if (x.web) continue;
    const sdk = (byPid.get(x.pid)?.args ?? []).find((v) => /\/v1\/code\/sessions\/cse_\w+/.test(v));
    if (sdk) Object.assign(x, { via: x.via === 'background' ? 'background' : 'remote', web: `https://claude.ai/code/session_${/cse_(\w+)/.exec(sdk)[1]}` });
  }
  // A running Muse is a session of the Muse account.
  if (muse) muse.sessions = sorted.others.filter((o) => o.engine === 'muse').map((o) => ({ engine: 'muse', id: String(o.pid), pid: o.pid, name: 'muse', live: true,
    state: 'working', cwd: o.cwd, startedAt: o.since, lastAt: o.since, doing: o.args }));
  return { ok: true, host: os.hostname(), platform: process.platform, at: Date.now(), accounts: [...claude, ...codex, ...(muse ? [muse] : [])], fleet: fleetState(),
           workers: sorted.workers, others: sorted.others.filter((o) => !(muse && o.engine === 'muse')), scanned: Boolean(procs), status };
}

// ---------------------------------------------------------------- launch

const MODELS = new Set(['', 'opus', 'opus[1m]', 'sonnet', 'haiku', 'fable']);
const EFFORTS = new Set(['low', 'medium', 'high', 'xhigh', 'max']);
const FLEET_TASK = /^(#?\d{1,7})$/;

function folder(cwd) {
  let c = typeof cwd === 'string' ? cwd.trim() : '';
  if (c === '~' || c.startsWith('~/')) c = path.join(HOME, c.slice(1));
  c = path.resolve(c || HOME);
  try { if (!fs.statSync(c).isDirectory()) return null; } catch { return null; }
  return c;
}

/** Start a program detached from this process, its output in a log of its own. One that fails within the first few
 *  seconds (a model its CLI is too old for, a sign-in gone) says why; one still running then, or done cleanly, started. */
function detached(cmd, args, { cwd, env, log, settleMs = 6000 }) {
  fs.mkdirSync(path.dirname(log), { recursive: true });
  const fd = fs.openSync(log, 'a');
  const child = spawn(cmd, args, { cwd, env, detached: true, stdio: ['ignore', fd, fd] });
  fs.closeSync(fd);
  return new Promise((resolve) => {
    let timer = null;
    child.once('error', (e) => { clearTimeout(timer); resolve({ ok: false, error: String(e.message).slice(0, 200) }); });
    child.once('exit', (code) => {
      clearTimeout(timer);
      if (code === 0) return resolve({ ok: true, pid: child.pid, log: tilde(log) });
      const why = tail(log, 8192).reverse().find((l) => /error|denied|not found|requires|limit|failed/i.test(l)) ?? `it stopped (exit ${code})`;
      resolve({ ok: false, error: oneLine(why.replace(/^ERROR:\s*/, ''), 300) });
    });
    child.once('spawn', () => { timer = setTimeout(() => { child.unref(); resolve({ ok: true, pid: child.pid, log: tilde(log) }); }, settleMs); });
  });
}

/** Start an agent. `a` = { engine: claude|codex|muse|fleet, profile, cwd, prompt, name, model }. Every value is checked here too. */
export async function launch(a) {
  const prompt = typeof a?.prompt === 'string' ? a.prompt.trim() : '';
  if (!prompt || prompt.length > 20_000) return { ok: false, error: 'Write a prompt (up to 20,000 characters).' };
  const name = String(a.name ?? '').trim();
  if (name && !/^[\w .-]{1,60}$/.test(name)) return { ok: false, error: 'A name can use letters, digits, spaces, dots and dashes, 60 at most.' };
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  const logs = path.join(HOME, '.fleet-panel', 'logs');

  if (a.engine === 'claude') {
    const p = claudeProfiles().find((x) => x.id === a.profile);
    if (!p) return { ok: false, error: `There is no Claude profile "${a.profile}" here.` };
    const model = a.model ?? '', effort = a.effort ?? '';
    if (!MODELS.has(model)) return { ok: false, error: 'Pick a model from the list.' };
    if (effort && !EFFORTS.has(effort)) return { ok: false, error: 'Pick an effort from the list.' };
    const cwd = folder(a.cwd);
    if (!cwd) return { ok: false, error: `There is no folder at ${a.cwd}.` };
    const args = ['--bg'];
    if (name) args.push('--name', name);
    if (model) args.push('--model', model);
    if (effort) args.push('--effort', effort);
    args.push('--', prompt);
    const r = await run(bin('claude'), args, { cwd, env: profileEnv(p), timeout: 30_000 });
    const id = `${r.stdout}\n${r.stderr}`.match(/backgrounded · (\w+)/)?.[1];
    return id ? { ok: true, id, cwd: tilde(cwd) } : { ok: false, error: oneLine(r.stderr || r.stdout || r.err?.message, 300) || 'claude --bg failed' };
  }

  if (a.engine === 'codex') {
    const home = codexHomes().find((h) => path.basename(h).slice(1) === a.profile) ?? codexHomes()[0];
    if (!home) return { ok: false, error: 'Codex is not set up on this machine.' };
    const cwd = folder(a.cwd);
    if (!cwd) return { ok: false, error: `There is no folder at ${a.cwd}.` };
    const env = { ...baseEnv(), CODEX_HOME: home };
    return detached(bin('codex'), ['exec', '--sandbox', 'workspace-write', '--skip-git-repo-check', '-C', cwd, prompt],
      { cwd, env, log: path.join(logs, `codex-${stamp}.log`) });
  }

  if (a.engine === 'muse') {
    if (!museHome()) return { ok: false, error: 'Muse is not set up on this machine.' };
    const cwd = folder(a.cwd);
    if (!cwd) return { ok: false, error: `There is no folder at ${a.cwd}.` };
    const file = path.join(HOME, '.fleet-panel', 'prompts', `muse-${stamp}.md`);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, prompt);
    return detached(bin('muse'), ['exec', '--prompt-file', file, '--yolo', '--user-input-auto-resolve', '--workspace', cwd],
      { cwd, env: baseEnv(), log: path.join(logs, `muse-${stamp}.log`) });
  }

  if (a.engine === 'fleet') {
    const kit = fleetDir();
    if (!kit) return { ok: false, error: 'The fleet kit is not on this machine.' };
    // fleet-workers --dispatch runs chai-agent detached and returns at once; it takes an issue number or a prompt.
    const task = FLEET_TASK.test(prompt) ? prompt.replace('#', '') : prompt;
    const r = await run(path.join(kit, 'fleet-workers'), ['--dispatch', task], { cwd: kit, timeout: 120_000 });
    return r.err ? { ok: false, error: oneLine(r.stderr || r.stdout || r.err.message, 300) } : { ok: true, said: oneLine(r.stdout, 300) };
  }

  return { ok: false, error: 'Pick Claude, Codex, Muse or the fleet.' };
}

// ---------------------------------------------------------------- one session up close: what it is doing, and a message to it

/** A Claude session's transcript, by profile and id. */
function findTranscript(profile, sessionId) {
  if (!/^[0-9a-f-]{36}$/.test(String(sessionId))) return null;
  const p = claudeProfiles().find((x) => x.id === profile);
  return p ? transcriptOf(p, sessionId, null) : null;
}

/** The end of a session's conversation as a list: what you asked, what it said, and each step it took, newest last. */
export function peek(a) {
  const items = [];
  const push = (kind, text, at) => { if (text) items.push({ kind, text: kind === 'step' ? oneLine(text, 200) : keepLines(text, 6000), at }); };
  if (a.engine === 'codex') {
    const home = codexHomes().find((h) => path.basename(h).slice(1) === a.profile) ?? codexHomes()[0];
    const f = home && recentRollouts(home, 30 * DAY).map((r) => r.f).find((f) => parse(head(f))?.payload?.id === a.sessionId);
    if (!f) return { ok: false, error: 'Its log is gone.' };
    for (const l of tail(f, 512 * 1024)) {
      const d = parse(l), p = d?.payload ?? {}, at = Date.parse(d?.timestamp) || null;
      const ask = codexText(d, 'user'), said = codexText(d, 'assistant');
      // The same words can come twice, as an event and as a response item: keep one.
      if (ask && items.at(-1)?.text !== ask) push('ask', ask, at);
      else if (said && items.at(-1)?.text !== said) push('said', said, at);
      else if (p.type === 'function_call') {
        let x = {};
        try { x = JSON.parse(p.arguments ?? '{}'); } catch {}
        const cmd = Array.isArray(x.command) ? x.command.join(' ') : x.cmd ?? x.command;
        push('step', cmd ? `Run ${String(cmd).split('\n')[0]}` : p.name === 'apply_patch' ? 'Edit files' : p.name, at);
      }
    }
  } else {
    const f = findTranscript(a.profile, a.sessionId);
    if (!f) return { ok: false, error: 'Its conversation is not saved here.' };
    for (const l of tail(f, 768 * 1024)) {
      const d = parse(l);
      if (!d || d.isSidechain) continue;
      const at = Date.parse(d.timestamp) || null, ask = askOf(d);
      if (ask) { push('ask', ask, at); continue; }
      if (d.type !== 'assistant' || !Array.isArray(d.message?.content)) continue;
      for (const c of d.message.content) {
        if (c.type === 'text' && c.text?.trim()) push('said', c.text.trim(), at);
        else if (c.type === 'tool_use') push('step', stepOf(c.name, c.input ?? {}), at);
      }
    }
  }
  return { ok: true, items: items.slice(-60) };
}

const shq = (v) => `'${String(v).replace(/'/g, `'\\''`)}'`;
/** The Claude Code version a process runs: the versions/<x.y.z> program it was started from. */
async function versionOf(pid) {
  let exe = '';
  if (process.platform === 'linux') { try { exe = fs.readlinkSync(`/proc/${pid}/exe`); } catch {} }
  else exe = (await run('lsof', ['-a', '-p', String(pid), '-d', 'txt', '-Fn'])).stdout.split('\n').find((l) => /\/versions\/[\d.]+/.test(l)) ?? '';
  return /\/versions\/([\d.]+)/.exec(exe)?.[1] ?? null;
}
/** Your next message to a running session: a Remote Control session through its claude.ai session, a background job the way
 *  agent view's reply does (`claude --resume <id> -- <text>` in a terminal of its own, until the message is in its
 *  transcript). A plain terminal session or a desktop-app one takes input only where it runs. */
export async function send(a) {
  const text = typeof a?.text === 'string' ? a.text.trim() : '';
  if (!text || text.length > 20_000) return { ok: false, error: 'Write a message (up to 20,000 characters).' };
  const p = claudeProfiles().find((x) => x.id === a.profile);
  if (!p) return { ok: false, error: 'There is no such account here.' };
  const roster = await run(bin('claude'), ['agents', '--json', '--all'], { env: profileEnv(p) });
  let row = null;
  try { row = JSON.parse(roster.stdout).find((r) => r.sessionId === a.sessionId); } catch {}
  if (!row) return { ok: false, error: 'That session is not listed any more.' };
  if (!row.pid) return { ok: false, error: 'It is not running.' };
  // A Remote Control session (`--sdk-url …/sessions/cse_<id>`) takes a message from any client of its claude.ai session:
  // `claude -p --cloud session_<id> <text>`, run as the account that owns it.
  const argsOf = process.platform === 'linux'
    ? (() => { try { return fs.readFileSync(`/proc/${row.pid}/cmdline`, 'utf8').split('\0'); } catch { return []; } })()
    : (await run('ps', ['-ww', '-o', 'args=', '-p', String(row.pid)])).stdout.split(' ');
  const cse = remoteId(sessionRecord(p, row.pid)?.bridgeSessionId)?.slice('session_'.length)
    ?? argsOf.map((v) => /\/v1\/code\/sessions\/cse_(\w+)/.exec(v)?.[1]).find(Boolean);
  if (cse) {
    if (text.startsWith('-')) return { ok: false, error: 'Start it with a word, not a dash.' };
    const r = await run(bin('claude'), ['-p', '--cloud', `session_${cse}`, text], { env: profileEnv(p), cwd: row.cwd ?? HOME, timeout: 60_000 });
    return /Sent to cloud session/.test(r.stdout + r.stderr) ? { ok: true } : { ok: false, error: oneLine(r.stderr || r.stdout || r.err?.message, 300) || 'It did not take the message.' };
  }
  if (row.kind !== 'background') return { ok: false, error: 'Type /rc in its terminal once, then it takes messages here.' };
  if (/^[/!]/.test(text)) return { ok: false, error: 'Start it with a word: a / or ! message is not sent this way.' };
  // A job still running an older Claude Code than the one installed (it updates itself) never gets a message the new one
  // hands it. `claude respawn` restarts it on the current one, conversation kept: done for an idle job, never mid-work.
  const now = /^([\d.]+)/.exec((await run(bin('claude'), ['--version'])).stdout.trim())?.[1];
  const its = await versionOf(row.pid);
  if (now && its && its !== now) {
    if (row.status === 'busy' || row.state === 'working') return { ok: false, error: `It runs Claude Code ${its}, older than ${now}: it can take messages once it is idle and respawned.` };
    const r = await run(bin('claude'), ['respawn', row.id], { env: profileEnv(p), timeout: 60_000 });
    if (r.err) return { ok: false, error: `It runs Claude Code ${its}; respawning it on ${now} failed: ${oneLine(r.stderr || r.err.message, 200)}` };
    for (let i = 0; i < 20; i++) {
      await new Promise((ok) => setTimeout(ok, 500));
      try { const again = JSON.parse((await run(bin('claude'), ['agents', '--json', '--all'], { env: profileEnv(p) })).stdout).find((x) => x.id === row.id); if (again?.pid && again.pid !== row.pid) { row = again; break; } } catch {}
    }
    const id = remoteId(sessionRecord(p, row.pid)?.bridgeSessionId);
    if (id) {
      const c = await run(bin('claude'), ['-p', '--cloud', id, text], { env: profileEnv(p), cwd: row.cwd ?? HOME, timeout: 60_000 });
      if (/Sent to cloud session/.test(c.stdout + c.stderr)) return { ok: true, respawned: now };
    }
  }
  const file = transcriptOf(p, row.sessionId, row.cwd);
  if (!file) return { ok: false, error: 'It has no saved conversation yet.' };
  const argv = [bin('claude'), '--resume', row.sessionId, '--', text];
  const [cmd, args] = process.platform === 'linux' ? ['script', ['-q', '-e', '-c', argv.map(shq).join(' '), '/dev/null']] : ['script', ['-q', '/dev/null', ...argv]];
  const child = spawn(cmd, args, { cwd: row.cwd ?? HOME, env: { ...profileEnv(p), TERM: 'xterm-256color' }, stdio: ['ignore', 'pipe', 'pipe'], detached: true });
  let exited = false, out = '';
  child.stdout.on('data', (d) => { out = (out + d).slice(-4000); });
  child.stderr.on('data', (d) => { out = (out + d).slice(-4000); });
  child.on('exit', () => { exited = true; });
  child.on('error', () => { exited = true; });
  // Its words as Claude Code wrote them down: queued while it works, or as the next prompt (a string, or text blocks).
  const head = text.slice(0, 200);
  const textOf = (c) => (typeof c === 'string' ? c : Array.isArray(c) ? c.filter((x) => x?.type === 'text').map((x) => x.text).join('\n') : '');
  const has = (d) => (d.type === 'queue-operation' && String(d.content ?? '').includes(head)) || (d.type === 'user' && textOf(d.message?.content).includes(head))
    || (d.type === 'attachment' && d.attachment?.type === 'queued_command' && String(d.attachment.prompt ?? '').includes(head));
  let seen = false;
  for (const end = Date.now() + 60_000; !seen && Date.now() < end; await new Promise((r) => setTimeout(r, 400))) {
    seen = tail(file, 512_000).some((l) => { const d = parse(l); return d && has(d); });
    if (exited && !seen) break;
  }
  await new Promise((r) => setTimeout(r, seen ? 800 : 0));
  try { process.kill(-child.pid, 'SIGTERM'); } catch {}
  if (seen) return { ok: true };
  const said = out.replace(/\x1b\[[0-9;?<>]*[A-Za-z]|\x1b\][^\x07]*\x07|[\x00-\x08\x0e-\x1f]/g, '').replace(/\s+/g, ' ').trim().slice(-200);
  return { ok: false, error: `It did not take the message.${said ? ` Claude Code said: ${said}` : ''}` };
}

/** Close an open session; its conversation is kept. A background job: `claude stop`. A terminal or Remote Control session:
 *  SIGTERM, which Claude Code takes as a clean exit, to a pid read fresh from the roster that is still Claude Code, and never
 *  one this process runs under. A desktop-app session closes in the app. */
export async function close(a) {
  const p = claudeProfiles().find((x) => x.id === a?.profile);
  if (!p) return { ok: false, error: 'There is no such account here.' };
  const roster = await run(bin('claude'), ['agents', '--json', '--all'], { env: profileEnv(p) });
  let row = null;
  try { row = JSON.parse(roster.stdout).find((r) => r.sessionId === a.sessionId); } catch {}
  if (!row) return { ok: false, error: 'It is not listed any more.' };
  if (row.kind === 'background' && /^[0-9a-f]{8}$/.test(row.id ?? '')) {
    const r = await run(bin('claude'), ['stop', row.id], { env: profileEnv(p) });
    return r.err ? { ok: false, error: oneLine(r.stderr || r.err.message, 300) } : { ok: true };
  }
  if (!Number.isInteger(row.pid) || row.pid <= 1) return { ok: false, error: 'It is not running.' };
  if (sessionRecord(p, row.pid)?.entrypoint === 'claude-desktop') return { ok: false, error: 'Close it in the Claude app.' };
  for (let pid = process.pid, i = 0; pid > 1 && i < 20; i++) {
    if (pid === row.pid) return { ok: false, error: 'The panel runs inside that session: closing it would stop the panel.' };
    pid = Number((await run('ps', ['-o', 'ppid=', '-p', String(pid)])).stdout.trim());
  }
  const cmd = (await run('ps', ['-o', 'args=', '-p', String(row.pid)])).stdout.trim();
  if (!/(^|\/)claude(\s|$)|\/claude\/versions\/|MacOS\/claude/.test(cmd)) return { ok: false, error: 'Its process is no longer Claude Code, so it was left alone.' };
  try { process.kill(row.pid, 'SIGTERM'); } catch (e) { return { ok: false, error: `Could not close it (${e.code}).` }; }
  return { ok: true };
}

/** Take a closed background job's record away (`claude rm`, which keeps its transcript and refuses when its worktree holds
 *  unsaved work). Any other closed session has no record to take: the panel just stops showing it. */
export async function archive(a) {
  const p = claudeProfiles().find((x) => x.id === a?.profile);
  if (!p || !/^[0-9a-f]{8}$/.test(a?.id ?? '')) return { ok: true };
  const r = await run(bin('claude'), ['rm', a.id], { env: profileEnv(p) });
  return r.err && !/not found|no such/i.test(r.stderr) ? { ok: false, error: oneLine(r.stderr || r.err.message, 300) } : { ok: true };
}

// ---------------------------------------------------------------- run over SSH: `FLEET_DO=collect node --input-type=module -`

if (process.env.FLEET_DO) {
  const args = process.env.FLEET_ARGS ? JSON.parse(Buffer.from(process.env.FLEET_ARGS, 'base64').toString('utf8')) : {};
  const todo = { collect: () => collect(), launch: () => launch(args), peek: () => peek(args), send: () => send(args),
                 close: () => close(args), archive: () => archive(args) }[process.env.FLEET_DO];
  const r = todo ? await todo() : { ok: false, error: 'unknown FLEET_DO' };
  process.stdout.write(JSON.stringify(r));
}
