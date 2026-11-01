# Fleet on the Phone Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Fleet page reachable from Ross's phone over Tailscale as an installed Home Screen app, with a one-column phone layout and push for "Needs you".

**Architecture:** The existing loopback Node server learns one public hostname (Tailscale Serve terminates HTTPS and proxies to it), serves a manifest, icons and a service worker, and the single `public/fleet.html` grows a `body.phone` mode with two sliding screens. Push is implemented on Node's `crypto` (no dependency) and fired where the Mac notification already fires.

**Tech Stack:** Node 20+ ESM, hand-written HTTP/WS server (`server.mjs`, `fleet/ws.mjs`), vanilla DOM in `public/fleet.html`, `node:test`, one vendored MIT QR encoder.

**Spec:** `docs/superpowers/specs/2026-11-01-fleet-mobile-and-documents-design.md`

## Global Constraints

- Server stays bound to `127.0.0.1` (`server.mjs:1963`); only the `Host`/`Origin` allow-lists widen.
- No npm dependencies, no build step. Vendored files go in `public/vendor/` with a LICENSE file and an entry in `VENDOR` (`server.mjs:135`).
- No new polling: every live update rides the existing WebSocket (`{"op":"fleet"}`) or a hook event.
- Repos are public: no real hostnames, tailnet names or keys in committed files or tests.
- Tests: `npm test` must stay green (614 at start). Add tests next to the existing files (`test/*.test.mjs`).
- Commit style: `feat(fleet): …` / `fix(fleet): …`, imperative, one line, body optional; end with the attribution lines in the session's system reminder.

## Review Focus

1. A request with a `Host` that is neither loopback nor the configured public host must still get 421, and an `Origin` of `http://<public>` (wrong scheme) must be refused — Task 1 tests both.
2. A browser that has the cookie from a loopback pairing is not paired on the public host (different cookie jar per origin); the pair page must render there, not an error — Task 1 test.
3. The service worker must never cache `/api/*` or the HTML page (each start mints a token the page carries) — Task 2 asserts the fetch handler ignores non-navigation requests and uses network-first for navigations.
4. On the phone, a `select()` while a terminal is attached must not call `fitShown()` on a hidden xterm (it throws on zero-size) — Task 3 guards `termView` with `!phone || pane.phoneTerm`.
5. A push subscription whose endpoint returns 404/410 must be dropped, not retried forever — Task 5 test.

---

### Task 1: Public host and secure pairing (`server.mjs`)

**Files:**
- Modify: `server.mjs:75-76` (HOSTS/ORIGINS), `server.mjs:1762` (cookie), `server.mjs:1937` (upgrade check uses the same sets, verify only)
- Modify: `fleet/fleet.mjs` `config()` (~line 80): read `public`
- Test: `test/daemon-server.test.mjs` (new tests appended; the harness `start(port, prefsDir, extraEnv)` already spawns the real server)

**Interfaces:**
- Produces: `export function publicHost(prefsPath)` in `server.mjs` → `string|null`: `process.env.ACP_PUBLIC_HOST` if it matches `/^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/i`, else `fleet.json` (beside `prefsPath`) field `public` if it matches, else `null`. `HOSTS` adds it (no port), `ORIGINS` adds `https://<it>`.
- Produces: `fleet.config().public` (same validation) for the page (Task 4 shows it in the Profile box).

- [ ] **Step 1: Write the failing tests** in `test/daemon-server.test.mjs`:

```js
test('a public host from fleet.json is accepted as Host and https Origin, nothing else is', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const get = (host, extra = {}) => fetch(`http://127.0.0.1:${port}/`, { headers: { host, ...extra }, redirect: 'manual' });
  assert.equal((await get('mac.example-tailnet.ts.net')).status, 200, 'pair page on the public host');
  assert.equal((await get('evil.example.com')).status, 421);
  const post = (origin) => fetch(`http://127.0.0.1:${port}/api/fleet`, { method: 'POST', headers: { host: 'mac.example-tailnet.ts.net', origin, 'content-type': 'application/json' }, body: '{}' });
  assert.equal((await post('http://mac.example-tailnet.ts.net')).status, 403, 'http origin on the public host is refused');
  // https origin passes the Origin check (403 here is still the missing cookie/token, so check the body says forbidden, not wrong host)
  const r = await post('https://mac.example-tailnet.ts.net'); assert.equal(r.status, 403); assert.deepEqual(await r.json(), { error: 'forbidden' });
});

