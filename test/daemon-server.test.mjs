// The server as launchd runs it: ACP_DAEMON=1 opens World at boot and waits for a taken port. Spawns real processes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path'; import net from 'node:net';
import { spawn } from 'node:child_process';
import http from 'node:http';
import crypto from 'node:crypto';

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

test('ACP_PUBLIC_HOST overrides fleet.json', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'a.example-tailnet.ts.net' }));
  const s = start(port, prefs, { ACP_PUBLIC_HOST: 'b.example-tailnet.ts.net' }); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const status = (host) => new Promise((resolve, reject) => { const q = http.request({ host: '127.0.0.1', port, path: '/', headers: { host } }, (res) => { res.resume(); resolve(res.statusCode); }); q.on('error', reject); q.end(); });
  assert.equal(await status('b.example-tailnet.ts.net'), 200);
  assert.equal(await status('a.example-tailnet.ts.net'), 421);
});

test('an invalid ACP_PUBLIC_HOST is ignored: the allow-list does not widen', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs, { ACP_PUBLIC_HOST: 'bad host!' }); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const status = (host) => new Promise((resolve, reject) => { const q = http.request({ host: '127.0.0.1', port, path: '/', headers: { host } }, (res) => { res.resume(); resolve(res.statusCode); }); q.on('error', reject); q.end(); });
  assert.equal(await status('bad host!'), 421);
  assert.equal(await status('bad'), 421);
  assert.equal(await status('mac.example-tailnet.ts.net'), 421);
  assert.equal(await status(`127.0.0.1:${port}`), 200);
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

// A paired page, over the loopback: its cookie and the token this start's page carries.
async function paired(port, prefs) {
  const key = fs.readFileSync(path.join(prefs, 'key'), 'utf8').trim();
  const base = `http://127.0.0.1:${port}`;
  const html = await fetch(`${base}/`, { headers: { cookie: `acp_k=${key}` } }).then((r) => r.text());
  const token = /const TOKEN = '([0-9a-f]+)'/.exec(html)?.[1];
  assert.ok(token, 'the page carries its token');
  return { key, base, token, mint: () => fetch(`${base}/api/pair-url`, { method: 'POST', headers: { origin: base, cookie: `acp_k=${key}`, 'x-acp-token': token, 'content-type': 'application/json' }, body: '{}' }) };
}

test('/api/pair-url hands a one-time code to a paired page, never the key, and only to a paired page', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const bare = (headers) => fetch(`${p.base}/api/pair-url`, { method: 'POST', headers: { 'content-type': 'application/json', ...headers }, body: '{}' });
  assert.equal((await bare({ origin: p.base })).status, 403, 'no cookie, no token');
  assert.equal((await bare({ origin: p.base, cookie: `acp_k=${p.key}` })).status, 403, 'no token');
  assert.equal((await bare({ cookie: `acp_k=${p.key}`, 'x-acp-token': p.token })).status, 403, 'no origin');
  assert.equal((await bare({ origin: 'http://mac.example-tailnet.ts.net', cookie: `acp_k=${p.key}`, 'x-acp-token': p.token })).status, 403, 'wrong origin');
  const r = await p.mint();
  assert.equal(r.status, 200);
  const raw = await r.text();
  assert.ok(!raw.includes(p.key), 'the key is not in the answer');
  const j = JSON.parse(raw);
  assert.match(j.url, /^https:\/\/mac\.example-tailnet\.ts\.net\/\?p=[0-9a-f]{48}$/);
  assert.equal(j.public, 'mac.example-tailnet.ts.net');
  assert.equal(j.expiresInMs, 120000);
  assert.equal((await fetch(`${p.base}/vendor/qrcode.js`)).status, 200);
});

test('without a public host the code address is the loopback', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const j = await (await (await paired(port, prefs)).mint()).json();
  assert.match(j.url, new RegExp(`^http://127\\.0\\.0\\.1:${port}/\\?p=[0-9a-f]{48}$`));
  assert.equal(j.public, null);
});

test('a pairing code is spent by its first use: the cookie once, a 403 after', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const code = new URL((await (await p.mint()).json()).url).searchParams.get('p');
  const use = (c) => fetch(`${p.base}/?p=${c}`, { redirect: 'manual' });
  const first = await use(code);
  assert.equal(first.status, 303);
  assert.match(first.headers.get('set-cookie'), new RegExp(`^acp_k=${p.key}; Path=/; HttpOnly; SameSite=Strict`));
  const second = await use(code);
  assert.equal(second.status, 403);
  assert.equal(await second.text(), 'That pairing code has expired: press Pair a phone again.');
  assert.equal(second.headers.get('set-cookie'), null);
  assert.equal((await use('f'.repeat(48))).status, 403, 'a code that was never minted');
  assert.equal((await use('')).status, 403);
});

