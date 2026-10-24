// Agent control panel: a local browser view over every Claude Code session on this machine.
//
// Data in:   `claude agents --json --all` (the roster) + user-level `type: "http"` hooks (live events).
// Decisions: a PermissionRequest hook is held open here and answered from the browser. The terminal
//            dialog stays up in parallel, so whichever answers first wins and nothing ever waits on us;
//            a terminal answer leaves our request open, so PostToolUse and the roster retire the card.
// Control:   interrupt = a flag the agent's next PreToolUse hook answers with continue:false (+ deny);
//            end = `claude stop` (background) or SIGTERM to the roster's pid (terminal); remove = `claude rm`.
// Exposure:  127.0.0.1 only; Host is checked (DNS rebinding), browser calls need the per-start token
//            and a same-origin Origin (CSRF), and hook calls must carry no Origin (only Claude Code posts them).

import http from 'node:http';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { execFile, spawn } from 'node:child_process';
import { createWorld, claudeArgs, readAnswer } from './world.mjs';
import { findRemoved } from './versions.mjs';
import { readChat } from './history.mjs';
import { UNTRUSTED, trustKey, isTrusted, trustScript } from './trust.mjs';
import { prefsPath } from './cli.mjs';
import { createHub } from './hub.mjs';
import { createHubSync } from './hub-sync.mjs';
import { createHubOrders } from './hub-orders.mjs';
import { admit, takeBuild } from './web-orders.mjs';
import { workerOf, markStarted, rolledBack, checkForUpdate, recordCheck, lastCheck } from './update.mjs';
import { checkClone, updateClone, appFor } from './clone-update.mjs';
import { createFleet } from './fleet/fleet.mjs';
import { createTerms } from './fleet/term.mjs';

// `node server.mjs <command>` runs the CLI (cli.mjs) and never starts the server.
if (process.argv.length > 2) { const { main } = await import('./cli.mjs'); process.exit(await main(process.argv.slice(2))); }

const PORT = Number(process.env.ACP_PORT ?? 4747);
const DAEMON = process.env.ACP_DAEMON === '1';
// Run from an installed worker (`worker/<sha>/`), this is that version; from a clone of the repo it is null.
const WORKER = workerOf(path.dirname(fileURLToPath(import.meta.url)));
// The first update check waits ten minutes, so a login or a restart is not slowed by a download; then once a day,
// looked at every hour.
const UPDATE_FIRST_MS = 600_000, UPDATE_MS = 86_400_000, UPDATE_TICK_MS = 3_600_000;
// Run from a clone, the panel only says when the hub has a newer release: a minute after start, then every 6 hours.
const APP_UPDATE_FIRST_MS = 60_000, APP_UPDATE_MS = 6 * 3_600_000;
const REPO = path.dirname(fileURLToPath(import.meta.url));
const RETRY_MS = Number(process.env.ACP_RETRY_MS ?? 5000);
const ROSTER_MS = 3000;
// A background service must not cost battery while nobody looks: under ACP_DAEMON with no page connected, the roster
// is read once a minute and the version check waits. A page that connects brings both back at once.
const IDLE_ROSTER_MS = 60_000;
// Just under the hook's own timeout (hooks.example.json: 3600 s); past it Claude Code drops the request anyway.
const HOLD_MS = Number(process.env.ACP_HOLD_MS ?? 3590_000);
// A reload (yours, or the app's when a health check is slow) leaves the panel unwatched for a moment. Held requests
// wait this long for a page to come back: let go, a background session's plan can't be answered from here again.
const RECONNECT_MS = Number(process.env.ACP_RECONNECT_MS ?? 15_000);
const SUBAGENT_LINGER_MS = 600_000;
const STOP_TTL_MS = 15 * 60_000;
const STOP_REASON = 'Interrupted from the agent panel.';
const REFUSE_WINDOW_MS = 5000;
const MAX_BODY = 2_000_000;
// What the launcher may pass to `claude --bg`. bypassPermissions is deliberately not offered.
const MODELS = new Set(['', 'opus', 'opus[1m]', 'sonnet', 'haiku', 'fable']);
const EFFORTS = new Set(['low', 'medium', 'high', 'xhigh', 'max']); // --effort's levels
const MODES = new Set(['', 'default', 'acceptEdits', 'plan', 'auto']);
// The modes an approved plan may switch to: the terminal's own plan-approval choices.
const PLAN_MODES = new Set(['default', 'acceptEdits', 'auto']);
// Dialogs that are a question or a plan rather than a call with arguments: one is open per agent at a time.
const DIALOGS = new Set(['AskUserQuestion', 'ExitPlanMode']);
const SPAWN_TOOLS = new Set(['Agent', 'Task']);

const TOKEN = crypto.randomBytes(24).toString('hex');
const HOSTS = new Set([`127.0.0.1:${PORT}`, `localhost:${PORT}`]);
const ORIGINS = new Set([...HOSTS].map((h) => `http://${h}`));
// Read per load, so a UI edit shows on refresh without a restart.
const page = () => fs.readFileSync(new URL('./public/index.html', import.meta.url), 'utf8');
const fleetPage = () => fs.readFileSync(new URL('./public/fleet.html', import.meta.url), 'utf8');

/** sessionId → session record (roster fields + what the hooks told us). */
const sessions = new Map();
/** request id → a held PermissionRequest. */
const pending = new Map();
/** Interrupts waiting for the agent's next tool call: `${sessionId}:main` or `${sessionId}:${agentId}` → { at }. */
const stops = new Map();
/** Sessions removed or hidden from the panel. One comes back only if it runs again (a resumed terminal session). */
const dismissed = new Set();
const clients = new Set();
/** Set while no page is connected but one may still come back (RECONNECT_MS). */
let releaseTimer = null;
const watched = () => clients.size > 0 || releaseTimer !== null;
let armed = true;
let rosterError = null;

// A server started from inside a Claude Code session inherits its markers (CLAUDE_CODE_CHILD_SESSION and
// friends); passed on, they make a launched session skip its transcript and stay out of `claude agents`.
// CLAUDE_CONFIG_DIR is the profile's (below).
const baseEnv = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE')));

// ---------------------------------------------------------------- profiles
// A profile is a Claude Code config folder: ~/.claude for plain `claude`, ~/.claude-<name> for one run with
// CLAUDE_CONFIG_DIR=~/.claude-<name>. The panel shows one at a time; the choice is remembered across starts.
const HOME = os.homedir();
// ACP_PREFS gives a second panel (another ACP_PORT) a profile choice of its own.
const PREFS = prefsPath();
// Every machine's agents and accounts in one view (fleet/); its machines are listed in fleet.json beside prefs.json.
const fleet = createFleet({ dir: path.dirname(PREFS), quiet: (id) => { quietIds.add(id); setTimeout(() => quietIds.delete(id), 300_000); } });
const terms = createTerms({ config: () => fleet.config() });
// The terminal in Fleet's page: xterm.js, kept here so the page needs no network for it.
const VENDOR = { 'xterm.js': 'text/javascript; charset=utf-8', 'addon-fit.js': 'text/javascript; charset=utf-8', 'xterm.css': 'text/css; charset=utf-8' };
const HOOK_URL = `127.0.0.1:${PORT}/hook/`;
function listProfiles() {
  let names = [];
  try { names = fs.readdirSync(HOME).filter((n) => n === '.claude' || /^\.claude-[\w.-]{1,40}$/.test(n)).sort(); } catch {}
  const out = [];
  for (const n of names) {
    const dir = path.join(HOME, n);
    try { if (!fs.statSync(dir).isDirectory()) continue; } catch { continue; }
    if (!['settings.json', '.claude.json', 'projects'].some((f) => fs.existsSync(path.join(dir, f)))) continue;
    const plain = n === '.claude'; // plain `claude` keeps its config file in the home folder
    out.push({ id: plain ? 'default' : n.slice('.claude-'.length), dir, plain, config: plain ? path.join(HOME, '.claude.json') : path.join(dir, '.claude.json') });
  }
  return out;
}
/** The command that runs Claude Code on a profile. */
const cliOf = (p) => (p.plain ? 'claude' : `CLAUDE_CONFIG_DIR=~/${path.basename(p.dir)} claude`);
/** What the picker shows: whose account it is, and whether its sessions report to this panel. */
function describeProfile(p) {
  let account = null;
  try { const a = JSON.parse(small(p.config, 8 << 20)).oauthAccount; if (a) account = { email: a.emailAddress ?? null, org: a.organizationName ?? null }; } catch {}
  let connected = false, effort = null;
  try {
    const text = small(path.join(p.dir, 'settings.json'), 4 << 20);
    connected = text.includes(HOOK_URL);
    effort = JSON.parse(text).effortLevel ?? null; // its default for new sessions, which /effort saves there
  } catch {}
  return { id: p.id, label: path.basename(p.dir).slice(1), dir: p.dir, where: p.dir.startsWith(HOME + path.sep) ? '~' + p.dir.slice(HOME.length) : p.dir, account, connected,
           effort: EFFORTS.has(effort) ? effort : null,
           command: cliOf(p) };
}
let profile = null, profileInfo = null, profileGen = 0, polls = 0, configDir = null, childEnv = baseEnv;
function useProfile(p) {
  profile = p;
  profileInfo = describeProfile(p);
  configDir = p.dir;
  childEnv = p.plain ? baseEnv : { ...baseEnv, CLAUDE_CONFIG_DIR: p.dir };
  profileGen++;
}
{
  const all = listProfiles();
  let saved = null;
  try { saved = JSON.parse(small(PREFS, 65536)).profile; } catch {}
  const env = process.env.CLAUDE_CONFIG_DIR ? path.resolve(process.env.CLAUDE_CONFIG_DIR.replace(/^~(?=\/|$)/, HOME)) : null;
  // Picked before, else the one it was started under, else one that already reports here (an app opened from the Dock
  // has no CLAUDE_CONFIG_DIR), else plain `claude`'s.
  useProfile(all.find((p) => p.id === saved) ?? all.find((p) => p.dir === env) ?? all.find((p) => describeProfile(p).connected)
    ?? all.find((p) => p.plain) ?? all[0]
    ?? { id: 'default', dir: path.join(HOME, '.claude'), plain: true, config: path.join(HOME, '.claude.json') });
}
/** A hook event from a session of another profile: not ours to show or answer. */
const otherProfile = (e) => typeof e?.transcript_path === 'string' && !e.transcript_path.startsWith(configDir + path.sep);
/** The profile folder a hook event's session runs under: its transcript lives in that folder's projects/. */
function profileDirOf(e) {
  const tp = typeof e?.transcript_path === 'string' ? e.transcript_path : '';
  const i = tp.indexOf(path.sep + 'projects' + path.sep);
  return i > 0 ? tp.slice(0, i) : configDir;
}
const run = (cmd, args, opts = {}) => new Promise((resolve) =>
  execFile(cmd, args, { timeout: 15_000, maxBuffer: 8_000_000, env: childEnv, ...opts },
    (err, stdout, stderr) => resolve({ err, stdout: String(stdout ?? ''), stderr: String(stderr ?? '') })));
const firstLine = (r) => `${r.stderr}\n${r.stdout}`.split('\n').map((l) => l.trim()).find(Boolean)?.slice(0, 300) || String(r.err?.message ?? 'failed');

// The processes this server runs under. Ending the session it was started from would take the panel down with it.
const hostPids = new Set();
(async () => {
  for (let pid = process.ppid, i = 0; pid > 1 && i < 16; i++) {
    hostPids.add(pid);
    pid = Number((await run('ps', ['-o', 'ppid=', '-p', String(pid)])).stdout.trim());
  }
})();

// ---------------------------------------------------------------- state

function session(id) {
  let s = sessions.get(id);
  if (!s) {
    s = { sessionId: id, name: null, cwd: null, hereCwd: null, rosterCwd: null, kind: null, status: null, state: null, waitingFor: null,
          inRoster: false, lastTool: null, lastMessage: null, note: null, ended: false, subagents: new Map(),
          spawns: [], lastEventAt: Date.now() };
    sessions.set(id, s);
  }
  return s;
}

function summarize(tool, input = {}) {
  const pick = input.command ?? input.file_path ?? input.notebook_path ?? input.pattern ?? input.url
    ?? input.description ?? input.query ?? input.prompt
    ?? Object.values(input).find((v) => typeof v === 'string') ?? '';
  return { tool, text: String(pick).replace(/\s+/g, ' ').slice(0, 240), at: Date.now() };
}

const stopKey = (sessionId, agentId) => `${sessionId}:${agentId ?? 'main'}`;
const isEnded = (s) => s.ended || s.state === 'done' || s.state === 'failed' || s.state === 'stopped';
function clearStops(sessionId) {
  for (const k of stops.keys()) if (k.startsWith(sessionId + ':')) stops.delete(k);
}

