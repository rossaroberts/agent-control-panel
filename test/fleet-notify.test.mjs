// notify(): what happens when a session comes to need you. The Mac notification and the phone push are two channels with
// one switch, one rate limit, and separate failures.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path';
import { createFleet } from '../fleet/fleet.mjs';

function setup({ config = { public: 'mac.example-tailnet.ts.net' }, platform = 'darwin', build, push } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-notify-'));
  if (config) fs.writeFileSync(path.join(dir, 'fleet.json'), JSON.stringify(config));
  const pushed = [], macs = [];
  const fleet = createFleet({ dir, notifier: {
    platform,
    push: push ?? (async (a) => { pushed.push(a); return { sent: 1, failed: 0, dropped: 0 }; }),
    build,
    exec: (cmd, args) => { macs.push([cmd, args]); },
  } });
  return { fleet, pushed, macs, dir };
}
const sess = { sessionId: 'abc-123', title: 'Migrate', waitingFor: 'Approve the migration?' };

test('a session that needs you notifies the Mac and the phones, with the hash link and the public host as VAPID subject', () => {
  const t = setup();
  t.fleet.notify('Mac', sess);
  assert.equal(t.macs.length, 1);
  assert.equal(t.macs[0][0], 'osascript');
  assert.equal(t.pushed.length, 1);
  assert.equal(t.pushed[0].subject, 'https://mac.example-tailnet.ts.net');
  assert.equal(t.pushed[0].dir, t.dir);
  assert.deepEqual(t.pushed[0].payload, { title: 'Fleet · needs you', body: 'Approve the migration?\nMigrate · Mac', data: { url: '/#Mac%3Aabc-123' } });
});

test('"notify": false in fleet.json turns off both channels', () => {
  const t = setup({ config: { public: 'mac.example-tailnet.ts.net', notify: false } });
  t.fleet.notify('Mac', sess);
  assert.equal(t.macs.length, 0);
  assert.equal(t.pushed.length, 0);
});

test('without a public host there is no push, and the Mac notification still goes', () => {
  for (const config of [{}, null]) {
    const t = setup({ config });
    t.fleet.notify('Mac', sess);
    assert.equal(t.pushed.length, 0);
    assert.equal(t.macs.length, 1);
  }
});

test('off the Mac (no osascript) a configured public host still pushes, and nothing else is run', () => {
  const t = setup({ platform: 'linux' });
  t.fleet.notify('server', sess);
  assert.equal(t.pushed.length, 1);
  assert.equal(t.macs.length, 0);
  const none = setup({ platform: 'linux', config: {} });
  none.fleet.notify('server', sess);
  assert.equal(none.pushed.length, 0);
});

test('one notification per session per minute, on both channels; another session is not held back', (t0) => {
  const t = setup();
  const real = Date.now; let now = 1_000_000;
  Date.now = () => now; t0.after(() => { Date.now = real; });
  t.fleet.notify('Mac', sess);
  now += 59_000; t.fleet.notify('Mac', sess);
  assert.equal(t.macs.length, 1); assert.equal(t.pushed.length, 1);
  t.fleet.notify('Mac', { ...sess, sessionId: 'other' });
  assert.equal(t.macs.length, 2); assert.equal(t.pushed.length, 2);
  now += 2_000; t.fleet.notify('Mac', sess);
  assert.equal(t.macs.length, 3); assert.equal(t.pushed.length, 3);
});

test('a payload builder that throws never blocks the Mac notification, and notify() itself does not throw', () => {
  const t = setup({ build: () => { throw new URIError('URI malformed'); } });
  assert.doesNotThrow(() => t.fleet.notify('Mac', sess));
  assert.equal(t.macs.length, 1);
  assert.equal(t.pushed.length, 0);
});

test('a push that throws or rejects never blocks the Mac notification either', async () => {
  for (const push of [() => { throw new Error('sync'); }, async () => { throw new Error('async'); }]) {
    const t = setup({ push });
    assert.doesNotThrow(() => t.fleet.notify('Mac', sess));
    assert.equal(t.macs.length, 1);
    await new Promise((r) => setImmediate(r)); // an unhandled rejection would fail the run here
  }
});

test('the real payload builder survives a lone surrogate in a session id', () => {
  const t = setup({ build: undefined });
  assert.doesNotThrow(() => t.fleet.notify('Mac', { ...sess, sessionId: 'a\ud800b' }));
  assert.equal(t.pushed.length, 1);
  assert.equal(t.pushed[0].payload.data.url, '/#Mac%3Aa%EF%BF%BDb');
});
