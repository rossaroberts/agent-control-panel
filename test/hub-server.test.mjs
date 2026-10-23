// The panel against a real hub: the server is spawned as launchd runs it, pointed at a hub running in this process, with a
// token already in hub.json (so no browser sign-in is needed). It proves the round trip: World opens at boot and is
// pushed to the hub, an order placed on the hub's site comes back as the Mac's own answer, and the page has a world-only view.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path'; import net from 'node:net'; import http from 'node:http';
import { spawn } from 'node:child_process';
import { createHub } from '../hub/server.mjs';
import { loadConfig } from '../hub/config.mjs';

const SERVER = new URL('../server.mjs', import.meta.url).pathname;
const freePort = () => new Promise((r) => { const s = net.createServer().listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => r(port)); }); });
const until = async (check, ms = 15_000) => { const end = Date.now() + ms; while (Date.now() < end) { const v = await check(); if (v) return v; await new Promise((r) => setTimeout(r, 150)); } return null; };
const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), 'acp-hub-'));

let hub, hubUrl, panel, panelPort, prefs, cookie;
const ORIGIN = 'http://127.0.0.1';
/** One call to the hub as alice in a browser (her cookie and the Origin). */
const hubCall = (method, p, body) => fetch(hubUrl + p, {
  method, headers: { cookie: `hub_s=${cookie}`, origin: ORIGIN, ...(body && { 'content-type': 'application/json' }) },
  body: body && JSON.stringify(body),
}).then(async (r) => ({ status: r.status, json: await r.json().catch(() => null) }));

