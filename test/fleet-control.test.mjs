// Control over a session from the page (fleet/machine.mjs): what counts as a permission prompt, when a background job
// with no process can still be starting, and a step's brief name. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { atPrompt, jobFresh, jobOpen, resume, sessionState } from '../fleet/machine.mjs';
import { easeMs } from '../fleet/fleet.mjs';

test('a permission prompt: the roster says so, or the last hook event was the request itself', () => {
  assert.equal(atPrompt({ waitingFor: 'permission prompt' }, null), true);
  assert.equal(atPrompt({ waitingFor: 'Permission: Bash' }, null), true);
  assert.equal(atPrompt({ status: 'waiting' }, { e: 'PermissionRequest', t: 'Bash', at: 1 }), true);
  assert.equal(atPrompt({ status: 'waiting' }, { e: 'Notification', n: 'permission_prompt', at: 1 }), true);
  assert.equal(atPrompt({ status: 'waiting' }, { e: 'PreToolUse', t: 'AskUserQuestion', at: 1 }), false, 'a question to you is not a permission');
  assert.equal(atPrompt({ status: 'busy' }, { e: 'PostToolUse', t: 'Bash', at: 1 }), false);
  assert.equal(atPrompt({}, null), false);
  assert.equal(atPrompt(null, null), false);
});

test('a background job with no process is fresh only while its record was written to in the last ten minutes', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-jobs-'));
  const p = { dir };
  const write = (id, s) => { fs.mkdirSync(path.join(dir, 'jobs', id), { recursive: true }); fs.writeFileSync(path.join(dir, 'jobs', id, 'state.json'), JSON.stringify(s)); };
  const NOW = Date.parse('2026-10-25T14:00:00Z');
  write('aaaaaaaa', { state: 'running', createdAt: '2026-10-25T13:58:00Z', updatedAt: '2026-10-25T13:59:00Z' });
  write('bbbbbbbb', { state: 'running', createdAt: '2026-10-25T10:17:00Z', updatedAt: '2026-10-25T13:04:00Z' }); // died with the Mac at 13:xx
  write('cccccccc', { state: 'running', createdAt: '2026-10-25T13:55:00Z' }); // no updatedAt yet: created counts
  assert.equal(jobFresh(p, 'aaaaaaaa', NOW), true, 'just started: a startup question, say');
  assert.equal(jobFresh(p, 'bbbbbbbb', NOW), false, 'an hour old and no process: it died');
  assert.equal(jobFresh(p, 'cccccccc', NOW), true);
  assert.equal(jobFresh(p, 'dddddddd', NOW), false, 'no record at all');
  assert.equal(jobFresh(p, '../etc', NOW), false, 'only a job id is read');
  // The roster still lists such a job as running: without the record's word it would read live (and needs-you, from its
  // last hook) — with `active` withheld it is closed.
  assert.deepEqual(sessionState({ kind: 'background', state: 'blocked' }, { turnOver: false, lastAt: NOW - 3600_000 }, { e: 'PermissionRequest', t: 'Bash', at: NOW - 3600_000 }, NOW), { state: 'closed', live: false });
  fs.rmSync(dir, { recursive: true, force: true });
});

test('a background job idle at its prompt is waiting, whatever its job state says', () => {
  const NOW = Date.parse('2026-10-25T14:00:00Z');
  const t = { turnOver: false, lastAt: NOW - 48 * 60_000, writtenAt: NOW - 48 * 60_000 };
  // Started again after a reboot: its last step (from before) has no result, Claude Code says the job works, its status says idle.
  assert.deepEqual(sessionState({ pid: 50, kind: 'background', state: 'working', status: 'idle' }, t, null, NOW), { state: 'ready', live: true });
  assert.deepEqual(sessionState({ pid: 50, kind: 'background', state: 'working', status: 'busy' }, t, null, NOW), { state: 'working', live: true });
  assert.deepEqual(sessionState({ pid: 50, kind: 'background', state: 'working' }, t, null, NOW), { state: 'working', live: true }, 'no status: the state stands');
});

test('an overloaded machine is read less often: every 30 s past twice its cores, every 60 s past four times', () => {
  assert.equal(easeMs(0.4), 0, 'a quiet machine is read every time');
  assert.equal(easeMs(2), 0);
  assert.equal(easeMs(2.1), 30_000);
  assert.equal(easeMs(34.3 / 8), 60_000, 'redbat at load 34 on 8 cores');
  assert.equal(easeMs(NaN), 0);
});

test('a job with a process but a record that says stopped is closed, and continue respawns it rather than calling it running', () => {
  // 2026-11-01: a host died under load, the daemon respawned it, and the record stayed `stopped` beside the new idle pid.
  // The list read it closed; resume saw the pid and answered "running" without doing anything: closed and unopenable.
  const NOW = Date.parse('2026-11-01T10:54:00Z');
  const row = { kind: 'background', id: 'e0d17ae7', pid: 86137, status: 'idle', state: 'stopped' };
  assert.deepEqual(sessionState(row, null, null, NOW), { state: 'closed', live: false });
  assert.equal(jobOpen(row), false, 'the same answer resume must give: not running, so respawn');
  assert.equal(jobOpen({ ...row, state: 'failed' }), false);
  assert.equal(jobOpen({ ...row, state: 'working' }), true);
  assert.equal(jobOpen({ ...row, state: 'blocked', status: 'idle' }), true, 'idle with no prompt yet is open');
  assert.equal(jobOpen({ ...row, pid: undefined, state: 'working' }), false, 'no process is not open, whatever the record says');
  assert.equal(jobOpen(null), false);
});

test('continue on that row respawns it, and a respawn that fails never starts a copy beside the stale host', async () => {
  const row = { kind: 'background', id: 'e0d17ae7', pid: 86137, status: 'idle', state: 'stopped', sessionId: 'e0d17ae7-c11f-4956-9c08-e201827066b9', cwd: '/tmp' };
  const a = { profile: 'p', sessionId: row.sessionId, id: row.id };
  const calls = [];
  const fake = (roster, respawn) => async (_cmd, args) => {
    calls.push(args.join(' '));
    if (args[0] === 'agents') return { stdout: JSON.stringify(roster), stderr: '', err: null };
    if (args[0] === 'respawn') return respawn;
    return { stdout: '', stderr: `unexpected: ${args.join(' ')}`, err: new Error('unexpected') };
  };
  const profiles = () => [{ id: 'p', dir: '/nonexistent/profile' }];
  // 1. respawn works: the job comes back under its own record, and nothing else is started
  let r = await resume(a, { profiles, run: fake([row], { stdout: 'respawned e0d17ae7\n', stderr: '', err: null }) });
  assert.deepEqual(r, { ok: true, id: 'e0d17ae7' });
  assert.deepEqual(calls, ['agents --json --all', 'respawn e0d17ae7']);
  // 2. respawn fails while the stale host lives: an error, not a `--bg --resume` copy beside it
  calls.length = 0;
  r = await resume(a, { profiles, run: fake([row], { stdout: '', stderr: 'host busy', err: new Error('exit 1') }) });
  assert.equal(r.ok, false);
  assert.match(r.error, /host busy/);
  assert.ok(!calls.some((c) => c.includes('--bg --resume')), 'no copy: ' + calls.join(' | '));
  // 3. a row Claude Code calls live is left as it is
  calls.length = 0;
  r = await resume(a, { profiles, run: fake([{ ...row, state: 'working' }], null) });
  assert.deepEqual(r, { ok: true, id: 'e0d17ae7', running: true });
  assert.deepEqual(calls, ['agents --json --all']);
});
