// Fleet's per-machine worker and the panel's own records: the worker run here as it runs over SSH (machine.mjs as its
// first line, then a request a line), your names and groups, and the logins fleet.json corrects. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { WORKER_BOOT, createFleet } from '../fleet/fleet.mjs';

const SCRIPT = fileURLToPath(new URL('../fleet/machine.mjs', import.meta.url));

/** A home with one Claude profile whose status line has reported its limits. */
function home() {
  const h = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-worker-'));
  const p = path.join(h, '.claude-work');
  fs.mkdirSync(path.join(p, 'fleet-status'), { recursive: true });
  fs.writeFileSync(path.join(p, 'settings.json'), '{}');
  fs.writeFileSync(path.join(p, '.claude.json'), JSON.stringify({ oauthAccount: { emailAddress: 'me@example.com' } }));
  fs.writeFileSync(path.join(p, 'fleet-status', 's-1.json'), JSON.stringify({ at: 1000, session_id: 's-1', model: { id: 'claude-opus-5-5' },
    context_window: { context_window_size: 1_000_000, used_percentage: 20 }, rate_limits: { five_hour: { used_percentage: 7, resets_at: 1 }, seven_day: { used_percentage: 3, resets_at: 2 } } }));
  return h;
}

function startWorker(t, h) {
  const child = spawn(process.execPath, ['--input-type=module', '-e', WORKER_BOOT],
    { env: { HOME: h, PATH: '/usr/bin:/bin', FLEET_PROC_SCAN: '0' }, stdio: ['pipe', 'pipe', 'pipe'] });
  const lines = [];
  let buf = '', waiters = [];
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (d) => {
    buf += d;
    for (let i; (i = buf.indexOf('\n')) >= 0;) { lines.push(JSON.parse(buf.slice(0, i))); buf = buf.slice(i + 1); }
    for (const w of waiters) w();
  });
  const next = (match, ms = 20_000) => new Promise((resolve, reject) => {
    const check = () => { const l = lines.find(match); if (l) { waiters = waiters.filter((w) => w !== check); clearTimeout(t); resolve(l); } };
    const t = setTimeout(() => reject(new Error(`no line in ${ms}ms; have ${JSON.stringify(lines).slice(0, 300)}`)), ms);
    waiters.push(check); check();
  });
  const exited = new Promise((r) => child.on('exit', (code) => r(code)));
  t.after(() => { if (child.exitCode === null) child.kill(); }); // a failed test never leaves it running
  return { child, lines, next, exited, send: (o) => child.stdin.write(JSON.stringify(o) + '\n') };
}

test('the worker loads machine.mjs from its first line, then answers requests by id, many at a time', async (t) => {
  const w = startWorker(t, home());
  w.child.stdin.write(fs.readFileSync(SCRIPT).toString('base64') + '\n');
  await w.next((l) => l.ready);
  w.send({ id: 1, do: 'collect' });
  w.send({ id: 2, do: 'peek', args: { profile: 'work', sessionId: 'not-a-session' } });
  w.send({ id: 3, do: 'unlink', args: {} }); // not one of its jobs: ignored
  const collect = await w.next((l) => l.id === 1);
  assert.equal(collect.r.accounts.find((a) => a.id === 'work').account.email, 'me@example.com');
  assert.equal(collect.r.accounts.find((a) => a.id === 'work').limits.five_hour.used_percentage, 7);
  assert.equal((await w.next((l) => l.id === 2)).r.ok, false);
  w.child.stdin.end();
  assert.equal(await w.exited, 0);
  assert.equal(w.lines.some((l) => l.id === 3), false, 'an unknown job gets no answer');
});

test('end of input: what it started is answered before it exits', async (t) => {
  const w = startWorker(t, home());
  w.child.stdin.write(fs.readFileSync(SCRIPT).toString('base64') + '\n');
  await w.next((l) => l.ready);
  w.send({ id: 7, do: 'collect' });
  w.child.stdin.end(); // straight away, while collect runs
  assert.equal(await w.exited, 0);
  assert.ok(w.lines.some((l) => l.id === 7 && l.r.accounts), 'collect was answered first');
});

test('your name and group for a session are kept; empty puts haiku\'s back; a bad group is refused', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-labels-'));
  const fleet = createFleet({ dir });
  const sid = '11111111-2222-3333-4444-555555555555';
  assert.deepEqual(fleet.label({ machine: 'Mac', sessionId: sid, title: 'My name', group: 'Experiments' }), { ok: true });
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(dir, 'fleet-labels.json'), 'utf8')), { [`Mac:${sid}`]: { title: 'My name', group: 'Experiments' } });
  assert.deepEqual(fleet.label({ machine: 'Mac', sessionId: sid, title: '' }), { ok: true });
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(dir, 'fleet-labels.json'), 'utf8')), { [`Mac:${sid}`]: { group: 'Experiments' } });
  assert.equal(fleet.label({ machine: 'Mac', sessionId: sid, group: 'a/b' }).ok, false);
  assert.equal(fleet.label({ machine: 'Mac', sessionId: '../../x', title: 'x' }).ok, false);
});

test('fleet.json logins: a profile\'s real account, where its config says another; bad entries are dropped', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-logins-'));
  fs.writeFileSync(path.join(dir, 'fleet.json'), JSON.stringify({ logins: { 'Mac:default': 'rar@example.com', 'Mac:bad;rm': 'x@example.com', 'Mac:work': 'not an email' } }));
  assert.deepEqual(createFleet({ dir }).config().logins, { 'Mac:default': 'rar@example.com' });
});
