#!/usr/bin/env node
// The bench gate: starts `fleetd serve` on a temp socket over a 400-session fixture home and measures hook -> delta,
// transcript append -> delta, snapshot on connect, and idle CPU/RSS over 60 s. Compares each p95 with budgets.json (exit 1
// if any is over), prints the table against results/before.json (Node), writes results/latest.json.
// `--ratchet` then lowers each budget to max(p95 x 1.25, floor), never above what it was, and rewrites budgets.json.
// The timing is done by `fleetd bench` (in-process, on the client side of the socket), not by Node polling.
// Run: cargo build --release -p fleetd && node native/bench/run.mjs [--ratchet] [--idle-secs N]
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, execFileSync, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { buildHome, sid } from './fixture.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FLEETD = process.env.FLEETD_BIN ?? path.join(HERE, '..', 'target', 'release', 'fleetd');
const BUDGETS = path.join(HERE, 'budgets.json');
const BEFORE = path.join(HERE, 'results', 'before.json');
const LATEST = path.join(HERE, 'results', 'latest.json');
const RATCHET = process.argv.includes('--ratchet');
const argAfter = (k) => { const i = process.argv.indexOf(k); return i > 0 ? Number(process.argv[i + 1]) : null; };
const IDLE_SECS = argAfter('--idle-secs') ?? 60;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pct = (xs, p) => { const s = [...xs].sort((a, b) => a - b); return s[Math.min(s.length - 1, Math.ceil(p / 100 * s.length) - 1)]; };
const round = (n) => Math.round(n * 100) / 100;

if (!fs.existsSync(FLEETD)) { console.error(`bench: ${FLEETD} not found; run cargo build --release -p fleetd`); process.exit(2); }

const fx = buildHome(process.pid);
const sockDir = fs.mkdtempSync(path.join(os.tmpdir(), 'fb-'));
const sock = path.join(sockDir, 'f.sock');
const env = { PATH: `${path.join(fx.home, '.local', 'bin')}:/usr/bin:/bin`, HOME: fx.home, FLEETD_HOME: fx.home, FLEETD_SOCK: sock, FLEETD_MACHINE: 'bench', FLEETD_ROSTER: 'cli' };
const kids = [];
const cleanup = () => { for (const k of kids) try { k.kill('SIGKILL'); } catch {} try { fs.rmSync(fx.home, { recursive: true, force: true }); fs.rmSync(sockDir, { recursive: true, force: true }); } catch {} };
process.on('SIGINT', () => { cleanup(); process.exit(130); });
process.on('uncaughtException', (e) => { console.error(e); cleanup(); process.exit(1); });

const t0 = performance.now();
const server = spawn(FLEETD, ['serve'], { env, stdio: ['ignore', 'ignore', 'inherit'] });
kids.push(server);
for (let i = 0; i < 300 && !fs.existsSync(sock); i++) await sleep(50);
if (!fs.existsSync(sock)) { console.error('bench: fleetd never bound its socket'); cleanup(); process.exit(1); }
const startupMs = round(performance.now() - t0);

