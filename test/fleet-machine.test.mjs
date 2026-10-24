// Fleet's machine read (fleet/machine.mjs) over a made-up home folder: run as it runs over SSH, with FLEET_DO=collect and
// the script on stdin. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const SCRIPT = fileURLToPath(new URL('../fleet/machine.mjs', import.meta.url));

function home() {
  const h = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-home-'));
  // A Claude profile whose status line has reported the account's limits.
  const p = path.join(h, '.claude-work');
  fs.mkdirSync(path.join(p, 'fleet-status'), { recursive: true });
  fs.writeFileSync(path.join(p, 'settings.json'), '{}');
  fs.writeFileSync(path.join(p, '.claude.json'), JSON.stringify({ oauthAccount: { emailAddress: 'me@example.com', organizationName: 'Example' } }));
  const snap = (id, at, five) => fs.writeFileSync(path.join(p, 'fleet-status', `${id}.json`), JSON.stringify({ at, session_id: id,
    model: { id: 'claude-opus-5-5' }, context_window: { context_window_size: 1_000_000, used_percentage: 20 },
    rate_limits: { five_hour: { used_percentage: five, resets_at: 1 }, seven_day: { used_percentage: 3, resets_at: 2 } } }));
  snap('aaaaaaaa-1', 1000, 40);
  snap('bbbbbbbb-2', 2000, 12); // the newer reading wins
  // A Codex home: its sign-in, and a session that finished a turn.
  const c = path.join(h, '.codex'), d = new Date();
  const day = path.join(c, 'sessions', String(d.getFullYear()), String(d.getMonth() + 1).padStart(2, '0'), String(d.getDate()).padStart(2, '0'));
  fs.mkdirSync(day, { recursive: true });
  const claims = Buffer.from(JSON.stringify({ email: 'me@example.com', 'https://api.openai.com/auth': { chatgpt_plan_type: 'pro' } })).toString('base64url');
  fs.writeFileSync(path.join(c, 'auth.json'), JSON.stringify({ tokens: { id_token: `x.${claims}.y`, access_token: 'secret' } }));
  const t = (s) => new Date(Date.now() - 60_000 + s * 1000).toISOString();
  const lines = [
    { timestamp: t(0), type: 'session_meta', payload: { id: 's-1', cwd: path.join(h, 'repo'), timestamp: t(0), originator: 'codex_exec', source: 'exec' } },
    { timestamp: t(1), type: 'turn_context', payload: { model: 'gpt-5.5-codex' } },
    { timestamp: t(2), type: 'event_msg', payload: { type: 'task_started' } },
    { timestamp: t(3), type: 'response_item', payload: { type: 'function_call', name: 'exec_command', arguments: '{"cmd":"ls"}' } },
    { timestamp: t(4), type: 'event_msg', payload: { type: 'token_count', info: { last_token_usage: { input_tokens: 64_600 }, model_context_window: 258_400 },
      rate_limits: { primary: { used_percent: 40, window_minutes: 10080, resets_at: 1793100081 }, secondary: null, plan_type: 'pro' } } },
    { timestamp: t(5), type: 'event_msg', payload: { type: 'task_complete', last_agent_message: 'Done: the list is in out.txt.' } },
  ];
  fs.writeFileSync(path.join(day, 'rollout-s-1.jsonl'), lines.map((l) => JSON.stringify(l)).join('\n') + '\n');
  return h;
}

function collect(h) {
  const out = execFileSync(process.execPath, ['--input-type=module', '-'], {
    input: fs.readFileSync(SCRIPT), env: { HOME: h, PATH: '/usr/bin:/bin', FLEET_DO: 'collect', FLEET_PROC_SCAN: '0' }, timeout: 30_000 });
  return JSON.parse(String(out));
}

test('a Claude profile: its account from its config, and its newest status-line limits', () => {
  const r = collect(home());
  const a = r.accounts.find((x) => x.engine === 'claude' && x.id === 'work');
  assert.ok(a, 'the profile is listed');
  assert.equal(a.account.email, 'me@example.com');
  assert.equal(a.limits.five_hour.used_percentage, 12);
  assert.equal(a.limits.at, 2000);
});

test('a Codex home: who it is, its limits, and a session with its context and last words', () => {
  const r = collect(home());
  const a = r.accounts.find((x) => x.engine === 'codex');
  assert.deepEqual(a.account, { email: 'me@example.com', plan: 'pro', loggedIn: true });
  assert.equal(a.limits.primary.used_percent, 40);
  assert.equal(a.sessions.length, 1);
  const s = a.sessions[0];
  assert.equal(s.state, 'ready'); // no process scan: written to in the last ten minutes, its turn over
  assert.equal(s.model, 'gpt-5.5-codex');
  assert.deepEqual(s.context, { pct: 25, tokens: 64_600, size: 258_400 });
  assert.equal(s.said, 'Done: the list is in out.txt.');
  assert.ok(!JSON.stringify(r).includes('secret'), 'no token leaves the machine');
});

test('no fleet kit, no fleet', () => {
  assert.equal(collect(home()).fleet, null);
});

// ---------------------------------------------------------------- what a session is doing, from its roster row and transcript
import { sessionState } from '../fleet/machine.mjs';

const NOW = Date.UTC(2026, 9, 23, 12, 0, 0);
const turn = (over, quietMs, extra = {}) => ({ turnOver: over, writtenAt: NOW - quietMs, limitHit: null, ...extra });

