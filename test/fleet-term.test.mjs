// Fleet's live terminal (fleet/term.mjs) and the WebSocket a page reaches them over (fleet/ws.mjs): what it runs for a
// session here and on another machine, what it refuses, and round trips through its pty with a program that echoes,
// directly and over a real WebSocket. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';

// A home of its own whose `claude` is a stand-in: a shell that prints its window size, echoes one line, then prints the
// size again after the next. Job deadbeef is one that ignores being hung up on (it writes its pid and waits); job feedf00d
// reads keys raw, as Claude Code does (a line typed in the terminal's own line mode is cut at 1 KB), counts 70,001 of them
// and prints 70,000 back. `codex` waits for input. The module reads the home when it loads, so it is imported after.
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-term-'));
fs.mkdirSync(path.join(home, '.local', 'bin'), { recursive: true });
fs.writeFileSync(path.join(home, '.local', 'bin', 'claude'), `#!/bin/sh
if [ "$2" = deadbeef ]; then trap '' HUP; echo $$ > "$HOME/stubborn.pid"; echo ready; while :; do sleep 1; done; fi
if [ "$2" = feedf00d ]; then stty raw -echo; echo raw; n=$(head -c 70001 | wc -c | tr -d ' '); echo "n=$n"; head -c 70000 /dev/zero | tr '\\0' y; echo; exit; fi
stty size; read l; echo "got:$l"; read l2; stty size
`, { mode: 0o755 });
fs.writeFileSync(path.join(home, '.local', 'bin', 'codex'), '#!/bin/sh\necho codex-up; read l\n', { mode: 0o755 });
fs.mkdirSync(path.join(home, 'work'));
process.env.HOME = home;
const { createTerms } = await import('../fleet/term.mjs');
const { accept } = await import('../fleet/ws.mjs');

const config = () => ({ self: 'Mac', machines: [{ name: 'server', ssh: 'my-server', user: 'agents' }, { name: 'plain', ssh: 'other', user: null }] });
const until = async (fn, ms = 5000) => { for (const end = Date.now() + ms; Date.now() < end; await new Promise((r) => setTimeout(r, 25))) if (fn()) return true; return fn(); };

test('here: claude attach, with the profile its job lives in', () => {
  const t = createTerms({ config });
  const def = t.command('Mac', 'default', 'abcd1234');
  assert.deepEqual(def.argv.slice(1), ['attach', 'abcd1234']);
  assert.equal(def.env.CLAUDE_CONFIG_DIR, undefined);
  assert.match(t.command('Mac', 'work', 'abcd1234').env.CLAUDE_CONFIG_DIR, /\/\.claude-work$/);
  assert.ok(Object.keys(def.env).every((k) => !k.startsWith('CLAUDE')), 'none of this session\'s own markers');
});

