import { test } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path';
import http from 'node:http';
import { vapidKeys, encrypt, vapidHeaders, send, pushStore, pushAll, needsYou, b64u } from '../fleet/push.mjs';

const tmp = () => fs.mkdtempSync(path.join(os.tmpdir(), 'acp-push-'));
const unb64 = (s) => Buffer.from(s, 'base64url');
/** A phone: its subscription as the browser hands it over, and the private half to read what it was sent. */
function phone(endpoint = 'https://push.example.com/send/abc') {
  const ecdh = crypto.createECDH('prime256v1'); ecdh.generateKeys();
  const auth = crypto.randomBytes(16);
  return { ecdh, auth, sub: { endpoint, keys: { p256dh: b64u(ecdh.getPublicKey()), auth: b64u(auth) } } };
}
/** RFC 8291 section 3.4 from the receiving end, written out step by step so it does not share code with encrypt(). */
function decrypt(ecdh, auth, body) {
  const salt = body.subarray(0, 16), rs = body.readUInt32BE(16), idlen = body[20];
  const asPub = body.subarray(21, 21 + idlen), cipher = body.subarray(21 + idlen);
  assert.equal(rs, 4096); assert.equal(idlen, 65);
  const secret = ecdh.computeSecret(asPub);
  const hmac = (k, d) => crypto.createHmac('sha256', k).update(d).digest();
  const prkKey = hmac(auth, secret);
  const ikm = hmac(prkKey, Buffer.concat([Buffer.from('WebPush: info\0'), ecdh.getPublicKey(), asPub, Buffer.from([1])]));
  const prk = hmac(salt, ikm);
  const cek = hmac(prk, Buffer.concat([Buffer.from('Content-Encoding: aes128gcm\0'), Buffer.from([1])])).subarray(0, 16);
  const nonce = hmac(prk, Buffer.concat([Buffer.from('Content-Encoding: nonce\0'), Buffer.from([1])])).subarray(0, 12);
  const d = crypto.createDecipheriv('aes-128-gcm', cek, nonce);
  d.setAuthTag(cipher.subarray(cipher.length - 16));
  const plain = Buffer.concat([d.update(cipher.subarray(0, cipher.length - 16)), d.final()]);
  assert.equal(plain[plain.length - 1], 2, 'the last record ends with the 0x02 delimiter');
  return plain.subarray(0, plain.length - 1);
}

test('vapidKeys creates a P-256 pair once, 0600, and reads the same pair back', () => {
  const dir = tmp();
  const a = vapidKeys(dir), b = vapidKeys(dir);
  assert.equal(a.publicKey, b.publicKey);
  assert.equal(unb64(a.publicKey).length, 65);
  assert.equal(unb64(a.publicKey)[0], 4);
  assert.equal(fs.statSync(path.join(dir, 'push-vapid.json')).mode & 0o777, 0o600);
  const sig = crypto.sign('SHA256', Buffer.from('x'), crypto.createPrivateKey({ key: unb64(a.privateKey), format: 'der', type: 'pkcs8' }));
  assert.ok(sig.length > 0);
});

test('a damaged key file is replaced, not trusted', () => {
  const dir = tmp();
  fs.writeFileSync(path.join(dir, 'push-vapid.json'), '{"publicKey":"x","privateKey":"y"}');
  assert.equal(unb64(vapidKeys(dir).publicKey).length, 65);
});

test('encrypt() round-trips: the phone reads exactly what was sent (RFC 8291 aes128gcm)', () => {
  const p = phone();
  const msg = Buffer.from(JSON.stringify({ title: 'Fleet · needs you', body: 'Approve the migration? é ✓', data: { url: '/#Mac%3Aabc' } }));
  const { body, headers } = encrypt(p.sub, msg);
  assert.equal(headers['Content-Encoding'], 'aes128gcm');
  assert.equal(Number(headers['Content-Length']), body.length);
  assert.deepEqual(decrypt(p.ecdh, p.auth, body), msg);
  // fresh salt and ephemeral key every time
  assert.notDeepEqual(encrypt(p.sub, msg).body, body);
});

test('encrypt() refuses a payload that would not fit one record, and a subscription that is not a key', () => {
  const p = phone();
  assert.throws(() => encrypt(p.sub, Buffer.alloc(4000)), /too large/);
  assert.throws(() => encrypt({ endpoint: p.sub.endpoint, keys: { p256dh: 'AAAA', auth: p.sub.keys.auth } }, Buffer.from('x')), /subscription/);
});

