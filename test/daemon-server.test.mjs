// The server as launchd runs it: ACP_DAEMON=1 opens World at boot and waits for a taken port. Spawns real processes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path'; import net from 'node:net';
import { spawn } from 'node:child_process';

const SERVER = new URL('../server.mjs', import.meta.url).pathname;
const freePort = () => new Promise((r) => { const s = net.createServer().listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => r(port)); }); });
const until = async (check, ms = 8000) => { const end = Date.now() + ms; while (Date.now() < end) { if (await check()) return true; await new Promise((r) => setTimeout(r, 150)); } return false; };
const answers = (port) => fetch(`http://127.0.0.1:${port}/`).then((r) => r.ok, () => false);
// A home of its own: the real one's Claude Code profiles would be read (and titled, at a cost) by every run.
const HOME = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-home-'));
process.on('exit', () => { try { fs.rmSync(HOME, { recursive: true, force: true }); } catch {} });
function start(port, prefsDir, extraEnv = {}) {
  const child = spawn(process.execPath, [SERVER], { env: { ...process.env, HOME, ACP_PORT: String(port), ACP_PREFS: path.join(prefsDir, 'prefs.json'), ACP_DAEMON: '1', ...extraEnv }, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = ''; child.stdout.on('data', (d) => { out += d; }); child.stderr.on('data', (d) => { out += d; });
  // A child that already exited never emits 'exit' again, so stop resolves at once for it.
  const stop = () => new Promise((r) => { if (child.exitCode !== null || child.signalCode !== null) return r(); child.once('exit', r); child.kill(); });
  return { child, log: () => out, stop };
}

test('under ACP_DAEMON the server opens World at boot, with no button pressed', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const file = path.join(prefs, 'world', 'world.json');
  assert.ok(await until(() => fs.existsSync(file)), 'world.json was not written');
  assert.equal(JSON.parse(fs.readFileSync(file, 'utf8')).v, 2);
});

test('without ACP_DAEMON nothing is written until World is opened', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs, { ACP_DAEMON: '' }); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  await new Promise((r) => setTimeout(r, 500));
  assert.equal(fs.existsSync(path.join(prefs, 'world', 'world.json')), false);
});

test('a taken port makes the daemon wait, not exit; it answers once the port frees up', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const holder = net.createServer().listen(port, '127.0.0.1');
  await new Promise((r) => holder.once('listening', r));
  let held = true; // closed once, by the test or by t.after when an assertion fails first
  const free = () => (held ? (held = false, new Promise((r) => holder.close(r))) : Promise.resolve());
  t.after(free);
  const s = start(port, prefs, { ACP_RETRY_MS: '300' }); t.after(s.stop);
  assert.ok(await until(() => /port \d+ is taken/.test(s.log())), s.log());
  await new Promise((r) => setTimeout(r, 400));
  assert.equal(s.child.exitCode, null, 'it exited: ' + s.log());
  await free();
  assert.ok(await until(() => answers(port)), s.log());
  assert.ok(await until(() => fs.existsSync(path.join(prefs, 'world', 'world.json'))), 'World did not open after the port freed up: ' + s.log());
});

test('an argument runs the CLI instead of the server', async () => {
  const r = await new Promise((resolve) => { const c = spawn(process.execPath, [SERVER, 'nonsense'], { env: process.env }); let out = ''; c.stdout.on('data', (d) => { out += d; }); c.stderr.on('data', (d) => { out += d; }); c.on('exit', (code) => resolve({ code, out })); });
  assert.equal(r.code, 2);
  assert.match(r.out, /daemon install \[--program <path>\]\|remove\|status/);
});
