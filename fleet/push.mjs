// Web Push, from this panel to a phone, on node:crypto alone: a notification when a session comes to need you, delivered
// while the Fleet app is closed or the phone is locked.
//   RFC 8291  message encryption (aes128gcm): the push service sees only ciphertext, and only the phone can read it;
//   RFC 8292  VAPID: an ES256-signed token that says which server is pushing, so the service accepts the send.
// Keys and subscriptions live beside prefs.json: push-vapid.json (this panel's key pair, made once) and push.json (the
// phones that asked). Both are 0600.
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

export const b64u = (b) => Buffer.from(b).toString('base64url');
const unb64 = (s) => Buffer.from(String(s), 'base64url');
const SPKI_P256 = Buffer.from('3059301306072a8648ce3d020106082a8648ce3d030107034200', 'hex'); // header of a P-256 SubjectPublicKeyInfo

const RS = 4096; // record size written into the header: the one record holds the whole message
// A push service accepts at most 4096 bytes of body (RFC 8030 section 7.2): header 86 (salt 16, rs 4, idlen 1, key 65), the
// 0x02 delimiter, the 16-byte GCM tag; what is left is the message.
const MAX_PAYLOAD = 4096 - 86 - 1 - 16;

/** Write `text` to `file` with mode 0600, whole or not at all. */
function writePrivate(file, text) {
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, text, { mode: 0o600 });
  fs.chmodSync(tmp, 0o600);
  fs.renameSync(tmp, file);
}

/** This panel's VAPID pair: { publicKey (65-byte uncompressed point, base64url), privateKey (PKCS8 DER, base64url) }.
 *  Made on first use, when there is no file. A file that is there but does not hold a usable pair (not JSON, wrong curve, halves
 *  that do not match) is replaced, and every phone then subscribes again. A file that cannot be READ (permissions, too many open
 *  files) is an error, not a missing key: writing a new pair over it would destroy the real private key and orphan every phone. */
export function vapidKeys(dir) {
  const file = path.join(dir, 'push-vapid.json');
  let text = null;
  try { text = fs.readFileSync(file, 'utf8'); } catch (e) { if (e?.code !== 'ENOENT') throw e; }
  if (text !== null) {
    try {
      const k = JSON.parse(text);
      const priv = crypto.createPrivateKey({ key: unb64(k.privateKey), format: 'der', type: 'pkcs8' });
      const pub = unb64(k.publicKey);
      const derived = crypto.createPublicKey(priv).export({ format: 'der', type: 'spki' }).subarray(-65);
      if (priv.asymmetricKeyDetails?.namedCurve === 'prime256v1' && pub.length === 65 && pub.equals(derived)) return { publicKey: k.publicKey, privateKey: k.privateKey };
    } catch {}
  }
  const { publicKey, privateKey } = crypto.generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  const k = { publicKey: b64u(publicKey.export({ format: 'der', type: 'spki' }).subarray(-65)), privateKey: b64u(privateKey.export({ format: 'der', type: 'pkcs8' })) };
  fs.mkdirSync(dir, { recursive: true });
  writePrivate(file, JSON.stringify(k));
  return k;
}

/** A browser subscription's keys as bytes, or a throw: p256dh is an uncompressed P-256 point, auth is 16 random bytes. */
function subscriberKeys(sub) {
  const p256dh = unb64(sub?.keys?.p256dh), auth = unb64(sub?.keys?.auth);
  if (p256dh.length !== 65 || p256dh[0] !== 4 || auth.length !== 16) throw new Error('not a push subscription: bad keys');
  return { p256dh, auth };
}