/** Record a hook event. Returns the hook's reply: only an interrupted agent's PreToolUse gets one. */
function onHookEvent(e) {
  if (!e?.session_id || quietIds.has(e.session_id)) return null;
  if (dismissed.has(e.session_id)) {
    if (e.hook_event_name === 'SessionEnd' || e.hook_event_name === 'Stop') return null;
    dismissed.delete(e.session_id); // it is running again
  }
  const s = session(e.session_id);
  s.lastEventAt = Date.now();
  if (e.cwd && !s.cwd) s.cwd = e.cwd;
  // Where it is now, for showing its branch only: a session can move (into a worktree, say) after it starts.
  if (!e.agent_id && typeof e.cwd === 'string' && path.isAbsolute(e.cwd) && e.cwd.length < 4096) s.hereCwd = e.cwd;
  if (!e.agent_id && typeof e.permission_mode === 'string') s.mode = e.permission_mode;
  const sub = e.agent_id ? s.subagents.get(e.agent_id) : null;
  let reply = null;

  switch (e.hook_event_name) {
    case 'SubagentStart': {
      // Title it with the task its parent gave it: the Agent call that just went out with the same type.
      const i = s.spawns.findIndex((c) => c.type === e.agent_type);
      const call = i >= 0 ? s.spawns.splice(i, 1)[0] : s.spawns.shift();
      s.subagents.set(e.agent_id, { agentId: e.agent_id, agentType: e.agent_type ?? 'subagent',
                                    title: call?.description ?? null, task: call?.prompt ?? null,
                                    startedAt: Date.now(), lastTool: null, stoppedAt: null });
      break;
    }
    case 'SubagentStop':
      if (sub) { sub.stoppedAt = Date.now(); sub.result = trim(e.last_assistant_message, 1500); }
      stops.delete(stopKey(e.session_id, e.agent_id));
      break;
    case 'PreToolUse': {
      // Interrupted: refuse the call and end the agent's turn. The flag stays until the turn is over, so the
      // other calls of a parallel batch are refused too (continue:false alone lets the call run). A call well
      // after the first refusal belongs to a new turn: a session with a background task never reads idle, so
      // the roster can't be what clears it.
      const key = stopKey(e.session_id, e.agent_id), flag = stops.get(key);
      if (flag?.refusedAt && Date.now() - flag.refusedAt > REFUSE_WINDOW_MS) stops.delete(key);
      else if (flag) {
        flag.refusedAt ??= Date.now();
        reply = { continue: false, stopReason: STOP_REASON,
                  hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: STOP_REASON } };
        // A subagent stopped this way may never send SubagentStop: it is over now.
        if (sub && !sub.stoppedAt) { sub.stoppedAt = Date.now(); sub.result ??= 'Stopped from the panel.'; }
        break;
      }
      const t = summarize(e.tool_name, e.tool_input);
      if (e.agent_id) {
        if (!sub) s.subagents.set(e.agent_id, { agentId: e.agent_id, agentType: e.agent_type ?? 'subagent',
                                                startedAt: Date.now(), lastTool: t, stoppedAt: null });
        else {
          sub.lastTool = t;
          if (sub.stale) { sub.stale = false; sub.stoppedAt = null; } // read as gone from disk, but it is working
        }
      } else s.lastTool = t;
      if (!e.agent_id && SPAWN_TOOLS.has(e.tool_name)) {
        const now = Date.now();
        s.spawns = s.spawns.filter((c) => now - c.at < 120_000).slice(-20);
        s.spawns.push({ type: e.tool_input?.subagent_type ?? 'general-purpose', description: trim(e.tool_input?.description, 120),
                        prompt: trim(e.tool_input?.prompt, 400), at: now });
      }
      break;
    }
    case 'PostToolUse':
    case 'PostToolUseFailure':
    case 'PermissionDenied':
      // The tool ran or was refused without us: the terminal answered a request we still hold.
      // A question comes back with its answers added to the input, so dialogs match on the tool alone.
      for (const p of pending.values()) {
        if (p.sessionId === e.session_id && p.agentId === (e.agent_id ?? null) && p.toolName === e.tool_name
            && (DIALOGS.has(e.tool_name) || JSON.stringify(p.toolInput) === JSON.stringify(e.tool_input ?? {}))) settle(p.id, null, 'answered in the terminal');
      }
      break;
    case 'Notification':
      s.note = { type: e.notification_type ?? null, text: trim(e.message, 200), at: Date.now() };
      break;
    case 'Stop': {
      s.lastMessage = trimMid(e.last_assistant_message, 3000);
      s.lastMessageAt = Date.now();
      s.note = null;
      stops.delete(stopKey(e.session_id, null));
      setTimeout(pollRoster, 1500); // a message waiting for this turn to end can go now
      break;
    }
    case 'SessionEnd':
      s.ended = true;
      settleSubagents(s);
      clearStops(e.session_id);
      break;
  }
  broadcast();
  return reply;
}

const trim = (v, n) => (typeof v === 'string' ? (v.length > n ? v.slice(0, n) + '…' : v) : null);
// Keeps the end too: a long reply usually closes with the question or options it puts to you.
const trimMid = (v, n) => (typeof v === 'string' ? (v.length > n ? v.slice(0, n * 0.4) + '\n\n…\n\n' + v.slice(-n * 0.6) : v) : null);

/** A session that has ended has no running subagents, whether or not their SubagentStop ever arrived. */
function settleSubagents(s) {
  for (const a of s.subagents.values()) a.stoppedAt ??= Date.now();
}

/** `claude agents --json --all`, or { error }. */
async function readRoster() {
  const r = await run('claude', ['agents', '--json', '--all'], { timeout: 5000 });
  if (r.err) return { error: String(r.err.message).slice(0, 200) };
  try { return { list: JSON.parse(r.stdout) }; } catch { return { error: 'claude agents --json returned non-JSON' }; }
}

async function pollRoster() {
  const gen = profileGen;
  const { list, error } = await readRoster();
  if (gen !== profileGen) return; // the profile changed while it read: that roster is the old one's
  if (++polls % 20 === 0) profileInfo = describeProfile(profile); // its hooks or account may have changed outside the panel
  if (error) { rosterError = error; broadcast(); return; }
  rosterError = null;
  const seen = new Set();
  for (const r of list) {
    if (!r.sessionId || quietIds.has(r.sessionId)) continue;
    const over = r.state === 'done' || r.state === 'failed' || r.state === 'stopped';
    if (dismissed.has(r.sessionId)) { if (over) continue; dismissed.delete(r.sessionId); }
    seen.add(r.sessionId);
    const s = session(r.sessionId);
    // The roster moving it (into its worktree, say) is newer than the folder its last hook event named.
    if (r.cwd && r.cwd !== s.rosterCwd) { s.rosterCwd = r.cwd; s.hereCwd = null; }
    Object.assign(s, { id: r.id ?? null, name: r.name ?? null, cwd: r.cwd ?? s.cwd, kind: r.kind ?? null,
                       status: r.status ?? null, state: r.state ?? null, waitingFor: r.waitingFor ?? null,
                       pid: r.pid ?? null, startedAt: r.startedAt ?? null, inRoster: true, leftRosterAt: null });
    if (!over) Object.assign(s, { ended: false, closedByPanel: false }); // running again, e.g. resumed with a reply
    // What it last said comes from the Stop hook; a session that ended its turn before this server started
    // gets it from its transcript instead.
    if (!saidRead.has(s.sessionId)) {
      saidRead.add(s.sessionId);
      s.lastMessage ??= trimMid(lastSaid(s.sessionId), 3000);
      s.mode ??= lastTurn(s.sessionId).mode ?? null;
    }
    const on = ranOn(s.sessionId);
    // Where it works: the folder its transcript last names, which Claude Code writes itself (hook events and the roster
    // only stand in until there is one: the roster gives a stopped job its starting folder back).
    Object.assign(s, { model: on.model ?? null, effort: on.effort ?? null, editTops: on.edits ?? [], ranCwd: on.cwd ?? s.ranCwd ?? null });
    loadSubagents(s);
    if (over) { settleSubagents(s); clearStops(s.sessionId); }
    // An interrupted session that has gone idle is done stopping. (A continue:false ends the turn without a Stop event.)
    const st = stops.get(stopKey(s.sessionId, null));
    if (st && r.status === 'idle' && Date.now() - st.at > 2500) stops.delete(stopKey(s.sessionId, null));
  }
  // Claude Code reads a session as busy while a background task (a dev server, a watch) runs, even when its
  // turn is over and it waits at its prompt. The transcript tells the two apart.
  for (const s of sessions.values()) s.atPrompt = atPrompt(s);
  const tasks = [...sessions.values()].some((s) => s.atPrompt) ? await backgroundTasks() : new Map();
  for (const s of sessions.values()) s.bgTasks = s.atPrompt ? tasks.get(s.pid) ?? 0 : 0;
  if (inbox.size) deliverQueued();
  // Gone from the roster = closed or removed: drop its last status so it can't still read "needs you".
  for (const s of sessions.values()) {
    if (seen.has(s.sessionId) || !s.inRoster) continue;
    Object.assign(s, { inRoster: false, leftRosterAt: Date.now(), ended: true, status: null, waitingFor: null });
    settleSubagents(s);
    clearStops(s.sessionId);
  }
  // Claude Code keeps our request open after the terminal says yes, so a held main-agent request
  // whose session has stopped waiting for two polls running was answered there (the backstop for a
  // PostToolUse that doesn't match, e.g. a command amended in the terminal). Not for subagents: a
  // background subagent can wait on us while its parent session reads as idle.
  for (const p of [...pending.values()]) {
    const s = sessions.get(p.sessionId);
    if (p.agentId || !s?.inRoster || Date.now() - p.createdAt < 4000) continue;
    p.notWaiting = s.status === 'waiting' ? 0 : (p.notWaiting ?? 0) + 1;
    if (p.notWaiting >= 2) settle(p.id, null, 'answered in the terminal');
  }
  broadcast();
}

function prune() {
  const now = Date.now();
  for (const [k, st] of stops) if (now - st.at > STOP_TTL_MS) stops.delete(k);
  while (dismissed.size > 500) dismissed.delete(dismissed.values().next().value);
  for (const [id, s] of sessions) {
    for (const [aid, sub] of s.subagents) if (sub.stoppedAt && now - sub.stoppedAt > SUBAGENT_LINGER_MS) s.subagents.delete(aid);
    // Closed sessions linger 10 minutes; hook-only ones (claude -p, SDK) never enter the roster and go an hour after they go quiet.
    const stale = s.leftRosterAt ? now - s.leftRosterAt > 600_000 : now - s.lastEventAt > 3600_000;
    if (!s.inRoster && stale && ![...pending.values()].some((p) => p.sessionId === id)) sessions.delete(id);
  }
}

// ---------------------------------------------------------------- held permission requests

// A request is held for the profile its session runs under, shown while that profile is on the board, and kept
// through a switch: handing it back would leave a background session's prompt open with no screen to answer it on.
function holdPermission(e, res) {
  const id = crypto.randomUUID(), profileDir = profileDirOf(e);
  const p = { id, sessionId: e.session_id, agentId: e.agent_id ?? null, agentType: e.agent_type ?? null,
              toolName: e.tool_name, toolInput: e.tool_input ?? {}, suggestions: e.permission_suggestions ?? [],
              cwd: e.cwd ?? null, permissionMode: e.permission_mode ?? null, createdAt: Date.now(),
              deadline: Date.now() + HOLD_MS, profileDir, res, timer: null };
  p.timer = setTimeout(() => settle(id, null, 'expired'), HOLD_MS);
  // The hook timed out or its session went away. (A terminal answer does NOT close it: see pollRoster.)
  res.on('close', () => { if (pending.has(id)) settle(id, null, 'closed by Claude Code'); });
  pending.set(id, p);
  if (e.session_id && profileDir === configDir) session(e.session_id).lastEventAt = Date.now();
  broadcast({ type: 'new-request', id });
}
/** The held requests of the profile on the board. */
const shownPending = () => [...pending.values()].filter((p) => p.profileDir === configDir);

/** Answer a held request. `decision` null = no answer, the terminal dialog decides. */
function settle(id, decision, why) {
  const p = pending.get(id);
  if (!p) return false;
  pending.delete(id);
  clearTimeout(p.timer);
  if (!p.res.writableEnded && !p.res.destroyed) {
    p.res.writeHead(200, { 'content-type': 'application/json' });
    p.res.end(decision ? JSON.stringify({ hookSpecificOutput: { hookEventName: 'PermissionRequest', decision } }) : '');
  }
  broadcast({ type: 'settled', id, why });
  return true;
}

function decide(body) {
  const p = pending.get(body?.id);
  if (!p) return { ok: false, error: 'That request is gone: answered in the terminal or expired.' };
  if (p.profileDir !== configDir) return { ok: false, error: 'That request belongs to a profile that is not on the board.' };
  let decision;
  if (body.behavior === 'deny') {
    decision = { behavior: 'deny', message: String(body.message || 'Denied from the agent control panel.') };
  } else if (body.behavior === 'answer') {
    // AskUserQuestion takes its answers as part of its input: { "<question text>": "<label or typed answer>" }.
    if (p.toolName !== 'AskUserQuestion') return { ok: false, error: 'Only a question takes answers.' };
    const asked = new Set((p.toolInput.questions ?? []).map((q) => q.question));
    const a = body.answers;
    if (!a || typeof a !== 'object' || Array.isArray(a)) return { ok: false, error: 'Answers must be an object.' };
    for (const [q, v] of Object.entries(a)) {
      if (!asked.has(q) || typeof v !== 'string' || !v.trim() || v.length > 5000) return { ok: false, error: 'Answer each question it asked, in words.' };
    }
    if (Object.keys(a).length !== asked.size) return { ok: false, error: 'Answer every question before sending.' };
    decision = { behavior: 'allow', updatedInput: { ...p.toolInput, answers: a } };
  } else if (body.behavior === 'approve-plan') {
    // A bare allow leaves the plan dialog open; it takes the plan back as input plus the mode to carry on in.
    if (p.toolName !== 'ExitPlanMode') return { ok: false, error: 'Only a plan can be approved.' };
    if (!PLAN_MODES.has(body.mode)) return { ok: false, error: 'Pick how it should carry on.' };
    decision = { behavior: 'allow', updatedInput: p.toolInput,
                 updatedPermissions: [{ type: 'setMode', mode: body.mode, destination: 'session' }] };
  } else if (body.behavior === 'allow') {
    decision = { behavior: 'allow' };
    if (body.updatedInput !== undefined) {
      if (typeof body.updatedInput !== 'object' || body.updatedInput === null || Array.isArray(body.updatedInput))
        return { ok: false, error: 'The edited input must be a JSON object.' };
      decision.updatedInput = body.updatedInput;
    }
    // "Always allow" adopts the request's own suggestions, never rules from the browser.
    if (body.always && p.suggestions.length) decision.updatedPermissions = p.suggestions;
    // Switch its mode as it carries on, the way an approved plan does. Main agent only: a subagent's request
    // would switch the whole session.
    if (body.mode) {
      if (p.agentId || !RESUME_MODES.has(body.mode)) return { ok: false, error: 'Its mode can only switch to ask, accept edits, plan or auto, on a main-agent request.' };
      decision.updatedPermissions = [...(decision.updatedPermissions ?? []), { type: 'setMode', mode: body.mode, destination: 'session' }];
    }
  } else return { ok: false, error: 'behavior must be allow, deny, answer or approve-plan' };
  settle(p.id, decision, body.behavior);
  return { ok: true };
}

// ---------------------------------------------------------------- launch + stop

