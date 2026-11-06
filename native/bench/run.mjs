#!/usr/bin/env node
// The bench gate: starts `fleetd serve` on a temp socket over a 400-session fixture home and measures hook -> delta,
// transcript append -> delta, snapshot on connect, idle CPU/RSS over 60 s, and terminals (echo added over a direct pty,
// warm reopen, cold open). The measurement runs 3 times (a fresh
// server each); each budget in budgets.json is compared with the MEDIAN of the three p95s (Ruling R19). Exit 1 if any is
// over. Prints the table against results/before.json (Node) and writes results/latest.json.
// `--ratchet` then lowers each budget to max(median p95 x 1.25, floor), never above what it was (ratchet.mjs).
// The timing is done by `fleetd bench` (in-process, on the client side of the socket), not by Node polling.
// Run: cargo build --release -p fleetd && node native/bench/run.mjs [--ratchet] [--quick]
// `--quick` shortens the idle window (--idle-secs N, N < 60) and one round: a smoke run, never a gate, never ratcheted.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, execFileSync, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { buildHome, sid } from './fixture.mjs';
import { median, ratchet, round } from './ratchet.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FLEETD = process.env.FLEETD_BIN ?? path.join(HERE, '..', 'target', 'release', 'fleetd');
const BUDGETS = path.join(HERE, 'budgets.json');
const BEFORE = path.join(HERE, 'results', 'before.json');
const LATEST = path.join(HERE, 'results', 'latest.json');
const RATCHET = process.argv.includes('--ratchet');
const QUICK = process.argv.includes('--quick');
const usage = (m) => { console.error(`bench: ${m}`); process.exit(2); };
const ai = process.argv.indexOf('--idle-secs');
const IDLE_SECS = ai > 0 ? Number(process.argv[ai + 1]) : 60;
if (!Number.isFinite(IDLE_SECS) || IDLE_SECS <= 0) usage('--idle-secs needs a positive number');
if (IDLE_SECS < 60 && !QUICK) usage('--idle-secs under 60 is a smoke run: add --quick (it is never a gate or a ratchet)');
if (QUICK && RATCHET) usage('--quick cannot ratchet');
const ROUNDS = QUICK ? 1 : 3;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pct = (xs, p) => { const s = [...xs].sort((a, b) => a - b); return s[Math.min(s.length - 1, Math.ceil(p / 100 * s.length) - 1)]; };

if (!fs.existsSync(FLEETD)) usage(`${FLEETD} not found; run cargo build --release -p fleetd`);

const fx = buildHome(process.pid);
const sockDir = fs.mkdtempSync(path.join(os.tmpdir(), 'fb-'));
const sock = path.join(sockDir, 'f.sock');
const env = { PATH: `${path.join(fx.home, '.local', 'bin')}:/usr/bin:/bin`, HOME: fx.home, FLEETD_HOME: fx.home, FLEETD_SOCK: sock, FLEETD_MACHINE: 'bench', FLEETD_ROSTER: 'cli' };
let kids = [];
const killKids = () => { for (const k of kids) try { k.kill('SIGKILL'); } catch {} kids = []; };
const cleanup = () => { killKids(); try { fs.rmSync(fx.home, { recursive: true, force: true }); fs.rmSync(sockDir, { recursive: true, force: true }); } catch {} };
for (const [sig, code] of [['SIGINT', 130], ['SIGTERM', 143]]) process.on(sig, () => { cleanup(); process.exit(code); });
process.on('uncaughtException', (e) => { console.error(e); cleanup(); process.exit(1); });