test('an expired pairing code is refused', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs, { ACP_PAIR_CODE_MS: '250' }); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const code = new URL((await (await p.mint()).json()).url).searchParams.get('p');
  await new Promise((r) => setTimeout(r, 450));
  const r = await fetch(`${p.base}/?p=${code}`, { redirect: 'manual' });
  assert.equal(r.status, 403);
  assert.match(await r.text(), /expired/);
});

test('a code used on the public host gets a Secure cookie even without X-Forwarded-Proto', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const code = new URL((await (await p.mint()).json()).url).searchParams.get('p');
  const r = await new Promise((resolve, reject) => { const q = http.request({ host: '127.0.0.1', port, path: `/?p=${code}`, headers: { host: 'mac.example-tailnet.ts.net' } }, (res) => { res.resume(); resolve(res); }); q.on('error', reject); q.end(); });
  assert.equal(r.statusCode, 303);
  assert.match(r.headers['set-cookie'][0], /; Secure/);
});

test('the terminal socket upgrades on the public host with an https Origin, and not with an http one', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const upgrade = (origin) => new Promise((resolve) => {
    const q = http.request({ host: '127.0.0.1', port, path: `/api/term/ws?t=${p.token}`, headers: {
      host: 'mac.example-tailnet.ts.net', origin, cookie: `acp_k=${p.key}`, connection: 'Upgrade', upgrade: 'websocket',
      'sec-websocket-version': '13', 'sec-websocket-key': 'dGhlIHNhbXBsZSBub25jZQ==' } });
    q.on('upgrade', (res, socket) => { socket.destroy(); resolve(res.statusCode); });
    q.on('response', (res) => { res.resume(); resolve(res.statusCode); });
    q.on('error', () => resolve('error'));
    q.end();
  });
  assert.equal(await upgrade('https://mac.example-tailnet.ts.net'), 101);
  assert.equal(await upgrade('http://mac.example-tailnet.ts.net'), 403);
});

test('/sw.js carries a cache name made from the offline page, with no placeholder left', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const body = await fetch(`http://127.0.0.1:${port}/sw.js`).then((r) => r.text());
  assert.match(body, /'fleet-offline-[0-9a-f]{12}'/);
  assert.doesNotMatch(body, /__OFFLINE_HASH__/);
});

test('/api/push/*: the VAPID key, subscribe and unsubscribe, for a paired page only; a bad subscription is refused; push.js is served', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const call = (route, body, headers) => fetch(`${p.base}${route}`, { method: 'POST', headers: { 'content-type': 'application/json', origin: p.base, cookie: `acp_k=${p.key}`, 'x-acp-token': p.token, ...headers }, body: JSON.stringify(body ?? {}) });
  for (const route of ['/api/push/key', '/api/push/subscribe', '/api/push/unsubscribe']) {
    assert.equal((await call(route, {}, { 'x-acp-token': 'nope' })).status, 403, `${route} wants the page's token`);
    assert.equal((await call(route, {}, { cookie: '' })).status, 403, `${route} wants the cookie`);
  }
  const key = await (await call('/api/push/key')).json();
  assert.equal(key.ok, true);
  assert.equal(Buffer.from(key.publicKey, 'base64url').length, 65);
  assert.equal((await (await call('/api/push/key')).json()).publicKey, key.publicKey, 'the same key every time');
  assert.equal(fs.statSync(path.join(prefs, 'push-vapid.json')).mode & 0o777, 0o600);

  const ecdh = crypto.createECDH('prime256v1'); ecdh.generateKeys();
  const subscription = { endpoint: 'https://fcm.googleapis.com/fcm/send/abc', keys: { p256dh: ecdh.getPublicKey().toString('base64url'), auth: crypto.randomBytes(16).toString('base64url') } };
  const bad = await call('/api/push/subscribe', { subscription: { ...subscription, endpoint: 'http://127.0.0.1:9/x' } });
  assert.equal(bad.status, 422);
  assert.equal((await call('/api/push/subscribe', {})).status, 422);
  assert.equal((await call('/api/push/subscribe', { subscription })).status, 200);
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(prefs, 'push.json'), 'utf8')).map((x) => x.endpoint), [subscription.endpoint]);
  assert.equal(fs.statSync(path.join(prefs, 'push.json')).mode & 0o777, 0o600);
  assert.deepEqual(await (await call('/api/push/unsubscribe', { endpoint: subscription.endpoint })).json(), { ok: true, removed: true });
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(prefs, 'push.json'), 'utf8')), []);

  const js = await fetch(`${p.base}/push.js`, { headers: { cookie: `acp_k=${p.key}` } });
  assert.equal(js.status, 200);
  assert.match(js.headers.get('content-type'), /^text\/javascript/);
});

