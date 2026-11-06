// Writes golden/decide.json: Node's `sessionState` over the full product grid of roster rows, transcript tails, hook
// events and quiet times. The Rust `decide()` is checked against it (native/crates/fleet-model/tests/decide.rs).
// Inputs are stored in the Rust shapes (RosterRow, TailFacts, HookEvent) and mapped to Node's camelCase here.
// Everything is fixed (no Date.now()), so the file is deterministic and committed. Run: node native/parity/gen-decide-golden.mjs
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { sessionState } from '../../fleet/machine.mjs';

const WRITTEN_AT = 1_793_932_000_000; // a fixed epoch-ms; the transcript's last write and last entry
const PID = 4242;

const states = [null, 'working', 'running', 'blocked', 'idle', 'stopped', 'failed', 'done'];
const statuses = [null, 'busy', 'idle', 'waiting'];
// Node has no pid liveness; `pid_alive` only matters to Rust.
const pids = [{ pid: null, pid_alive: false }, { pid: PID, pid_alive: true }, { pid: PID, pid_alive: false }];
const actives = [false, true];
const waitingFors = [null, 'permission'];

// One event per thing a hook can say (and one that says nothing), so every hookState answer is crossed with the roster.
const hooks = [
  { e: 'SessionStart' }, // ready
  { e: 'SessionStart', r: 'compact' }, // says nothing
  { e: 'PreToolUse', t: 'Bash' }, // working
  { e: 'Notification', n: 'permission_prompt' }, // needs-you
  { e: 'PostToolUseFailure', t: 'Bash', i: true }, // ready (Esc)
  { e: 'StopFailure', r: 'rate_limit' }, // capped
  { e: 'SessionEnd' }, // closed
];
const hookOffsets = [-20_000, -10_000, 0]; // hook `at` relative to writtenAt
const quiets = [1_000, 5_000, 90_000]; // now - max(hook at, writtenAt)
const limits = [null, WRITTEN_AT - 60_000]; // limitHit absent, or present and recent

/** The tail + hook + now combinations: no hook (quiet still varied), or each hook at each offset and quiet. */
function scenarios() {
  const out = [];
  for (const limit_hit_at of limits) {
    const t = { last_at: WRITTEN_AT, written_at: WRITTEN_AT, limit_hit_at, turn_over: null };
    for (const q of quiets) out.push({ t, h: null, now: WRITTEN_AT + q });
    for (const base of hooks) {
      for (const off of hookOffsets) {
        const h = { e: base.e, t: base.t ?? null, n: base.n ?? null, r: base.r ?? null, i: base.i ?? null, at: WRITTEN_AT + off };
        for (const q of quiets) out.push({ t, h, now: Math.max(h.at, WRITTEN_AT) + q });
      }
    }
  }
  return out;
}

const toNodeRow = (r) => ({ pid: r.pid ?? undefined, state: r.state ?? undefined, status: r.status ?? undefined,
  waitingFor: r.waiting_for ?? undefined, active: r.active });
const toNodeTail = (t) => ({ lastAt: t.last_at ?? undefined, writtenAt: t.written_at ?? undefined,
  limitHit: t.limit_hit_at == null ? undefined : { at: t.limit_hit_at } });
const toNodeHook = (h) => (h == null ? null : { e: h.e, t: h.t ?? undefined, n: h.n ?? undefined, r: h.r ?? undefined,
  i: h.i ?? undefined, at: h.at });

// Factored, losslessly: the full product is ~50k cases, ~22 MB as one object per case. Rows and scenarios are listed once;
// `node[ri]` is a string with one character per scenario: Node's state as a letter, upper case when live, lower when not.
const CODES = { closed: 'c', 'needs-you': 'n', working: 'w', ready: 'r', capped: 'p' };
const rows = [];
for (const state of states) for (const status of statuses) for (const p of pids) for (const active of actives) {
  for (const waiting_for of waitingFors) rows.push({ pid: p.pid, pid_alive: p.pid_alive, state, status, waiting_for, active });
}
const sc = scenarios();
const node = rows.map((r) => sc.map(({ t, h, now }) => {
  const n = sessionState(toNodeRow(r), toNodeTail(t), toNodeHook(h), now);
  const c = CODES[n.state];
  if (!c || typeof n.live !== 'boolean') throw new Error(`no code for ${JSON.stringify(n)}`);
  return n.live ? c.toUpperCase() : c;
}).join(''));

const out = path.join(path.dirname(fileURLToPath(import.meta.url)), 'golden', 'decide.json');
fs.mkdirSync(path.dirname(out), { recursive: true });
const lines = (xs) => xs.map((x) => `    ${JSON.stringify(x)}`).join(',\n');
fs.writeFileSync(out, `{
  "codes": ${JSON.stringify(Object.fromEntries(Object.entries(CODES).map(([k, v]) => [v, k])))},
  "rows": [
${lines(rows)}
  ],
  "scenarios": [
${lines(sc)}
  ],
  "node": [
${lines(node)}
  ]
}
`);
console.log(`${rows.length} rows x ${sc.length} scenarios = ${rows.length * sc.length} cases -> ${path.relative(process.cwd(), out)}`);