test('vapidHeaders() signs an ES256 JWT with aud, sub and an exp within a day, that the public key verifies', () => {
  const keys = vapidKeys(tmp());
  const now = 1_800_000_000_000;
  const { Authorization } = vapidHeaders('https://push.example.com', 'https://mac.example.ts.net', keys, now);
  const m = /^vapid t=([\w-]+)\.([\w-]+)\.([\w-]+), k=([\w-]+)$/.exec(Authorization);
  assert.ok(m, Authorization);
  assert.equal(m[4], keys.publicKey);
  assert.deepEqual(JSON.parse(unb64(m[1])), { typ: 'JWT', alg: 'ES256' });
  const claims = JSON.parse(unb64(m[2]));
  assert.equal(claims.aud, 'https://push.example.com');
  assert.equal(claims.sub, 'https://mac.example.ts.net');
  assert.ok(claims.exp > now / 1000 && claims.exp <= now / 1000 + 86400, `exp ${claims.exp}`);
  const sig = unb64(m[3]);
  assert.equal(sig.length, 64, 'raw r||s, not DER');
  const pub = crypto.createPublicKey({ key: Buffer.concat([Buffer.from('3059301306072a8648ce3d020106082a8648ce3d030107034200', 'hex'), unb64(keys.publicKey)]), format: 'der', type: 'spki' });
  assert.ok(crypto.verify('SHA256', Buffer.from(`${m[1]}.${m[2]}`), { key: pub, dsaEncoding: 'ieee-p1363' }, sig));
  assert.ok(!crypto.verify('SHA256', Buffer.from(`${m[1]}.${m[2]}x`), { key: pub, dsaEncoding: 'ieee-p1363' }, sig));
});

/** A push service on a local port that answers `status` and keeps what it was sent. */
async function service(status) {
  const got = [];
  const srv = http.createServer((req, res) => {
    const chunks = []; req.on('data', (c) => chunks.push(c));
    req.on('end', () => { got.push({ url: req.url, headers: req.headers, body: Buffer.concat(chunks) }); res.writeHead(status); res.end(); });
  });
  await new Promise((r) => srv.listen(0, '127.0.0.1', r));
  return { got, base: `http://127.0.0.1:${srv.address().port}`, close: () => new Promise((r) => srv.close(r)) };
}

test('send() posts the encrypted body with VAPID and TTL; 201 is ok; 410 and 404 are gone; 500 is neither', async (t) => {
  const keys = vapidKeys(tmp());
  for (const [status, want] of [[201, { ok: true, status: 201, gone: false }], [410, { ok: false, status: 410, gone: true }],
                                [404, { ok: false, status: 404, gone: true }], [500, { ok: false, status: 500, gone: false }], [429, { ok: false, status: 429, gone: false }]]) {
    const svc = await service(status); t.after(svc.close);
    const p = phone(`${svc.base}/wpush/xyz`);
    const msg = Buffer.from('{"title":"t"}');
    assert.deepEqual(await send(p.sub, msg, keys, { subject: 'https://mac.example.ts.net' }), want);
    const r = svc.got[0];
    assert.equal(r.url, '/wpush/xyz');
    assert.match(r.headers.authorization, /^vapid t=.+, k=.+$/);
    assert.equal(r.headers['content-encoding'], 'aes128gcm');
    assert.equal(r.headers.urgency, 'high');
    assert.ok(Number(r.headers.ttl) > 0);
    assert.deepEqual(decrypt(p.ecdh, p.auth, r.body), msg);
    // aud is the push service's origin
    const claims = JSON.parse(unb64(/t=[\w-]+\.([\w-]+)\./.exec(r.headers.authorization)[1]));
    assert.equal(claims.aud, svc.base);
  }
});

test('send() to a service that is not there is a failure, not a throw, and not gone', async () => {
  const keys = vapidKeys(tmp());
  const r = await send(phone('http://127.0.0.1:1/x').sub, Buffer.from('x'), keys, { subject: 'https://mac.example.ts.net' });
  assert.deepEqual(r, { ok: false, status: 0, gone: false });
});