// ---------------------------------------------------------------- held permission requests on the Fleet page

// A home with one Claude profile whose `claude` (a stub, first on PATH and in ~/.local/bin where Fleet looks) lists one
// background job that waits at a permission prompt: Fleet shows it as a row of this machine, the board as a session.
function homeWithJob(sessionId) {
  const h = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-home-job-'));
  process.on('exit', () => { try { fs.rmSync(h, { recursive: true, force: true }); } catch {} });
  fs.mkdirSync(path.join(h, '.claude'), { recursive: true });
  fs.writeFileSync(path.join(h, '.claude', 'settings.json'), '{}');
  fs.mkdirSync(path.join(h, '.local', 'bin'), { recursive: true });
  const roster = JSON.stringify([{ id: 'a1b2c3d4', sessionId, name: 'fixture', kind: 'background', pid: 4242, state: 'blocked', status: 'waiting',
    waitingFor: 'permission prompt', cwd: h, startedAt: Date.now() - 60_000 }]);
  fs.writeFileSync(path.join(h, '.local', 'bin', 'claude'), `#!/bin/sh\ncase "$1" in\n  agents) cat <<'EOF_ROSTER'\n${roster}\nEOF_ROSTER\n  ;;\n  auth) echo '{"loggedIn":true,"email":"me@example.com"}' ;;\n  *) exit 1 ;;\nesac\n`, { mode: 0o755 });
  return { home: h, env: { HOME: h, PATH: `${path.join(h, '.local', 'bin')}:${process.env.PATH}` } };
}
// A Fleet page's socket as a browser holds it: paired, with the page's token. `frames` are the texts the panel sends it.
function fleetPage(port, p) {
  return new Promise((resolve, reject) => {
    const frames = []; let buf = Buffer.alloc(0);
    const q = http.request({ host: '127.0.0.1', port, path: `/api/term/ws?t=${p.token}`, headers: { origin: p.base, cookie: `acp_k=${p.key}`, connection: 'Upgrade',
      upgrade: 'websocket', 'sec-websocket-version': '13', 'sec-websocket-key': 'dGhlIHNhbXBsZSBub25jZQ==' } });
    q.on('upgrade', (res, socket, head) => {
      const feed = (d) => {
        buf = Buffer.concat([buf, d]);
        while (buf.length >= 2) { // the panel's frames are not masked
          let len = buf[1] & 0x7f, at = 2;
          if (len === 126) { if (buf.length < 4) return; len = buf.readUInt16BE(2); at = 4; }
          if (buf.length < at + len) return;
          if ((buf[0] & 0x0f) === 1) frames.push(buf.subarray(at, at + len).toString());
          buf = buf.subarray(at + len);
        }
      };
      if (head.length) feed(head);
      socket.on('data', feed); socket.on('error', () => {});
      resolve({ frames, close: () => socket.destroy() });
    });
    q.on('response', (res) => { res.resume(); reject(new Error(`the socket was refused: ${res.statusCode}`)); });
    q.on('error', reject);
    q.end();
  });
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// What Claude Code posts when a tool wants a yes. No Origin: a hook is not a browser. `settled` is the hook's answer, once it comes.
function holdRequest(port, over = {}) {
  const body = { hook_event_name: 'PermissionRequest', session_id: 's1-fixture', tool_name: 'Bash', tool_input: { command: 'ls -la' }, cwd: '/tmp',
    permission_mode: 'default', permission_suggestions: [{ type: 'addRules', rules: [{ toolName: 'Bash', ruleContent: 'ls:*' }], behavior: 'allow', destination: 'session' }], ...over };
  const ctl = new AbortController();
  const settled = fetch(`http://127.0.0.1:${port}/hook/permission`, { method: 'POST', body: JSON.stringify(body), signal: ctl.signal }).then(async (r) => ({ status: r.status, text: await r.text() }));
  settled.catch(() => {});
  return { settled, abort: () => ctl.abort(), answeredWithin: async (ms) => (await Promise.race([settled, sleep(ms).then(() => null)])) };
}
const apiCall = (p, route, body) => fetch(`${p.base}${route}`, { method: 'POST', headers: { 'content-type': 'application/json', origin: p.base, cookie: `acp_k=${p.key}`, 'x-acp-token': p.token }, body: JSON.stringify(body ?? {}) });
/** The row Fleet lists for a session on this machine. */
async function fleetRow(p, sessionId) {
  const snap = await (await apiCall(p, '/api/fleet')).json();
  const m = snap.machines.find((x) => x.local);
  return { snap, machine: m, row: (m?.accounts ?? []).flatMap((a) => a.sessions ?? []).find((x) => x.sessionId === sessionId) };
}

test('a Fleet page alone is enough to hold a permission request: no board page, and it is held, not answered empty', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-')), job = homeWithJob('s1-fixture');
  const s = start(port, prefs, job.env); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  // Nobody watching: the hook is answered at once with nothing, and the terminal decides.
  const idle = holdRequest(port);
  assert.equal((await idle.answeredWithin(2000))?.text, '', 'with no page it is answered empty');
  const page = await fleetPage(port, p); t.after(page.close);
  const hook = holdRequest(port);
  assert.equal(await hook.answeredWithin(400), null, 'with a Fleet page it is held');
  const { row } = await fleetRow(p, 's1-fixture');
  assert.ok(row?.pending?.id, 'the row carries it');
  const r = await apiCall(p, '/api/decide', { id: row.pending.id, behavior: 'allow' });
  assert.equal(r.status, 200, await r.clone().text());
  const answer = await hook.answeredWithin(2000);
  assert.deepEqual(JSON.parse(answer.text), { hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: { behavior: 'allow' } } });
});