/** Start a background session: `claude --bg` with argv only (no shell), every value checked. */
function launch(body) {
  return new Promise((resolve) => {
    const fail = (error) => resolve({ ok: false, error });
    const prompt = typeof body?.prompt === 'string' ? body.prompt.trim() : '';
    if (!prompt || prompt.length > 20_000) return fail('Write a prompt for the agent (up to 20,000 characters).');
    let cwd = typeof body.cwd === 'string' ? body.cwd.trim() : '';
    if (cwd === '~' || cwd.startsWith('~/')) cwd = path.join(os.homedir(), cwd.slice(1));
    cwd = path.resolve(cwd || os.homedir());
    try { if (!fs.statSync(cwd).isDirectory()) throw new Error(); } catch { return fail(`There is no folder at ${cwd}.`); }
    const model = body.model ?? '', mode = body.mode ?? '', effort = body.effort ?? '';
    const name = String(body.name ?? '').trim(), agent = String(body.agent ?? '').trim();
    if (!MODELS.has(model)) return fail('Pick a model from the list.');
    if (effort && !EFFORTS.has(effort)) return fail('Pick an effort level from the list.');
    if (!MODES.has(mode)) return fail('Pick a permission mode from the list.');
    if (name && !/^[\w .-]{1,60}$/.test(name)) return fail('A name can use letters, digits, spaces, dots and dashes, 60 at most.');
    if (agent && !/^[\w:.-]{1,80}$/.test(agent)) return fail('An agent type is a name like code-reviewer or plugin:agent.');
    // Its own worktree: Claude Code makes .claude/worktrees/<name> on a new branch worktree-<name>, and names it when
    // you don't.
    const worktree = body.worktree === true, wtName = String(body.worktreeName ?? '').trim();
    if (worktree) {
      if (wtName && !WORKTREE_NAME.test(wtName)) return fail('A worktree name can use letters, digits, dots, dashes and underscores, 60 at most.');
      const place = gitPlace(cwd);
      if (!place) return fail(`${tilde(cwd)} is not in a git repository, so it can't have a worktree of its own.`);
      if (wtName && fs.existsSync(path.join(place.repo, '.claude', 'worktrees', wtName))) return fail(`There is already a worktree named ${wtName}. Pick another name, or leave it blank for a new one.`);
    }
    const args = ['--bg'];
    if (name) args.push('--name', name);
    if (model) args.push('--model', model);
    if (effort) args.push('--effort', effort);
    if (mode) args.push('--permission-mode', mode);
    if (agent) args.push('--agent', agent);
    if (worktree) args.push('--worktree', ...(wtName ? [wtName] : []));
    args.push('--', prompt);
    run('claude', args, { cwd, timeout: 30_000 }).then((r) => {
      const id = `${r.stdout}\n${r.stderr}`.match(/backgrounded · (\w+)/)?.[1];
      if (id) { setTimeout(pollRoster, 400); return resolve({ ok: true, id, cwd }); }
      const why = firstLine(r);
      if (UNTRUSTED.test(why)) return resolve({ ok: false, untrusted: true,
        error: `The ${profileLabel()} profile hasn't trusted ${tilde(trustKey(cwd, gitPlace(cwd)?.top))} yet. Claude Code asks once per folder.` });
      fail(why);
    });
  });
}

// ---------------------------------------------------------------- interrupt, end, remove

const BG_ID = /^[0-9a-f]{8}$/;
const liveSession = (body) => {
  const s = sessions.get(String(body?.sessionId ?? ''));
  return s && !dismissed.has(s.sessionId) ? s : null;
};

/** Stop an agent at its next tool call, like Esc. The session (or, for a subagent, its parent) stays open. */
function interrupt(body) {
  const s = liveSession(body);
  if (!s || isEnded(s)) return { ok: false, error: 'That session is not running.' };
  const aid = body.agentId == null ? null : String(body.agentId);
  const now = Date.now();
  if (aid) {
    const a = s.subagents.get(aid);
    if (!a || a.stoppedAt) return { ok: false, error: 'That subagent has already finished.' };
    stops.set(stopKey(s.sessionId, aid), { at: now });
  } else {
    stops.set(stopKey(s.sessionId, null), { at: now });
    for (const a of s.subagents.values()) if (!a.stoppedAt) stops.set(stopKey(s.sessionId, a.agentId), { at: now });
  }
  // What it is asking here right now is refused at once rather than left for its next tool call.
  for (const p of [...pending.values()]) {
    if (p.sessionId !== s.sessionId || (aid && p.agentId !== aid)) continue;
    settle(p.id, { behavior: 'deny', message: STOP_REASON, ...(aid ? {} : { interrupt: true }) }, 'interrupted');
  }
  broadcast();
  return { ok: true };
}

/** End a session; its conversation is kept. Background: `claude stop`. Terminal: SIGTERM, as closing its window would. */
async function end(body) {
  const s = liveSession(body);
  if (!s || !s.inRoster || isEnded(s)) return { ok: false, error: 'That session is not running.' };
  return endSession(s);
}
async function endSession(s) {
  if (s.kind === 'background') {
    if (!BG_ID.test(s.id ?? '')) return { ok: false, error: 'It has no background id to stop.' };
    const r = await run('claude', ['stop', s.id]);
    setTimeout(pollRoster, 400);
    return r.err ? { ok: false, error: firstLine(r) } : { ok: true };
  }
  if (s.kind !== 'interactive') return { ok: false, error: 'Only background and terminal sessions can be ended from here.' };
  // The pid comes from a fresh roster read, never from the browser, and must still be a claude process.
  const { list } = await readRoster();
  const pid = list?.find((r) => r.sessionId === s.sessionId && r.kind === 'interactive')?.pid;
  if (!Number.isInteger(pid) || pid <= 1) return { ok: false, error: 'Could not find its process. It may have closed already.' };
  if (hostPids.has(pid)) return { ok: false, error: 'The panel server runs inside this session. Ending it would stop the panel too.' };
  const cmd = (await run('ps', ['-o', 'command=', '-p', String(pid)])).stdout.trim();
  if (!/(^|\/)claude(\s|$)/.test(cmd)) return { ok: false, error: 'Its process is no longer Claude Code, so it was left alone.' };
  try { process.kill(pid, 'SIGTERM'); } catch (e) { return { ok: false, error: `Could not signal its process (${e.code}).` }; }
  s.closedByPanel = true;
  setTimeout(pollRoster, 600);
  return { ok: true };
}

/** Take a finished session off the list. Background: `claude rm`, which keeps the transcript and refuses
 *  (we never pass --discard-unpushed or --force-remove-worktree) when its worktree has unsaved work.
 *  Anything else has already left `claude agents`, so the panel just stops showing it. */
async function remove(body) {
  const s = liveSession(body);
  if (!s) return { ok: true };
  if (!isEnded(s)) return { ok: false, error: 'It is still running. End it first.' };
  const rm = s.kind === 'background' && s.inRoster;
  if (rm) {
    if (!BG_ID.test(s.id ?? '')) return { ok: false, error: 'It has no background id to remove.' };
    const r = await run('claude', ['rm', s.id]);
    if (r.err) return { ok: false, error: firstLine(r) };
  }
  dismissed.add(s.sessionId);
  sessions.delete(s.sessionId);
  clearStops(s.sessionId);
  broadcast();
  return { ok: true, removed: rm };
}

// ---------------------------------------------------------------- reply

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const RESUME_MODES = new Set(['default', 'acceptEdits', 'plan', 'auto']);

/** The model, effort and permission mode of a session's last turn, read from the tail of its transcript. */
const files = new Map(); // sessionId → its transcript
function transcriptFile(sessionId) {
  if (!UUID.test(sessionId)) return null;
  const hit = files.get(sessionId);
  if (hit && fs.existsSync(hit)) return hit;
  try {
    const root = path.join(configDir, 'projects');
    for (const d of fs.readdirSync(root)) {
      const f = path.join(root, d, sessionId + '.jsonl');
      if (fs.existsSync(f)) { files.set(sessionId, f); return f; }
    }
  } catch {}
  return null;
}
function tailOf(file, bytes) {
  const fd = fs.openSync(file, 'r');
  try {
    const size = fs.fstatSync(fd).size, len = Math.min(size, bytes), buf = Buffer.alloc(len);
    fs.readSync(fd, buf, 0, len, size - len);
    return buf.toString('utf8').split('\n').reverse();
  } finally { fs.closeSync(fd); }
}

// Hooks only tell this server about subagents that start while it runs. Claude Code keeps each one on disk
// (<session>/subagents/agent-<id>.jsonl, and a .meta.json with its type, description and `finished`), so the first
// time a session shows up its subagents come from there: after a restart they are all still listed.
const STALE_SUB_MS = 15 * 60_000; // unfinished but untouched this long: it went with an interrupted turn
function headLine(file, bytes = 65536) {
  const fd = fs.openSync(file, 'r');
  try { const buf = Buffer.alloc(bytes), n = fs.readSync(fd, buf, 0, bytes, 0); return buf.toString('utf8', 0, n).split('\n')[0]; }
  finally { fs.closeSync(fd); }
}
const msgText = (m) => (typeof m?.content === 'string' ? m.content
  : Array.isArray(m?.content) ? m.content.filter((c) => c?.type === 'text').map((c) => c.text).join('\n') : '');
function loadSubagents(s) {
  if (s.subsLoaded) return;
  const file = transcriptFile(s.sessionId);
  if (!file) return; // nothing saved yet: the next poll tries again
  s.subsLoaded = true;
  const dir = path.join(file.slice(0, -'.jsonl'.length), 'subagents');
  let names = [];
  try { names = fs.readdirSync(dir).filter((n) => /^agent-[\w-]{1,80}\.meta\.json$/.test(n)); } catch { return; }
  const found = [];
  for (const n of names) {
    try {
      const id = n.slice('agent-'.length, -'.meta.json'.length), log = path.join(dir, `agent-${id}.jsonl`);
      const meta = JSON.parse(small(path.join(dir, n), 65536)), first = JSON.parse(headLine(log));
      const tail = tailOf(log, 65536).map((l) => { try { return JSON.parse(l); } catch { return null; } }).filter(Boolean);
      const touched = fs.statSync(log).mtimeMs, lastAt = Date.parse(tail.find((d) => d.timestamp)?.timestamp) || touched;
      const said = tail.find((d) => d.type === 'assistant' && msgText(d.message).trim());
      const use = tail.find((d) => d.type === 'assistant' && Array.isArray(d.message?.content) && d.message.content.some((c) => c?.type === 'tool_use'))
        ?.message.content.filter((c) => c?.type === 'tool_use').at(-1);
      const stale = meta.finished !== true && Date.now() - touched > STALE_SUB_MS;
      const over = meta.finished === true || stale;
      // Its report is its last words, or what it handed back with SubagentHandback.
      const report = use?.name === 'SubagentHandback' && typeof use.input?.message === 'string' ? use.input.message : msgText(said?.message);
      found.push({ agentId: id, agentType: typeof meta.agentType === 'string' ? meta.agentType : 'subagent', title: trim(meta.description, 120),
                   task: trim(msgText(first.message), 400), startedAt: Date.parse(first.timestamp) || lastAt,
                   lastTool: use ? { ...summarize(use.name, use.input ?? {}), at: lastAt } : null,
                   stoppedAt: over ? lastAt : null, result: meta.finished === true ? trim(report, 1500) : null, stale });
    } catch {}
  }
  for (const a of found.sort((x, y) => x.startedAt - y.startedAt).slice(-40)) {
    const have = s.subagents.get(a.agentId);
    if (have) { have.title ??= a.title; have.task ??= a.task; have.startedAt = Math.min(have.startedAt, a.startedAt); }
    else s.subagents.set(a.agentId, a);
  }
  s.subagents = new Map([...s.subagents].sort((x, y) => x[1].startedAt - y[1].startedAt));
}

