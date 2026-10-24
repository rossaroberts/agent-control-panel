// Fleet's hook into Claude Code settings (fleet/install-hooks.mjs): what it adds, what it leaves alone, that a second run
// changes nothing, that it comes out again cleanly, and that a settings file it can't read is left untouched. Plus the hook
// script itself (fleet/hook.sh) on real and hostile input. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';
import { withHook, EVENTS } from '../fleet/install-hooks.mjs';

const HOOK = '/Users/x/.fleet-panel/hook.sh';
const ours = (s, e) => (s.hooks?.[e] ?? []).flatMap((m) => m.hooks ?? []).filter((h) => /fleet-panel\/hook\.sh/.test(h.command ?? ''));

test('adds the hook for every event, sync with a short timeout, and keeps what was there', () => {
  const before = { model: 'opus', hooks: { SessionStart: [{ hooks: [{ type: 'command', command: 'catchup.sh' }] }],
    PreToolUse: [{ hooks: [{ type: 'http', url: 'http://127.0.0.1:4747/hook/event' }] }] } };
  const { settings, changes } = withHook(before, HOOK);
  assert.equal(settings.model, 'opus');
  for (const e of EVENTS) assert.deepEqual(ours(settings, e), [{ type: 'command', command: `sh '${HOOK}'`, timeout: 5 }], e);
  assert.equal(settings.hooks.SessionStart[0].hooks[0].command, 'catchup.sh', 'its own hooks stay first and as they were');
  assert.equal(settings.hooks.PreToolUse[0].hooks[0].url, 'http://127.0.0.1:4747/hook/event');
  assert.equal(settings.hooks.PreToolUse[0].hooks[0].timeout, undefined, 'the panel\'s HTTP hooks only get a timeout when asked');
  assert.equal(changes.length, EVENTS.length);
  assert.deepEqual(before.hooks.SessionStart.length, 1, 'the settings it was given are not changed');
});

test('a second run changes nothing; --cap-http times out only the panel\'s event hooks; --remove takes it out cleanly', () => {
  const start = { hooks: { PermissionRequest: [{ hooks: [{ type: 'http', url: 'http://127.0.0.1:4747/hook/permission' }] }],
    Stop: [{ hooks: [{ type: 'http', url: 'http://127.0.0.1:4747/hook/event' }] }] } };
  const once = withHook(start, HOOK, { capHttp: true }).settings;
  assert.deepEqual(withHook(once, HOOK, { capHttp: true }).changes, []);
  assert.equal(once.hooks.Stop[0].hooks[0].timeout, 5);
  assert.equal(once.hooks.PermissionRequest[0].hooks[0].timeout, undefined, 'a held permission request keeps its own wait');
  const out = withHook(once, HOOK, { remove: true }).settings;
  for (const e of EVENTS) assert.deepEqual(ours(out, e), [], e);
  assert.deepEqual(out.hooks.PermissionRequest, start.hooks.PermissionRequest);
  assert.equal(out.hooks.SessionEnd, undefined, 'an event that only had ours is gone, not left empty');
  assert.deepEqual(withHook({}, HOOK, { remove: true }).settings, {});
});

test('a "hooks" that is not an object is refused (left as it is)', () => {
  assert.throws(() => withHook({ hooks: [] }, HOOK), /not an object/);
});

test('the installer: a copy kept, the file swapped whole, unreadable JSON left untouched', () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-hooks-'));
  const good = path.join(d, 'good'), bad = path.join(d, 'bad');
  fs.mkdirSync(good); fs.mkdirSync(bad);
  fs.writeFileSync(path.join(good, 'settings.json'), JSON.stringify({ model: 'opus' }));
  fs.writeFileSync(path.join(bad, 'settings.json'), '{ not json');
  const r = spawnSync(process.execPath, [new URL('../fleet/install-hooks.mjs', import.meta.url).pathname, HOOK, good, bad], { encoding: 'utf8' });
  assert.equal(r.status, 1, 'it says one failed');
  assert.match(r.stderr, /bad\/settings\.json: left as it is/);
  assert.equal(fs.readFileSync(path.join(bad, 'settings.json'), 'utf8'), '{ not json');
  const s = JSON.parse(fs.readFileSync(path.join(good, 'settings.json'), 'utf8'));
  assert.equal(s.model, 'opus');
  assert.equal(ours(s, 'Stop').length, 1);
  assert.equal(fs.readdirSync(good).filter((n) => n.startsWith('settings.json.before-fleet-hooks-')).length, 1, 'a copy of what was there');
  assert.equal(fs.readdirSync(good).filter((n) => n.includes('fleet-tmp')).length, 0);
});

test('the hook: one small file per session, main conversation only, nothing printed, never a failure', () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-hook-'));
  const run = (payload) => spawnSync('sh', [new URL('../fleet/hook.sh', import.meta.url).pathname], { input: payload, encoding: 'utf8', env: { ...process.env, CLAUDE_CONFIG_DIR: d } });
  const file = path.join(d, 'fleet-state', 'abc-123.json'), read = () => JSON.parse(fs.readFileSync(file, 'utf8'));
  let r = run(JSON.stringify({ session_id: 'abc-123', hook_event_name: 'PreToolUse', tool_name: 'AskUserQuestion' }));
  assert.equal(r.status, 0); assert.equal(r.stdout, ''); assert.equal(read().e, 'PreToolUse'); assert.equal(read().t, 'AskUserQuestion');
  run(JSON.stringify({ session_id: 'abc-123', hook_event_name: 'PostToolUse', tool_name: 'Bash', agent_id: 'sub1' }));
  assert.equal(read().t, 'AskUserQuestion', 'a subagent\'s event is left out');
  run(JSON.stringify({ session_id: 'abc-123', hook_event_name: 'Notification', notification_type: 'auth_success' }));
  assert.equal(read().e, 'PreToolUse', 'a notification that says nothing of its state is left out');
  for (const bad of ['not json', '{}', JSON.stringify({ session_id: '../../etc', hook_event_name: 'Stop' })]) {
    r = run(bad); assert.equal(r.status, 0); assert.equal(r.stdout, '');
  }
  assert.deepEqual(fs.readdirSync(path.join(d, 'fleet-state')), ['abc-123.json'], 'nothing else written, nowhere else');
  run(JSON.stringify({ session_id: 'abc-123', hook_event_name: 'Stop' }));
  assert.equal(read().e, 'Stop');
});
