#!/usr/bin/env node
// "Before" numbers for the Node panel (server.mjs): hook-to-snapshot latency, snapshot latency, idle CPU and RSS.
// Starts its own panel on port 4797 over a made-up home (as test/fleet-machine.test.mjs's home() does) with its own
// prefs folder. Never touches the live panel on 4747 or its prefs. Writes results/before.json (numbers only).
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn, execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('../../', import.meta.url));
const OUT = fileURLToPath(new URL('./results/before.json', import.meta.url));
const PORT = 4797, BASE = `http://127.0.0.1:${PORT}`;
const RUNS = 50, IDLE_MS = 60_000, WATCH_LAPSE_MS = 95_000; // the panel stops its warm reads 90 s after a page last asked
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pct = (xs, p) => { const s = [...xs].sort((a, b) => a - b); return s[Math.min(s.length - 1, Math.ceil(p / 100 * s.length) - 1)]; };
const round = (n) => Math.round(n * 100) / 100;

// A fake HOME: one Claude profile, and a stand-in `claude` that lists one live session (the bench's own pid is its process).
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-bench-home-'));
const prefsDir = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-bench-prefs-'));
const SID = 'bbbbbbbb-0000-4000-8000-000000000001';
const profile = path.join(home, '.claude-work');
fs.mkdirSync(path.join(profile, 'fleet-state'), { recursive: true });
fs.mkdirSync(path.join(home, '.local', 'bin'), { recursive: true });
fs.writeFileSync(path.join(profile, 'settings.json'), '{}');
fs.writeFileSync(path.join(profile, '.claude.json'), JSON.stringify({ oauthAccount: { emailAddress: 'me@example.com', organizationName: 'Example' } }));
const row = { id: 'abcd1234', sessionId: SID, name: 'bench', kind: 'interactive', state: 'working', status: 'idle', pid: process.pid, cwd: path.join(home, 'repo'), startedAt: Date.now() - 60_000 };
fs.mkdirSync(row.cwd);
fs.writeFileSync(path.join(home, '.local', 'bin', 'claude'), `#!/bin/sh
case "$1" in
  agents) echo '${JSON.stringify([row])}' ;;
  auth) echo '{"loggedIn":true,"email":"me@example.com"}' ;;
  *) echo '[]' ;;
esac
`, { mode: 0o755 });
const hook = (e) => {
  const f = path.join(profile, 'fleet-state', `${SID}.json`);
  fs.writeFileSync(f + '.tmp', JSON.stringify({ e, at: Date.now() }));
  fs.renameSync(f + '.tmp', f);
};
hook('Stop');

const server = spawn(process.execPath, [path.join(ROOT, 'server.mjs')], {
  cwd: ROOT, stdio: 'ignore',
  env: { PATH: '/usr/bin:/bin:/opt/homebrew/bin', HOME: home, ACP_PORT: String(PORT), ACP_PREFS: path.join(prefsDir, 'prefs.json') } });
const done = (code) => { try { server.kill('SIGKILL'); } catch {} try { fs.rmSync(home, { recursive: true, force: true }); fs.rmSync(prefsDir, { recursive: true, force: true }); } catch {} process.exit(code); };
process.on('SIGINT', () => done(130));
process.on('uncaughtException', (e) => { console.error(e); done(1); });

// Pair as a browser does: the key from the 0600 file beside prefs.json becomes a cookie, then the fleet page carries the token.
let key = null;
for (let i = 0; i < 100 && !key; i++) {
  try { const k = fs.readFileSync(path.join(prefsDir, 'key'), 'utf8').trim(); if (/^[0-9a-f]{48}$/.test(k)) key = k; } catch {}
  if (!key) await sleep(100);
}
if (!key) throw new Error('the panel never wrote its key');
let paired;
for (let i = 0; i < 100 && !paired; i++) {
  try { const r = await fetch(`${BASE}/fleet?k=${key}`, { redirect: 'manual' }); paired = r.headers.get('set-cookie')?.split(';')[0]; } catch { await sleep(100); }
}
if (!paired) throw new Error('could not pair with the panel');
const html = await (await fetch(`${BASE}/fleet`, { headers: { cookie: paired } })).text();
const token = /const TOKEN = '([0-9a-f]+)'/.exec(html)?.[1];
if (!token) throw new Error('no token in the fleet page');
const snapshot = async () => {
  const r = await fetch(`${BASE}/api/fleet`, { method: 'POST', headers: { cookie: paired, origin: BASE, 'x-acp-token': token, 'content-type': 'application/json' }, body: '{}' });
  return r.json();
};
const find = (o) => { // the bench session's row, wherever the snapshot nests it
  if (Array.isArray(o)) { for (const v of o) { const f = find(v); if (f) return f; } return null; }
  if (o && typeof o === 'object') { if (o.sessionId === SID && 'state' in o) return o; for (const v of Object.values(o)) { const f = find(v); if (f) return f; } }
  return null;
};
const stateOf = async () => find(await snapshot())?.state;

// Wait until the panel has read the made-up home.
for (let i = 0; i < 200 && (await stateOf()) !== 'ready'; i++) await sleep(100);
if ((await stateOf()) !== 'ready') throw new Error('the panel never listed the bench session as ready');

// (b) snapshot latency
const snapMs = [];
for (let i = 0; i < RUNS; i++) { const t = performance.now(); await snapshot(); snapMs.push(performance.now() - t); }

// (a) hook to snapshot: write a hook event, poll every 5 ms until the row's state flips.
const flip = [];
let cur = 'ready';
for (let i = 0; i < RUNS; i++) {
  const want = cur === 'ready' ? 'working' : 'ready';
  const t = performance.now();
  hook(want === 'working' ? 'UserPromptSubmit' : 'Stop');
  while (performance.now() - t < 60_000 && (await stateOf()) !== want) await sleep(5);
  flip.push(performance.now() - t);
  cur = want;
  await sleep(200);
}

// (c) idle: no page asking, so the warm reads have lapsed; total CPU time the server uses over 60 s, and its RSS.
const ps = (f) => execFileSync('ps', ['-o', `${f}=`, '-p', String(server.pid)]).toString().trim();
const cpuMs = () => { const [m, s] = ps('time').split(':'); return (Number(m) * 60 + Number(s)) * 1000; };
await sleep(WATCH_LAPSE_MS);
const c0 = cpuMs(), rss = [];
for (let i = 0; i < IDLE_MS / 1000; i++) { rss.push(Number(ps('rss')) / 1024); await sleep(1000); }
const idleCpuMsPer60s = Math.round(cpuMs() - c0);

const out = {
  measuredAt: new Date().toISOString(), machine: 'Mac',
  node: {
    hookToSnapshotMs: { p50: round(pct(flip, 50)), p95: round(pct(flip, 95)), max: round(Math.max(...flip)) },
    snapshotMs: { p50: round(pct(snapMs, 50)), p95: round(pct(snapMs, 95)) },
    idleCpuMsPer60s, idleCpuPct: round(idleCpuMsPer60s / 600),
    rssMb: round(pct(rss, 50)),
  },
};
fs.mkdirSync(path.dirname(OUT), { recursive: true });
fs.writeFileSync(OUT, JSON.stringify(out, null, 2) + '\n');
console.log(JSON.stringify(out, null, 2));
done(0);