const turns = new Map(); // sessionId → { size, mtimeMs, over }: the last read of its transcript
function turnIsOver(sessionId) {
  const file = transcriptFile(sessionId);
  let st;
  try { st = file && fs.statSync(file); } catch {}
  if (!st) return false;
  const c = turns.get(sessionId);
  if (c && c.size === st.size && c.mtimeMs === st.mtimeMs) return c.over;
  let over = false;
  for (const line of tailOf(file, 256_000)) {
    let d;
    try { d = JSON.parse(line); } catch { continue; }
    if (d.isSidechain) break;
    if (d.type === 'system' && (d.subtype === 'turn_duration' || d.subtype === 'stop_hook_summary')) { over = true; break; }
    if (d.type === 'assistant') { over = d.message?.stop_reason === 'end_turn'; break; }
    if (d.type === 'user') {
      const c0 = d.message?.content, text = typeof c0 === 'string' ? c0 : Array.isArray(c0) && c0[0]?.type === 'text' ? c0[0].text : '';
      over = /^\[Request interrupted by user/.test(text ?? '');
      break;
    }
    if (d.type === 'api-request' || d.type === 'api-request-blob') break; // a model call in flight
  }
  turns.set(sessionId, { size: st.size, mtimeMs: st.mtimeMs, over });
  return over;
}
const saidRead = new Set();
function lastSaid(sessionId) {
  const file = transcriptFile(sessionId);
  if (!file) return null;
  for (const line of tailOf(file, 512_000)) {
    let d;
    try { d = JSON.parse(line); } catch { continue; }
    // A local /command leaves a made-up reply ("No response requested.") that Claude never wrote.
    if (d.isSidechain || d.type !== 'assistant' || !Array.isArray(d.message?.content) || d.message.model === '<synthetic>') continue;
    const text = d.message.content.filter((c) => c.type === 'text').map((c) => c.text).join('\n').trim();
    if (text) return text;
  }
  return null;
}
/** A session's conversation for the panel's chat, newest last; asking for more reads further back. */
async function history(body) {
  const file = transcriptFile(body?.sessionId);
  if (!file) return { ok: false, error: 'Its transcript is not in this profile.' };
  const limit = Number.isInteger(body?.limit) ? Math.min(2000, Math.max(20, body.limit)) : 60;
  return { ok: true, ...(await readChat(file, { limit })) };
}
const atPrompt = (s) => Boolean(s.inRoster && s.status === 'busy' && !isEnded(s)
  && !hasRunningSub(s) && turnIsOver(s.sessionId));

// Shells Claude Code's Bash tool started that outlive the turn: its background tasks. They end with the session.
async function backgroundTasks() {
  const r = await run('ps', ['-A', '-o', 'pid=,ppid=,command=']);
  const n = new Map();
  for (const line of r.stdout.split('\n')) {
    const m = line.match(/^\s*\d+\s+(\d+)\s+(.*)$/);
    if (m && m[2].includes('/shell-snapshots/')) n.set(+m[1], (n.get(+m[1]) ?? 0) + 1);
  }
  return n;
}

function lastTurn(sessionId) {
  const out = {};
  const file = transcriptFile(sessionId);
  if (!file) return out;
  out.file = file;
  for (const line of tailOf(file, 512_000)) {
    if (out.model && out.mode) break;
    let d;
    try { d = JSON.parse(line); } catch { continue; }
    if (d.type === 'user' || d.type === 'assistant') out.talked = true;
    if (!out.model && d.type === 'assistant' && !d.isSidechain && modelOf(d)) Object.assign(out, { model: modelOf(d), effort: effortOf(d) });
    if (!out.mode && typeof d.permissionMode === 'string') Object.assign(out, { mode: d.permissionMode, modeAt: Date.parse(d.timestamp) || 0 });
  }
  return out;
}
// A reply's model as it was asked for: requestedModel keeps a [1m] that the reply's own model name drops.
const modelOf = (d) => [d.requestedModel, d.message?.model].find((m) => typeof m === 'string' && /^[\w.\-[\]]{1,80}$/.test(m)) ?? null;
const effortOf = (d) => (EFFORTS.has(d.effort) ? d.effort : null);
/** The model and effort its replies run on now, for the panel to show: from its newest reply, read again only when the
 *  transcript has grown. Neither changes without a restart, so a tail with no reply in it keeps the last read. Also the
 *  folder of its newest step, which follows it into a worktree it entered mid-session (the roster keeps the folder it
 *  started in), and the checkouts it last edited files in, newest first. */
const EDITS = new Set(['Edit', 'Write', 'MultiEdit', 'NotebookEdit']);
const ran = new Map(); // sessionId → { size, mtimeMs, model, effort, cwd, edits }
function ranOn(sessionId) {
  const file = transcriptFile(sessionId);
  let st;
  try { st = file && fs.statSync(file); } catch {}
  const c = ran.get(sessionId);
  if (!st) return c ?? {};
  if (c && c.size === st.size && c.mtimeMs === st.mtimeMs) return c;
  const out = { size: st.size, mtimeMs: st.mtimeMs, model: c?.model ?? null, effort: c?.effort ?? null, cwd: c?.cwd ?? null };
  let model = false, cwd = false;
  const edits = [];
  for (const line of tailOf(file, c ? 128_000 : 512_000)) {
    let d;
    try { d = JSON.parse(line); } catch { continue; }
    if (d.isSidechain) continue;
    if (!cwd && typeof d.cwd === 'string' && path.isAbsolute(d.cwd)) { out.cwd = d.cwd; cwd = true; }
    if (d.type !== 'assistant') continue;
    if (!model && modelOf(d)) { Object.assign(out, { model: modelOf(d), effort: effortOf(d) }); model = true; }
    for (const b of Array.isArray(d.message?.content) ? d.message.content : []) {
      const f = b?.type === 'tool_use' && EDITS.has(b.name) ? b.input?.file_path ?? b.input?.notebook_path : null;
      if (typeof f === 'string' && path.isAbsolute(f)) edits.push(f);
    }
  }
  const tops = edits.map((f) => gitPlace(path.dirname(f))?.top).filter(Boolean);
  out.edits = [...new Set([...tops, ...(c?.edits ?? [])])].slice(0, 4);
  ran.set(sessionId, out);
  return out;
}
/** `opus` or `opus[1m]` names the model it runs on now (claude-opus-5-5[1m]). */
const sameModel = (alias, full) => typeof full === 'string' && full.includes(`-${alias.replace('[1m]', '')}-`) && alias.endsWith('[1m]') === full.endsWith('[1m]');

async function rosterSettles(sessionId, done, ms = 12_000) {
  for (const end = Date.now() + ms; Date.now() < end; await new Promise((r) => setTimeout(r, 700))) {
    const { list } = await readRoster();
    if (list && done(list.find((r) => r.sessionId === sessionId))) return true;
  }
  return false;
}

/** Answer a session that has stopped to ask you something in words. Claude Code has no way to type into a running
 *  terminal session from outside, so it is restarted: stopped (a terminal session closes in its window), then resumed
 *  in the background under the same id with the reply as its next prompt, keeping its name, model, effort and
 *  permission mode unless you picked others. A background job wakes as itself (see below). */
// Sending to a background session: `claude --resume <id> -- <text>` without --bg hands the text to the running job
// as your next message (queued if it is working, as if typed in agent view's peek panel), then attaches. It needs a
// terminal, so it runs under script(1), and the client is closed once the message is in the transcript.
const SEND_MS = 25_000;
const sendingTo = new Set();
async function sendToBackground(s, text) {
  if (sendingTo.has(s.sessionId)) return { ok: false, error: 'Your last message or mode switch is still on its way.' };
  if (/^[/!]/.test(text)) return { ok: false, error: 'A message that starts with / or ! is not sent this way. Start it with a word.' };
  const file = transcriptFile(s.sessionId);
  if (!file) return { ok: false, error: 'It has no saved conversation yet.' };
  sendingTo.add(s.sessionId);
  const p = spawn('script', ['-q', '/dev/null', 'claude', '--resume', s.sessionId, '--', text],
    { cwd: s.cwd, env: { ...childEnv, TERM: 'xterm-256color' }, stdio: ['ignore', 'pipe', 'pipe'], detached: true });
  let out = '';
  p.stdout.on('data', (d) => { out = (out + d).slice(-4000); });
  p.stderr.on('data', (d) => { out = (out + d).slice(-4000); });
  let exited = false;
  p.on('exit', () => { exited = true; });
  p.on('error', () => { exited = true; });
  const has = (d) => (d.type === 'queue-operation' && d.content === text) || (d.type === 'user' && d.message?.content === text)
    || (d.type === 'attachment' && d.attachment?.type === 'queued_command' && d.attachment.prompt === text);
  let seen = false;
  for (const end = Date.now() + SEND_MS; !seen && Date.now() < end; await new Promise((r) => setTimeout(r, 400))) {
    for (const line of tailOf(file, 256_000)) { try { if (has(JSON.parse(line))) { seen = true; break; } } catch {} }
    if (exited && !seen) break;
  }
  await new Promise((r) => setTimeout(r, seen ? 800 : 0));
  try { process.kill(-p.pid, 'SIGTERM'); } catch {}
  sendingTo.delete(s.sessionId);
  if (!seen) return { ok: false, error: 'It did not take your message. ' + (out.replace(/\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07/g, '').replace(/\s+/g, ' ').trim().slice(-160) || 'Try claude attach.') };
  logSent(s, text, 'now');
  setTimeout(pollRoster, 600);
  return { ok: true, sent: true };
}

// A /command. A running background job takes none from outside (claude --resume refuses one), so slash-command.py types
// it at its prompt the way you would in claude attach, only while it is idle there with nothing open. Anything else runs
// it as the prompt it restarts with. Refused here: commands that only open a view or a menu on its screen, which nobody
// would see, and /model and /effort, which also make it your default for new sessions (the menus by the chat box don't).
const SCREEN_ONLY = new Set(['status', 'permissions', 'theme', 'help', 'resume', 'rewind', 'usage', 'cost', 'stats']);
const BARE_SCREEN_ONLY = new Set(['config', 'settings', 'advisor']); // these open one only without arguments
function commandOf(text) {
  const m = /^\/([\w:.-]{1,80})(?: ([^\n\r]{0,400}))?$/.exec(text);
  if (!m) return { error: 'A command is one line: /name, then its arguments (400 characters at most).' };
  const [, name, args = ''] = m;
  if (name === 'model' || name === 'effort') return { error: `Pick its ${name} in the menu by the chat box: /${name} would also make it your default for new sessions.` };
  if (SCREEN_ONLY.has(name) || (BARE_SCREEN_ONLY.has(name) && !args.trim())) return { error: `/${name} opens a view on its screen, which nobody sees in the background. Run it in claude attach.` };
  return { name };
}
const SLASH = fileURLToPath(new URL('./slash-command.py', import.meta.url));
async function runCommand(s, text) {
  if (s.status === 'waiting' || [...pending.values()].some((p) => p.sessionId === s.sessionId)) return { ok: false, error: 'Answer what it is asking first.' };
  if (sendingTo.has(s.sessionId)) return { ok: false, error: 'Your last message or mode switch is still on its way.' };
  const file = transcriptFile(s.sessionId);
  if (!file) return { ok: false, error: 'It has no saved conversation yet.' };
  sendingTo.add(s.sessionId);
  const r = await run('python3', [SLASH, s.id, text, file], { cwd: os.homedir(), timeout: 60_000 });
  sendingTo.delete(s.sessionId);
  let out;
  try { out = JSON.parse(r.stdout.trim().split('\n').pop()); } catch { out = { ok: false, error: firstLine(r) || 'The command did not report back.' }; }
  if (!out.ok) return { ok: false, error: out.error };
  logSent(s, text, 'command', Date.now(), { ran: out.ran ?? null, output: trimMid(out.output || null, 3000), status: out.status ?? null });
  setTimeout(pollRoster, 600);
  broadcast();
  return { ok: true, command: true, ran: out.ran ?? null, status: out.status ?? null };
}

// A terminal session has no way in from outside. One that is working keeps your message here, and once it is free
// (its turn over, no subagent running, nothing waiting on you) the panel replies as it would to an idle one:
// Claude closes in its terminal and carries on in the background with it.
const inbox = new Map(); // sessionId → [{ id, text, at }]
const delivering = new Set();
const hasRunningSub = (s) => [...s.subagents.values()].some((a) => !a.stoppedAt);
const isWorking = (s) => !isEnded(s) && (s.inRoster || !s.kind) && !atPrompt(s)
  && ([...pending.values()].some((p) => p.sessionId === s.sessionId) || hasRunningSub(s)
      || (s.inRoster ? Boolean(s.status) && s.status !== 'idle' : Date.now() - (s.lastTool?.at ?? 0) < 60_000));
async function deliverQueued() {
  for (const [id, q] of inbox) {
    const s = sessions.get(id);
    if (!s || delivering.has(id) || isWorking(s)) continue;
    delivering.add(id);
    // A /command goes in on its own; the messages before it go in together.
    const cut = q.findIndex((m) => m.text.startsWith('/'));
    const batch = q.slice(0, cut === 0 ? 1 : cut < 0 ? q.length : cut);
    const pick = (k) => batch.findLast((m) => m[k])?.[k];
    const mode = pick('mode'), model = pick('model'), effort = pick('effort');
    const r = await reply({ sessionId: id, text: batch.map((m) => m.text).join('\n\n'), fromQueue: true,
                            ...(mode ? { mode } : {}), ...(model ? { model } : {}), ...(effort ? { effort } : {}) });
    delivering.delete(id);
    if (r.ok) {
      q.splice(0, q.length, ...q.filter((m) => !batch.includes(m)));
      if (!q.length) inbox.delete(id);
      if (!r.command) for (const m of batch) logSent(s, m.text, 'once it was done', m.at);
      s.queueError = null;
    } else s.queueError = r.error;
    broadcast();
  }
}
/** What you sent it from here, newest last: the panel's half of the chat. */
function logSent(s, text, how, at = Date.now(), extra = {}) {
  (s.sent ??= []).push({ text: trim(text, 3000), at, how, ...extra });
  if (s.sent.length > 5) s.sent.shift();
}
function unqueue(body) {
  const q = inbox.get(body?.sessionId);
  const i = q ? q.findIndex((m) => m.id === body?.id) : -1;
  if (i < 0) return { ok: false, error: 'It already has that message.' };
  q.splice(i, 1);
  if (!q.length) inbox.delete(body.sessionId);
  broadcast();
  return { ok: true };
}

/** The worktree a session works in, if any: its roster folder or the folder of its newest step, else the worktree its
 *  transcript last says it entered (and didn't leave). */
function worktreeOf(s) {
  const inTree = [s.ranCwd, s.hereCwd, s.cwd].find((c) => /\/\.claude\/worktrees\/[^/]+/.test(c ?? ''));
  if (inTree) return inTree;
  const file = transcriptFile(s.sessionId);
  if (!file) return null;
  for (const line of tailOf(file, 2_000_000)) {
    if (!line.includes('"worktree-state"')) continue;
    try { const p = JSON.parse(line).worktreeSession?.worktreePath; return typeof p === 'string' && fs.existsSync(p) ? p : null; } catch {}
  }
  return null;
}

async function reply(body) {
  const s = liveSession(body);
  const fail = (error) => ({ ok: false, error });
  const text = typeof body?.text === 'string' ? body.text.trim() : '';
  if (!s) return fail('That session is gone.');
  if (!text || text.length > 20_000) return fail('Write a reply (up to 20,000 characters).');
  if (text.startsWith('!')) return fail('A message that starts with ! (a shell command) is not sent from here.');
  const command = text.startsWith('/') ? commandOf(text) : null;
  if (command?.error) return fail(command.error);
  const running = s.inRoster && !isEnded(s);
  if (s.kind !== 'background' && s.kind !== 'interactive') return fail('Only background and terminal sessions take a reply here.');
  if (!UUID.test(s.sessionId)) return fail('It has no session id to resume.');
  if (s.pid != null && hostPids.has(s.pid)) return fail('The panel server runs inside this session, so reply to it in its terminal.');
  // A model or effort you picked, when it differs from what it runs on now. Either takes a restart: a running session
  // changes them only at its own prompt (/model, /effort), and both of those also make it your default for new sessions.
  if (body?.model && !MODELS.has(body.model)) return fail('Pick a model from the list.');
  if (body?.effort && !EFFORTS.has(body.effort)) return fail('Pick an effort level from the list.');
  const now = ranOn(s.sessionId);
  const want = { model: body?.model && !sameModel(body.model, now.model) ? body.model : null,
                 effort: body?.effort && body.effort !== now.effort ? body.effort : null };
  const change = Boolean(want.model || want.effort);
  // A background job still running takes it as your next message, working or not, unless it is to restart. (One that
  // finished and was stopped stays on the list as done, with no process.)
  const bgUp = s.kind === 'background' && s.inRoster && BG_ID.test(s.id ?? '') && !['stopped', 'failed'].includes(s.state) && s.pid != null;
  if (bgUp && !change && !command) return sendToBackground(s, text);
  // A background job Claude Code still lists keeps the options it started with (its name, model, effort, mode and
  // worktree): resumed with none, it wakes as itself under the same id. Passing any starts a copy instead: the same
  // conversation under a new id, in the folder it is started from. So a model, effort or mode you pick needs its
  // record removed first (claude rm) for it to wake as itself, and that would take a worktree the job made along with
  // it. One that works in its own worktree carries on as a copy started from inside that worktree instead; its record
  // stays, ended, and keeps the worktree (Claude Code refuses to remove it while the copy works there).
  const modePick = RESUME_MODES.has(body?.mode) ? body.mode : null;
  const keeps = s.kind === 'background' && s.inRoster && BG_ID.test(s.id ?? '');
  const repick = Boolean(change || modePick);
  const tree = keeps && repick ? worktreeOf(s) : null;
  // One that is working keeps your message until it is done: a terminal session always, a background job that is to
  // restart or run a /command.
  if ((s.kind === 'interactive' || bgUp) && isWorking(s) && !body?.fromQueue) {
    if (stops.has(stopKey(s.sessionId, null))) return fail('It is stopping. Send your message once it is idle.');
    const q = inbox.get(s.sessionId) ?? [];
    if (q.length >= 10) return fail('Ten messages are already waiting for it.');
    q.push({ id: crypto.randomUUID(), text, at: Date.now(), mode: RESUME_MODES.has(body?.mode) ? body.mode : null, ...want });
    inbox.set(s.sessionId, q);
    broadcast();
    return { ok: true, queued: true };
  }
  if (isWorking(s)) return fail('It is still working. Wait for it to finish, or interrupt it first.');
  if (bgUp && !change) return runCommand(s, text);
  const cwd = tree ?? s.cwd; // a copy starts from inside its worktree (the roster gives a stopped job its starting folder back)
  if (!cwd || !fs.existsSync(cwd)) return fail(`Its folder is gone (${cwd}), so it can't be resumed.`);
  // Checked before anything is ended: a session that never saved a conversation has nothing to resume.
  const turn = lastTurn(s.sessionId);
  if (!turn.file || !turn.talked) return fail(s.kind === 'interactive' ? 'It has no saved conversation yet, so it would not come back. Reply in its terminal.' : 'It has no saved conversation yet, so it would not come back.');

  // A job that is done with its task is still up, waiting at its prompt: it is stopped too, before its record goes.
  if (bgUp || (running && s.pid != null)) {
    const r = await endSession(s);
    if (!r.ok) return r;
    const gone = await rosterSettles(s.sessionId, (r) => !r || ['stopped', 'failed'].includes(r.state) || (bgUp ? r.pid == null : r.state === 'done'));
    if (!gone) return fail('It did not stop in time, so your reply was not sent. Try again.');
  }
  const args = ['--bg', '--resume', s.sessionId];
  if (!keeps || repick) {
    if (keeps && !tree) {
      const r = await run('claude', ['rm', s.id]);
      if (r.err && !/No job matching/i.test(firstLine(r))) return fail(`It can't be restarted with your reply: ${firstLine(r)}`);
    }
    if (s.name && !s.name.startsWith('-')) args.push('--name', s.name.slice(0, 120));
    const model = want.model ?? turn.model, effort = want.effort ?? turn.effort;
    if (model && /^[\w.\-[\]]{1,80}$/.test(model)) args.push('--model', model);
    if (EFFORTS.has(effort)) args.push('--effort', effort);
    // The mode you picked, else one the panel switched it to since, else the one its transcript last shows (a switch is
    // written there only with its next message). Never the hook-reported one: hooks carry no token, so a forged event
    // must not be able to pick the mode a session restarts in.
    const switched = bgUp && s.switchedTo?.at > (turn.modeAt ?? 0) ? s.switchedTo.mode : null;
    const mode = [modePick, switched, turn.mode].find((m) => RESUME_MODES.has(m));
    if (mode) args.push('--permission-mode', mode);
  }
  args.push('--', text);
  const r = await run('claude', args, { cwd, timeout: 30_000 });
  const id = `${r.stdout}\n${r.stderr}`.match(/backgrounded · (\w+)/)?.[1];
  if (!id) return fail(firstLine(r));
  const copy = Boolean(tree);
  if (copy) s.continuedAs = { id, at: Date.now() }; // the record stays on the board, ended, and says where it went
  else { dismissed.delete(s.sessionId); Object.assign(s, { ended: false, closedByPanel: false, note: null }); }
  setTimeout(pollRoster, 400);
  broadcast();
  if (!body?.fromQueue) logSent(s, text, 'restarted');
  return { ok: true, id, same: s.sessionId.startsWith(id), ...(copy ? { copy } : {}), ...(change ? { restartedOn: want } : {}) };
}

// ---------------------------------------------------------------- mode

// A running session's mode only changes from its own screen (Shift+Tab) or with a permission answer. For a background
// job the panel does the former: mode-switch.py attaches in a pseudo-terminal and presses Shift+Tab until the footer
// shows the mode. Terminal sessions are on someone's screen, so they switch there.
const MODE_SWITCH = fileURLToPath(new URL('./mode-switch.py', import.meta.url));
async function switchMode(body) {
  const s = liveSession(body);
  const mode = body?.mode;
  if (!s) return { ok: false, error: 'That session is gone.' };
  if (!RESUME_MODES.has(mode)) return { ok: false, error: 'Pick ask, accept edits, plan or auto.' };
  // A job that is done with its task is still there to attach to; a stopped or failed one would be started again.
  if (s.kind !== 'background' || !s.inRoster || !BG_ID.test(s.id ?? '') || s.ended || s.state === 'stopped' || s.state === 'failed' || s.pid == null)
    return { ok: false, error: s.kind === 'interactive' ? 'A terminal session switches with Shift+Tab in its terminal. Here, a message or a permission answer can switch it.' : 'Only a running background session switches here.' };
  // Shift+Tab in an open dialog would answer it, so not while it waits on you.
  if (s.status === 'waiting' || [...pending.values()].some((p) => p.sessionId === s.sessionId))
    return { ok: false, error: 'Answer what it is asking first. Allowing it can switch the mode too.' };
  if (sendingTo.has(s.sessionId)) return { ok: false, error: 'Your last message or mode switch is still on its way.' };
  sendingTo.add(s.sessionId);
  const r = await run('python3', [MODE_SWITCH, s.id, mode], { cwd: os.homedir(), timeout: 40_000 });
  sendingTo.delete(s.sessionId);
  let out;
  try { out = JSON.parse(r.stdout.trim().split('\n').pop()); } catch { out = { ok: false, error: firstLine(r) || 'The switch did not report back.' }; }
  if (!out.ok) {
    if (RESUME_MODES.has(out.to)) { s.mode = out.to; s.switchedTo = { mode: out.to, at: Date.now() }; broadcast(); } // where it really is now
    return { ok: false, error: out.error };
  }
  s.mode = mode;
  s.switchedTo = { mode, at: Date.now() };
  broadcast();
  return { ok: true, from: out.from };
}

// ---------------------------------------------------------------- a question or plan nobody holds any more

// A background session's question (AskUserQuestion) or plan (ExitPlanMode) that no hook request holds any more (the
// hold ran out, or the panel restarted) is still open on its own screen, and a background session has no terminal to
// answer it in. The panel reads it from the transcript and answers it there: answer-question.py / answer-plan.py attach
// in a pseudo-terminal and press the keys you would. A plan's choices are the ones its held card offers anyway. A
// permission prompt is never answered by keys from here: that would go around Claude Code's permission system.
const ANSWER_QUESTION = fileURLToPath(new URL('./answer-question.py', import.meta.url));
const ANSWER_PLAN = fileURLToPath(new URL('./answer-plan.py', import.meta.url));
// What a plan's own screen approves into: auto mode, or asking before edits.
const OPEN_PLAN_MODES = new Set(['auto', 'default']);
const asks = new Map(); // sessionId → { size, mtimeMs, ask }: the last read of its transcript
/** The tool call a waiting background session's dialog is for, from its transcript, when the panel isn't holding it. */
function openAsk(s) {
  if (s.kind !== 'background' || !s.inRoster || s.status !== 'waiting' || isEnded(s) || !BG_ID.test(s.id ?? '')) return null;
  if ([...pending.values()].some((p) => p.sessionId === s.sessionId && !p.agentId)) return null;
  const file = transcriptFile(s.sessionId);
  let st;
  try { st = file && fs.statSync(file); } catch {}
  if (!st) return null;
  const c = asks.get(s.sessionId);
  if (c && c.size === st.size && c.mtimeMs === st.mtimeMs) return c.ask;
  let ask = null, n = 0;
  const answered = new Set();
  for (const line of tailOf(file, 512_000)) {
    if (++n > 800) break;
    let d;
    try { d = JSON.parse(line); } catch { continue; }
    if (d.isSidechain) continue;
    const content = d.message?.content;
    if (d.type === 'user' && !Array.isArray(content)) break; // a prompt you typed: nothing before it is still open
    if (!Array.isArray(content)) continue;
    if (d.type === 'user') for (const b of content) if (b.type === 'tool_result') answered.add(b.tool_use_id);
    const t = d.type === 'assistant' && content.findLast((b) => b.type === 'tool_use' && !answered.has(b.id));
    if (t) { ask = { toolUseId: t.id, toolName: t.name, toolInput: t.input ?? {}, at: Date.parse(d.timestamp) || null }; break; }
  }
  asks.set(s.sessionId, { size: st.size, mtimeMs: st.mtimeMs, ask });
  return ask;
}
/** Open dialogs, shaped like held requests so the page shows them the same way. */
const openRequests = () => [...sessions.values()].flatMap((s) => {
  const a = openAsk(s);
  return a ? [{ id: `open:${s.sessionId}:${a.toolUseId}`, open: true, sessionId: s.sessionId, agentId: null, toolName: a.toolName,
                toolInput: a.toolInput, toolUseId: a.toolUseId, permissionMode: s.mode ?? null, createdAt: a.at ?? Date.now() }] : [];
});
/** What its conversation got back for a tool call, once it has. */
function resultOf(s, toolUseId) {
  const file = transcriptFile(s.sessionId);
  if (!file) return null;
  for (const line of tailOf(file, 512_000)) {
    let d;
    try { d = JSON.parse(line); } catch { continue; }
    const r = d.type === 'user' && Array.isArray(d.message?.content) && d.message.content.find((b) => b.type === 'tool_result' && b.tool_use_id === toolUseId);
    if (r) return typeof r.content === 'string' ? r.content : (r.content ?? []).map((b) => b.text ?? '').join('\n');
  }
  return null;
}
async function answerOpen(body) {
  const s = liveSession(body);
  if (!s) return { ok: false, error: 'That session is gone.' };
  const ask = openAsk(s);
  if (!ask || ask.toolUseId !== body?.toolUseId) return { ok: false, error: 'It is not asking that any more.' };
  if (ask.toolName === 'ExitPlanMode') return answerOpenPlan(s, ask, body);
  if (ask.toolName !== 'AskUserQuestion')
    return { ok: false, error: `The panel only answers questions and plans this way. Open it with claude attach ${s.id} to answer its permission prompt.` };
  const qs = Array.isArray(ask.toolInput.questions) ? ask.toolInput.questions : [];
  if (!qs.length) return { ok: false, error: 'Its question has nothing in it to answer.' };
  if (qs.some((q) => q.multiSelect)) return { ok: false, error: `It asks for several picks at once, which the panel can't press for it. Open it with claude attach ${s.id}.` };
  const answers = body?.answers && typeof body.answers === 'object' ? body.answers : {};
  const picks = [];
  for (const q of qs) {
    const a = String(answers[q.question] ?? '').replace(/[\x00-\x1f\x7f]+/g, ' ').trim();
    if (!a || a.length > 2000) return { ok: false, error: 'Answer every question (up to 2,000 characters each).' };
    const opts = Array.isArray(q.options) ? q.options : [];
    const j = opts.findIndex((o) => o.label === a);
    picks.push(j >= 0 ? { option: j + 1 } : { other: opts.length + 1, text: a });
  }
  if (sendingTo.has(s.sessionId)) return { ok: false, error: 'Your last message or mode switch is still on its way.' };
  sendingTo.add(s.sessionId);
  const r = await run('python3', [ANSWER_QUESTION, s.id, JSON.stringify({ questions: qs.map((q) => q.question), picks,
    toolUseId: ask.toolUseId, transcript: transcriptFile(s.sessionId) })],
    { cwd: os.homedir(), timeout: 60_000 });
  let out;
  try { out = JSON.parse(r.stdout.trim().split('\n').pop()); } catch { out = { ok: false, error: firstLine(r) || 'It did not report back.' }; }
  // Its conversation shows the answers once Claude Code takes them: check they are the ones picked.
  let got = null;
  for (const end = Date.now() + 15_000; out.ok && got == null && Date.now() < end; await new Promise((r) => setTimeout(r, 500))) got = resultOf(s, ask.toolUseId);
  sendingTo.delete(s.sessionId);
  asks.delete(s.sessionId);
  setTimeout(pollRoster, 500);
  broadcast();
  if (!out.ok) return { ok: false, error: out.error };
  if (got == null) return { ok: false, error: 'Its screen took the answers, but its conversation does not show them yet. Look again in a moment.' };
  const wrong = qs.filter((q) => !got.replace(/\s+/g, ' ').includes(String(answers[q.question]).replace(/\s+/g, ' ').trim()));
  if (wrong.length) return { ok: false, error: `It took different answers than you picked: ${trim(got, 300)}` };
  logSent(s, qs.map((q) => `${q.header || q.question}: ${String(answers[q.question]).trim()}`).join('\n'), 'as its answers');
  return { ok: true };
}
/** Approve an open plan into a mode, or send it back with what to change, on its own screen. */
async function answerOpenPlan(s, ask, body) {
  let spec;
  if (body?.behavior === 'approve-plan') {
    if (!OPEN_PLAN_MODES.has(body.mode)) return { ok: false, error: 'Its screen approves into auto mode or asking before edits.' };
    spec = { choice: body.mode };
  } else if (body?.behavior === 'deny') {
    const text = String(body.message ?? '').replace(/[\x00-\x1f\x7f]+/g, ' ').trim();
    if (!text || text.length > 5000) return { ok: false, error: 'Say what to change (up to 5,000 characters).' };
    spec = { choice: 'feedback', text };
  } else return { ok: false, error: 'Approve the plan or send it back.' };
  if (sendingTo.has(s.sessionId)) return { ok: false, error: 'Your last message or mode switch is still on its way.' };
  sendingTo.add(s.sessionId);
  const r = await run('python3', [ANSWER_PLAN, s.id, JSON.stringify({ ...spec, toolUseId: ask.toolUseId, transcript: transcriptFile(s.sessionId) })],
    { cwd: os.homedir(), timeout: 60_000 });
  let out;
  try { out = JSON.parse(r.stdout.trim().split('\n').pop()); } catch { out = { ok: false, error: firstLine(r) || 'It did not report back.' }; }
  // Its conversation shows the answer once Claude Code takes it: check it is the one picked.
  let got = null;
  for (const end = Date.now() + 15_000; out.ok && got == null && Date.now() < end; await new Promise((r) => setTimeout(r, 500))) got = resultOf(s, ask.toolUseId);
  sendingTo.delete(s.sessionId);
  asks.delete(s.sessionId);
  setTimeout(pollRoster, 500);
  broadcast();
  if (!out.ok) return { ok: false, error: out.error };
  if (got == null) return { ok: false, error: 'Its screen took it, but its conversation does not show it yet. Look again in a moment.' };
  const approved = got.startsWith('User has approved your plan');
  if (spec.choice === 'feedback' ? approved || !got.replace(/\s+/g, ' ').includes(spec.text) : !approved)
    return { ok: false, error: `It took a different answer than you picked: ${trim(got, 300)}` };
  logSent(s, spec.text ?? `Approved its plan, ${spec.choice === 'auto' ? 'in auto mode' : 'asking before edits'}`,
    spec.text ? 'as what to change in its plan' : 'on its screen');
  return { ok: true };
}

// ---------------------------------------------------------------- folders for a new agent

const expand = (p) => (p === '~' || p.startsWith('~/') ? path.join(os.homedir(), p.slice(1)) : p);
const tilde = (p) => (p === os.homedir() ? '~' : p.startsWith(os.homedir() + '/') ? '~' + p.slice(os.homedir().length) : p);
/** Folders under the one being typed, for the launcher's suggestions. */
// ---------------------------------------------------------------- git: the branch and worktree a folder is on

// Read from the files under .git, never by running git: a repo's own config can make git run commands, and a hook
// event can name any folder.
const WORKTREE_NAME = /^(?!.*\.\.)(?!.*\.lock$)[A-Za-z0-9][\w.-]{0,59}$/;
const places = new Map(); // folder → { at, place }
/** The start of a regular file, read without blocking: a FIFO, a device or a directory is refused, never waited on. */
function small(f, max = 4096) {
  const fd = fs.openSync(f, fs.constants.O_RDONLY | fs.constants.O_NONBLOCK);
  try {
    if (!fs.fstatSync(fd).isFile()) throw new Error(`${f} is not a file`);
    const buf = Buffer.alloc(max);
    return buf.toString('utf8', 0, fs.readSync(fd, buf, 0, max, 0)).trim();
  } finally { fs.closeSync(fd); }
}
function gitPlace(dir) {
  if (typeof dir !== 'string' || !path.isAbsolute(dir)) return null;
  const hit = places.get(dir);
  if (hit && Date.now() - hit.at < 4000) return hit.place;
  let place = null;
  try { place = readPlace(dir); } catch {}
  places.set(dir, { at: Date.now(), place });
  if (places.size > 500) places.delete(places.keys().next().value);
  return place;
}
function readPlace(dir) {
  for (let top = dir, i = 0; i < 64; i++) {
    const dot = path.join(top, '.git');
    let st = null;
    try { st = fs.statSync(dot); } catch {}
    if (st && !st.isFile() && !st.isDirectory()) return null;
    if (st) {
      let gitDir = dot, common = dot;
      if (st.isFile()) { // a linked worktree (or a submodule): .git names its git dir
        const m = small(dot).match(/^gitdir:\s*(.+)$/m);
        if (!m) return null;
        gitDir = common = path.resolve(top, m[1].trim());
        try { common = path.resolve(gitDir, small(path.join(gitDir, 'commondir'))); } catch {} // a submodule has none
      }
      const head = small(path.join(gitDir, 'HEAD'));
      const branch = head.match(/^ref:\s*refs\/heads\/(.+)$/)?.[1] ?? null;
      const linked = common !== gitDir;
      return { branch, commit: !branch && /^[0-9a-f]{40,64}$/.test(head) ? head.slice(0, 10) : null, worktree: linked ? path.basename(top) : null,
               top, repo: linked && path.basename(common) === '.git' ? path.dirname(common) : top, common };
    }
    const up = path.dirname(top);
    if (up === top) return null;
    top = up;
  }
  return null;
}
const placeOut = (p) => p && { branch: p.branch, commit: p.commit, worktree: p.worktree, top: p.top, repo: p.repo };
/** A `worktree` setting as Claude Code would see it from here: local, then project, then user settings. */
const worktreeSetting = (place, key) => [path.join(place.top, '.claude', 'settings.local.json'), path.join(place.top, '.claude', 'settings.json'), path.join(configDir, 'settings.json')]
  .map((f) => { try { return JSON.parse(small(f, 512 * 1024))?.worktree?.[key]; } catch { return undefined; } })
  .find((v) => typeof v === 'string');
/** What a worktree Claude Code makes from here starts at: the remote's default branch, unless settings say HEAD. */
function worktreeBase(place) {
  if (worktreeSetting(place, 'baseRef') !== 'head') {
    try { const ref = small(path.join(place.common, 'refs', 'remotes', 'origin', 'HEAD')).match(/^ref:\s*refs\/remotes\/(.+)$/)?.[1]; if (ref) return ref; } catch {}
  }
  return place.branch ?? 'its current commit';
}
/** The launcher's look at a folder: is it there, and is it in a git repository. */
function folderInfo(body) {
  const dir = path.resolve(expand(String(body?.path ?? '').trim()) || os.homedir());
  try { if (!fs.statSync(dir).isDirectory()) return { ok: true, folder: false }; } catch { return { ok: true, folder: false }; }
  const p = gitPlace(dir), home = dir === os.homedir();
  // A background session moves itself into a worktree before it edits, unless the repo turned that off.
  return { ok: true, folder: true, git: p && { branch: p.branch, worktree: p.worktree, repo: tilde(p.repo), base: worktreeBase(p),
                                               isolates: worktreeSetting(p, 'bgIsolation') !== 'none' },
           // Whether this profile has trusted it; a background session starts only where it has (never the home folder).
           trusted: home ? null : isTrusted(profile.config, trustKey(dir, p?.top)), home, profile: profileLabel() };
}

const profileLabel = () => path.basename(profile.dir).slice(1);
/** Claude Code's own path, as the panel's commands find it. */
const claudePath = () => String(childEnv.PATH ?? '').split(':').filter(Boolean).map((d) => path.join(d, 'claude'))
  .find((f) => { try { fs.accessSync(f, fs.constants.X_OK); return fs.statSync(f).isFile(); } catch { return false; } });
/** Terminal in the folder, on this profile, running Claude Code: its trust prompt waits there for you to accept. The
 *  window opens from a .command file, so the panel needs no permission to control Terminal. */
async function openTrustTerminal(body) {
  if (process.platform !== 'darwin') return { ok: false, error: 'Opening Terminal is macOS only. Run claude in the folder once and accept its prompt.' };
  const dir = path.resolve(expand(String(body?.path ?? '').trim()) || os.homedir());
  try { if (!fs.statSync(dir).isDirectory()) throw new Error(); } catch { return { ok: false, error: `There is no folder at ${tilde(dir)}.` }; }
  if (dir === os.homedir()) return { ok: false, error: 'Claude Code never keeps trust for your home folder. Pick a project folder.' };
  const claude = claudePath();
  if (!claude) return { ok: false, error: "Claude Code wasn't found on the panel's PATH." };
  const where = path.join(path.dirname(PREFS), 'trust');
  fs.mkdirSync(where, { recursive: true, mode: 0o700 });
  for (const n of fs.readdirSync(where)) try { const f = path.join(where, n); if (Date.now() - fs.statSync(f).mtimeMs > 3_600_000) fs.unlinkSync(f); } catch {}
  const file = path.join(where, `trust-${crypto.randomBytes(4).toString('hex')}.command`);
  fs.writeFileSync(file, trustScript({ dir, configDir: profile.plain ? null : profile.dir, claude, label: profileLabel() }), { mode: 0o700 });
  const r = await run('open', ['-a', 'Terminal', file], { timeout: 10_000 });
  return r.err ? { ok: false, error: firstLine(r) } : { ok: true };
}

function listFolders(body) {
  const typed = expand(String(body?.path ?? '').trim());
  if (!typed.startsWith('/')) return { ok: true, folders: [] };
  const parent = typed.endsWith('/') ? typed : path.dirname(typed), stem = typed.endsWith('/') ? '' : path.basename(typed).toLowerCase();
  let names = [];
  try {
    names = fs.readdirSync(parent, { withFileTypes: true })
      .filter((d) => (stem.startsWith('.') || !d.name.startsWith('.')) && d.name.toLowerCase().startsWith(stem)
        && (d.isDirectory() || (d.isSymbolicLink() && (() => { try { return fs.statSync(path.join(parent, d.name)).isDirectory(); } catch { return false; } })())))
      .map((d) => d.name).sort((a, b) => a.localeCompare(b)).slice(0, 60);
  } catch {}
  return { ok: true, folders: names.map((n) => tilde(path.join(parent, n))) };
}
// ---------------------------------------------------------------- switching profile

/** Who a profile is signed in as, and on which plan, from Claude Code itself: its files can lag a fresh sign-in, and
 *  two profiles on one email differ only by plan. Undefined when it can't tell. */
async function signedIn(p) {
  const r = await run('claude', ['auth', 'status', '--json'], { env: p.plain ? baseEnv : { ...baseEnv, CLAUDE_CONFIG_DIR: p.dir }, timeout: 5000 });
  try {
    const a = JSON.parse(r.stdout);
    return a.loggedIn ? { email: a.email ?? null, org: a.orgName ?? null, plan: a.subscriptionType ?? null } : null;
  } catch { return undefined; }
}
const profiles = async () => ({ ok: true, active: profile.id, profiles: await Promise.all(listProfiles().map(async (p) => {
  const d = describeProfile(p), a = await signedIn(p);
  return a === undefined ? d : { ...d, account: a };
})) });
/** Shows another profile's sessions. Requests held here stay held, for when this profile is back; messages waiting
 *  to be sent are dropped. */
function switchProfile(body) {
  const p = listProfiles().find((x) => x.id === body?.id);
  if (!p) return { ok: false, error: 'There is no such profile.' };
  if (p.dir === profile.dir) return { ok: true };
  const queued = [...inbox.values()].reduce((n, q) => n + q.length, 0);
  if (queued && body.force !== true) return { ok: false, confirm: true, queued };
  for (const m of [inbox, sessions, stops, files, turns]) m.clear();
  for (const s of [dismissed, saidRead]) s.clear();
  useProfile(p);
  rosterError = null;
  try { fs.mkdirSync(path.dirname(PREFS), { recursive: true }); fs.writeFileSync(PREFS, JSON.stringify({ profile: p.id }) + '\n'); } catch {}
  broadcast();
  pollRoster().then(checkVersions);
  return { ok: true };
}
/**
 * Points a profile's hooks at this panel: adds the entries of hooks.example.json that its settings.json lacks, after
 * copying the file aside. Nothing else in it changes. Its sessions pick the hooks up as Claude Code reloads settings.
 */
function connectProfile(body) {
  const p = listProfiles().find((x) => x.id === body?.id);
  if (!p) return { ok: false, error: 'There is no such profile.' };
  const file = path.join(p.dir, 'settings.json');
  let settings = {};
  if (fs.existsSync(file)) {
    try { settings = JSON.parse(small(file, 4 << 20)); } catch { return { ok: false, error: `${tilde(file)} isn't valid JSON, so it was left as it is.` }; }
    if (!settings || typeof settings !== 'object' || Array.isArray(settings)) return { ok: false, error: `${tilde(file)} isn't a settings object, so it was left as it is.` };
  }
  if (settings.hooks != null && (typeof settings.hooks !== 'object' || Array.isArray(settings.hooks))) return { ok: false, error: `Its "hooks" setting isn't an object, so it was left as it is.` };
  const want = JSON.parse(fs.readFileSync(new URL('./hooks.example.json', import.meta.url), 'utf8').replaceAll('127.0.0.1:4747/', HOOK_URL.replace(/hook\/$/, ''))).hooks;
  settings.hooks ??= {};
  let added = 0;
  for (const [event, entries] of Object.entries(want)) {
    const list = Array.isArray(settings.hooks[event]) ? settings.hooks[event] : (settings.hooks[event] = []);
    if (list.some((m) => (m?.hooks ?? []).some((h) => typeof h?.url === 'string' && h.url.includes(HOOK_URL)))) continue;
    list.push(...entries);
    added++;
  }
  if (!added) { if (p.dir === profile.dir) profileInfo = describeProfile(p); broadcast(); return { ok: true, added: 0 }; }
  let backup = null;
  if (fs.existsSync(file)) {
    backup = `${file}.before-agent-panel-${new Date().toISOString().replace(/[:.]/g, '-')}`;
    fs.copyFileSync(file, backup);
  }
  const tmp = `${file}.agent-panel-tmp`;
  fs.writeFileSync(tmp, JSON.stringify(settings, null, 2) + '\n');
  fs.renameSync(tmp, file);
  if (p.dir === profile.dir) profileInfo = describeProfile(p);
  broadcast();
  return { ok: true, added, backup: backup && tilde(backup) };
}

/** The Mac's own folder dialog, opened where the field points. */
let picking = false;
async function pickFolder(body) {
  if (process.platform !== 'darwin') return { ok: false, error: 'The folder dialog is macOS only. Type the path.' };
  if (picking) return { ok: false, error: 'The folder dialog is already open. It may be behind this window.' };
  let start = expand(String(body?.path ?? '').trim()) || os.homedir();
  try { if (!fs.statSync(start).isDirectory()) start = os.homedir(); } catch { start = os.homedir(); }
  picking = true;
  const r = await run('osascript', ['-e', 'on run argv', '-e', 'tell me to activate',
    '-e', 'return POSIX path of (choose folder with prompt "Folder for the new agent" default location (POSIX file (item 1 of argv)))',
    '-e', 'end run', start], { timeout: 600_000 });
  picking = false;
  if (r.err) return /-128|cancel/i.test(r.stderr) ? { ok: false, cancelled: true } : { ok: false, error: firstLine(r) };
  return { ok: true, path: tilde(r.stdout.trim().replace(/(.)\/$/, '$1')) };
}

// ---------------------------------------------------------------- world

// One World for this Mac, fed by every profile's sessions, kept beside the prefs (so a second panel with its own
// ACP_PREFS has its own). It counts tokens every minute and commits every fifth time; a scan never overlaps another.
const world = createWorld({ file: path.join(path.dirname(PREFS), 'world', 'world.json'), configDirs: () => listProfiles().map((p) => p.dir),
  gitPlace, run: (...a) => run(...a) });
const WORLD_MS = 60_000;
let worldScans = 0, worldScanning = false;
async function scanWorld(git) {
  if (!world.started() || worldScanning) return;
  worldScanning = true;
  const before = worldSummary().earned;
  try { await world.scan({ git }); } catch (err) { console.error('world scan failed:', err?.message ?? err); }
  finally { worldScanning = false; }
  if (worldSummary().earned !== before) broadcast();
  hubSync.schedule(); // after every scan, not only a changed one: the half-hour heartbeat is judged when this is called
}
/** World's numbers for the page. Nothing in them may take the panel down. */
function worldSummary() {
  try { return world.summary(); } catch (err) { console.error('world summary failed:', err?.message ?? err); return { started: false }; }
}
function openWorld() {
  world.open();
  scanWorld(true);
  hubSync.schedule({ urgent: true });
  return { ok: true, world: worldSummary() };
}
/** Spend World's credit: push the frontier (from Home or a colony), or send a rocket (world.mjs decides the price and
 *  whether it can). */
function buyInWorld(body) {
  const r = world.buy({ kind: body?.kind, ang: body?.ang, from: body?.from, planet: body?.planet, name: body?.name });
  if (!r.ok) return r;
  broadcast();
  hubSync.schedule({ urgent: true });
  return { ok: true, world: worldSummary() };
}
/** Where the hub says a probe toward `ang` goes, `skip` being what this account found or has on the way (world.probe's
 *  `find`). */
async function findProbeTarget(ang, skip) {
  const r = await hub.request('POST', '/api/me/probe', { body: { ang, skip } });
  if (r.ok && r.body.target) return { ok: true, target: r.body.target };
  return r.status === 404 && r.body.none ? { ok: false, none: true } : { ok: false, error: r.body.error ?? 'The hub could not be reached.' };
}
/** This account's spot in the Universe, or null signed out. */
function mySpot() {
  const a = hub.status().account;
  return a ? { x: a.x, y: a.y } : null;
}
/** Send a probe: the hub aims it, World pays for it. */
async function probeInWorld(body) {
  const r = await world.probe({ ang: body?.ang, me: mySpot(), find: findProbeTarget });
  if (!r.ok) return r;
  broadcast();
  hubSync.schedule({ urgent: true });
  return { ok: true, caption: 'a probe is on its way', world: worldSummary() };
}
/** Whether a probe would find anyone now (the Probe card asks before its button can be pressed); nothing is bought. */
async function probeReady() {
  const me = mySpot();
  if (!me || !world.started()) return { ok: true, ready: false };
  const skip = (worldSummary().probes ?? []).filter((q) => q.by?.x === me.x && q.by?.y === me.y).map((q) => q.to);
  const r = await findProbeTarget(0, skip);
  return { ok: true, ready: r.ok, none: Boolean(r.none) };
}
/** The names of the galaxies at these spots (the ones this account's probes found), from the hub; none signed out. */
async function foundGalaxies(body) {
  const at = Array.isArray(body?.at) ? body.at.filter((c) => Number.isInteger(c?.x) && Number.isInteger(c?.y)).slice(0, 64) : [];
  if (!at.length || !hub.status().loggedIn) return { ok: true, galaxies: [] };
  const r = await hub.request('GET', `/api/systems/named?at=${at.map((c) => `${c.x},${c.y}`).join(';')}`);
  // No answer is not "no one": the page keeps what it drew and asks again.
  return r.ok && Array.isArray(r.body?.galaxies) ? { ok: true, galaxies: r.body.galaxies } : { ok: false, error: 'The hub could not be reached.' };
}
// The Claude calls behind Build it. `claude agents` lists one while it runs, so the roster and the hooks skip these.
const quietIds = new Set();
/** Ask Claude (under the profile the board shows) to turn a description into pieces World can draw. */
async function askClaude(prompt) {
  const id = crypto.randomUUID(), cwd = path.join(path.dirname(PREFS), 'world', 'build');
  quietIds.add(id);
  try {
    fs.mkdirSync(cwd, { recursive: true });
    const r = await new Promise((resolve) => {
      const child = execFile('claude', claudeArgs(id, prompt), { cwd, env: childEnv, timeout: 120_000, maxBuffer: 4_000_000 },
        (err, stdout, stderr) => resolve({ err, stdout: String(stdout ?? ''), stderr: String(stderr ?? '') }));
      child.stdin?.end(); // left open, `claude -p` waits for more input first
    });
    return readAnswer(r);
  } finally { setTimeout(() => quietIds.delete(id), 120_000); } // until the roster has seen it go
}
let building = false; // a Build it is waiting on Claude; the hub's site shows it
async function buildInWorld(body, ask = askClaude) {
  building = true;
  hubSync.schedule({ urgent: true });
  try {
    const r = await world.build({ on: body?.on, description: body?.description, ask });
    if (!r.ok) return r;
    broadcast();
    return { ok: true, caption: r.purchase.caption, world: worldSummary() };
  } finally {
    building = false;
    hubSync.schedule({ urgent: true }); // the purchase, if there was one, and the end of "building"
  }
}

// ---------------------------------------------------------------- hub

// The hosted hub (hub.mjs): signing in, keeping it in step with World (hub-sync.mjs) and running the orders placed on
// its site (hub-orders.mjs). Signed out, none of this makes a request. The credentials live in hub.json beside the prefs.
const hub = createHub({ file: path.join(path.dirname(PREFS), 'hub.json') });
/** What only the owner sees on the hub: the balance, the prices, today and whether a build is under way. Null until World starts. */
function ownerStatus() {
  const w = worldSummary();
  if (!w.started) return null;
  return {
    balance: w.balance, prices: { frontier: w.prices.frontier, rocket: w.prices.rocket, build: w.prices.build, probe: w.prices.probe },
    // multiplier is gone from the credit, but a hub from before requires it, so a Mac that updates first still reports.
    today: { credit: w.today.credit, commits: w.today.commits, added: w.today.added, removed: w.today.removed, tokens: w.today.tokens,
      active: w.today.active, multiplier: 1 },
    home: { xp: w.home.xp, today: w.home.today }, building,
    mac: { name: os.hostname().slice(0, 40), online: true }, // the hub refuses a longer name
  };
}
const hubSync = createHubSync({ hub, projection: () => (world.started() ? worldSummary() : null), status: ownerStatus });
// What the website may make this Mac do (web-orders.mjs): off, or a kind it doesn't know, is refused before anything
// runs, and a build asks Claude only while the day's cap allows.
const WEB_ORDERS = path.join(path.dirname(PREFS), 'web-orders.json');
const ORDER_WORDS = { frontier: 'Push', rocket: 'Rocket', build: 'Build', probe: 'Probe' };
/** A macOS notification for every website order this Mac ran or refused, so one the owner didn't place is seen at once.
 *  The text goes in as an argument, never into the script. */
function notifyOrder(order, r) {
  if (process.platform !== 'darwin') return;
  const what = ORDER_WORDS[order?.kind] ?? 'An order';
  const text = (r.ok ? `${what} done${r.caption ? `: ${r.caption}` : '.'}` : `${what} not done: ${r.error ?? 'it failed.'}`).slice(0, 240);
  run('osascript', ['-e', 'on run argv', '-e', 'display notification (item 2 of argv) with title (item 1 of argv)', '-e', 'end run',
    'Agent Panel · ordered on the website', text], { timeout: 10_000 });
}
/** An order from the hub's site, run like the panel's own buttons; only the outcome goes back. */
async function runOrder(order) {
  const refused = admit(WEB_ORDERS, order);
  let r;
  if (refused) r = { ok: false, error: refused };
  else if (order.kind === 'build') {
    r = await buildInWorld(order, (prompt) => { const capped = takeBuild(WEB_ORDERS); return capped ? { ok: false, error: capped } : askClaude(prompt); });
  } else r = order.kind === 'probe' ? await probeInWorld(order) : buyInWorld(order);
  notifyOrder(order, r);
  return { ok: r.ok, error: r.error, caption: r.caption };
}
// Each time the stream (re)connects, the account is asked for again: a handle changed on the site moves "Your page".
const hubOrders = createHubOrders({
  hub, execute: runOrder,
  onDone: () => { hubSync.schedule({ urgent: true }); broadcast(); },
  onOpen: () => hub.refreshAccount().then((changed) => { if (changed) broadcast(); }),
});
/** The hub as the page sees it (never the token). */
function hubInfo() {
  const { url, loggedIn, account, pending } = hub.status();
  return { url, loggedIn, account, pending, sync: hubSync.state(), orders: hubOrders.state() };
}

// A sign-in started from the page is finished here: poll at the hub's pace (it may grow it, so it is read every round)
// until the hub says yes, no or never. One loop at a time; a cancel (or the code running out) ends it. A poll that
// fails (the hub is restarting, the network blinked) is tried again while the code is still live, and a panel that
// restarts mid-sign-in picks the code up again from hub.json at start.
let loginTimer = null, loginRun = 0, loginActive = false;
function stopLoginLoop() { clearTimeout(loginTimer); loginRun++; loginActive = false; }
function startLoginLoop() {
  if (loginActive) return;
  loginActive = true;
  const run = ++loginRun;
  const round = () => {
    const pending = hub.status().pending;
    if (!pending) { loginActive = false; broadcast(); return; }
    loginTimer = setTimeout(async () => {
      const outcome = await hub.pollDeviceLogin();
      if (run !== loginRun) return;
      if (outcome === 'pending' || (outcome === 'error' && hub.status().pending)) return round();
      loginActive = false;
      if (outcome === 'ok') { hubOrders.start(); hubSync.schedule({ urgent: true }); }
      broadcast();
    }, pending.interval * 1000);
  };
  round();
}
async function hubLoginStart() {
  const live = hub.status().pending;
  if (loginActive && live) return { ok: true, userCode: live.userCode, verificationUri: live.verificationUri, expiresAt: live.expiresAt };
  const r = await hub.startDeviceLogin();
  if (!r.ok) return r;
  startLoginLoop();
  broadcast();
  return { ok: true, userCode: r.userCode, verificationUri: r.verificationUri, expiresAt: r.expiresAt };
}
function hubLoginCancel() {
  stopLoginLoop();
  const r = hub.cancelDeviceLogin();
  broadcast();
  return { ok: r };
}
async function hubLogout() {
  stopLoginLoop();
  hubOrders.stop();
  const r = await hub.logout();
  broadcast();
  return r;
}
async function hubSyncNow() {
  await hubSync.flush();
  return { ok: true, sync: hubSync.state() };
}

// ---------------------------------------------------------------- deleted versions

// Every profile's running sessions, checked each minute for a Claude Code version deleted under them (versions.mjs):
// macOS can then refuse them ~/Documents at any moment. The shown profile's are marked on their tiles; all of them,
// other profiles' too, are listed in a banner, since the one that loses access is often not the one on screen.
const VERSIONS_MS = 60_000;
let oldVersions = [], checkingVersions = false;
/** A profile's running sessions: the shown one's as the panel has them, another's from its own roster. */
async function runningIn(p) {
  if (p.dir === profile.dir) return [...sessions.values()].filter((s) => s.inRoster && !isEnded(s));
  const r = await run('claude', ['agents', '--json', '--all'], { timeout: 5000, env: p.plain ? baseEnv : { ...baseEnv, CLAUDE_CONFIG_DIR: p.dir } });
  try { return JSON.parse(r.stdout).filter((x) => !['done', 'failed', 'stopped'].includes(x.state)); } catch { return []; }
}
async function checkVersions() {
  if (checkingVersions) return;
  checkingVersions = true;
  try {
    const gen = profileGen, rows = [];
    for (const p of listProfiles()) for (const s of await runningIn(p)) if (Number.isInteger(s.pid) && s.sessionId) rows.push({ s, p });
    const found = await findRemoved(rows.map(({ s }) => s.pid), { run });
    if (gen !== profileGen) return; // the profile changed while it looked: the next check sorts it out
    const next = rows.filter(({ s }) => found.has(s.pid)).map(({ s, p }) => ({
      sessionId: s.sessionId, id: s.id ?? null, name: s.name ?? null, kind: s.kind ?? null, shown: p.dir === profile.dir,
      profile: { id: p.id, label: path.basename(p.dir).slice(1), command: cliOf(p) },
      version: found.get(s.pid).version, self: found.get(s.pid).self }));
    for (const s of sessions.values()) {
      const o = next.find((x) => x.shown && x.sessionId === s.sessionId);
      s.oldVersion = o ? { version: o.version, self: o.self } : null;
    }
    const changed = JSON.stringify(next) !== JSON.stringify(oldVersions);
    oldVersions = next;
    if (changed) broadcast();
  } catch (err) { console.error('version check failed:', err?.message ?? err); }
  finally { checkingVersions = false; }
}

// ---------------------------------------------------------------- snapshot + SSE

function snapshot() {
  const now = Date.now();
  return {
    now, armed, rosterError, port: PORT, profile: profileInfo, world: worldSummary(), hub: hubInfo(), oldVersions,
    version: WORKER?.sha ?? null, rolledBack: WORKER ? rolledBack(WORKER.dir) : null, lastCheck: WORKER ? lastCheck(WORKER.dir) : null,
    appUpdate,
    sessions: [...sessions.values()].map(({ spawns, hereCwd, rosterCwd, ranCwd, editTops, ...s }) => {
      const here = ranCwd ?? hereCwd ?? s.cwd, git = placeOut(gitPlace(here));
      return {
      ...s, git, workCwd: here, elsewhere: (editTops ?? []).filter((t) => t !== git?.top).slice(0, 3).map((t) => placeOut(gitPlace(t))).filter(Boolean),
      stopping: stops.has(stopKey(s.sessionId, null)), hostsPanel: hostPids.has(s.pid),
      queued: inbox.get(s.sessionId) ?? [], working: isWorking(s),
      subagents: [...s.subagents.values()].map((a) => ({ ...a, stopping: !a.stoppedAt && stops.has(stopKey(s.sessionId, a.agentId)) })),
    }; }),
    pending: [...shownPending().map(({ res, timer, profileDir, ...p }) => p), ...openRequests()],
    _ts: now,
  };
}

let queued = false;
let lastEvent = null;
function broadcast(event) {
  if (event) lastEvent = event;
  if (queued) return;
  queued = true;
  setTimeout(() => {
    queued = false;
    const data = `data: ${JSON.stringify({ ...snapshot(), event: lastEvent })}\n\n`;
    lastEvent = null;
    for (const c of clients) c.write(data);
  }, 80);
}

// ---------------------------------------------------------------- http

function readBody(req) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    req.on('data', (c) => {
      size += c.length;
      if (size > MAX_BODY) { reject(new Error('body too large')); req.destroy(); return; }
      chunks.push(c);
    });
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      try { resolve(raw ? JSON.parse(raw) : {}); } catch { reject(new Error('bad json')); }
    });
    req.on('error', reject);
  });
}

