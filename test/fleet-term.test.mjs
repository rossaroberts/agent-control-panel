// Fleet's live terminal (fleet/term.mjs): what it runs for a session here and on another machine, what it refuses, and a
// round trip through its pty with a program that echoes. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { PassThrough } from 'node:stream';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

// A home of its own whose `claude` is a stand-in: a shell that prints its window size, echoes one line, then prints the
// size again after the next. The module reads the home when it loads, so it is imported after.
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-term-'));
fs.mkdirSync(path.join(home, '.local', 'bin'), { recursive: true });
fs.writeFileSync(path.join(home, '.local', 'bin', 'claude'), '#!/bin/sh\nstty size; read l; echo "got:$l"; read l2; stty size\n', { mode: 0o755 });
process.env.HOME = home;
const { createTerms } = await import('../fleet/term.mjs');

const config = () => ({ self: 'Mac', machines: [{ name: 'server', ssh: 'my-server', user: 'agents' }, { name: 'plain', ssh: 'other', user: null }] });

test('here: claude attach, with the profile its job lives in', () => {
  const t = createTerms({ config });
  const def = t.command('Mac', 'default', 'abcd1234');
  assert.deepEqual(def.argv.slice(1), ['attach', 'abcd1234']);
  assert.equal(def.env.CLAUDE_CONFIG_DIR, undefined);
  assert.match(t.command('Mac', 'work', 'abcd1234').env.CLAUDE_CONFIG_DIR, /\/\.claude-work$/);
  assert.ok(Object.keys(def.env).every((k) => !k.startsWith('CLAUDE')), 'none of this session\'s own markers');
});

test('another machine: ssh -tt, as its user, one quoted command', () => {
  const t = createTerms({ config });
  const { argv } = t.command('server', 'rar', 'abcd1234');
  assert.deepEqual(argv.slice(0, 2), ['ssh', '-tt']);
  assert.equal(argv.at(-2), 'my-server');
  assert.equal(argv.at(-1), "sudo -n -u agents -H bash -lc 'export XDG_RUNTIME_DIR=/run/user/$(id -u) TERM=xterm-256color COLORTERM=truecolor COLORFGBG='\\''15;0'\\'' LANG=C.UTF-8 CLAUDE_CONFIG_DIR=$HOME/.claude-rar; exec claude attach abcd1234'");
  assert.match(t.command('plain', 'default', 'abcd1234').argv.at(-1), /^bash -lc '.*exec claude attach abcd1234'$/);
});

test('refuses what is not a background job on a known machine', () => {
  const t = createTerms({ config });
  for (const [m, p, id] of [['nowhere', 'rar', 'abcd1234'], ['server', 'rar;rm -rf', 'abcd1234'], ['server', 'rar', 'abcd123$'], ['Mac', 'default', 'ABCD1234'], ['Mac', '', 'abcd1234']]) {
    assert.equal(t.command(m, p, id), null, `${m} ${p} ${id}`);
  }
});

test('a round trip: keys in, the program\'s screen out, the size it is told', async () => {
  const t = createTerms({ config });
  const res = new PassThrough();
  let out = '', head = null;
  res.writeHead = (status, h) => { head = { status, h }; }; res.flushHeaders = () => {};
  res.on('data', (d) => { out += d; });
  const closed = new Promise((r) => res.on('end', r));
  t.open({ machine: 'Mac', profile: 'default', id: 'abcd1234', cols: 91, rows: 27 }, res);
  assert.equal(head.status, 200);
  const term = head.h['x-term-id'];
  const until = async (re) => { for (let i = 0; i < 100 && !re.test(out); i++) await new Promise((r) => setTimeout(r, 50)); return re.test(out); };
  assert.ok(await until(/27 91/), `it starts at the page's size: ${JSON.stringify(out)}`);
  assert.deepEqual(t.input({ term, data: 'hello\r' }), { ok: true });
  assert.ok(await until(/got:hello/), `keys reach it: ${JSON.stringify(out)}`);
  assert.deepEqual(t.resize({ term, cols: 120, rows: 40 }), { ok: true });
  await new Promise((r) => setTimeout(r, 200));
  t.input({ term, data: '\r' });
  assert.ok(await until(/40 120/), `a new size reaches it: ${JSON.stringify(out)}`);
  await closed;
  assert.deepEqual(t.input({ term, data: 'x' }), { ok: false, error: 'That terminal has closed.' });
});
