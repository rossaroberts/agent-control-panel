// Fleet's service worker. It does three small things and nothing else:
//   - keeps one page, /offline.html, so a phone whose Mac is unreachable says so instead of showing the browser's error;
//   - for a page navigation only, tries the network first and falls back to that page;
//   - shows a push and opens the session it names when tapped.
// It caches nothing from /api/ and not the Fleet page itself: each start of the panel mints a token the page carries, so a
// stored page would be a dead one. Every other request (api, scripts, the socket) is left to the browser untouched.
const OFFLINE = '/offline.html';
// The server replaces the placeholder with a hash of offline.html, so editing that page gives phones a new cache.
const CACHE = 'fleet-offline-__OFFLINE_HASH__';

self.addEventListener('install', (event) => {
  self.skipWaiting?.();
  event.waitUntil(caches.open(CACHE).then((c) => c.addAll([OFFLINE])));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(Promise.all([
    caches.keys().then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)))),
    self.clients.claim(),
  ]));
});

function handleFetch(event) {
  const req = event.request;
  if (req.mode !== 'navigate' || req.method !== 'GET') return undefined;
  event.respondWith(fetch(req).catch(() => caches.match(OFFLINE)));
}
self.addEventListener('fetch', handleFetch);

// A push is {title, body, data: {url}}. A body that cannot be read still shows, so a missed "needs you" is never silent.
self.addEventListener('push', (event) => {
  let p = {};
  try { p = event.data ? event.data.json() : {}; } catch {}
  const title = typeof p.title === 'string' && p.title ? p.title.slice(0, 120) : 'Fleet';
  const url = typeof p.data?.url === 'string' ? p.data.url : '/';
  event.waitUntil(self.registration.showNotification(title, {
    body: typeof p.body === 'string' ? p.body.slice(0, 300) : '',
    data: { url },
    icon: '/icons/icon-192.png',
    tag: typeof p.tag === 'string' ? p.tag : undefined,
  }));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const raw = event.notification.data?.url;
  const url = typeof raw === 'string' && raw.startsWith('/') && !raw.startsWith('//') ? raw : '/'; // this origin only
  event.waitUntil((async () => {
    const wins = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const open = wins[0];
    if (open) {
      await open.focus();
      try { await open.navigate(url); } catch {}
      return;
    }
    await self.clients.openWindow(url);
  })());
});