test('an open session at its prompt is ready, not idle: its process is alive', () => {
  assert.deepEqual(sessionState({ pid: 41, kind: 'interactive', status: 'idle' }, turn(true, 5 * 86400_000), NOW), { state: 'ready', live: true });
});

test('working while the transcript is mid-turn', () => {
  assert.deepEqual(sessionState({ pid: 41, kind: 'interactive', status: 'busy' }, turn(false, 3000), NOW), { state: 'working', live: true });
});

test('busy only because a background task runs: its turn ended and nothing was written for a minute', () => {
  assert.deepEqual(sessionState({ pid: 41, kind: 'interactive', status: 'busy' }, turn(true, 6 * 60_000), NOW), { state: 'ready', live: true });
  // a turn that just ended may be the moment between two steps
  assert.deepEqual(sessionState({ pid: 41, kind: 'interactive', status: 'busy' }, turn(true, 5000), NOW), { state: 'working', live: true });
});

test('its last word was a limit stop: capped, whatever the roster says', () => {
  const hit = { text: "You've reached your Fable limit.", at: NOW - 3_400_000 };
  assert.deepEqual(sessionState({ pid: 41, kind: 'interactive', status: 'busy' }, turn(true, 3_400_000, { limitHit: hit, lastAt: hit.at }), NOW), { state: 'capped', live: true });
});

test('waiting on a permission, a question or a plan: needs you', () => {
  assert.deepEqual(sessionState({ pid: 41, kind: 'interactive', status: 'waiting' }, turn(false, 1000), NOW), { state: 'needs-you', live: true });
  assert.deepEqual(sessionState({ pid: 42, kind: 'background', state: 'blocked' }, turn(false, 1000), NOW), { state: 'needs-you', live: true });
});

test('a background job idle with no prompt yet reads blocked: it is open, not waiting on you', () => {
  assert.deepEqual(sessionState({ pid: 45, kind: 'background', state: 'blocked', status: 'idle' }, turn(true, 60_000), NOW), { state: 'ready', live: true });
});

test('a background job with no process is not running: blocked and stopped jobs keep their record, and are closed', () => {
  assert.deepEqual(sessionState({ kind: 'background', state: 'blocked' }, turn(false, 101 * 86400_000), NOW), { state: 'closed', live: false });
});

test('a background job that has ended is closed', () => {
  for (const state of ['done', 'stopped', 'failed']) assert.deepEqual(sessionState({ kind: 'background', state }, turn(true, 1000), NOW), { state: 'closed', live: false });
  for (const state of ['stopped', 'failed']) assert.deepEqual(sessionState({ pid: 44, kind: 'background', state }, turn(true, 1000), NOW), { state: 'closed', live: false });
});

test('a background job whose turn is done but whose process runs is open: it takes your next message', () => {
  assert.deepEqual(sessionState({ pid: 44, kind: 'background', state: 'done', status: 'idle' }, turn(true, 60_000), NOW), { state: 'ready', live: true });
  assert.deepEqual(sessionState({ pid: 43, kind: 'background', state: 'working' }, turn(false, 1000), NOW), { state: 'working', live: true });
});

// ---------------------------------------------------------------- how full a session's context is
import { compactAt, contextOf } from '../fleet/machine.mjs';

const SETTINGS = { modelSettings: { 'claude-fable-5': { effortLevel: 'xhigh' }, 'claude-fable-5-1': { autoCompactWindow: 400_000 } } };
const usage = (n) => ({ input_tokens: 0, cache_creation_input_tokens: 0, cache_read_input_tokens: n });

test('the compaction point: the most specific model entry that sets one, else the top level, else none', () => {
  assert.equal(compactAt(SETTINGS, 'claude-fable-5-1'), 400_000);
  assert.equal(compactAt(SETTINGS, 'claude-fable-5-1[1m]'), 400_000);
  assert.equal(compactAt(SETTINGS, 'claude-fable-5'), null);
  assert.equal(compactAt({ autoCompactWindow: 300_000 }, 'claude-opus-5-5'), 300_000);
  assert.equal(compactAt({}, 'claude-opus-5-5'), null);
});

test('context against where it compacts: 333k of 400k is 83%, not 33% of the window', () => {
  const sl = { at: 2000, context_window: { context_window_size: 1_000_000, used_percentage: 33, current_usage: usage(333_000) } };
  assert.deepEqual(contextOf(sl, { model: 'claude-fable-5-1', lastAt: 1000, contextTokens: 300_000 }, 1_000_000, SETTINGS),
    { pct: 83.3, tokens: 333_000, size: 1_000_000, compactAt: 400_000, fresh: true });
});

test('a session already past the setting started before it: its window counts', () => {
  const c = contextOf(null, { model: 'claude-fable-5-1', lastAt: 1000, contextTokens: 932_000 }, 1_000_000, SETTINGS);
  assert.deepEqual([c.pct, c.compactAt], [93.2, null]);
});

test('a status line that reads 0 right after a restart does not hide the context its transcript carries', () => {
  const sl = { at: 9000, context_window: { context_window_size: 1_000_000, used_percentage: 0, current_usage: usage(0) } };
  const c = contextOf(sl, { model: 'claude-opus-5-5', lastAt: 1000, contextTokens: 924_070 }, 1_000_000, {});
  assert.deepEqual([c.tokens, c.pct, c.fresh], [924_070, 92.4, false]);
});