test('pairing over https sets a Secure cookie', async (t) => {
  const port = await freePort(), prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const s = start(port, prefs); t.after(s.stop);
  assert.ok(await until(() => answers(port)), s.log());
  const key = fs.readFileSync(path.join(prefs, 'key'), 'utf8').trim();   // pairKey(PREFS) writes `key` beside prefs.json: confirm the name in cli.mjs pairKey and adjust
  const r = await fetch(`http://127.0.0.1:${port}/?k=${key}`, { headers: { host: 'mac.example-tailnet.ts.net', 'x-forwarded-proto': 'https' }, redirect: 'manual' });
  assert.equal(r.status, 303);
  assert.match(r.headers.get('set-cookie'), /; Secure/);
  const r2 = await fetch(`http://127.0.0.1:${port}/?k=${key}`, { redirect: 'manual' });
  assert.doesNotMatch(r2.headers.get('set-cookie'), /; Secure/, 'loopback over http keeps a non-Secure cookie');
});
```

- [ ] **Step 2: Run** `node --test test/daemon-server.test.mjs` — expected: the two new tests FAIL (421 on the public host).
- [ ] **Step 3: Implement** `publicHost(PREFS)` near line 75; build `HOSTS`/`ORIGINS` from it; in the pairing branch (line 1762) append `; Secure` when `req.headers['x-forwarded-proto'] === 'https'`. In `fleet/fleet.mjs` `config()`, add `public` with the same regex and include it in what `snapshot()` returns to the page (find where `self`/`epics` are returned and add `public`).
- [ ] **Step 4: Run** `node --test test/daemon-server.test.mjs` — expected PASS; then `npm test` — all green.
- [ ] **Step 5: Commit** `feat(fleet): one public host (Tailscale Serve) for pages, api and the socket; Secure cookie over https`.

### Task 2: Installable: manifest, icons, service worker, offline page

**Files:**
- Create: `public/manifest.webmanifest`, `public/sw.js`, `public/offline.html`, `public/icons/icon-192.png`, `public/icons/icon-512.png`, `public/icons/apple-touch-icon.png` (180)
- Modify: `server.mjs` routes block (~1783-1800): serve the four, `public/fleet.html` `<head>` only
- Test: `test/sw.test.mjs` (new), `test/daemon-server.test.mjs` (routes)

**Interfaces:**
- Produces: `GET /manifest.webmanifest` (`application/manifest+json`), `GET /sw.js` (`text/javascript`, `cache-control: no-cache`, `service-worker-allowed: /`), `GET /offline.html`, `GET /icons/<name>` allow-listed exactly like `VENDOR` (a new `ICONS` map name → content-type). All served without the pairing cookie (a manifest fetch carries none on iOS).
- Produces in `public/sw.js`: a pure `export`-free script whose logic is also loadable in Node for the test via a `globalThis.self` shim: define `function handleFetch(event)` that returns `undefined` for non-`navigate` requests and `event.respondWith(fetch(req).catch(() => caches.match('/offline.html')))` for navigations; `install` pre-caches `/offline.html` only; `push` shows `event.data.json()` as `{title, body, data:{url}}`; `notificationclick` focuses a client at `data.url` or opens it.

- [ ] **Step 1: Write the failing test** `test/sw.test.mjs`: load `public/sw.js` with `vm.runInNewContext` providing `self = { addEventListener(type, fn) { handlers[type] = fn } }`, `caches`, `fetch` stubs; assert that a fetch event with `request.mode === 'cors'` and url `/api/fleet` leaves `respondWith` uncalled; a `navigate` request calls `respondWith` and, when `fetch` rejects, resolves with the `caches.match('/offline.html')` value; `install` caches exactly `['/offline.html']`.
- [ ] **Step 2: Run** `node --test test/sw.test.mjs` — FAIL (file missing).
- [ ] **Step 3: Render icons**: `swift app/render-icon.swift app/icon.svg /tmp/fleet.iconset` then copy `icon_512x512.png` → `public/icons/icon-512.png`, `icon_256x256.png` is not needed, `icon_128x128@2x.png` (256) not needed; `icon_192`: render by resizing with `sips -z 192 192 icon_512x512.png --out public/icons/icon-192.png`; `apple-touch-icon.png` = `sips -z 180 180`. Add `public/icons/*.png` to git.
- [ ] **Step 4: Write** `public/manifest.webmanifest` (`name: "Fleet"`, `short_name: "Fleet"`, `start_url: "/"`, `scope: "/"`, `display: "standalone"`, `background_color` and `theme_color` = the page's `--paper` value from `fleet.html` `:root`, icons 192 and 512 with `purpose: "any"`), `public/sw.js` per the interface, `public/offline.html` (plain page, the fleet font, one sentence: "Your Mac is unreachable. Fleet opens again when the tailnet is back."). Add the four routes to `server.mjs` beside `/vendor/`.
- [ ] **Step 5: Edit `public/fleet.html` `<head>` only**: viewport → `width=device-width, initial-scale=1, viewport-fit=cover`; add `<link rel="manifest" href="/manifest.webmanifest">`, `<link rel="apple-touch-icon" href="/icons/apple-touch-icon.png">`, `<meta name="apple-mobile-web-app-capable" content="yes">`, `<meta name="apple-mobile-web-app-status-bar-style" content="default">`, `<meta name="theme-color" content="<paper>">`. Add one script line at the end of the inline script: `if (location.protocol === 'https:' && 'serviceWorker' in navigator) navigator.serviceWorker.register('/sw.js').catch(() => {});`
- [ ] **Step 6: Run** `node --test test/sw.test.mjs test/daemon-server.test.mjs` (add a route test: `/manifest.webmanifest` 200 with the right content-type, `/icons/nope.png` 404) — PASS; `npm test` green.
- [ ] **Step 7: Commit** `feat(fleet): installable on a phone — manifest, icons, service worker with an offline page`.

### Task 3: Phone layout in `public/fleet.html`

**Files:**
- Modify: `public/fleet.html` CSS (`@media (max-width: 720px)` block, lines ~269-279, replaced) and JS (`select`, `deselect`, `renderPane`, `renderHead`, `termView`, `renderMachines`, `prefetch`, `wired` untouched)
- No `<head>` edits (Task 2 owns them; if run in parallel, rebase).

**Interfaces:**
- Consumes: nothing new from the server.
- Produces: `const phone = matchMedia('(max-width: 720px)')` with `body.phone` kept in sync; `pane.phoneTerm` (boolean, default false) — on the phone the terminal shows only when true; `history.pushState({ key }, '', location.pathname)` in `select()` and a `popstate` handler that calls `deselect()`; a `--kb` CSS variable (px) set from `visualViewport` resize to lift `#p-foot`.

- [ ] **Step 1: Verification harness** (no unit tests exist for the page): write `tools/phone-shot.mjs` that uses Playwright **only if** `node -e "require.resolve('playwright')"` succeeds, else prints "playwright not installed; verify by hand at 390×844" and exits 0. With Playwright: open `http://127.0.0.1:<ACP_PORT>/?k=<key>` at viewport 390×844 (`isMobile: true, hasTouch: true`), screenshot `tools/out/phone-queues.png`, tap the first row, screenshot `phone-session.png`, `page.goBack()`, assert the queues are visible again. The key comes from the file `pairKey` writes beside `prefs.json`.
- [ ] **Step 2: CSS.** Replace the 720 px block: `body.phone main { width: 100%; padding: 0 16px calc(16px + env(safe-area-inset-bottom)); }`; hide `.top .tabs a:not([href="/"])`, `#how`, drag handles, group fields, keyboard-hint strip (`.keys` or whatever class line 320 uses), `#p-grip`; rows `min-height: 56px; padding: 10px 0`; `#pane { position: fixed; inset: 0; width: 100vw; transform: translateX(100%); transition: transform .18s ease-out }`, `body.has-pane #pane { transform: none }`; `#p-head` gets a back button slot; `#p-foot { padding-bottom: calc(12px + env(safe-area-inset-bottom) + var(--kb, 0px)) }`; `.write textarea, #p-reply { font-size: 16px }` (iOS zooms under 16). Fold Working/Idle/Finished by default on the phone (reuse the fold-state store with a phone-specific default).
- [ ] **Step 3: JS.** `phone` matchMedia → `document.body.classList.toggle('phone', phone.matches)` on load and `change`. In `termView(x)`: `return (!phone.matches || pane.phoneTerm) && (…existing…)`. In `renderHead`: on the phone, prepend `h('button', { class: 'act back', text: '‹ Fleet', onclick: () => history.back() })` and, when `attachable(x)`, a `Terminal`/`Chat` toggle flipping `pane.phoneTerm` then `renderPane()`. In `select(x)`: `if (phone.matches) history.pushState({ key: pane.key }, '', location.pathname)`; `addEventListener('popstate', () => { if (pane.key) deselect(); })`; `deselect()` must not call `history.back()` itself (loop). Rows: add `touchstart` → `prefetch(x)` (passive). `visualViewport?.addEventListener('resize', () => document.documentElement.style.setProperty('--kb', `${Math.max(0, innerHeight - visualViewport.height - visualViewport.offsetTop)}px`))`. Hide the prompt box behind a `+` button in the top bar on the phone (toggle a `body.compose` class). Guard `fitShown()`: return early when the terminal element is hidden or has zero width.
- [ ] **Step 4: Run** `node tools/phone-shot.mjs` against the dev server (`ACP_PORT=4799 node server.mjs` with `ACP_PREFS` pointing at a temp prefs dir) — screenshots exist; back returns to queues. Then open the desktop width in the same harness (1440×900) and screenshot to confirm nothing changed there. `npm test` green.
- [ ] **Step 5: Commit** `feat(fleet): a phone layout — queues and session as two sliding screens, chat first, terminal behind a tap`.

### Task 4: Pair a phone (QR)

**Files:**
- Create: `public/vendor/qrcode.js` (+ `public/vendor/LICENSE-qrcode`), vendored from `qrcode-generator` (MIT, UMD, works in Node and browser); add to `VENDOR`.
- Modify: `cli.mjs` (the `open` subcommand's neighbour): add `pair`; `server.mjs`: `POST /api/pair-url` in `ACTIONS` (paired + token, returns `{ url }`); `public/fleet.html` Profile box: **Pair a phone** button → fetch `/api/pair-url` → render QR SVG into a dialog.
- Test: `test/pair.test.mjs`

**Interfaces:**
- Produces: `export function pairUrl({ publicHost, port, key })` in `cli.mjs` → `https://<publicHost>/?k=<key>` when `publicHost`, else `http://127.0.0.1:<port>/?k=<key>`. `node server.mjs pair` prints the URL and a terminal QR (two rows per line with `▀▄█ `).

- [ ] **Step 1: Failing test** `test/pair.test.mjs`: `pairUrl({ publicHost: 'mac.example-tailnet.ts.net', port: 4747, key: 'a'.repeat(48) })` equals the https form; with `publicHost: null` the loopback form; and `qrcode(0,'M')` from the vendored file encodes the https URL (module count > 21).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** PASS; `npm test` green. **Step 5: Commit** `feat(fleet): pair a phone by QR — node server.mjs pair, and a button in the Profile box`.

### Task 5: Push for "Needs you" (wave 2, after Tasks 1–3 merge)

**Files:**
- Create: `fleet/push.mjs`, `test/push.test.mjs`
- Modify: `server.mjs` (`/api/push/subscribe`, `/api/push/unsubscribe`, `/api/push/key`), `fleet/fleet.mjs` (~line 379 where the Mac notification fires), `public/sw.js` (already handles `push`), `public/fleet.html` (subscribe when `matchMedia('(display-mode: standalone)')` and permission granted; read `?s=` on load to `select` that key).

**Interfaces:**
- Produces in `fleet/push.mjs`: `export function vapidKeys(dir)` (reads or creates `push-vapid.json` with P-256 `publicKey` base64url and `privateKey` PKCS8), `export function encrypt(subscription, payloadBuffer)` → `{ body, headers }` per RFC 8291 `aes128gcm`, `export function vapidHeaders(audience, subject, keys)` → `{ Authorization: 'vapid t=…, k=…' }` per RFC 8292, `export async function send(subscription, payload, keys)` → `{ ok, status, gone: boolean }` (`gone` when 404/410). Subscriptions stored in `<prefs dir>/push.json` as `{ endpoint, keys: { p256dh, auth }, at }[]`.

- [ ] **Step 1: Failing tests**: a round trip — generate a fake subscriber key pair in the test, `encrypt()` then decrypt with the test's private key following RFC 8291 §3 and assert the plaintext; `vapidHeaders()` JWT has `aud`, `exp` ≤ 24 h, `sub`, and verifies with the public key; `send()` against a local `http.createServer` returning 410 yields `gone: true`.
- [ ] **Step 2: Run** FAIL. **Step 3: Implement** on `node:crypto` only (ECDH `prime256v1`, HKDF-SHA256, `aes-128-gcm`, `createSign('SHA256')` with DER→raw r||s). **Step 4: PASS**, `npm test` green. **Step 5: Wire** server routes and the fleet.mjs trigger (send to every stored subscription; drop `gone` ones). **Step 6: Commit** `feat(fleet): push a phone when a session comes to need you`.