test('a local Fleet row carries its held request, without the plumbing of the held one', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-')), job = homeWithJob('s1-fixture');
  const s = start(port, prefs, job.env); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const before = await fleetRow(p, 's1-fixture');
  assert.equal(before.row?.sessionId, 's1-fixture', 'the fixture session is listed: ' + s.log());
  assert.equal(before.row.pending, null, 'nothing is held yet');
  const page = await fleetPage(port, p); t.after(page.close);
  const hook = holdRequest(port, { transcript_path: path.join(job.home, '.claude', 'projects', '-x', 's1-fixture.jsonl') });
  assert.equal(await hook.answeredWithin(300), null);
  const { row } = await fleetRow(p, 's1-fixture');
  assert.equal(row.pending.toolName, 'Bash');
  assert.deepEqual(row.pending.toolInput, { command: 'ls -la' });
  assert.equal(row.pending.open, false);
  assert.equal(row.pending.permissionMode, 'default');
  assert.equal(row.pending.suggestions.length, 1);
  assert.ok(row.pending.createdAt > 0 && row.pending.deadline > row.pending.createdAt);
  for (const k of ['res', 'timer', 'profileDir']) assert.ok(!(k in row.pending), `${k} stays on the server`);
  assert.ok(!JSON.stringify(row).includes(job.home + '/.claude/projects'), 'no transcript path leaves the server');
});

test('the Fleet page hears {"op":"fleet"} once when a request is held and once when it settles', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-')), job = homeWithJob('s1-fixture');
  const s = start(port, prefs, job.env); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const page = await fleetPage(port, p); t.after(page.close);
  await fleetRow(p, 's1-fixture'); // Fleet's first read tells the page too (a session it had not listed): let that land first
  await sleep(400);
  const fleetPushes = () => page.frames.filter((f) => f === '{"op":"fleet"}').length;
  const base = fleetPushes();
  const hook = holdRequest(port);
  assert.equal(await hook.answeredWithin(300), null);
  assert.equal(fleetPushes() - base, 1, 'one push when it is held');
  const { row } = await fleetRow(p, 's1-fixture');
  await sleep(200);
  const base2 = fleetPushes();
  assert.equal((await apiCall(p, '/api/decide', { id: row.pending.id, behavior: 'deny', message: 'no' })).status, 200);
  await hook.settled;
  await sleep(200);
  assert.equal(fleetPushes() - base2, 1, 'one push when it settles');
});

test('the last board page going does not hand a request back while a Fleet page is still there', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-')), job = homeWithJob('s1-fixture');
  const s = start(port, prefs, { ...job.env, ACP_RECONNECT_MS: '300' }); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const page = await fleetPage(port, p); t.after(page.close);
  const sse = new AbortController();
  const res = await fetch(`${p.base}/events?t=${p.token}`, { headers: { cookie: `acp_k=${p.key}` }, signal: sse.signal });
  assert.equal(res.status, 200);
  const hook = holdRequest(port);
  assert.equal(await hook.answeredWithin(300), null);
  sse.abort();
  await sleep(1000); // well past the 300 ms a board page is given to come back
  assert.equal(await hook.answeredWithin(10), null, 'still held: someone is looking on Fleet');
});