/** RFC 8291 section 3: `payload` (a Buffer) encrypted for `sub` as one aes128gcm record. → { body, headers }. */
export function encrypt(sub, payload) {
  const { p256dh, auth } = subscriberKeys(sub);
  if (payload.length > MAX_PAYLOAD) throw new Error(`payload too large for one push record (${payload.length} bytes)`);
  const ecdh = crypto.createECDH('prime256v1'); ecdh.generateKeys();
  const asPublic = ecdh.getPublicKey();
  let secret;
  try { secret = ecdh.computeSecret(p256dh); } catch { throw new Error('not a push subscription: p256dh is not a point on the curve'); }
  const salt = crypto.randomBytes(16);
  // HKDF-Extract(auth, ecdh) then Expand with the key info, as one call; then the content keys from the salt.
  const ikm = Buffer.from(crypto.hkdfSync('sha256', secret, auth, Buffer.concat([Buffer.from('WebPush: info\0'), p256dh, asPublic]), 32));
  const cek = Buffer.from(crypto.hkdfSync('sha256', ikm, salt, 'Content-Encoding: aes128gcm\0', 16));
  const nonce = Buffer.from(crypto.hkdfSync('sha256', ikm, salt, 'Content-Encoding: nonce\0', 12));
  const c = crypto.createCipheriv('aes-128-gcm', cek, nonce);
  const sealed = Buffer.concat([c.update(Buffer.concat([payload, Buffer.from([2])])), c.final(), c.getAuthTag()]); // 0x02: this is the last record
  const header = Buffer.alloc(21); salt.copy(header); header.writeUInt32BE(RS, 16); header[20] = asPublic.length;
  const body = Buffer.concat([header, asPublic, sealed]);
  return { body, headers: { 'Content-Encoding': 'aes128gcm', 'Content-Type': 'application/octet-stream', 'Content-Length': String(body.length) } };
}

/** RFC 8292: the Authorization header for a push to the service at `audience` (its origin), from `subject` (a mailto: or https: contact). */
export function vapidHeaders(audience, subject, keys, nowMs = Date.now()) {
  const head = b64u(JSON.stringify({ typ: 'JWT', alg: 'ES256' }));
  const claims = b64u(JSON.stringify({ aud: audience, exp: Math.floor(nowMs / 1000) + 12 * 3600, sub: subject }));
  const sig = crypto.sign('SHA256', Buffer.from(`${head}.${claims}`), { key: crypto.createPrivateKey({ key: unb64(keys.privateKey), format: 'der', type: 'pkcs8' }), dsaEncoding: 'ieee-p1363' });
  return { Authorization: `vapid t=${head}.${claims}.${b64u(sig)}, k=${keys.publicKey}` };
}

/** Push `payload` (an object, sent as JSON) to one subscription. Never throws: { ok, status, gone } where gone (404 or 410)
 *  says the phone unsubscribed and the subscription should be forgotten; status 0 is "could not reach the service". */
export async function send(sub, payload, keys, { subject, ttl = 3600, timeoutMs = 10_000 } = {}) {
  try {
    const raw = Buffer.isBuffer(payload) ? payload : Buffer.from(JSON.stringify(payload));
    const { body, headers } = encrypt(sub, raw);
    const aud = new URL(sub.endpoint).origin;
    const r = await fetch(sub.endpoint, { method: 'POST', body, redirect: 'manual', signal: AbortSignal.timeout(timeoutMs),
      headers: { ...headers, ...vapidHeaders(aud, subject, keys), TTL: String(ttl), Urgency: 'high' } });
    await r.arrayBuffer().catch(() => {});
    return { ok: r.status >= 200 && r.status < 300, status: r.status, gone: r.status === 404 || r.status === 410 };
  } catch {
    return { ok: false, status: 0, gone: false };
  }
}

