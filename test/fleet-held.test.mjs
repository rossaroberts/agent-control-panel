// Held permission requests laid over Fleet's rows (fleet/held.mjs). Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { heldIndex, shown, withHeld } from '../fleet/held.mjs';

const req = (over) => ({ id: 'r1', sessionId: 's1', agentId: null, agentType: null, toolName: 'Bash', toolInput: { command: 'ls' }, suggestions: [],
  permissionMode: 'default', createdAt: 1000, deadline: 9000, profileDir: '/p', res: {}, timer: {}, ...over });
const snapshot = () => ({ ok: true, at: 5, machines: [
  { name: 'mac', local: true, accounts: [{ engine: 'claude', id: 'default', sessions: [{ sessionId: 's1', state: 'needs-you' }, { sessionId: 's2' }] }, { engine: 'codex', id: 'x', sessions: [{ sessionId: 's1' }] }] },
  { name: 'far', local: false, accounts: [{ engine: 'claude', id: 'default', sessions: [{ sessionId: 's1', waitingFor: 'permission prompt' }] }] },
] });

test('a local Claude row carries its held request, or null; a remote machine\'s rows carry no pending key at all', () => {
  const out = withHeld(snapshot(), heldIndex([req()]));
  const [s1, s2] = out.machines[0].accounts[0].sessions;
  assert.equal(s1.pending.id, 'r1');
  assert.equal(s1.pending.toolName, 'Bash');
  assert.equal(s2.pending, null);
  assert.equal('pending' in out.machines[1].accounts[0].sessions[0], false, 'same session id, other machine: nothing is held for it here');
  assert.equal('pending' in out.machines[0].accounts[1].sessions[0], false, 'a Codex thread has no hooks to hold');
});

test('the snapshot Fleet shares between pages is not written to', () => {
  const snap = snapshot(), before = JSON.stringify(snap);
  withHeld(snap, heldIndex([req()]));
  assert.equal(JSON.stringify(snap), before);
});

test('a row shows what the card needs and none of the plumbing', () => {
  const p = shown(req({ suggestions: [{ type: 'setMode' }] }));
  assert.deepEqual(Object.keys(p).sort(), ['agentId', 'agentType', 'createdAt', 'deadline', 'id', 'open', 'permissionMode', 'suggestions', 'toolInput', 'toolName']);
  assert.equal(p.open, false);
});

test('a session\'s main agent comes before a subagent, then the oldest', () => {
  const held = heldIndex([req({ id: 'sub', agentId: 'a1', agentType: 'Explore', createdAt: 1 }), req({ id: 'late', createdAt: 3000 }), req({ id: 'early', createdAt: 2000 })]);
  assert.equal(held('s1').id, 'early');
  assert.equal(heldIndex([req({ id: 'sub', agentId: 'a1', agentType: 'Explore' })])('s1').agentType, 'Explore');
  assert.equal(held('nobody'), null);
});

test('an open question or plan from a transcript is shown as open; long file contents are cut, questions and plans are whole', () => {
  const long = 'x'.repeat(9000);
  assert.equal(shown(req({ open: true, id: 'open:s1:t1' })).open, true);
  assert.equal(shown(req({ toolName: 'Write', toolInput: { file_path: '/a', content: long } })).toolInput.content.length, 4000);
  assert.equal(shown(req({ toolName: 'ExitPlanMode', toolInput: { plan: long } })).toolInput.plan.length, 9000);
  assert.equal(shown(req({ toolName: 'AskUserQuestion', toolInput: { questions: [{ question: long }] } })).toolInput.questions[0].question.length, 9000);
});

test('a snapshot that is not a list of machines passes through', () => {
  assert.deepEqual(withHeld({ ok: false, error: 'x' }, () => null), { ok: false, error: 'x' });
});