/** One `fleetd bench <mode>` run: the sample list in ms. */
const bench = (mode, extra = []) => {
  const r = spawnSync(FLEETD, ['bench', mode, ...extra], { env, encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`fleetd bench ${mode} failed: ${r.stderr}`);
  return JSON.parse(r.stdout).ms;
};
const stats = (xs) => ({ p50: round(pct(xs, 50)), p95: round(pct(xs, 95)), max: round(Math.max(...xs)) });

await sleep(3000); // the first reads and the first roster run settle
const ps = (f) => execFileSync('ps', ['-o', `${f}=`, '-p', String(server.pid)]).toString().trim();
const cpuMs = () => { const [m, s] = ps('time').split(':'); return (Number(m) * 60 + Number(s)) * 1000; };
const rssMb = () => Number(ps('rss')) / 1024;

// The three load runs, once; with --ratchet three times, so the budget is set from the worst p95 of three, not one lucky run.
const REPS = RATCHET ? 3 : 1;
const snaps = [], hooks = [], transes = [];
for (let r = 0; r < REPS; r++) {
  snaps.push(bench('snapshot', ['--n', '50', '--gap-ms', '20']));
  hooks.push(bench('hook', ['--n', '200', '--gap-ms', '10', '--profile', fx.profile, '--sid', sid(0)]));
  transes.push(bench('transcript', ['--n', '200', '--gap-ms', '10', '--sid', sid(0), '--transcript', fx.transcriptOf(0)]));
}
const snap = snaps.flat(), hook = hooks.flat(), trans = transes.flat();
const worstP95 = (runs) => round(Math.max(...runs.map((xs) => pct(xs, 95))));
const rssAfterLoad = round(rssMb());

// Idle: one client connected (as the app is), nothing else; total CPU time the server uses over the window.
const watcher = spawn(FLEETD, ['watch'], { env, stdio: ['ignore', 'ignore', 'ignore'] });
kids.push(watcher);
await sleep(3000);
const c0 = cpuMs(), rss = [];
for (let i = 0; i < IDLE_SECS; i++) { rss.push(rssMb()); await sleep(1000); }
const idleCpuMsPer60s = Math.round((cpuMs() - c0) * 60 / IDLE_SECS);

const fleetd = {
  startupMs,
  hookToDeltaMs: stats(hook),
  transcriptToDeltaMs: stats(trans),
  snapshotMs: stats(snap),
  idleCpuMsPer60s, idleCpuPct: round(idleCpuMsPer60s / 600),
  rssMb: round(pct(rss, 50)), rssAfterLoadMb: rssAfterLoad,
};
const worst = { hookToDeltaP95Ms: worstP95(hooks), transcriptToDeltaP95Ms: worstP95(transes), snapshotOnConnectMs: worstP95(snaps) };
cleanup();

// ---- compare
const budgets = JSON.parse(fs.readFileSync(BUDGETS, 'utf8'));
const before = JSON.parse(fs.readFileSync(BEFORE, 'utf8')).node;
const rows = [
  ['hook -> delta', 'hookToDeltaP95Ms', fleetd.hookToDeltaMs, before.hookToSnapshotMs, 'ms'],
  ['transcript -> delta', 'transcriptToDeltaP95Ms', fleetd.transcriptToDeltaMs, null, 'ms'],
  ['snapshot on connect', 'snapshotOnConnectMs', fleetd.snapshotMs, before.snapshotMs, 'ms'],
  ['idle CPU per 60 s', 'idleCpuMsPer60s', { p50: idleCpuMsPer60s, p95: idleCpuMsPer60s }, { p50: before.idleCpuMsPer60s, p95: before.idleCpuMsPer60s }, 'ms'],
  ['RSS', 'rssMb', { p50: fleetd.rssMb, p95: fleetd.rssMb }, { p50: before.rssMb, p95: before.rssMb }, 'MB'],
];
const f = (n) => (n == null ? 'n/a' : String(n));
const lines = [['metric', 'Node p50', 'Node p95', 'fleetd p50', 'fleetd p95', 'budget', 'ok'].join(' | ')];
let failed = false;
for (const [label, key, now, was, unit] of rows) {
  const ok = now.p95 <= budgets[key].budget;
  if (!ok) failed = true;
  lines.push([`${label} (${unit})`, f(was?.p50), f(was?.p95), f(now.p50), f(now.p95), budgets[key].budget, ok ? 'yes' : 'NO'].join(' | '));
}
console.log(lines.join('\n'));
console.log(`startup to socket: ${startupMs} ms; RSS after the load runs: ${rssAfterLoad} MB`);

const out = { measuredAt: new Date().toISOString(), machine: 'Mac', fleetd };
fs.writeFileSync(LATEST, JSON.stringify(out, null, 2) + '\n');

if (RATCHET) {
  // A budget that is over is not moved (it is a failure to fix, not a number to adopt); the others only go down.
  for (const [, key, now] of rows) {
    const b = budgets[key];
    if (now.p95 > b.budget) { console.log(`not ratcheted (over budget): ${key}`); continue; }
    b.budget = round(Math.min(b.budget, Math.max((worst[key] ?? now.p95) * 1.25, b.floor)));
  }
  fs.writeFileSync(BUDGETS, JSON.stringify(budgets, null, 2) + '\n');
  console.log('budgets ratcheted:', JSON.stringify(budgets));
}
process.exit(failed ? 1 : 0);