const send = (res, code, body, type = 'application/json') => {
  res.writeHead(code, { 'content-type': type, 'cache-control': 'no-store' });
  res.end(typeof body === 'string' ? body : JSON.stringify(body));
};

// Nothing a request carries may throw out of the handler: an unhandled rejection would stop the server.
const server = http.createServer((req, res) => handle(req, res).catch((err) => {
  console.error('request failed:', err?.message ?? err);
  if (!res.headersSent) send(res, 500, { error: 'server error' }); else res.destroy();
}));
async function handle(req, res) {
  if (!HOSTS.has(req.headers.host)) return send(res, 421, { error: 'wrong host' });
  let url;
  try { url = new URL(req.url, 'http://127.0.0.1'); } catch { return send(res, 400, { error: 'bad url' }); }

  // Hook endpoints: Claude Code posts these. A browser always sends Origin on a cross-site POST, so refuse it.
  if (url.pathname.startsWith('/hook/')) {
    if (req.method !== 'POST' || req.headers.origin) return send(res, 403, { error: 'hooks only' });
    let e;
    try { e = await readBody(req); } catch { return send(res, 400, ''); }
    // Another profile's session (its hooks point here too) is not shown: its events are nothing to us. Its
    // permission requests are held all the same, for when its profile is back on the board.
    if (url.pathname === '/hook/event') { const reply = otherProfile(e) ? null : onHookEvent(e); return send(res, 200, reply ?? ''); }
    if (url.pathname === '/hook/permission') {
      // Nobody watching, or paused, or World's own Claude call: answer nothing, the terminal decides.
      if (!armed || !watched() || quietIds.has(e?.session_id)) return send(res, 200, '');
      return holdPermission(e, res);
    }
    return send(res, 404, '');
  }

  // The same page twice: /board is the board, /world opens straight on the universe (the page reads ACP_VIEW).
  if (req.method === 'GET' && (url.pathname === '/board' || url.pathname === '/world')) {
    const view = url.pathname === '/board' ? 'board' : 'world';
    return send(res, 200, page().replace('__ACP_TOKEN__', TOKEN).replace('__ACP_VIEW__', view), 'text/html; charset=utf-8');
  }

  // Fleet, every machine's sessions on one screen, is the home page; the board is /board.
  if (req.method === 'GET' && (url.pathname === '/' || url.pathname === '/fleet')) {
    return send(res, 200, fleetPage().replace('__ACP_TOKEN__', TOKEN), 'text/html; charset=utf-8');
  }

  if (req.method === 'GET' && url.pathname.startsWith('/vendor/') && Object.hasOwn(VENDOR, url.pathname.slice(8))) {
    const f = url.pathname.slice(8);
    res.writeHead(200, { 'content-type': VENDOR[f], 'cache-control': 'max-age=86400' });
    return res.end(fs.readFileSync(new URL(`./public/vendor/${f}`, import.meta.url)));
  }

  if (req.method === 'GET' && url.pathname === '/world.js') {
    return send(res, 200, fs.readFileSync(new URL('./public/world.js', import.meta.url), 'utf8'), 'text/javascript; charset=utf-8');
  }

  if (req.method === 'GET' && url.pathname === '/events') {
    if (url.searchParams.get('t') !== TOKEN) return send(res, 403, { error: 'token' });
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'keep-alive' });
    res.write(`retry: 2000\ndata: ${JSON.stringify(snapshot())}\n\n`);
    clients.add(res);
    if (DAEMON && setRosterPace(true)) pollRoster().then(checkVersions); // back from idle: catch up at once
    clearTimeout(releaseTimer);
    releaseTimer = null;
    const ping = setInterval(() => res.write(': ping\n\n'), 20_000);
    req.on('close', () => {
      clearInterval(ping);
      clients.delete(res);
      // Last tab gone and not back in time: hand every held request back to the terminal, and (under the daemon) slow
      // the roster down. Not before: until then a held request still needs the fast polls to see a terminal answer.
      if (clients.size === 0 && releaseTimer === null) releaseTimer = setTimeout(() => {
        releaseTimer = null;
        if (clients.size > 0) return;
        for (const id of [...pending.keys()]) settle(id, null, 'panel closed');
        if (DAEMON) setRosterPace(false);
      }, RECONNECT_MS);
    });
    return;
  }

  if (req.method === 'POST' && url.pathname.startsWith('/api/')) {
    if (!ORIGINS.has(req.headers.origin) || req.headers['x-acp-token'] !== TOKEN) return send(res, 403, { error: 'forbidden' });
    let body;
    try { body = await readBody(req); } catch { return send(res, 400, { error: 'bad body' }); }
    if (url.pathname === '/api/decide') {
      const r = decide(body);
      return send(res, r.ok ? 200 : 409, r);
    }
    if (url.pathname === '/api/launch') {
      const r = await launch(body);
      return send(res, r.ok ? 200 : 422, r);
    }
    if (url.pathname === '/api/term/open') return terms.open(body, res);
    if (url.pathname === '/api/interrupt') {
      const r = interrupt(body);
      return send(res, r.ok ? 200 : 422, r);
    }
    const ACTIONS = { '/api/end': end, '/api/remove': remove, '/api/reply': reply, '/api/unqueue': unqueue, '/api/mode': switchMode,
                      '/api/folders': listFolders, '/api/folder': folderInfo, '/api/pick-folder': pickFolder,
                      '/api/profiles': profiles, '/api/profile': switchProfile, '/api/profile/connect': connectProfile,
                      '/api/answer-open': answerOpen, '/api/world/open': openWorld, '/api/world/buy': buyInWorld,
                      '/api/world/build': buildInWorld, '/api/world/probe': probeInWorld, '/api/world/probe-ready': probeReady,
                      '/api/world/found': foundGalaxies, '/api/history': history,
                      '/api/trust-terminal': openTrustTerminal,
                      '/api/hub/status': () => ({ ok: true, hub: hubInfo() }), '/api/hub/login/start': hubLoginStart,
                      '/api/hub/login/cancel': hubLoginCancel, '/api/hub/logout': hubLogout, '/api/hub/sync': hubSyncNow,
                      '/api/update': updateApp, '/api/fleet': () => fleet.snapshot(), '/api/fleet/launch': (b) => fleet.launch(b),
                      '/api/fleet/peek': (b) => fleet.peek(b), '/api/fleet/send': (b) => fleet.send(b),
                      '/api/fleet/close': (b) => fleet.close(b), '/api/fleet/archive': (b) => fleet.archive(b),
                      '/api/fleet/archive-all': (b) => fleet.archiveAll(b),
                      '/api/fleet/label': (b) => fleet.label(b), '/api/fleet/adopt': (b) => fleet.adopt(b),
                      '/api/term/input': (b) => terms.input(b), '/api/term/resize': (b) => terms.resize(b) };
    if (Object.hasOwn(ACTIONS, url.pathname)) {
      let r;
      try { r = await ACTIONS[url.pathname](body); } catch (err) { r = { ok: false, error: String(err?.message ?? err).slice(0, 300) }; }
      return send(res, r.ok ? 200 : 422, r);
    }
    if (url.pathname === '/api/arm') {
      armed = Boolean(body.armed);
      if (!armed) for (const id of [...pending.keys()]) settle(id, null, 'paused');
      broadcast();
      return send(res, 200, { ok: true, armed });
    }
  }
  send(res, 404, { error: 'not found' });
}