test('/api/fleet/decide answers a request whichever local profile it belongs to; the board refuses that one, and a second answer finds it gone', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-')), job = homeWithJob('s1-fixture');
  const s = start(port, prefs, job.env); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const page = await fleetPage(port, p); t.after(page.close);
  // Its session runs under another profile than the one on the board: the transcript says which folder it is under.
  const other = path.join(job.home, '.claude-other');
  const hook = holdRequest(port, { transcript_path: path.join(other, 'projects', '-x', 's1-fixture.jsonl') });
  assert.equal(await hook.answeredWithin(300), null);
  const { row } = await fleetRow(p, 's1-fixture');
  assert.ok(row.pending?.id, 'Fleet\'s row has it whichever profile it is under');
  const board = await apiCall(p, '/api/decide', { id: row.pending.id, behavior: 'allow' });
  assert.equal(board.status, 409);
  assert.match((await board.json()).error, /belongs to a profile that is not on the board/);
  assert.equal(await hook.answeredWithin(100), null, 'the board\'s refusal answered nothing');
  const first = await apiCall(p, '/api/fleet/decide', { id: row.pending.id, behavior: 'allow', always: true });
  assert.deepEqual(await first.json(), { ok: true });
  const answer = JSON.parse((await hook.answeredWithin(2000)).text).hookSpecificOutput.decision;
  assert.equal(answer.behavior, 'allow');
  assert.equal(answer.updatedPermissions?.[0]?.rules?.[0]?.ruleContent, 'ls:*', '"always" adopts the request\'s own suggestions');
  // Two pages pressing Allow: the second finds it gone, and nothing is sent twice.
  const second = await apiCall(p, '/api/fleet/decide', { id: row.pending.id, behavior: 'allow' });
  const gone = await second.json();
  assert.equal(gone.ok, false);
  assert.match(gone.error, /gone/);
});

test('/api/fleet/decide keeps the board\'s answers: deny with a reason, a question\'s answers, and refuses what decide() refuses', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-')), job = homeWithJob('s1-fixture');
  const s = start(port, prefs, job.env); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const p = await paired(port, prefs);
  const page = await fleetPage(port, p); t.after(page.close);
  const decide = async (route, body) => (await apiCall(p, route, body)).json();
  const idOf = async () => (await fleetRow(p, 's1-fixture')).row.pending.id;
  // A bare allow of a question is not an answer: only `answer` is, and it must answer every question it asked.
  const asked = holdRequest(port, { tool_name: 'AskUserQuestion', tool_input: { questions: [{ question: 'Which?', options: [{ label: 'A' }, { label: 'B' }], multiSelect: true }] } });
  assert.equal(await asked.answeredWithin(300), null);
  const qid = await idOf();
  assert.match((await decide('/api/fleet/decide', { id: qid, behavior: 'answer', answers: { 'Not asked': 'A' } })).error, /Answer each question/);
  assert.deepEqual(await decide('/api/fleet/decide', { id: qid, behavior: 'answer', answers: { 'Which?': 'A, B' } }), { ok: true });
  const got = JSON.parse((await asked.answeredWithin(2000)).text).hookSpecificOutput.decision;
  assert.deepEqual(got.updatedInput.answers, { 'Which?': 'A, B' });
  const denied = holdRequest(port);
  assert.equal(await denied.answeredWithin(300), null);
  assert.deepEqual(await decide('/api/fleet/decide', { id: await idOf(), behavior: 'deny', message: 'not that one' }), { ok: true });
  assert.deepEqual(JSON.parse((await denied.answeredWithin(2000)).text).hookSpecificOutput.decision, { behavior: 'deny', message: 'not that one' });
  // Not a request at all, and an open one (read from a transcript) that is not open any more: the Board's own words.
  assert.match((await decide('/api/fleet/decide', { id: 'nope', behavior: 'allow' })).error, /That request is gone/);
  assert.match((await decide('/api/fleet/decide', { id: 'open:no-such-session:toolu_1', behavior: 'approve-plan', mode: 'auto' })).error, /That session is gone/);
  assert.match((await decide('/api/fleet/decide', { id: 'open:s1-fixture:toolu_1', behavior: 'approve-plan', mode: 'auto' })).error, /not asking that any more/);
  assert.match((await decide('/api/fleet/decide', { id: 'open:', behavior: 'allow' })).error, /gone/);
  const bare = await fetch(`${p.base}/api/fleet/decide`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' });
  assert.equal(bare.status, 403, 'only for a paired page with its token');
});
