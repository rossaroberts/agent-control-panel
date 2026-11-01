// "Notify me" on the Fleet page: a button in the top bar that turns on a push for this phone when a session comes to need
// you. Kept out of fleet.html like pair.js; it uses only the page's token. It appears only in the installed app (Home Screen
// on iOS, which is the only place iOS allows push) and only where the browser can push at all, and it never asks for
// permission by itself: the one prompt is the press of this button.
(() => {
  const nav = document.querySelector('nav.top');
  const installed = matchMedia('(display-mode: standalone)').matches || navigator.standalone === true;
  if (!nav || typeof TOKEN === 'undefined' || !installed) return;
  if (!('serviceWorker' in navigator) || !('PushManager' in window) || typeof Notification === 'undefined') return;

  const post = (route, body) => fetch(route, { method: 'POST', headers: { 'content-type': 'application/json', 'x-acp-token': TOKEN }, body: JSON.stringify(body) })
    .then((r) => r.json().catch(() => ({ ok: false, error: `the panel answered ${r.status}` })));
  const same = (a, b) => { if (!a || !b || a.byteLength !== b.byteLength) return false; const x = new Uint8Array(a), y = new Uint8Array(b); return x.every((v, i) => v === y[i]); };
  const bytes = (b64url) => Uint8Array.from(atob(b64url.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));

  const btn = document.createElement('button');
  btn.id = 'notify-me'; btn.type = 'button';
  let reg = null, busy = false;

  /** The label says what a press does next, and what is true now: off, on, or blocked by the phone's settings. */
  async function paint(problem) {
    const sub = reg ? await reg.pushManager.getSubscription() : null;
    const blocked = Notification.permission === 'denied';
    btn.disabled = blocked || busy;
    btn.textContent = blocked ? 'Notifications blocked' : problem ? 'Notify: could not' : sub ? 'Notify: on' : 'Notify me';
    btn.title = blocked ? 'Allow notifications for Fleet in the phone\'s Settings, then open the app again'
      : problem ? String(problem)
      : sub ? 'This phone is told when a session needs you. Press to stop.' : 'Tell this phone when a session needs you';
  }

  /** Subscribe this browser and tell the panel. Called from the press itself, so the permission prompt is allowed. */
  async function turnOn() {
    if (Notification.permission !== 'granted') { // first: the prompt must come straight from the press, before any await
      if (await Notification.requestPermission() !== 'granted') return;
    }
    const k = await post('/api/push/key', {});
    if (!k.ok) throw new Error(k.error || 'the panel gave no key');
    const key = bytes(k.publicKey);
    let sub = await reg.pushManager.getSubscription();
    if (sub && !same(sub.options?.applicationServerKey, key.buffer)) { await sub.unsubscribe(); sub = null; } // made for another key
    sub ||= await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
    const r = await post('/api/push/subscribe', { subscription: sub.toJSON() });
    if (!r.ok) { await sub.unsubscribe().catch(() => {}); throw new Error(r.error || 'the panel did not take the subscription'); }
  }

  async function turnOff() {
    const sub = await reg.pushManager.getSubscription();
    if (!sub) return;
    await post('/api/push/unsubscribe', { endpoint: sub.endpoint }).catch(() => {});
    await sub.unsubscribe();
  }

  btn.onclick = async () => {
    if (busy) return;
    busy = true; btn.disabled = true;
    let problem = null;
    try { (await reg.pushManager.getSubscription()) && Notification.permission === 'granted' ? await turnOff() : await turnOn(); }
    catch (e) { problem = e?.message || e; }
    busy = false;
    await paint(problem);
  };

  // Once the service worker is running: show the button, and quietly hand the panel the subscription again (a panel that lost
  // push.json, or a new key, would otherwise leave a phone that says "on" and hears nothing). No prompt happens here.
  navigator.serviceWorker.ready.then(async (r) => {
    reg = r;
    nav.insertBefore(btn, document.getElementById('refresh'));
    await paint();
    if (Notification.permission === 'granted' && await reg.pushManager.getSubscription()) {
      await turnOn().catch(() => {}); await paint();
    }
  }).catch(() => {});
})();
