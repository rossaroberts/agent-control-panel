// The service worker is a plain script; it runs here in a vm with the few globals it touches stubbed.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

function load({ fetchImpl = async () => 'network', match = async (u) => `cached:${u}` } = {}) {
  const handlers = {};
  const added = [];
  const shown = [];
  const opened = [];
  let clientList = [];
  const self = {
    addEventListener(type, fn) { handlers[type] = fn; },
    skipWaiting() {},
    clients: { claim: async () => {}, matchAll: async () => clientList, openWindow: async (u) => { opened.push(u); } },
    registration: { showNotification: async (title, o) => { shown.push({ title, o }); } },
  };
  const caches = { open: async () => ({ addAll: async (l) => { added.push(...l); } }), match, keys: async () => [], delete: async () => true };
  vm.runInNewContext(fs.readFileSync(new URL('../public/sw.js', import.meta.url), 'utf8'), { self, caches, fetch: fetchImpl, URL, Promise });
  return { handlers, added, shown, opened, setClients: (c) => { clientList = c; } };
}
const fetchEvent = (request) => { const e = { request, responded: undefined, respondWith(p) { e.responded = p; } }; return e; };
const waitEvent = (extra = {}) => { const e = { waited: undefined, waitUntil(p) { e.waited = p; }, ...extra }; return e; };

test('install pre-caches the offline page and nothing else', async () => {
  const sw = load();
  const e = waitEvent();
  sw.handlers.install(e);
  await e.waited;
  assert.equal(JSON.stringify(sw.added), '["/offline.html"]');
});

test('the fetch handler leaves api calls, scripts and everything but navigations to the network', () => {
  const sw = load();
  for (const request of [
    { mode: 'cors', method: 'GET', url: 'https://mac.example-tailnet.ts.net/api/fleet' },
    { mode: 'same-origin', method: 'GET', url: 'https://mac.example-tailnet.ts.net/vendor/xterm.js' },
    { mode: 'cors', method: 'POST', url: 'https://mac.example-tailnet.ts.net/api/decide' },
    { mode: 'websocket', method: 'GET', url: 'wss://mac.example-tailnet.ts.net/api/term/ws' },
  ]) { const e = fetchEvent(request); sw.handlers.fetch(e); assert.equal(e.responded, undefined, request.url); }
});

test('a navigation goes to the network, and to the offline page when that fails', async () => {
  const sw = load({ fetchImpl: async () => { throw new TypeError('offline'); }, match: async (u) => `cached:${u}` });
  const e = fetchEvent({ mode: 'navigate', method: 'GET', url: 'https://mac.example-tailnet.ts.net/' });
  sw.handlers.fetch(e);
  assert.equal(await e.responded, 'cached:/offline.html');
  const up = load({ fetchImpl: async () => 'live page' });
  const e2 = fetchEvent({ mode: 'navigate', method: 'GET', url: 'https://mac.example-tailnet.ts.net/' });
  up.handlers.fetch(e2);
  assert.equal(await e2.responded, 'live page');
});

test('a push shows its title and body, and a tap opens its url', async () => {
  const sw = load();
  const e = waitEvent({ data: { json: () => ({ title: 'api-1 needs you', body: 'Allow Bash?', data: { url: '/?s=mac:abc' } }) } });
  sw.handlers.push(e);
  await e.waited;
  assert.equal(sw.shown[0].title, 'api-1 needs you');
  assert.equal(sw.shown[0].o.body, 'Allow Bash?');
  assert.equal(sw.shown[0].o.data.url, '/?s=mac:abc');
  let closed = false;
  const click = waitEvent({ notification: { data: { url: '/?s=mac:abc' }, close() { closed = true; } } });
  sw.handlers.notificationclick(click);
  await click.waited;
  assert.ok(closed);
  assert.equal(sw.opened.join(), '/?s=mac:abc');
});

test('a tap focuses a window that is already open instead of opening another', async () => {
  const sw = load();
  const focused = [];
  sw.setClients([{ url: 'https://mac.example-tailnet.ts.net/', focus: async () => { focused.push('a'); }, navigate: async (u) => { focused.push(u); } }]);
  const click = waitEvent({ notification: { data: { url: '/?s=mac:abc' }, close() {} } });
  sw.handlers.notificationclick(click);
  await click.waited;
  assert.equal(sw.opened.length, 0);
  assert.ok(focused.length > 0);
});

test('a push with no readable payload still shows something', async () => {
  const sw = load();
  const e = waitEvent({ data: { json: () => { throw new Error('bad'); } } });
  sw.handlers.push(e);
  await e.waited;
  assert.equal(sw.shown[0].title, 'Fleet');
});

test('a notification url that leaves this origin is replaced by the root', async () => {
  const sw = load();
  for (const bad of ['https://evil.example.com/', '//evil.example.com', 'javascript:alert(1)']) {
    const click = waitEvent({ notification: { data: { url: bad }, close() {} } });
    sw.handlers.notificationclick(click);
    await click.waited;
  }
  assert.equal(sw.opened.join(), '/,/,/');
});
