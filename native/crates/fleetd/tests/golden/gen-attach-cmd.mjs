// Regenerates attach_cmd.json from the oracle (fleet/term.mjs command(), local branch). Run: node tests/golden/gen-attach-cmd.mjs
// The home is a synthetic temp folder with stand-in claude/codex under ~/.local/bin; paths in the output use {HOME}.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const home = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'fleetd-golden-')));
fs.mkdirSync(path.join(home, '.local', 'bin'), { recursive: true });
for (const b of ['claude', 'codex']) fs.writeFileSync(path.join(home, '.local', 'bin', b), '#!/bin/sh\n', { mode: 0o755 });
fs.mkdirSync(path.join(home, 'work', 'x'), { recursive: true });
process.env.HOME = home;
const { createTerms } = await import(path.join(here, '../../../../../fleet/term.mjs'));
const terms = createTerms({ config: () => ({ self: 'Mac', machines: [] }) });

const U = '0123abcd-4567-89ab-cdef-0123456789ab';
const sub = (s) => s.replaceAll('{HOME}', home);
const back = (s) => s.replaceAll(home, '{HOME}');
const cases = [
  { name: 'claude default', target: { Claude: { profile: 'default', job: 'abcd1234' } }, args: ['default', 'abcd1234', null, null] },
  { name: 'claude work', target: { Claude: { profile: 'work', job: '0a1b2c3d' } }, args: ['work', '0a1b2c3d', null, null] },
  { name: 'codex default', target: { Codex: { profile: 'codex', session_id: U, cwd: '{HOME}/work' } }, codex: { sessionId: U, cwd: '{HOME}/work' }, args: ['codex', '', null] },
  { name: 'codex other profile', target: { Codex: { profile: 'codex2', session_id: U, cwd: '~/work/x' } }, codex: { sessionId: U, cwd: '~/work/x' }, args: ['codex2', '', null] },
  { name: 'codex home cwd', target: { Codex: { profile: 'codex', session_id: U, cwd: '~' } }, codex: { sessionId: U, cwd: '~' }, args: ['codex', '', null] },
  { name: 'trust tilde', target: { Trust: { profile: 'default', dir: '~' } }, args: ['default', '', '~', null] },
  { name: 'trust under home, dotdot', target: { Trust: { profile: 'work', dir: '~/work/../work//x/' } }, args: ['work', '', '~/work/../work//x/', null] },
  { name: 'trust absolute', target: { Trust: { profile: 'default', dir: '{HOME}/work' } }, args: ['default', '', '{HOME}/work', null] },
];
const out = cases.map((c) => {
  const [profile, job, dir] = c.args;
  const cmd = terms.command('Mac', profile, job, dir == null ? null : sub(dir), null, c.codex ? { sessionId: c.codex.sessionId, cwd: sub(c.codex.cwd) } : null);
  if (!cmd) throw new Error(`oracle refused ${c.name}`);
  const keys = ['TERM', 'COLORTERM', 'COLORFGBG', 'CLAUDE_CONFIG_DIR', 'CODEX_HOME'];
  const env = Object.fromEntries(keys.filter((k) => cmd.env[k] !== undefined).map((k) => [k, back(cmd.env[k])]));
  return { name: c.name, target: c.target, argv: cmd.argv.map(back), env, cwd: back(cmd.cwd) };
});
fs.writeFileSync(path.join(here, 'attach_cmd.json'), JSON.stringify(out, null, 2) + '\n');
fs.rmSync(home, { recursive: true });