// Under launchd a server that quit on a taken port would be relaunched every 10 s forever. Wait instead: an app-started
// server from before the service was installed quits when the app does, and this one takes the port then.
// It says so on the first try and then every 60th, so a long wait adds a line every 5 minutes, not every 5 s.
let portTries = 0;
server.on('error', (err) => {
  if (!(DAEMON && err.code === 'EADDRINUSE')) throw err;
  if (++portTries === 1 || portTries % 60 === 0) console.log(`port ${PORT} is taken; trying again every ${RETRY_MS / 1000} s`);
  setTimeout(() => server.listen(PORT, '127.0.0.1'), RETRY_MS);
});

// ---------------------------------------------------------------- a clone's updates

// A newer release than this clone runs, as the board shows it: `ready` (with `blocked`: null, or why a click can't take
// it), `updating`, `restarting`, or `done` (updated, but nothing here restarts it). Null when there is none.
let appUpdate = null;
/** Ask the hub which release it runs. Only for a clone (a worker updates itself) and only signed in: signed out, the
 *  panel sends nothing to the hub. */
async function checkAppUpdate() {
  if (WORKER || !hub.status().loggedIn || appUpdate?.state === 'updating' || appUpdate?.state === 'restarting') return;
  const r = await checkClone({ repo: REPO, hubUrl: hub.status().url, run });
  if (r.outcome === 'error') return console.error('update check:', r.error);
  const next = r.outcome === 'available' ? { sha: r.sha, blocked: r.blocked, state: 'ready' } : null;
  if (JSON.stringify(next) !== JSON.stringify(appUpdate)) { appUpdate = next; broadcast(); }
}
/** The board's Update: fast-forward to the signed release the check found, rebuild the app, and restart on the new code. */
async function updateApp() {
  if (WORKER) return { ok: false, error: 'This install updates itself.' };
  if (appUpdate?.state === 'updating' || appUpdate?.state === 'restarting') return { ok: false, error: 'The panel is updating already.' };
  if (appUpdate?.state !== 'ready') return { ok: false, error: 'There is no new version to update to.' };
  if (appUpdate.blocked) return { ok: false, error: appUpdate.blocked };
  appUpdate = { ...appUpdate, state: 'updating', error: null };
  broadcast();
  const app = await appFor({ repo: REPO, port: PORT, run });
  const r = await updateClone({ repo: REPO, sha: appUpdate.sha, run, appPath: app });
  if (!r.ok) { appUpdate = { ...appUpdate, state: 'ready', error: r.error }; broadcast(); return r; }
  if (!r.built && r.buildError) console.error('update: the app did not rebuild:', r.buildError);
  const restart = DAEMON || app !== null; // launchd restarts the service; the app starts a server when its own is gone
  appUpdate = { sha: r.sha, state: restart ? 'restarting' : 'done', built: r.built, buildError: r.buildError ?? null };
  broadcast();
  if (restart) setTimeout(() => process.exit(0), 1500); // after this answer and the board's last snapshot
  return { ok: true, sha: r.sha, built: r.built, restart };
}

