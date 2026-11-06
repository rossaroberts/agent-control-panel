// Writes golden/codex.json: Node's readRollout over each fixture in fixtures/codex/ (whole, with its log held open and not,
// and after each of 3 appended chunks cut at seeded-random bytes, so mid-line), recentRollouts + yours over the fixture tree,
// the 200 cap, and codexAccount. The Rust codex module is checked against it (native/crates/fleet-read/tests/codex.rs).
// Deterministic (seeded cuts, fixed mtimes, a fixed clock); the output is committed. Run: node native/parity/gen-codex-golden.mjs
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { codexAccount, readRollout, recentRollouts, yours } from '../../fleet/machine.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.join(HERE, 'fixtures', 'codex');
const NOW = Date.UTC(2026, 10, 6, 12, 0, 0); // the clock for every read
Date.now = () => NOW;
const MIN = 60_000, DAY = 86_400_000;

// Minutes before NOW each fixture was last written. Distinct, so the newest-first order has no ties.
const AGE = {
  working: 1, exec: 1.5, odd: 2, subagent: 2.5, review: 3.5, 'no-meta': 3000, answered: 4, 'hidden-done': 5, nameless: 7, big: 8,
  aborted: 12, finished: 30, user: 0, 'user-thread': 20 * 60, asking: 120, break: 500, 'asking-stale': 4 * 24 * 60, old: 9 * 24 * 60,
};
const ageOf = (rel) => {
  const key = Object.keys(AGE).filter((k) => rel.endsWith(`-${k}.jsonl`)).sort((a, b) => b.length - a.length)[0];
  if (!key) throw new Error(`no age for ${rel}`);
  return AGE[key] * MIN;
};

function rng(seed) { // mulberry32
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
/** Two cut points, so three chunks; a cut just before a newline moves past it (the Rust reader holds a line back until
 *  its newline, where Node parses a whole line without one). */
function cutsOf(buf, rand) {
  const pick = () => { let c = 1 + Math.floor(rand() * (buf.length - 2)); if (buf[c] === 0x0a) c += 1; return c; };
  let a = pick(), b = pick();
  while (b === a) b = pick();
  return [Math.min(a, b), Math.max(a, b)];
}
const setTime = (f, msAgo) => { const t = (NOW - msAgo) / 1000; fs.utimesSync(f, t, t); };

/** What the Rust Session carries of a Node row. */
function norm(r, m) {
  const s = r.session;
  return {
    id: s.id, live: s.live, state: s.state, said: s.said ?? null, doing: s.doing ?? null, model: s.model || null, effort: s.effort ?? null,
    lastAt: s.lastAt ?? null, contextTokens: s.context?.tokens ?? null, lastAsk: s.lastAsk ?? null, firstAsk: s.firstAsk ?? null,
    startedAt: s.startedAt ?? null, cwd: s.cwd ?? null, name: s.name || null, writtenAt: m,
  };
}
const walk = (dir) => fs.readdirSync(dir, { recursive: true }).filter((f) => f.endsWith('.jsonl')).sort();

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-codex-golden-'));
const rand = rng(20261106);
const fixtures = [];

// The whole tree, as a Codex home, for recentRollouts.
const home = path.join(tmp, 'home');
fs.cpSync(path.join(FIXTURES, 'sessions'), path.join(home, 'sessions'), { recursive: true });
const rels = walk(path.join(home, 'sessions'));
for (const rel of rels) setTime(path.join(home, 'sessions', rel), ageOf(rel));
const relOf = (r) => path.relative(path.join(home, 'sessions'), r.f);
const recent = {
  sevenDays: recentRollouts(home, 7 * DAY).filter((r) => yours(r.f)).map(relOf),
  oneDay: recentRollouts(home, DAY).filter((r) => yours(r.f)).map(relOf),
};

// Each rollout read whole (log held open or not), and grown by chunks in a copy.
for (const rel of rels) {
  const buf = fs.readFileSync(path.join(FIXTURES, 'sessions', rel));
  const file = path.join(tmp, 'grow.jsonl');
  const mtimeAgo = ageOf(rel);
  const m = NOW - mtimeAgo;
  const whole = path.join(home, 'sessions', rel);
  const read = (f, open) => norm(readRollout(f, fs.statSync(f).mtimeMs, open), fs.statSync(f).mtimeMs);
  const entry = { rel, size: buf.length, mtimeMs: m, held: read(whole, new Set([whole])), notHeld: read(whole, new Set()), chunks: [] };
  const cuts = cutsOf(buf, rand);
  const bounds = [0, ...cuts, buf.length];
  fs.writeFileSync(file, '');
  for (let i = 0; i < 3; i++) {
    fs.appendFileSync(file, buf.subarray(bounds[i], bounds[i + 1]));
    setTime(file, mtimeAgo);
    entry.chunks.push(read(file, new Set([file])));
  }
  entry.cuts = cuts;
  fixtures.push(entry);
}

// More than 200 rollouts in a day: newest 200, by mtime.
const capHome = path.join(tmp, 'cap');
const capDir = path.join(capHome, 'sessions', '2026', '11', '06');
fs.mkdirSync(capDir, { recursive: true });
for (let i = 0; i < 205; i++) {
  const f = path.join(capDir, `rollout-cap-${String(i).padStart(3, '0')}.jsonl`);
  fs.writeFileSync(f, JSON.stringify({ type: 'session_meta', payload: { id: `cap-${i}` } }) + '\n');
  setTime(f, (i + 1) * MIN);
}
const cap = recentRollouts(capHome, 7 * DAY).map((r) => path.basename(r.f));

const account = {
  fixture: codexAccount(FIXTURES).email,
};

fs.rmSync(tmp, { recursive: true, force: true });
const g = path.join(HERE, 'golden');
fs.mkdirSync(g, { recursive: true });
fs.writeFileSync(path.join(g, 'codex.json'), JSON.stringify({ nowMs: NOW, ages: AGE, fixtures, recent, cap, account }, null, 1) + '\n');
console.log(`${fixtures.length} rollouts, ${recent.sevenDays.length} in seven days, cap ${cap.length} -> ${path.relative(process.cwd(), g)}`);