const MAX_SUBS = 20;
// The only hosts the panel will POST a push to: the browsers' own push services (Safari/iOS, Chrome/Android, Firefox, Edge).
// A subscription comes from a page, and the panel makes the request, so an endpoint the page chose must never be able to point
// the panel at anything else (the loopback, the LAN, a name that resolves there). A suffix matches the name itself and its
// subdomains, exactly: `localhost.`, `127.0.0.1.nip.io` and the like are not on this list, so they are refused.
const PUSH_HOSTS = ['web.push.apple.com', 'fcm.googleapis.com', 'android.googleapis.com', 'updates.push.services.mozilla.com', 'notify.windows.com'];
const onList = (h) => PUSH_HOSTS.some((d) => h === d || h.endsWith(`.${d}`));
/** The phones that asked, in `<dir>/push.json`: [{ endpoint, keys: { p256dh, auth }, at }], newest last, at most MAX_SUBS.
 *  What a page posts is checked here: strings of the right shape, an https endpoint (default port, no credentials) on one of
 *  PUSH_HOSTS, and keys of the right shape. */
export function pushStore(dir) {
  const file = path.join(dir, 'push.json');
  const list = () => { try { const v = JSON.parse(fs.readFileSync(file, 'utf8')); return Array.isArray(v) ? v : []; } catch { return []; } };
  const save = (v) => { fs.mkdirSync(dir, { recursive: true }); writePrivate(file, JSON.stringify(v)); };
  function check(sub) {
    if (!sub || typeof sub !== 'object' || typeof sub.endpoint !== 'string' || !sub.keys || typeof sub.keys !== 'object'
      || typeof sub.keys.p256dh !== 'string' || typeof sub.keys.auth !== 'string') return 'not a push subscription: endpoint and keys must be strings';
    let u;
    try { u = new URL(sub.endpoint); } catch { return 'no endpoint'; }
    if (sub.endpoint.length > 2048 || u.protocol !== 'https:' || u.username || u.password || u.port) return 'endpoint must be a plain https address';
    if (!onList(u.hostname.toLowerCase())) return `endpoint host ${u.hostname.slice(0, 80)} is not a browser push service (${PUSH_HOSTS.join(', ')})`;
    try { subscriberKeys(sub); } catch (e) { return e.message; }
    return null;
  }
  return {
    list,
    add(sub) {
      const bad = check(sub);
      if (bad) return { ok: false, error: bad };
      const mine = { endpoint: sub.endpoint, keys: { p256dh: sub.keys.p256dh, auth: sub.keys.auth }, at: Date.now() };
      save([...list().filter((s) => s.endpoint !== mine.endpoint), mine].slice(-MAX_SUBS));
      return { ok: true };
    },
    remove(endpoint) {
      const all = list(), rest = all.filter((s) => s.endpoint !== endpoint);
      if (rest.length === all.length) return false;
      save(rest);
      return true;
    },
  };
}

/** Push `payload` to every stored phone, forgetting the ones the push service says are gone. Makes no keys when nobody asked. */
export async function pushAll({ dir, payload, subject, sender = send }) {
  const store = pushStore(dir), subs = store.list();
  const out = { sent: 0, failed: 0, dropped: 0 };
  if (!subs.length) return out;
  const keys = vapidKeys(dir);
  const raw = Buffer.from(JSON.stringify(payload));
  const results = await Promise.all(subs.map((s) => sender(s, raw, keys, { subject })));
  results.forEach((r, i) => {
    if (r.ok) out.sent++;
    else if (r.gone && store.remove(subs[i].endpoint)) out.dropped++;
    else out.failed++;
  });
  return out;
}

const tidy = (v, n) => String(v ?? '').replace(/[<>\x00-\x08\x0b-\x1f\x7f]/g, '').replace(/\s+/g, ' ').trim().slice(0, n);
/** What a phone shows when session `x` on `machine` comes to need you; tapping it opens `/#<machine>:<sessionId>` (the page's own deep link). */
export function needsYou(machine, x) {
  const what = tidy(typeof x.waitingFor === 'string' ? x.waitingFor : x.doing ?? 'It waits for you', 160) || 'It waits for you';
  return { title: 'Fleet · needs you', body: `${what}\n${tidy(`${x.title ?? x.name ?? 'A session'} · ${machine}`, 120)}`, data: { url: `/#${encodeURIComponent(`${machine}:${x.sessionId ?? x.id}`.toWellFormed())}` } };
}
