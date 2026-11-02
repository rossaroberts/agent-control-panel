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
  assert.deepEqual(Object.keys(p).sort(), ['agentId', 'agentType', 'clipped', 'createdAt', 'deadline', 'id', 'open', 'permissionMode', 'suggestions', 'toolInput', 'toolName']);
  assert.equal(p.open, false);
});

test('a session\'s main agent comes before a subagent, then the oldest', () => {
  const held = heldIndex([req({ id: 'sub', agentId: 'a1', agentType: 'Explore', createdAt: 1 }), req({ id: 'late', createdAt: 3000 }), req({ id: 'early', createdAt: 2000 })]);
  assert.equal(held('s1').id, 'early');
  assert.equal(heldIndex([req({ id: 'sub', agentId: 'a1', agentType: 'Explore' })])('s1').agentType, 'Explore');
  assert.equal(held('nobody'), null);
});

test('an open question or plan from a transcript is shown as open', () => {
  assert.equal(shown(req({ open: true, id: 'open:s1:t1' })).open, true);
});

test('a long string is cut at 4,000 characters with a marker that says how many were left out, never silently', () => {
  const p = shown(req({ toolName: 'Write', toolInput: { file_path: '/a', content: 'x'.repeat(10_000) } }));
  assert.equal(p.toolInput.content, `${'x'.repeat(4000)}\n… 6000 more chars`);
  assert.deepEqual(p.clipped, ['content']);
  assert.equal(p.toolInput.file_path, '/a');
  assert.deepEqual(shown(req({ toolInput: { command: 'ls' } })).clipped, [], 'nothing cut, nothing to say');
});

test('strings are cut wherever they sit: a MultiEdit of four 400 kB edits is a few kB on the row, not 1.6 MB', () => {
  const big = 'y'.repeat(200_000);
  const input = { file_path: '/a', edits: Array.from({ length: 4 }, () => ({ old_string: big, new_string: big })) };
  const p = shown(req({ toolName: 'MultiEdit', toolInput: input }));
  assert.ok(JSON.stringify(p).length < 20_000, `row is ${JSON.stringify(p).length} bytes`);
  // Eight cut strings are 32 kB of JSON, over the 16 kB a row carries: the input is replaced, and says how big it was.
  assert.equal(p.toolInput._truncated, true);
  assert.ok(p.toolInput.bytes > 1_600_000);
  const two = shown(req({ toolName: 'MultiEdit', toolInput: { file_path: '/a', edits: [{ old_string: big, new_string: 'z' }] } }));
  assert.equal(two.toolInput._truncated, undefined, 'one cut string is within the 16 kB');
  assert.match(two.toolInput.edits[0].old_string, /… 196000 more chars$/);
  assert.deepEqual(two.clipped, ['edits']);
});

test('many small strings that add up past 16 kB are replaced too, with the size they were', () => {
  const p = shown(req({ toolName: 'Task', toolInput: { items: Array.from({ length: 2000 }, (_, i) => `item number ${i}`) } }));
  assert.equal(p.toolInput._truncated, true);
  assert.ok(p.toolInput.bytes > 16_384);
});

test('a question or a plan is read whole up to 64 kB, then replaced like any other input', () => {
  const plan = 'p'.repeat(20_000);
  assert.equal(shown(req({ toolName: 'ExitPlanMode', toolInput: { plan } })).toolInput.plan.length, 20_000);
  assert.equal(shown(req({ toolName: 'ExitPlanMode', toolInput: { plan: 'p'.repeat(70_000) } })).toolInput._truncated, true);
  assert.equal(shown(req({ toolName: 'AskUserQuestion', toolInput: { questions: [{ question: 'q'.repeat(9000) }] } })).toolInput.questions[0].question.length, 9000);
});

test('a session with more than one held request says how many (`of`), showing the first', () => {
  const held = heldIndex([req({ id: 'a', createdAt: 1 }), req({ id: 'b', createdAt: 2 }), req({ id: 'c', sessionId: 's2' })]);
  assert.equal(held('s1').id, 'a');
  assert.equal(held('s1').of, 2);
  assert.equal(held('s2').of, 1);
});

test('a snapshot that is not a list of machines passes through', () => {
  assert.deepEqual(withHeld({ ok: false, error: 'x' }, () => null), { ok: false, error: 'x' });
});
