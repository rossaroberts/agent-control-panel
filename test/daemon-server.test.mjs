// The server as launchd runs it: ACP_DAEMON=1 opens World at boot and waits for a taken port. Spawns real processes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path'; import net from 'node:net';
import { spawn } from 'node:child_process';
import http from 'node:http';

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

test('a public host from fleet.json is accepted as Host and https Origin, nothing else is', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  // fetch will not let a script set Host, so these go over a raw http request.
  const raw = (method, p, headers, body) => new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, method, path: p, headers }, (res) => { let b = ''; res.on('data', (d) => { b += d; }); res.on('end', () => resolve({ status: res.statusCode, body: b, headers: res.headers })); });
    req.on('error', reject); req.end(body);
  });
  const get = (host) => raw('GET', '/', { host });
  const page = await get('mac.example-tailnet.ts.net');
  assert.equal(page.status, 200, 'the pair page renders on the public host');
  assert.match(page.body, /not paired/);
  assert.equal((await get('evil.example.com')).status, 421);
  const post = (origin) => raw('POST', '/api/fleet', { host: 'mac.example-tailnet.ts.net', origin, 'content-type': 'application/json' }, '{}');
  assert.equal((await post('http://mac.example-tailnet.ts.net')).status, 403, 'http origin on the public host is refused');
  const r = await post('https://mac.example-tailnet.ts.net');
  assert.equal(r.status, 403); assert.deepEqual(JSON.parse(r.body), { error: 'forbidden' });
});

test('with no public host configured, a tailnet name is still a wrong host', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const r = await new Promise((resolve, reject) => { const q = http.request({ host: '127.0.0.1', port, path: '/', headers: { host: 'mac.example-tailnet.ts.net' } }, (res) => { res.resume(); resolve(res.statusCode); }); q.on('error', reject); q.end(); });
  assert.equal(r, 421);
});

test('ACP_PUBLIC_HOST overrides fleet.json, and an invalid value is ignored', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'a.example-tailnet.ts.net' }));
  const s = start(port, prefs, { ACP_PUBLIC_HOST: 'b.example-tailnet.ts.net' }); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const status = (host) => new Promise((resolve, reject) => { const q = http.request({ host: '127.0.0.1', port, path: '/', headers: { host } }, (res) => { res.resume(); resolve(res.statusCode); }); q.on('error', reject); q.end(); });
  assert.equal(await status('b.example-tailnet.ts.net'), 200);
  assert.equal(await status('a.example-tailnet.ts.net'), 421);
});

test('pairing over https sets a Secure cookie; loopback over http does not', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const key = fs.readFileSync(path.join(prefs, 'key'), 'utf8').trim();
  const pair = (headers) => new Promise((resolve, reject) => { const q = http.request({ host: '127.0.0.1', port, path: `/?k=${key}`, headers }, (res) => { res.resume(); resolve(res); }); q.on('error', reject); q.end(); });
  const r = await pair({ host: 'mac.example-tailnet.ts.net', 'x-forwarded-proto': 'https' });
  assert.equal(r.statusCode, 303);
  assert.match(r.headers['set-cookie'][0], /; Secure/);
  const r2 = await pair({});
  assert.equal(r2.statusCode, 303);
  assert.doesNotMatch(r2.headers['set-cookie'][0], /; Secure/, 'loopback over http keeps a non-Secure cookie');
});

test('the manifest, service worker, offline page and icons are served without pairing; nothing else is under /icons', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const get = (p) => fetch(`http://127.0.0.1:${port}${p}`);
  const m = await get('/manifest.webmanifest');
  assert.equal(m.status, 200);
  assert.match(m.headers.get('content-type'), /^application\/manifest\+json/);
  const manifest = await m.json();
  assert.equal(manifest.display, 'standalone');
  const w = await get('/sw.js');
  assert.equal(w.status, 200);
  assert.match(w.headers.get('content-type'), /^text\/javascript/);
  assert.equal(w.headers.get('cache-control'), 'no-cache');
  assert.equal(w.headers.get('service-worker-allowed'), '/');
  assert.equal((await get('/offline.html')).status, 200);
  for (const icon of manifest.icons) {
    const r = await get(icon.src);
    assert.equal(r.status, 200, icon.src);
    assert.equal(r.headers.get('content-type'), 'image/png');
    assert.deepEqual([...new Uint8Array(await r.arrayBuffer()).slice(1, 4)], [0x50, 0x4e, 0x47], 'a PNG');
  }
  assert.equal((await get('/icons/apple-touch-icon.png')).status, 200);
  assert.equal((await get('/icons/nope.png')).status, 404);
  assert.equal((await get('/icons/..%2Fserver.mjs')).status, 404);
});

test('the Fleet page is installable: manifest, touch icon and viewport-fit in its head', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const key = fs.readFileSync(path.join(prefs, 'key'), 'utf8').trim();
  const html = await fetch(`http://127.0.0.1:${port}/`, { headers: { cookie: `acp_k=${key}` } }).then((r) => r.text());
  const head = html.slice(0, html.indexOf('</head>'));
  assert.match(head, /rel="manifest" href="\/manifest\.webmanifest"/);
  assert.match(head, /rel="apple-touch-icon" href="\/icons\/apple-touch-icon\.png"/);
  assert.match(head, /viewport-fit=cover/);
  assert.match(html, /serviceWorker\.register\('\/sw\.js'\)/);
});

test('/api/pair-url hands the pairing address to a paired page only', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const key = fs.readFileSync(path.join(prefs, 'key'), 'utf8').trim();
  const base = `http://127.0.0.1:${port}`;
  const post = (headers) => fetch(`${base}/api/pair-url`, { method: 'POST', headers: { origin: base, 'content-type': 'application/json', ...headers }, body: '{}' });
  assert.equal((await post({})).status, 403, 'no cookie, no token');
  const html = await fetch(`${base}/`, { headers: { cookie: `acp_k=${key}` } }).then((r) => r.text());
  const token = /const TOKEN = '([0-9a-f]+)'/.exec(html)?.[1];
  assert.ok(token, 'the page carries its token');
  const r = await post({ cookie: `acp_k=${key}`, 'x-acp-token': token });
  assert.equal(r.status, 200);
  const j = await r.json();
  assert.equal(j.url, `https://mac.example-tailnet.ts.net/?k=${key}`);
  assert.equal(j.public, 'mac.example-tailnet.ts.net');
  assert.equal((await fetch(`${base}/vendor/qrcode.js`)).status, 200);
});
