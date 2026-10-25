// Restarting a background job on another model: one that works in its own worktree carries on as a copy there, with
// nothing removed. Run with `npm test`. Starts the server on a spare port under a throwaway home, with a stand-in
// `claude` whose roster the test controls.
import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const SERVER = fileURLToPath(new URL('../server.mjs', import.meta.url));
const PORT = 47500 + Math.floor(Math.random() * 100);
const BASE = `http://127.0.0.1:${PORT}`;
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-home-'));
const SID = '11111111-2222-4333-8444-555555555555';
const tree = path.join(home, 'repo', '.claude', 'worktrees', 'wt-job');
fs.mkdirSync(tree, { recursive: true });
fs.mkdirSync(path.join(home, '.claude', 'projects', 'p'), { recursive: true });
fs.writeFileSync(path.join(home, '.claude', 'settings.json'), '{}');
// Its conversation: a prompt, and a reply on haiku, both in its worktree.
fs.writeFileSync(path.join(home, '.claude', 'projects', 'p', `${SID}.jsonl`), [
  JSON.stringify({ type: 'user', cwd: tree, permissionMode: 'default', timestamp: '2026-10-22T10:00:00.000Z', message: { role: 'user', content: 'start' } }),
  JSON.stringify({ type: 'assistant', cwd: tree, timestamp: '2026-10-22T10:00:01.000Z', message: { role: 'assistant', model: 'claude-haiku-5-5', content: [{ type: 'text', text: 'done' }] } }),
].join('\n') + '\n');
// The stand-in: a roster in a file, `stop` ends the job there, every call is logged with its folder.
const state = path.join(home, 'roster.json'), calls = path.join(home, 'calls.log');
const job = { pid: 4242, id: 'ab12cd34', cwd: tree, kind: 'background', startedAt: 1, sessionId: SID, name: 'wt job', status: 'idle', state: 'done' };
fs.writeFileSync(state, JSON.stringify([job]));
const bin = path.join(home, 'bin');
fs.mkdirSync(bin);
fs.writeFileSync(path.join(bin, 'claude'), `#!/bin/sh
echo "$PWD :: $*" >> "${calls}"
case "$1" in
  agents) cat "${state}";;
  auth) echo "{}";;
  stop) python3 -c 'import json,sys; p=sys.argv[1]; r=json.load(open(p)); [x.update({"state":"stopped","status":None,"pid":None}) for x in r]; json.dump(r, open(p,"w"))' "${state}"; echo "stopped $2";;
  rm) echo "removed $2";;
  --bg) echo "backgrounded · c0ffee00 · wt job";;
  *) exit 1;;
esac
`, { mode: 0o755 });
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
const snapshot = async () => {
  const c = new AbortController();
  const r = await fetch(`${BASE}/events?t=${token}`, { headers: paired, signal: c.signal });
  const reader = r.body.getReader();
  let buf = '';
  for (;;) { const { value } = await reader.read(); buf += Buffer.from(value).toString(); const m = buf.match(/^data: (.*)$/m); if (m) { c.abort(); return JSON.parse(m[1]); } }
};
after(() => { server.kill(); fs.rmSync(home, { recursive: true, force: true }); });

test('a job in its own worktree restarts on another model as a copy there, with its record and worktree kept', async () => {
  // The roster is read every 3 s; wait for the job to be on the board.
  let s;
  for (let i = 0; i < 20 && !s; i++) { await new Promise((r) => setTimeout(r, 400)); s = (await snapshot()).sessions.find((x) => x.sessionId === SID); }
  assert.ok(s, 'the job shows up');
  assert.equal(s.model, 'claude-haiku-5-5');

  const r = await api('/api/reply', { sessionId: SID, text: 'carry on', model: 'sonnet', effort: 'low' });
  assert.equal(r.ok, true, JSON.stringify(r));
  assert.equal(r.copy, true, 'it says the job carries on as a copy');
  assert.equal(r.id, 'c0ffee00');
  assert.deepEqual(r.restartedOn, { model: 'sonnet', effort: 'low' });

  const lines = fs.readFileSync(calls, 'utf8').trim().split('\n');
  assert.ok(lines.some((l) => l.endsWith(':: stop ab12cd34')), 'it is stopped first: ' + lines.join(' | '));
  assert.ok(!lines.some((l) => / :: rm /.test(l)), 'its record is not removed, which would take its worktree: ' + lines.join(' | '));
  const bg = lines.find((l) => l.includes(':: --bg'));
  assert.ok(bg, 'it is resumed in the background: ' + lines.join(' | '));
  assert.equal(fs.realpathSync(bg.split(' :: ')[0]), fs.realpathSync(tree), 'from inside its worktree');
  assert.equal(bg.split(' :: ')[1], `--bg --resume ${SID} --name wt job --model sonnet --effort low --permission-mode default -- carry on`);

  // The old record stays on the board as ended, and says where the conversation went.
  const after = (await snapshot()).sessions.find((x) => x.sessionId === SID);
  assert.equal(after.continuedAs?.id, 'c0ffee00');
});
