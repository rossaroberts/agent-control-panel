// Writes golden/panelmeta.json: Node's naming rules (`nameOf`, `groupsOf`, `showable` of fleet/fleet.mjs) over the synthetic panel
// folder native/crates/fleet-read/tests/fixtures/panel/. The Rust `PanelMeta` is checked against it
// (native/crates/fleet-read/tests/panelmeta.rs). Everything is fixed, so the file is deterministic and committed.
// Run: node native/parity/gen-panelmeta-golden.mjs
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { nameOf, groupsOf, showable } from '../../fleet/fleet.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DIR = path.join(HERE, '..', 'crates', 'fleet-read', 'tests', 'fixtures', 'panel');
const kept = (f) => { try { const v = JSON.parse(fs.readFileSync(path.join(DIR, f), 'utf8')); return v && typeof v === 'object' && !Array.isArray(v) ? v : {}; } catch { return {}; } };
const titles = kept('fleet-titles.json'), labels = kept('fleet-labels.json'), archived = kept('fleet-archived.json');
// `config()` of fleet.mjs, for the epics the groups are measured against.
const epics = (kept('fleet.json').epics ?? []).filter((e) => /^[\w .-]{1,40}$/.test(e?.name ?? '')).slice(0, 12);

const M = 'fixture-mac';
const key = (n) => `${M}:sess-${String(n).padStart(4, '0')}-fixture`;
const names = [undefined, null, '', '  ', 'Claude own name', '  padded name  ', 'chai-expense-tracker-54', 'tracker-5g', 'a-b-0f', 'x'];

/** One case per (session number, engine, name). The sessions 1-9 have kept files; 20 has none. */
const naming = [];
for (const n of [1, 2, 3, 4, 5, 6, 7, 20]) for (const engine of ['claude', 'codex']) for (const name of names) {
  const x = { name, sessionId: `sess-${n}` };
  const r = nameOf(labels[key(n)], titles[key(n)], engine, x);
  naming.push({ key: key(n), engine, name: name ?? null, title: r.title, epic: r.epic });
}

/** Each archived key, live or not, started before or after it was hidden, needing you or not. */
const hid = archived[key(10)];
const hiding = [];
for (const n of [10, 11, 12, 13, 20]) for (const live of [false, true]) for (const startedAt of [null, hid - 1, hid + 1]) for (const state of ['working', 'needs-you']) {
  const x = { live, startedAt: startedAt ?? undefined, state };
  hiding.push({ key: key(n), live, startedAt, needsYou: state === 'needs-you', archived: !showable(x, archived[key(n)]) });
}

const out = { naming, hiding, groups: groupsOf(labels, epics) };
fs.mkdirSync(path.join(HERE, 'golden'), { recursive: true });
fs.writeFileSync(path.join(HERE, 'golden', 'panelmeta.json'), JSON.stringify(out, null, 1) + '\n');
console.log(`panelmeta.json: ${naming.length} naming, ${hiding.length} hiding, groups ${JSON.stringify(out.groups)}`);