before(async () => {
  hub = createHub(loadConfig({ HUB_DATA: tmp(), HUB_DEV_LOGIN: '1', HUB_PUBLIC_URL: ORIGIN }));
  const { port } = await hub.listen(0, '127.0.0.1');
  hubUrl = `http://127.0.0.1:${port}`;
  const login = await fetch(hubUrl + '/auth/dev', { method: 'POST', headers: { origin: ORIGIN, 'content-type': 'application/json' }, body: JSON.stringify({ login: 'alice' }) });
  cookie = /hub_s=([^;]+)/.exec(login.headers.get('set-cookie'))[1];
  const { account } = await login.json();
  const { token } = hub.auth.mintToken(account.id, { label: 'test', device: 'test-mac' });
  prefs = tmp();
  fs.writeFileSync(path.join(prefs, 'hub.json'), JSON.stringify({ v: 1, url: hubUrl, device: 'test-mac', token, account: { id: account.id, handle: 'alice', x: account.x, y: account.y } }), { mode: 0o600 });
  panelPort = await freePort();
  const child = spawn(process.execPath, [SERVER], { env: { ...process.env, ACP_PORT: String(panelPort), ACP_PREFS: path.join(prefs, 'prefs.json'), ACP_DAEMON: '1', ACP_HUB: hubUrl }, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = ''; child.stdout.on('data', (d) => { out += d; }); child.stderr.on('data', (d) => { out += d; });
  panel = { child, log: () => out };
});
after(async () => {
  if (panel && panel.child.exitCode === null && panel.child.signalCode === null) await new Promise((r) => { panel.child.once('exit', r); panel.child.kill(); });
  await hub?.close();
});

test('the panel opens World at boot and the hub shows the system', async () => {
  const seen = await until(async () => (await hubCall('GET', '/api/systems/@alice')).json?.system);
  assert.ok(seen, 'the hub never got a system: ' + panel.log());
});

test('an order placed on the hub is run by the Mac and answered in its own words', async () => {
  const placed = await hubCall('POST', '/api/me/orders', { kind: 'frontier', ang: 0 });
  assert.equal(placed.status, 200, JSON.stringify(placed.json));
  const order = await until(async () => (await hubCall('GET', '/api/me/orders')).json?.orders?.find((o) => o.status !== 'pending' && o.status !== 'running'));
  assert.ok(order, 'the order was never answered: ' + panel.log());
  assert.equal(order.status, 'failed');
  assert.match(order.result.error, /costs 300 credit/);
});

test('/world is the same page opened on the world, /board opens the board, and / is Fleet', async () => {
  const world = await fetch(`http://127.0.0.1:${panelPort}/world`);
  assert.equal(world.status, 200);
  const html = await world.text();
  assert.match(html, /ACP_VIEW = 'world'/);
  assert.doesNotMatch(html, /__ACP_(VIEW|TOKEN)__/);
  assert.match(await fetch(`http://127.0.0.1:${panelPort}/board`).then((r) => r.text()), /ACP_VIEW = 'board'/);
  const home = await fetch(`http://127.0.0.1:${panelPort}/`).then((r) => r.text());
  assert.match(home, /<title>Fleet<\/title>/);
  assert.doesNotMatch(home, /__ACP_TOKEN__/);
});

/** Runs `node server.mjs <args>` against this hub with a prefs folder of its own; `open` is a stand-in that opens nothing. */
function cli(args, dir, onLine) {
  const bin = path.join(dir, 'bin');
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(bin, 'open'), '#!/bin/sh\nexit 0\n', { mode: 0o755 });
  const child = spawn(process.execPath, [SERVER, ...args], { env: { ...process.env, PATH: bin, ACP_PREFS: path.join(dir, 'prefs.json'), ACP_HUB: hubUrl, ACP_PORT: String(panelPort) }, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  child.stdout.on('data', (d) => { out += d; onLine?.(out); }); child.stderr.on('data', (d) => { out += d; });
  const done = new Promise((r) => child.on('exit', (code) => r({ code, out })));
  return { done, kill: () => child.kill() };
}

test('login from a terminal shows a code, finishes when it is approved, and status and logout follow', async (t) => {
  const dir = tmp();
  assert.match((await cli(['status'], dir).done).out, /^Not signed in \(http:\/\/127\.0\.0\.1:\d+\)\n/);
  let approved = false;
  const login = cli(['login'], dir, async (out) => {
    const m = /Open \S+ and enter ([A-Z0-9-]+)/.exec(out);
    if (!m || approved) return;
    approved = true;
    const r = await hubCall('POST', '/api/device/approve', { user_code: m[1], label: 'Terminal' });
    assert.equal(r.status, 200, JSON.stringify(r.json));
  });
  t.after(login.kill);
  const r = await login.done;
  assert.equal(r.code, 0, r.out);
  assert.match(r.out, /Signed in as @alice\. Your galaxy is at -?\d+, -?\d+\.\n$/);
  assert.match((await cli(['status'], dir).done).out, /^Signed in as @alice \(http:\/\/127\.0\.0\.1:\d+\)\n/);
  assert.equal(JSON.parse(fs.readFileSync(path.join(dir, 'hub.json'), 'utf8')).token.startsWith('hub_'), true);
  const out = await cli(['logout'], dir).done;
  assert.deepEqual([out.code, out.out], [0, 'Signed out.\n']);
  assert.equal(fs.existsSync(path.join(dir, 'hub.json')), false);
});

test('a command the CLI does not know prints the usage and exits 2', async () => {
  const r = await cli(['nonsense'], tmp()).done;
  assert.equal(r.code, 2);
  assert.match(r.out, /usage: node server\.mjs daemon install \[--program <path>\]\|remove\|status \| login \[--link <code>\] \| logout \| status/);
});

test('signing in from the page of a panel finishes by itself, and signing out ends it', async (t) => {
  const dir = tmp(), port = await freePort();
  const child = spawn(process.execPath, [SERVER], { env: { ...process.env, ACP_PORT: String(port), ACP_PREFS: path.join(dir, 'prefs.json'), ACP_DAEMON: '1', ACP_HUB: hubUrl }, stdio: 'ignore' });
  t.after(() => child.kill());
  const base = `http://127.0.0.1:${port}`;
  const token = await until(async () => /const TOKEN = '([^']+)'/.exec(await fetch(base + '/').then((r) => r.text(), () => ''))?.[1]);
  assert.ok(token, 'the panel did not answer');
  const api = (p) => fetch(base + p, { method: 'POST', headers: { origin: base, 'x-acp-token': token, 'content-type': 'application/json' }, body: '{}' }).then((r) => r.json());
  const start = await api('/api/hub/login/start');
  assert.equal(start.ok, true);
  assert.equal((await api('/api/hub/login/start')).userCode, start.userCode, 'a second start returns the code already shown');
  assert.equal((await hubCall('POST', '/api/device/approve', { user_code: start.userCode, label: 'Panel' })).status, 200);
  const signedIn = await until(async () => { const s = await api('/api/hub/status'); return s.hub.loggedIn && s.hub; });
  assert.equal(signedIn.account.handle, 'alice');
  assert.equal(JSON.stringify(signedIn).includes('hub_'), false, 'the token never reaches the page');
  assert.equal((await api('/api/hub/logout')).ok, true);
  assert.equal((await api('/api/hub/status')).hub.loggedIn, false);
  assert.equal(fs.existsSync(path.join(dir, 'hub.json')), false);
});

/** A hub.json holding a sign-in under way for a fresh code from the hub, polled every second; returns the code. */
async function pendingFile(dir, url) {
  const code = await fetch(hubUrl + '/api/device/code', { method: 'POST' }).then((r) => r.json());
  const pending = { userCode: code.user_code, verificationUri: code.verification_uri_complete, expiresAt: Date.now() + 600_000, interval: 1, deviceCode: code.device_code };
  fs.writeFileSync(path.join(dir, 'hub.json'), JSON.stringify({ v: 1, url, device: 'restarted-mac', pending }), { mode: 0o600 });
  return code;
}
const tokenIn = (dir) => { try { return JSON.parse(fs.readFileSync(path.join(dir, 'hub.json'), 'utf8')).token ?? null; } catch { return null; } };
const spawnPanel = async (t, dir, hub) => {
  const child = spawn(process.execPath, [SERVER], { env: { ...process.env, ACP_PORT: String(await freePort()), ACP_PREFS: path.join(dir, 'prefs.json'), ACP_DAEMON: '1', ACP_HUB: hub }, stdio: 'ignore' });
  t.after(() => child.kill());
};

test('a panel restarted while a sign-in was under way finishes it, with no new code asked for', async (t) => {
  const dir = tmp();
  const code = await pendingFile(dir, hubUrl);
  assert.equal((await hubCall('POST', '/api/device/approve', { user_code: code.user_code, label: 'Restarted' })).status, 200);
  await spawnPanel(t, dir, hubUrl);
  const token = await until(() => tokenIn(dir));
  assert.match(token ?? '', /^hub_/, 'the panel never finished the sign-in');
});

test('a sign-in poll that fails is tried again while the code is live', async (t) => {
  const dir = tmp();
  const polls = [], asked = [];
  // In front of the hub: the first poll for the token fails as a restarting hub would; everything else goes through.
  const front = http.createServer((req, res) => {
    asked.push(`${req.method} ${req.url}`);
    if (req.url === '/api/device/token' && polls.push(Date.now()) === 1) { res.writeHead(503).end(); return; }
    req.headers.host = '127.0.0.1';
    hub.server.emit('request', req, res);
  });
  await new Promise((r) => front.listen(0, '127.0.0.1', r));
  t.after(() => new Promise((r) => { front.closeAllConnections(); front.close(r); }));
  const frontUrl = `http://127.0.0.1:${front.address().port}`;
  const code = await pendingFile(dir, frontUrl);
  assert.equal((await hubCall('POST', '/api/device/approve', { user_code: code.user_code, label: 'Flaky' })).status, 200);
  await spawnPanel(t, dir, frontUrl);
  const token = await until(() => tokenIn(dir));
  assert.match(token ?? '', /^hub_/, `the panel stopped after the failed poll (${polls.length} polls)`);
  assert.ok(polls.length >= 2);
  assert.equal(asked.includes('POST /api/device/code'), false, 'no new code was asked for');
});