let rosterTimer = null, rosterFast = null;
/** Reads the roster every ROSTER_MS when `fast`, every IDLE_ROSTER_MS when not. Returns whether the pace changed. */
function setRosterPace(fast) {
  if (fast === rosterFast) return false;
  clearInterval(rosterTimer);
  rosterFast = fast;
  rosterTimer = setInterval(pollRoster, fast ? ROSTER_MS : IDLE_ROSTER_MS);
  return true;
}
/** Whether someone may be looking: always outside the daemon, and under it while a page is connected. */
const looking = () => !DAEMON || clients.size > 0;

server.listen(PORT, '127.0.0.1', () => {
  console.log(`agent control panel → http://127.0.0.1:${PORT}`);
  pollRoster().then(() => { if (looking()) checkVersions(); });
  setRosterPace(looking());
  setInterval(() => { if (looking()) checkVersions(); }, VERSIONS_MS);
  setInterval(() => { prune(); broadcast(); }, 10_000);
  setInterval(() => scanWorld(worldScans++ % 5 === 0), WORLD_MS);
  // The order stream ends by itself when the hub says 401, and a `login` from a terminal never tells this process, so
  // look every minute: a stream that has ended is opened again, and one that is up still asks the hub for its pending
  // orders, in case the stream missed one (a Mac that slept through a drop can hold a stream that hears nothing).
  const startOrders = () => {
    if (!hub.status().loggedIn) return;
    if (hubOrders.state().listening) hubOrders.drain(); else hubOrders.start();
  };
  startOrders();
  if (hub.status().pending) startLoginLoop();   // a sign-in begun before a restart still finishes
  setInterval(startOrders, WORLD_MS);
  // A World that can't be saved (an unwritable prefs folder) must not take the panel down: under launchd it would crash-loop.
  if (DAEMON && !world.started()) {
    try { openWorld(); } catch (err) { console.error('world could not start:', err?.message ?? err); }
  }
  if (!WORKER) { // a clone: say when the hub runs a newer release; its owner updates it with a click
    setTimeout(checkAppUpdate, APP_UPDATE_FIRST_MS);
    setInterval(checkAppUpdate, APP_UPDATE_MS);
  }
  if (DAEMON && WORKER) { // only a worker install updates itself; a service from a clone is the app's and never does
    try { markStarted(WORKER); } catch (err) { console.error('worker: could not record the start:', err?.message ?? err); }
    // A timer's clock stops while the Mac sleeps, so a day-long interval on a laptop could go many days without a check.
    // An hourly tick that checks once a day of wall time has passed keeps it daily.
    let checkedAt = Date.now();
    const check = () => {
      checkedAt = Date.now();
      checkForUpdate({ worker: WORKER, hubUrl: hub.status().url, run }).then((r) => {
        try { recordCheck(WORKER.dir, r); } catch (err) { console.error('worker: could not record the check:', err?.message ?? err); }
        if (r.outcome === 'error') console.error('update check:', r.error);
      });
    };
    setTimeout(check, UPDATE_FIRST_MS);
    setInterval(() => { if (Date.now() - checkedAt >= UPDATE_MS) check(); }, UPDATE_TICK_MS);
  }
});