/** One `fleetd bench <mode>` run: the sample list in ms. */
const benchJson = (mode, extra = []) => {
  const r = spawnSync(FLEETD, ['bench', mode, ...extra], { env, encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`fleetd bench ${mode} failed: ${r.stderr}`);
  return JSON.parse(r.stdout);
};
const bench = (mode, extra = []) => benchJson(mode, extra).ms;
const stats = (xs) => ({ p50: round(pct(xs, 50)), p95: round(pct(xs, 95)), max: round(Math.max(...xs)) });

/** One whole measurement over a fresh server. */
async function measure() {
  try { fs.rmSync(sock, { force: true }); fs.rmSync(sock + '.lock', { force: true }); } catch {}
  const t0 = performance.now();
  const server = spawn(FLEETD, ['serve'], { env, stdio: ['ignore', 'ignore', 'inherit'] });
  kids.push(server);
  for (let i = 0; i < 300 && !fs.existsSync(sock); i++) await sleep(50);
  if (!fs.existsSync(sock)) throw new Error('fleetd never bound its socket');
  const startupMs = round(performance.now() - t0);
  await sleep(3000); // the first reads and the first roster run settle
  const ps = (f) => execFileSync('ps', ['-o', `${f}=`, '-p', String(server.pid)]).toString().trim();
  const cpuMs = () => { const [m, s] = ps('time').split(':'); return (Number(m) * 60 + Number(s)) * 1000; };
  const rssMb = () => Number(ps('rss')) / 1024;

  const snap = bench('snapshot', ['--n', '50', '--gap-ms', '20']);
  const hook = bench('hook', ['--n', '200', '--gap-ms', '10', '--profile', fx.profile, '--sid', sid(0)]);
  const trans = bench('transcript', ['--n', '200', '--gap-ms', '10', '--sid', sid(0), '--transcript', fx.transcriptOf(0)]);
  const rssAfterLoadMb = round(rssMb());

  // Idle: one client connected (as the app is), nothing else; total CPU time the server uses over the window.
  const watcher = spawn(FLEETD, ['watch'], { env, stdio: ['ignore', 'ignore', 'ignore'] });
  kids.push(watcher);
  await sleep(3000);
  const c0 = cpuMs(), rss = [];
  for (let i = 0; i < IDLE_SECS; i++) { rss.push(rssMb()); await sleep(1000); }
  const idleCpuMsPer60s = Math.round((cpuMs() - c0) * 60 / IDLE_SECS);

  // Terminals (plan 2), after the idle window so the plan-1 numbers are as they were. Echo: the same keystroke through
  // fleetd and on a direct pty, in turn; the budget is the p95 of the paired differences (through_i - direct_i). Warm: reopen to the first repaint byte. Cold: open
  // a terminal that never ran to its first byte.
  const echo = benchJson('term-echo', ['--n', '300', '--gap-ms', '5']);
  const termWarm = bench('term-warm', ['--n', '50', '--gap-ms', '20']);
  const termCold = bench('term-cold', ['--n', '30', '--gap-ms', '50']);
  const termEcho = { p50: round(pct(echo.added, 50)), p95: round(pct(echo.added, 95)), throughP95: round(pct(echo.ms, 95)), directP95: round(pct(echo.direct, 95)) };
  killKids();
  await sleep(300);
  return { startupMs, hook: stats(hook), trans: stats(trans), snap: stats(snap), idleCpuMsPer60s, rssMb: round(pct(rss, 50)), rssAfterLoadMb, termEcho, termWarm: stats(termWarm), termCold: stats(termCold) };
}

const runs = [];
for (let i = 0; i < ROUNDS; i++) {
  runs.push(await measure());
  console.error(`round ${i + 1}/${ROUNDS}: hook p95 ${runs[i].hook.p95} transcript p95 ${runs[i].trans.p95} snapshot p95 ${runs[i].snap.p95} echo +${runs[i].termEcho.p95} warm ${runs[i].termWarm.p95} cold ${runs[i].termCold.p95} idle ${runs[i].idleCpuMsPer60s} ms rss ${runs[i].rssMb}`);
}
cleanup();

const med = (f) => round(median(runs.map(f)));
const fleetd = {
  rounds: ROUNDS, idleWindowSecs: IDLE_SECS, gate: !QUICK,
  startupMs: med((r) => r.startupMs),
  hookToDeltaMs: { p50: med((r) => r.hook.p50), p95: med((r) => r.hook.p95), max: med((r) => r.hook.max) },
  transcriptToDeltaMs: { p50: med((r) => r.trans.p50), p95: med((r) => r.trans.p95), max: med((r) => r.trans.max) },
  snapshotMs: { p50: med((r) => r.snap.p50), p95: med((r) => r.snap.p95), max: med((r) => r.snap.max) },
  idleCpuMsPer60s: med((r) => r.idleCpuMsPer60s), idleCpuPct: round(med((r) => r.idleCpuMsPer60s) / 600),
  rssMb: med((r) => r.rssMb), rssAfterLoadMb: med((r) => r.rssAfterLoadMb),
  termEchoAddedMs: { p50: med((r) => r.termEcho.p50), p95: med((r) => r.termEcho.p95), throughP95: med((r) => r.termEcho.throughP95), directP95: med((r) => r.termEcho.directP95) },
  termWarmReopenMs: { p50: med((r) => r.termWarm.p50), p95: med((r) => r.termWarm.p95), max: med((r) => r.termWarm.max) },
  termColdOpenMs: { p50: med((r) => r.termCold.p50), p95: med((r) => r.termCold.p95), max: med((r) => r.termCold.max) },
  perRound: runs.map((r) => ({ hookP95: r.hook.p95, transcriptP95: r.trans.p95, snapshotP95: r.snap.p95, termEchoAddedP95: r.termEcho.p95, termWarmP95: r.termWarm.p95, termColdP95: r.termCold.p95, idleCpuMsPer60s: r.idleCpuMsPer60s, rssMb: r.rssMb })),
};

// ---- compare: each budget against the median of the rounds' p95
const budgets = JSON.parse(fs.readFileSync(BUDGETS, 'utf8'));
const before = JSON.parse(fs.readFileSync(BEFORE, 'utf8')).node;
const one = (v) => ({ p50: v, p95: v });
const rows = [
  ['hook -> delta', 'hookToDeltaP95Ms', fleetd.hookToDeltaMs, before.hookToSnapshotMs, 'ms'],
  ['transcript -> delta', 'transcriptToDeltaP95Ms', fleetd.transcriptToDeltaMs, null, 'ms'],
  ['snapshot on connect', 'snapshotOnConnectMs', fleetd.snapshotMs, before.snapshotMs, 'ms'],
  ['terminal echo, paired added over a direct pty', 'termEchoPairedAddedP95Ms', fleetd.termEchoAddedMs, null, 'ms'],
  ['terminal warm reopen to first byte', 'termWarmReopenP95Ms', fleetd.termWarmReopenMs, null, 'ms'],
  ['terminal cold open to first byte', 'termColdOpenP95Ms', fleetd.termColdOpenMs, null, 'ms'],
  ['idle CPU per 60 s', 'idleCpuMsPer60s', one(fleetd.idleCpuMsPer60s), one(before.idleCpuMsPer60s), 'ms'],
  ['RSS', 'rssMb', one(fleetd.rssMb), one(before.rssMb), 'MB'],
];
const f = (n) => (n == null ? 'n/a' : String(n));
const lines = [`median of ${ROUNDS} round(s); idle window ${IDLE_SECS} s`, ['metric', 'Node p50', 'Node p95', 'fleetd p50', 'fleetd p95', 'budget', 'ok'].join(' | ')];
let failed = false;
for (const [label, key, now, was, unit] of rows) {
  const ok = now.p95 <= budgets[key].budget;
  if (!ok) failed = true;
  lines.push([`${label} (${unit})`, f(was?.p50), f(was?.p95), f(now.p50), f(now.p95), budgets[key].budget, ok ? 'yes' : 'NO'].join(' | '));
}
console.log(lines.join('\n'));
console.log(`startup to socket: ${fleetd.startupMs} ms; RSS after the load runs: ${fleetd.rssAfterLoadMb} MB`);
console.log(failed ? 'GATE: FAIL' : 'GATE: PASS');

fs.writeFileSync(LATEST, JSON.stringify({ measuredAt: new Date().toISOString(), machine: 'Mac', fleetd }, null, 2) + '\n');

if (RATCHET) {
  const medians = Object.fromEntries(rows.map(([, key, now]) => [key, now.p95]));
  const r = ratchet(budgets, medians);
  for (const k of r.skipped) console.log(`not ratcheted (over budget): ${k}`);
  fs.writeFileSync(BUDGETS, JSON.stringify(r.budgets, null, 2) + '\n');
  console.log('budgets ratcheted:', JSON.stringify(r.budgets));
}
process.exit(failed ? 1 : 0);