test('another machine: ssh -tt with no escape character, as its user, one quoted command', () => {
  const t = createTerms({ config });
  const { argv } = t.command('server', 'rar', 'abcd1234');
  assert.deepEqual(argv.slice(0, 4), ['ssh', '-tt', '-e', 'none']);
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

test('a round trip: keys in, the program\'s screen out, the size it is told, its end', async () => {
  const t = createTerms({ config });
  let out = '', over = false;
  const term = t.start({ machine: 'Mac', profile: 'default', id: 'abcd1234', cols: 91, rows: 27 }, (d) => { out += d; }, () => { over = true; });
  assert.equal(term.ok, true);
  assert.ok(await until(() => /27 91/.test(out)), `it starts at the page's size: ${JSON.stringify(out)}`);
  term.write('hello\r');
  assert.ok(await until(() => /got:hello/.test(out)), `keys reach it: ${JSON.stringify(out)}`);
  term.size(120, 40);
  await new Promise((r) => setTimeout(r, 200));
  term.write('\r');
  assert.ok(await until(() => /40 120/.test(out)), `a new size reaches it: ${JSON.stringify(out)}`);
  assert.ok(await until(() => over), 'its end is told once it has exited');
  assert.equal(t.open, 0);
});

test('a program that ignores the hang-up is killed a moment later, nothing left behind', async () => {
  const t = createTerms({ config });
  let out = '', over = false;
  const term = t.start({ machine: 'Mac', profile: 'default', id: 'deadbeef' }, (d) => { out += d; }, () => { over = true; });
  assert.ok(await until(() => /ready/.test(out)), `it started: ${JSON.stringify(out)}`);
  const pid = Number(fs.readFileSync(path.join(home, 'stubborn.pid'), 'utf8'));
  term.stop();
  assert.ok(await until(() => over, 6000), 'it ended');
  assert.throws(() => process.kill(pid, 0), /ESRCH/, 'and its program is gone');
});

test('a Codex thread opens in one terminal at a time, and not while it runs elsewhere', async () => {
  const busy = new Set(['22222222-2222-4222-8222-222222222222']);
  const t = createTerms({ config, codexBusy: (m, id) => (busy.has(id) ? 'It is open in Codex.' : null) });
  const codex = { sessionId: '11111111-1111-4111-8111-111111111111', cwd: '~/work' };
  let out = '';
  const first = t.start({ machine: 'Mac', profile: 'codex', codex }, (d) => { out += d; }, () => {});
  assert.equal(first.ok, true);
  assert.ok(await until(() => /codex-up/.test(out)));
  assert.deepEqual(t.start({ machine: 'Mac', profile: 'codex', codex }, () => {}, () => {}), { ok: false, error: 'It is open in another Fleet terminal already.' });
  assert.deepEqual(t.start({ machine: 'Mac', profile: 'codex', codex: { ...codex, sessionId: [...busy][0] } }, () => {}, () => {}), { ok: false, error: 'It is open in Codex.' });
  first.stop();
  assert.ok(await until(() => t.open === 0));
  const again = t.start({ machine: 'Mac', profile: 'codex', codex }, () => {}, () => {});
  assert.equal(again.ok, true, 'once it has closed, it opens again');
  again.stop();
  await until(() => t.open === 0);
});

test('over a WebSocket: open by number, type, resize, its end; a 70 KB paste each way; closing the socket closes its terminals', async () => {
  const t = createTerms({ config });
  const server = http.createServer();
  server.on('upgrade', (req, socket, head) => { const ws = accept(req, socket, head); if (ws) t.serve(ws); });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  try {
    const ws = new WebSocket(`ws://127.0.0.1:${server.address().port}/`);
    ws.binaryType = 'arraybuffer';
    let out = '';
    const ends = [];
    ws.onmessage = (e) => {
      if (typeof e.data === 'string') { ends.push(JSON.parse(e.data)); return; }
      assert.ok([6, 7, 8].includes(new DataView(e.data).getUint32(0)), 'its output carries its number');
      out += Buffer.from(e.data, 4).toString();
    };
    await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
    ws.send(JSON.stringify({ op: 'open', ref: 7, machine: 'Mac', profile: 'default', id: 'abcd1234', cols: 91, rows: 27 }));
    assert.ok(await until(() => /27 91/.test(out)), `it starts at the page's size: ${JSON.stringify(out)}`);
    ws.send(JSON.stringify({ op: 'in', ref: 7, data: 'hello\r' }));
    assert.ok(await until(() => /got:hello/.test(out)), `keys reach it: ${JSON.stringify(out)}`);
    ws.send(JSON.stringify({ op: 'size', ref: 7, cols: 120, rows: 40 }));
    await new Promise((r) => setTimeout(r, 200));
    ws.send(JSON.stringify({ op: 'in', ref: 7, data: '\r' }));
    assert.ok(await until(() => /40 120/.test(out)), 'a new size reaches it');
    assert.ok(await until(() => ends.some((m) => m.op === 'end' && m.ref === 7)), `its end is told: ${JSON.stringify(ends)}`);
    // A paste larger than a short frame goes whole, and a screenful as large comes back whole.
    let big = '';
    const before = out.length;
    ws.send(JSON.stringify({ op: 'open', ref: 6, machine: 'Mac', profile: 'default', id: 'feedf00d' }));
    assert.ok(await until(() => /raw/.test(out.slice(before))), 'the raw reader is up');
    ws.send(JSON.stringify({ op: 'in', ref: 6, data: `${'x'.repeat(70_000)}\r` }));
    assert.ok(await until(() => { big = out.slice(before); return /n=70001/.test(big) && /y{70000}/.test(big); }, 15_000), `70,001 keys in, 70,000 back: ${big.length} bytes`);
    // Another one, left open: the socket closing takes it down.
    ws.send(JSON.stringify({ op: 'open', ref: 8, machine: 'Mac', profile: 'default', id: 'abcd1234' }));
    ws.send(JSON.stringify({ op: 'open', ref: 9, machine: 'nowhere', profile: 'default', id: 'abcd1234' }));
    assert.ok(await until(() => ends.some((m) => m.ref === 9 && m.error)), 'one it refuses says why');
    assert.ok(await until(() => t.open === 1));
    ws.close();
    assert.ok(await until(() => t.open === 0, 6000), 'closing the socket closed its terminal');
  } finally { server.close(); }
});

test('a page that goes without a goodbye (its connection just ends) has its terminals closed too', async () => {
  const t = createTerms({ config });
  const server = http.createServer();
  server.on('upgrade', (req, socket, head) => { const ws = accept(req, socket, head); if (ws) t.serve(ws); });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  try {
    // A bare socket speaking just enough WebSocket to open one terminal, then ending its side without a close frame.
    const net = await import('node:net');
    const c = net.connect(server.address().port, '127.0.0.1');
    await new Promise((r) => c.once('connect', r));
    c.write('GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n');
    await new Promise((r) => c.once('data', r));
    const body = Buffer.from(JSON.stringify({ op: 'open', ref: 1, machine: 'Mac', profile: 'default', id: 'deadbeef' }));
    const mask = Buffer.from([1, 2, 3, 4]), masked = Buffer.from(body.map((b, i) => b ^ mask[i & 3]));
    c.write(Buffer.concat([Buffer.from([0x81, 0x80 | 126, body.length >> 8, body.length & 255]), mask, masked]));
    assert.ok(await until(() => t.open === 1), 'its terminal opened');
    c.end();
    assert.ok(await until(() => t.open === 0, 6000), 'and closed when the connection ended');
    c.destroy();
  } finally { server.close(); }
});
