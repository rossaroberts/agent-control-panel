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
const REAL = new URL('../fleet/hook.sh', import.meta.url).pathname; // the installer wants a hook that exists
const ours = (s, e) => (s.hooks?.[e] ?? []).flatMap((m) => m.hooks ?? []).filter((h) => /\/\.?(fleet-panel|fleet)\/hook\.sh'$/.test(h.command ?? ''));

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
  const r = spawnSync(process.execPath, [new URL('../fleet/install-hooks.mjs', import.meta.url).pathname, REAL, good, bad], { encoding: 'utf8' });
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

// ---------------------------------------------------------------------------------------------- the review of 2026-10-24

test('ours is exactly `sh \'<…>/fleet-panel/hook.sh\'`: a command that only mentions the hook is someone else\'s and stays', () => {
  const theirs = { type: 'command', command: `sh '${HOOK}' && rm -rf ~/.claude` };
  const s = { hooks: { Stop: [{ hooks: [theirs] }] } };
  const added = withHook(s, HOOK).settings;
  assert.equal(added.hooks.Stop.length, 2, 'not taken for ours: ours is added beside it');
  const removed = withHook(added, HOOK, { remove: true }).settings;
  assert.deepEqual(removed.hooks.Stop, [{ hooks: [theirs] }], 'theirs is kept, ours alone goes');
  // the dotted folder spelling is ours too; a hook.sh under another name is not
  assert.deepEqual(withHook({ hooks: { Stop: [{ hooks: [{ type: 'command', command: "sh '/x/.fleet-panel/hook.sh'" }] }] } }, HOOK).changes.filter((c) => c === '+ Stop'), []);
  assert.deepEqual(withHook({ hooks: { Stop: [{ hooks: [{ type: 'command', command: "sh '/x/other/hook.sh'" }] }] } }, HOOK).changes.filter((c) => c === '+ Stop'), ['+ Stop']);
});

test('a hook path with a quote is refused; a hooks.<Event> that is not a list is refused; entries that are not objects are dropped', () => {
  assert.throws(() => withHook({}, "/Users/o'brien/.fleet-panel/hook.sh"), /quote/);
  assert.throws(() => withHook({ hooks: { Stop: { hooks: [] } } }, HOOK), /hooks\.Stop is not a list/);
  const s = withHook({ hooks: { Stop: [null, 'x', { hooks: [{ type: 'command', command: 'theirs.sh' }] }] } }, HOOK).settings;
  assert.equal(s.hooks.Stop.length, 2);
  assert.equal(s.hooks.Stop[0].hooks[0].command, 'theirs.sh');
});

test('the installer: a settings.json that is a link stays a link; a hook path that does not exist is refused before any file is touched', () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-hooks-'));
  const real = path.join(d, 'dotfiles', 'settings.json'), prof = path.join(d, 'prof');
  fs.mkdirSync(path.dirname(real)); fs.mkdirSync(prof);
  fs.writeFileSync(real, JSON.stringify({ model: 'opus' }));
  fs.symlinkSync(real, path.join(prof, 'settings.json'));
  const installer = new URL('../fleet/install-hooks.mjs', import.meta.url).pathname;
  const missing = spawnSync(process.execPath, [installer, '/nowhere/fleet-panel/hook.sh', prof], { encoding: 'utf8' });
  assert.equal(missing.status, 2);
  assert.match(missing.stderr, /no such file/);
  assert.deepEqual(JSON.parse(fs.readFileSync(real, 'utf8')), { model: 'opus' }, 'nothing written');
  const r = spawnSync(process.execPath, [installer, REAL, prof], { encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
  assert.ok(fs.lstatSync(path.join(prof, 'settings.json')).isSymbolicLink(), 'still a link');
  const s = JSON.parse(fs.readFileSync(real, 'utf8'));
  assert.equal(s.model, 'opus');
  assert.equal(ours(s, 'Stop').length, 1, 'the file it points to has the hook');
  // --remove needs no hook file: the path may already be gone
  const gone = spawnSync(process.execPath, [installer, '/nowhere/fleet-panel/hook.sh', prof, '--remove'], { encoding: 'utf8' });
  assert.equal(gone.status, 0, gone.stderr);
});

test('the hook keeps the event small: a tool\'s error text is not stored, and a write that fails leaves no half file', () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-hook-'));
  const run = (payload, env = {}) => spawnSync('sh', [new URL('../fleet/hook.sh', import.meta.url).pathname], { input: payload, encoding: 'utf8', env: { ...process.env, CLAUDE_CONFIG_DIR: d, ...env } });
  const r = run(JSON.stringify({ session_id: 'abc-123', hook_event_name: 'PostToolUseFailure', tool_name: 'Bash', error: 'x'.repeat(100_000), is_interrupt: false }));
  assert.equal(r.status, 0);
  const got = JSON.parse(fs.readFileSync(path.join(d, 'fleet-state', 'abc-123.json'), 'utf8'));
  assert.equal(got.e, 'PostToolUseFailure');
  assert.ok(JSON.stringify(got).length < 500, 'the error text stays out');
  // a folder that cannot be written: nothing printed, exit 0, no temp file left
  fs.chmodSync(path.join(d, 'fleet-state'), 0o500);
  try {
    const bad = run(JSON.stringify({ session_id: 'def-456', hook_event_name: 'Stop' }));
    assert.equal(bad.status, 0); assert.equal(bad.stdout, '');
    assert.deepEqual(fs.readdirSync(path.join(d, 'fleet-state')).filter((n) => n.startsWith('.')), []);
  } finally { fs.chmodSync(path.join(d, 'fleet-state'), 0o700); }
});
