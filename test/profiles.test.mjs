// Held requests and profiles: a permission request from any profile is held, shown while its profile is the one on
// the board, and kept when you switch. Run with `npm test`. Starts the server on a spare port under a throwaway home.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const SERVER = fileURLToPath(new URL('../server.mjs', import.meta.url));
const PORT = 47400 + Math.floor(Math.random() * 100);
const BASE = `http://127.0.0.1:${PORT}`;
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-home-'));
// Two profiles, both with hooks pointing here. `claude` itself is a stand-in: an empty roster, nobody signed in.
for (const d of ['.claude', '.claude-other']) {
  fs.mkdirSync(path.join(home, d, 'projects', 'p'), { recursive: true });
  fs.writeFileSync(path.join(home, d, 'settings.json'), JSON.stringify({ hooks: { PermissionRequest: [{ hooks: [{ type: 'http', url: `${BASE}/hook/permission` }] }] } }));
}
const bin = path.join(home, 'bin');
fs.mkdirSync(bin);
fs.writeFileSync(path.join(bin, 'claude'), '#!/bin/sh\ncase "$1" in agents) echo "[]";; auth) echo "{}";; *) exit 1;; esac\n', { mode: 0o755 });
const prefs = path.join(home, 'panel', 'prefs.json');
fs.mkdirSync(path.dirname(prefs));
fs.writeFileSync(prefs, JSON.stringify({ profile: 'default' }));

const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE')));
const server = spawn(process.execPath, [SERVER], { env: { ...env, HOME: home, PATH: `${bin}:${env.PATH}`, ACP_PORT: String(PORT), ACP_PREFS: prefs }, stdio: ['ignore', 'pipe', 'pipe'] });
let log = '';
server.stdout.on('data', (d) => { log += d; });
server.stderr.on('data', (d) => { log += d; });
await new Promise((resolve, reject) => {
  const t = setTimeout(() => reject(new Error('server did not start: ' + log)), 10_000);
  server.stdout.on('data', () => { if (log.includes(BASE)) { clearTimeout(t); resolve(); } });
  server.on('exit', () => reject(new Error('server exited: ' + log)));
});
// The pairing cookie a browser holds once `node server.mjs open` has run: the server made the key beside prefs.json as it booted.
const paired = { cookie: `acp_k=${fs.readFileSync(path.join(path.dirname(prefs), 'key'), 'utf8').trim()}` };
const token = (await (await fetch(BASE + '/', { headers: paired })).text()).match(/[0-9a-f]{48}/)[0];
const api = (p, body) => fetch(BASE + p, { method: 'POST', headers: { 'content-type': 'application/json', origin: BASE, 'x-acp-token': token, ...paired }, body: JSON.stringify(body) }).then((r) => r.json());

// A page watching, so requests are held at all. Frames arrive as they are sent.
const frames = [];
const watcher = new AbortController();
fetch(`${BASE}/events?t=${token}`, { headers: paired, signal: watcher.signal }).then(async (r) => {
  let buf = '';
  for await (const chunk of r.body) {
    buf += Buffer.from(chunk).toString();
    for (const m of buf.matchAll(/^data: (.*)$/gm)) frames.push(JSON.parse(m[1]));
    buf = buf.slice(buf.lastIndexOf('\n\n') + 2);
  }
}).catch(() => {});
const latest = async () => { await api('/api/arm', { armed: true }); await new Promise((r) => setTimeout(r, 250)); return frames.at(-1); };
const pause = (ms) => new Promise((r) => setTimeout(r, ms));
after(() => { watcher.abort(); server.kill(); fs.rmSync(home, { recursive: true, force: true }); });

/** Posts a permission request as Claude Code would, from a session of `profileDir`. Resolves when the panel answers. */
function hook(sessionId, profileDir) {
  const e = { session_id: sessionId, transcript_path: path.join(profileDir, 'projects', 'p', `${sessionId}.jsonl`), cwd: home,
              hook_event_name: 'PermissionRequest', tool_name: 'Bash', tool_input: { command: 'echo ' + sessionId }, permission_mode: 'default' };
  const done = { answered: false, body: null };
  done.promise = fetch(BASE + '/hook/permission', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(e) })
    .then(async (r) => { done.answered = true; done.body = await r.text(); return done.body; });
  return done;
}
const pendingOf = (snap, sessionId) => (snap?.pending ?? []).filter((p) => p.sessionId === sessionId);

test('a request from the profile on the board is held and shown', async () => {
  const h = hook('s-default', path.join(home, '.claude'));
  await pause(300);
  assert.equal(h.answered, false, 'the panel keeps the request open');
  const snap = await latest();
  assert.equal(snap.profile.dir, path.join(home, '.claude'));
  assert.equal(pendingOf(snap, 's-default').length, 1);
  assert.notEqual(pendingOf(snap, 's-default')[0].open, true);
  await api('/api/decide', { id: pendingOf(snap, 's-default')[0].id, behavior: 'deny', message: 'not now' });
  assert.match(await h.promise, /"behavior":"deny"/);
});

test('a request from another profile is held too, out of sight until that profile is on the board', async () => {
  const h = hook('s-other', path.join(home, '.claude-other'));
  await pause(300);
  assert.equal(h.answered, false, 'the panel keeps a request from another profile open instead of handing it back');
  let snap = await latest();
  assert.equal(pendingOf(snap, 's-other').length, 0, 'not shown while its profile is not on the board');

  const r = await api('/api/profile', { id: 'other' });
  assert.equal(r.ok, true, JSON.stringify(r));
  snap = await latest();
  assert.equal(snap.profile.dir, path.join(home, '.claude-other'));
  assert.equal(h.answered, false, 'switching profiles keeps it held');
  assert.equal(pendingOf(snap, 's-other').length, 1);
  assert.notEqual(pendingOf(snap, 's-other')[0].open, true);

  // Back and forth: still held, still answerable once its profile shows again.
  assert.equal((await api('/api/profile', { id: 'default' })).ok, true);
  await pause(200);
  assert.equal(h.answered, false);
  assert.equal(pendingOf(await latest(), 's-other').length, 0);
  assert.equal((await api('/api/profile', { id: 'other' })).ok, true);
  snap = await latest();
  const p = pendingOf(snap, 's-other')[0];
  assert.ok(p);
  await api('/api/decide', { id: p.id, behavior: 'allow' });
  assert.match(await h.promise, /"behavior":"allow"/);
});
