// Control over a session from the page (fleet/machine.mjs): what counts as a permission prompt, when a background job
// with no process can still be starting, and a step's brief name. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { atPrompt, jobFresh, sessionState } from '../fleet/machine.mjs';

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