test('the store keeps valid subscriptions once per endpoint, 0600, and drops by endpoint', () => {
  const dir = tmp(), store = pushStore(dir);
  assert.deepEqual(store.list(), []);
  const a = phone('https://push.example.com/a'), b = phone('https://push.example.com/b');
  assert.equal(store.add(a.sub).ok, true);
  assert.equal(store.add(a.sub).ok, true);
  assert.equal(store.add(b.sub).ok, true);
  assert.equal(store.list().length, 2);
  assert.equal(fs.statSync(path.join(dir, 'push.json')).mode & 0o777, 0o600);
  assert.deepEqual(Object.keys(store.list()[0]).sort(), ['at', 'endpoint', 'keys']);
  assert.equal(pushStore(dir).list().length, 2, 'it survives a restart');
  assert.equal(store.remove(a.sub.endpoint), true);
  assert.equal(store.remove(a.sub.endpoint), false);
  assert.deepEqual(store.list().map((s) => s.endpoint), ['https://push.example.com/b']);
});

test('the store refuses what is not a browser subscription: http, loopback, odd keys, extra fields do not get through', () => {
  const store = pushStore(tmp()), ok = phone().sub;
  for (const bad of [null, {}, { ...ok, endpoint: 'http://push.example.com/a' }, { ...ok, endpoint: 'https://localhost/a' }, { ...ok, endpoint: 'https://127.0.0.1/a' },
    { ...ok, endpoint: 'https://[::1]/a' }, { ...ok, endpoint: 'https://user:pw@push.example.com/a' }, { ...ok, endpoint: 'x'.repeat(3000) },
    { ...ok, keys: { p256dh: 'AAAA', auth: ok.keys.auth } }, { ...ok, keys: { p256dh: ok.keys.p256dh, auth: 'AA' } }]) {
    assert.equal(store.add(bad).ok, false, JSON.stringify(bad)?.slice(0, 80));
  }
  assert.deepEqual(store.list(), []);
  assert.equal(store.add({ ...ok, evil: 1 }).ok, true);
  assert.equal(store.list()[0].evil, undefined);
});

test('the store holds a few phones, not an unbounded list', () => {
  const store = pushStore(tmp());
  for (let i = 0; i < 30; i++) store.add(phone(`https://push.example.com/${i}`).sub);
  assert.equal(store.list().length, 20);
  assert.equal(store.list().at(-1).endpoint, 'https://push.example.com/29', 'the newest stay');
});

test('pushAll() sends to every subscription and drops the ones the service says are gone', async (t) => {
  const dir = tmp(), store = pushStore(dir);
  const live = await service(201), dead = await service(410), flaky = await service(503);
  t.after(live.close); t.after(dead.close); t.after(flaky.close);
  const pl = phone(`${live.base}/l`), pd = phone(`${dead.base}/d`), pf = phone(`${flaky.base}/f`);
  assert.equal(store.add(pl.sub).ok, false, 'a loopback endpoint is not accepted from a page');
  // loopback endpoints are refused by add(); seed the file as an old panel would have
  fs.writeFileSync(path.join(dir, 'push.json'), JSON.stringify([pl, pd, pf].map((p) => ({ ...p.sub, at: 1 }))));
  const payload = { title: 'Fleet · needs you', body: 'b', data: { url: '/#x' } };
  const r = await pushAll({ dir, payload, subject: 'https://mac.example.ts.net' });
  assert.deepEqual(r, { sent: 1, failed: 1, dropped: 1 });
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(dir, 'push.json'), 'utf8')).map((s) => s.endpoint).sort(), [pf.sub.endpoint, pl.sub.endpoint].sort());
  assert.deepEqual(JSON.parse(decrypt(pl.ecdh, pl.auth, live.got[0].body)), payload);
});

test('pushAll() with nobody subscribed does nothing and makes no key file', async () => {
  const dir = tmp();
  assert.deepEqual(await pushAll({ dir, payload: {}, subject: 'https://a.b.c' }), { sent: 0, failed: 0, dropped: 0 });
  assert.equal(fs.existsSync(path.join(dir, 'push-vapid.json')), false);
});

test('needsYou() is the notification a phone shows, with the hash link the page opens', () => {
  const p = needsYou('Mac', { sessionId: 'abc-123', title: 'Migrate  <ledger>', waitingFor: 'Approve the migration?' });
  assert.deepEqual(p, { title: 'Fleet · needs you', body: 'Approve the migration?\nMigrate ledger · Mac', data: { url: `/#${encodeURIComponent('Mac:abc-123')}` } });
  assert.equal(decodeURIComponent(p.data.url.slice(2)), 'Mac:abc-123');
  assert.equal(needsYou('server', { id: 'z', doing: 'Reading' }).body, 'Reading\nA session · server');
  assert.equal(needsYou('server', { id: 'z' }).body, 'It waits for you\nA session · server');
});
