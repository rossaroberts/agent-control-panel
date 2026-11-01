// public/push.js run against a stand-in page: when the button exists, what a press does, and that nothing asks for
// permission by itself.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const SRC = fs.readFileSync(new URL('../public/push.js', import.meta.url), 'utf8');
const KEY = Buffer.alloc(65, 7); KEY[0] = 4;

function page({ standalone = true, pushManager = true, permission = 'default', answer = 'granted', subscribed = false, subscribeOk = true } = {}) {
  const log = { prompts: 0, posts: [], subscribeArgs: null, unsubscribed: 0 };
  const make = (endpoint, keyBuf) => ({ endpoint, options: { applicationServerKey: keyBuf }, toJSON() { return { endpoint: this.endpoint, keys: { p256dh: 'p', auth: 'a' } }; }, async unsubscribe() { log.unsubscribed++; sub = null; } });
  let sub = subscribed ? make('https://push.example.com/x', new Uint8Array(KEY).buffer) : null;
  const reg = { pushManager: {
    getSubscription: async () => sub,
    subscribe: async (opts) => { log.subscribeArgs = opts; sub = make('https://push.example.com/new', new Uint8Array(opts.applicationServerKey).buffer); return sub; },
  } };
  const refresh = { id: 'refresh' }, inserted = [];
  const nav = { insertBefore: (el, before) => { inserted.push(el); assert.equal(before, refresh); } };
  const Notification = { permission, requestPermission: async () => { log.prompts++; Notification.permission = answer; return answer; } };
  const ctx = {
    TOKEN: 'tok', Notification,
    matchMedia: (q) => ({ matches: standalone && q.includes('standalone') }),
    navigator: { serviceWorker: { ready: Promise.resolve(reg) } },
    document: { querySelector: (q) => (q === 'nav.top' ? nav : null), getElementById: (id) => (id === 'refresh' ? refresh : null), createElement: () => ({}) },
    fetch: async (route, init) => {
      log.posts.push([route, JSON.parse(init.body), init.headers['x-acp-token']]);
      const out = route === '/api/push/key' ? { ok: true, publicKey: KEY.toString('base64url') }
        : route === '/api/push/subscribe' ? (subscribeOk ? { ok: true } : { ok: false, error: 'refused' }) : { ok: true };
      return { status: 200, json: async () => out };
    },
    atob: (s) => Buffer.from(s, 'base64').toString('binary'),
    Uint8Array, ArrayBuffer,
  };
  ctx.window = ctx;
  if (pushManager) ctx.PushManager = function PushManager() {};
  vm.runInNewContext(SRC, ctx);
  return { log, inserted, tick: () => new Promise((r) => setTimeout(r, 20)) };
}

test('nothing is added in a browser tab, or where push cannot work', async () => {
  for (const p of [page({ standalone: false }), page({ pushManager: false })]) { await p.tick(); assert.equal(p.inserted.length, 0); assert.equal(p.log.prompts, 0); }
});

test('installed: a "Notify me" button appears, and loading the page never asks for permission or subscribes', async () => {
  const p = page(); await p.tick();
  assert.equal(p.inserted.length, 1);
  assert.equal(p.inserted[0].textContent, 'Notify me');
  assert.equal(p.log.prompts, 0);
  assert.deepEqual(p.log.posts, []);
});

test('a press asks permission, subscribes userVisibleOnly with the panel\'s key, and posts the subscription with the page token', async () => {
  const p = page(); await p.tick();
  await p.inserted[0].onclick();
  assert.equal(p.log.prompts, 1);
  assert.equal(p.log.subscribeArgs.userVisibleOnly, true);
  assert.deepEqual(Buffer.from(p.log.subscribeArgs.applicationServerKey), KEY);
  assert.deepEqual(p.log.posts.map((x) => x[0]), ['/api/push/key', '/api/push/subscribe']);
  assert.deepEqual(p.log.posts[1][1], { subscription: { endpoint: 'https://push.example.com/new', keys: { p256dh: 'p', auth: 'a' } } });
  assert.ok(p.log.posts.every((x) => x[2] === 'tok'));
  assert.equal(p.inserted[0].textContent, 'Notify: on');
});

test('a refused permission subscribes nothing; a blocked phone says so; a panel that refuses leaves no half-subscription', async () => {
  const no = page({ answer: 'denied' }); await no.tick();
  await no.inserted[0].onclick();
  assert.deepEqual(no.log.posts, []);
  assert.equal(no.inserted[0].textContent, 'Notifications blocked');
  assert.equal(no.inserted[0].disabled, true);

  const bad = page({ permission: 'granted', subscribeOk: false }); await bad.tick();
  await bad.inserted[0].onclick();
  assert.equal(bad.log.unsubscribed, 1);
  assert.equal(bad.inserted[0].textContent, 'Notify: could not');
  assert.equal(bad.inserted[0].title, 'refused');
});

test('already on: a press turns it off, here and on the panel; loading re-posts the subscription without a prompt', async () => {
  const p = page({ permission: 'granted', subscribed: true }); await p.tick();
  assert.equal(p.log.prompts, 0);
  assert.deepEqual(p.log.posts.map((x) => x[0]), ['/api/push/key', '/api/push/subscribe'], 'quiet re-sync on load');
  assert.equal(p.inserted[0].textContent, 'Notify: on');
  p.log.posts.length = 0;
  await p.inserted[0].onclick();
  assert.deepEqual(p.log.posts, [['/api/push/unsubscribe', { endpoint: 'https://push.example.com/x' }, 'tok']]);
  assert.equal(p.log.unsubscribed, 1);
  assert.equal(p.inserted[0].textContent, 'Notify me');
});
