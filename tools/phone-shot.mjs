// Looks at the Fleet page the way a phone does (390x844, touch) and the way a desktop does (1440x900), writes
// screenshots to tools/out/, and checks the phone's two screens: queues -> tap a row -> session -> back gesture -> queues.
//
//   ACP_PORT=4799 ACP_PREFS=/tmp/acp-phone/prefs.json node server.mjs &
//   NODE_PATH=/opt/homebrew/lib/node_modules ACP_PORT=4799 ACP_PREFS=/tmp/acp-phone/prefs.json node tools/phone-shot.mjs
//
// Read-only: it opens a session to look at it and never types into one. Playwright is not a dependency of this repo; where
// it is not installed (exit 2) or fails (exit 1) the pictures are still taken by headless Chrome, but the run is not a pass.
// (ESM ignores NODE_PATH, hence createRequire.)
//
//   --doc   the Documents pane. It needs a session that has written documents, so it builds one: a fixture world in
//           /tmp/acp-docpane (ACP_DOC_DIR moves it) with its own HOME, a stand-in `claude` that lists two sessions, a
//           brief.md, a mock.html and the fleet-state/<session>.docs.jsonl the hook would have kept, and starts its own
//           panel on it (ACP_PORT, default 4798). Nothing of yours is read or written, and the panel on :4747 is not touched.
//           It then runs the phone and desktop checks above against the same fixture. Pictures: tools/out/doc-*.png.
//   NODE_PATH=/opt/homebrew/lib/node_modules node tools/phone-shot.mjs --doc
import { createRequire } from 'node:module';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
import { spawn, spawnSync } from 'node:child_process';
let pw = null;
try { pw = require('playwright'); } catch {}

const here = path.dirname(fileURLToPath(import.meta.url)), out = path.join(here, 'out'), repo = path.dirname(here);
fs.mkdirSync(out, { recursive: true });
const docMode = process.argv.includes('--doc');
const fx = process.env.ACP_DOC_DIR ?? '/tmp/acp-docpane'; // the --doc fixture world
const port = process.env.ACP_PORT ?? (docMode ? '4798' : '4747');
const prefs = docMode ? path.join(fx, 'prefs.json') : process.env.ACP_PREFS ?? path.join(process.env.HOME, '.config/agent-control-panel/prefs.json');
if (docMode && port === '4747') { console.error('--doc starts a panel of its own and will not use 4747, the live one: run it with ACP_PORT=4798 (or any free port)'); process.exit(2); }
let url = null; // the page's address with its pairing key: known once the panel is up (--doc starts its own)
const setUrl = () => { url = `http://127.0.0.1:${port}/?k=${fs.readFileSync(path.join(path.dirname(prefs), 'key'), 'utf8').trim()}`; };
if (!docMode) setUrl();
let failed = 0;
const skipped = []; // a check that could not run for want of fixtures is not a pass
const check = (ok, what) => { console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`); if (!ok) failed++; };
const rowsSel = '#machines .s, #machines .c';
/** The browser Playwright expects, else the newest one in its cache (the global Playwright and the cache drift apart). */
async function launch(type) {
  try { return await pw[type].launch(); } catch (e) {
    const cache = path.join(process.env.HOME, 'Library/Caches/ms-playwright');
    const [dir, exe] = type === 'webkit' ? ['webkit-', 'pw_run.sh'] : ['chromium_headless_shell-', 'chrome-headless-shell-mac-arm64/chrome-headless-shell'];
    const found = (fs.existsSync(cache) ? fs.readdirSync(cache) : []).filter((d) => d.startsWith(dir)).sort().reverse().map((d) => path.join(cache, d, exe)).find((f) => fs.existsSync(f));
    if (!found) throw e;
    console.log(`note ${type}: using the cached ${path.relative(cache, found)}`);
    return pw[type].launch({ executablePath: found });
  }
}
const settle = (ms = 400) => new Promise((r) => setTimeout(r, ms));

async function phone() {
  const browser = await launch('webkit');
  const ctx = await browser.newContext({ ...pw.devices['iPhone 14'], viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(url);
  await page.waitForSelector(rowsSel, { timeout: 30_000 });
  await settle(600);
  check(await page.evaluate(() => document.body.classList.contains('phone')), 'body.phone is set at 390 px');
  check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'the queues do not scroll sideways');
  const paneBox = () => page.evaluate(() => { const r = document.getElementById('pane').getBoundingClientRect(); return { x: r.x, w: r.width, vis: getComputedStyle(document.getElementById('pane')).visibility }; });
  const off = await paneBox();
  check(off.x >= 389 || off.vis === 'hidden', `the session screen waits off to the right (x=${off.x})`);
  const tall = await page.$$eval(rowsSel, (els) => els.filter((e) => e.offsetParent).map((e) => e.getBoundingClientRect().height));
  check(tall.length > 0 && Math.min(...tall) >= 56, `every row is at least 56 px (min ${Math.min(...tall)})`);
  check(await page.evaluate(() => parseFloat(getComputedStyle(document.getElementById('prompt')).fontSize)) >= 16, 'the prompt box is at least 16 px (no iOS zoom)');
  await page.screenshot({ path: path.join(out, 'phone-queues.png') });

  // The + opens the prompt as a sheet; back closes it.
  await page.tap('#compose');
  await settle(450);
  check(await page.evaluate(() => { const r = document.querySelector('.write').getBoundingClientRect(); return r.y === 0 && r.width === 390; }), 'the + brings the prompt up as a sheet');
  await page.screenshot({ path: path.join(out, 'phone-compose.png') });
  await page.goBack();
  await settle(450);
  check(await page.evaluate(() => !document.body.classList.contains('compose') && document.querySelector('.write').getBoundingClientRect().y >= 844), 'back closes the sheet');
  // A folded section opens on a tap (and stays as it was left: put it back).
  const fold = await page.$('.fold-sec[aria-expanded="false"]');
  if (fold) { await fold.tap(); await settle(300); await page.screenshot({ path: path.join(out, 'phone-section-open.png') }); await (await page.$('.fold-sec[aria-expanded="true"]')).tap(); await settle(300); }

  // The first live row that is not a terminal-only one, else the first.
  const row = (await page.$('#machines .s')) ?? (await page.$(rowsSel));
  const rowKey = await row.getAttribute('data-key');
  await row.tap();
  await page.waitForFunction(() => document.body.classList.contains('has-pane'));
  await settle(450); // the slide is 180 ms
  const on = await paneBox();
  check(on.x === 0 && on.w === 390, `the session fills the screen (x=${on.x}, w=${on.w})`);
  check(await page.evaluate(() => Boolean(history.state?.fleetPane)), 'opening a session is a history step of its own');
  check(await page.evaluate(() => getComputedStyle(document.getElementById('p-reply')).fontSize) === '16px' || await page.evaluate(() => document.getElementById('p-foot').hidden), 'the reply box is 16 px (or absent for a session that takes no messages)');
  check(await page.evaluate(() => document.getElementById('p-term').hidden), 'the terminal is not the default view');
  check(await page.evaluate(() => !!document.querySelector('#p-head .p-back')), 'the header has a back control');
  await page.screenshot({ path: path.join(out, 'phone-session.png') });

  // The terminal toggle, only where there is one.
  // Off unless ACP_TERM=1: opening a terminal attaches to the real session (and sizes it to a phone).
  const toggle = process.env.ACP_TERM === '1' ? await page.$('#p-head .term-toggle') : null;
  if (toggle) {
    await toggle.tap(); await settle(3500);
    check(await page.evaluate(() => !document.getElementById('p-term').hidden), 'the Terminal toggle shows the terminal');
    await page.screenshot({ path: path.join(out, 'phone-terminal.png') });
    await (await page.$('#p-head .term-toggle')).tap(); await settle(300);
    check(await page.evaluate(() => document.getElementById('p-term').hidden), 'and Chat brings the conversation back');
  } else console.log('note Terminal toggle not exercised (ACP_TERM=1 opts in; it attaches to the real session)');

  // The back gesture.
  await page.goBack();
  await settle(450);
  check(await page.evaluate(() => !document.body.classList.contains('has-pane') && !history.state?.fleetPane), 'back returns to the queues');
  const gone = await paneBox();
  check(gone.x >= 389 || gone.vis === 'hidden', 'and the session slides away');

  // A reload on an open session leaves no dead history step: boot clears it, and one back goes to the queues.
  await (await page.$(`#machines [data-key="${rowKey}"]`)).tap();
  await page.waitForFunction(() => document.body.classList.contains('has-pane'));
  await settle(300);
  await page.reload();
  await page.waitForSelector(rowsSel, { timeout: 30_000 });
  await settle(600);
  check(await page.evaluate(() => history.state === null), 'after a reload the page boots with no stale session step in the history');
  await page.goBack().catch(() => {});
  await settle(500);
  check(await page.evaluate(() => !document.body.classList.contains('has-pane') && history.state === null), 'and a reload then back leaves the queues with history.state null');

  // Open A, then a link to B (#<key>): one back goes to the queues, nothing dead left behind.
  for (const f of await page.$$('.fold-sec[aria-expanded="false"]')) { await f.tap().catch(() => {}); await settle(150); } // folded sections hold the other rows
  const rowKeys = await page.$$eval('#machines .s, #machines .c', (els) => els.map((e) => e.dataset.key));
  if (rowKeys.length >= 2) {
    await (await page.$(`#machines [data-key="${rowKeys[0]}"]`)).tap();
    await page.waitForFunction(() => document.body.classList.contains('has-pane'));
    await settle(300);
    await page.evaluate((k) => { location.hash = encodeURIComponent(k); }, rowKeys[1]);
    await page.waitForFunction((k) => document.querySelector('#p-head .p-title') && history.state?.fleetPane === k, rowKeys[1]);
    await settle(300);
    await page.goBack();
    await settle(900); // the page takes any dead step off a tick after
    check(await page.evaluate(() => !document.body.classList.contains('has-pane') && history.state === null), 'open A, a link to B, one back: the queues, history.state null');
    // Nothing dead behind it: the next back leaves the page altogether (to the blank page the browser started on).
    await page.goBack().catch(() => {}); await settle(500);
    check(page.url() === 'about:blank', `and the next back leaves the page (${page.url().replace(/\?k=.*/, '')})`);
    await page.goForward().catch(() => {}); await settle(800);
    await page.waitForSelector(rowsSel, { timeout: 30_000 });
  } else skipped.push('open A, a link to B, back (needs two sessions listed)');

  // A deep link cut short (`/#mac%3`) must not leave the hash set: with it set, back would stop closing the session.
  const bad = await ctx.newPage();
  await bad.goto(`http://127.0.0.1:${port}/#mac%3`);
  await bad.waitForSelector(rowsSel, { timeout: 30_000 });
  await settle(600);
  check(await bad.evaluate(() => location.hash === ''), 'a truncated deep link (#mac%3) is cleared');
  await bad.tap('#machines .s');
  await bad.waitForFunction(() => document.body.classList.contains('has-pane'));
  await settle(300);
  await bad.goBack();
  await settle(500);
  check(await bad.evaluate(() => !document.body.classList.contains('has-pane')), 'and with it, back still closes a session');
  await bad.close();

  // A session opened at desktop width and then narrowed to a phone (rotating, resizing) still closes on back.
  const rz = await ctx.newPage();
  await rz.setViewportSize({ width: 1000, height: 800 });
  await rz.goto(`http://127.0.0.1:${port}/`);
  await rz.waitForSelector(rowsSel, { timeout: 30_000 });
  await settle(600);
  await rz.click('#machines .s');
  await rz.waitForFunction(() => document.body.classList.contains('has-pane'));
  await rz.setViewportSize({ width: 390, height: 844 });
  await settle(600);
  check(await rz.evaluate(() => document.body.classList.contains('phone') && Boolean(history.state?.fleetPane)), 'narrowed to a phone with a session open: it gets a history step');
  await rz.goBack().catch(() => {});
  await settle(600);
  check(rz.url().startsWith(`http://127.0.0.1:${port}/`) && await rz.evaluate(() => !document.body.classList.contains('has-pane')), 'and back closes it instead of leaving Fleet');
  await rz.close();

  // Opening, then closing with the on-screen back button, leaves no stale entry behind.
  await (await page.$(`#machines [data-key="${rowKey}"]`)).tap();
  await page.waitForFunction(() => document.body.classList.contains('has-pane'));
  await settle(300);
  await page.tap('#p-head .p-back');
  await settle(450);
  check(await page.evaluate(() => !document.body.classList.contains('has-pane')), 'the on-screen back button also returns');

  check(errors.length === 0, `no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
  await browser.close();
}

async function desktop() {
  const browser = await launch('chromium');
  const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(url);
  await page.waitForSelector(rowsSel, { timeout: 30_000 });
  await settle(600);
  check(await page.evaluate(() => !document.body.classList.contains('phone')), 'desktop: no body.phone at 1440 px');
  await page.screenshot({ path: path.join(out, 'desktop-queues.png') });
  await (await page.$('#machines .s')).click();
  await page.waitForFunction(() => document.body.classList.contains('has-pane'));
  await settle(500);
  const box = await page.evaluate(() => { const r = document.getElementById('pane').getBoundingClientRect(); return { x: r.x, w: r.width }; });
  check(Math.abs(box.w - 460) < 2 && Math.abs(box.x - (1440 - 460)) < 2, `desktop: the pane is the usual 460 px at the right edge (x=${box.x}, w=${box.w})`);
  check(await page.evaluate(() => !document.querySelector('#p-head .p-back')), 'desktop: no phone back control');
  await page.screenshot({ path: path.join(out, 'desktop-session.png') });
  await page.keyboard.press('Escape');
  await settle(300);
  check(await page.evaluate(() => !document.body.classList.contains('has-pane')), 'desktop: Esc closes the pane');
  check(errors.length === 0, `desktop: no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
  await browser.close();
}

// ---------------------------------------------------------------- --doc: a fixture world, its panel, and the Documents pane in it
const SID = ['fe11d0c5-0000-4000-8000-000000000001', 'fe11d0c5-0000-4000-8000-000000000002'];
const WORK = path.join(fx, 'work');
const rowOf = (sid) => `#machines [data-key$=":${sid}"]`;
const LONG = Array.from({ length: 160 }, (_, i) => `${i % 12 === 0 ? `\n## Part ${i / 12 + 1}\n\n` : ''}Line ${i + 1} of a long document, long enough to need the scroll bar.\n`).join('\n');
const BRIEF = `# Brief

A one-page brief a session wrote, shown by the panel.

## What it shows

- Markdown, through the vendored \`marked\`
- Raw HTML as text: <img src=x onerror="document.title='pwned'">
- A [link](https://example.com/) and a [bad one](javascript:alert(1))

\`\`\`
a_very_long_line_with_no_break_to_see_that_a_code_block_scrolls_sideways_and_never_widens_the_page_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
\`\`\`

| Column | Total |
|---|---|
| a | 0 |
`;
// A mockup that tries to read the panel: in the sandbox that throws, and it writes "blocked" over itself.
const MOCK = '<!doctype html><title>Mock</title><body style="font:16px sans-serif;margin:24px">open<script>try{parent.document}catch(e){document.body.textContent=\'blocked\'}</script>';

/** The world the panel runs on: its own HOME (so `~/.claude` is ours), a stand-in `claude` that lists two sessions, one
 *  of which wrote four documents, and the files themselves. Only `home` and `work` under the fixture folder are ever cleared. */
function buildFixture() {
  const marker = path.join(fx, '.acp-docpane-fixture');
  if (fs.existsSync(fx) && fs.readdirSync(fx).length && !fs.existsSync(marker)) throw new Error(`${fx} exists and is not this tool's fixture: set ACP_DOC_DIR to another folder`);
  fs.mkdirSync(fx, { recursive: true }); fs.writeFileSync(marker, '');
  for (const d of ['home', 'work']) fs.rmSync(path.join(fx, d), { recursive: true, force: true });
  const home = path.join(fx, 'home'), claude = path.join(home, '.claude'), now = Date.now();
  for (const d of [WORK, path.join(claude, 'fleet-state'), path.join(claude, 'projects', WORK.replace(/[^A-Za-z0-9]/g, '-')), path.join(home, '.local', 'bin')]) fs.mkdirSync(d, { recursive: true });
  const files = { 'brief.md': BRIEF, 'mock.html': MOCK, 'long.md': LONG, 'notes.txt': 'plain notes\nline two\n' };
  for (const [n, text] of Object.entries(files)) fs.writeFileSync(path.join(WORK, n), text);
  const age = { 'brief.md': 60, 'mock.html': 120, 'long.md': 180, 'notes.txt': 300 }; // seconds ago: newest first is brief.md
  fs.writeFileSync(path.join(claude, 'fleet-state', `${SID[0]}.docs.jsonl`),
    Object.entries(age).map(([n, s]) => JSON.stringify({ p: path.join(WORK, n), t: 'Write', at: now - s * 1000 })).join('\n') + '\n');
  fs.writeFileSync(path.join(claude, 'settings.json'), '{}');
  const ts = (s) => new Date(now - s * 1000).toISOString();
  const say = (s, text) => JSON.stringify({ type: 'assistant', timestamp: ts(s), message: { role: 'assistant', model: 'claude-opus-4-5', stop_reason: 'end_turn', usage: { input_tokens: 1200, output_tokens: 300 }, content: [{ type: 'text', text }] } });
  const ask = (s, text) => JSON.stringify({ type: 'user', timestamp: ts(s), message: { role: 'user', content: text } });
  for (const [i, lines] of [[0, [ask(400, 'Write a one-page brief for the Documents pane, and a mockup of it.'), say(70, 'Both are written: the brief is brief.md and the mockup is mock.html.')]],
                            [1, [ask(300, 'Look at the build.'), say(200, 'The build is green.')]]]) {
    fs.writeFileSync(path.join(claude, 'projects', WORK.replace(/[^A-Za-z0-9]/g, '-'), `${SID[i]}.jsonl`), lines.join('\n') + '\n');
  }
  // What `claude agents --json [--all]` and `claude auth status --json` print; nothing else the panel asks is answered.
  const roster = [{ sessionId: SID[0], name: 'docs fixture', cwd: WORK, pid: process.pid, startedAt: now - 500_000, status: 'idle' },
                  { sessionId: SID[1], name: 'build fixture', cwd: WORK, pid: process.pid, startedAt: now - 400_000, status: 'waiting' }]; // waiting: "needs you", the one section the phone always has open
  fs.writeFileSync(path.join(fx, 'roster.json'), JSON.stringify(roster));
  const bin = path.join(home, '.local', 'bin', 'claude');
  fs.writeFileSync(bin, `#!/bin/sh\ncase "$1" in\n  agents) cat '${path.join(fx, 'roster.json')}' ;;\n  auth) echo '{"loggedIn":true,"email":"fixture@example.test","subscriptionType":"max"}' ;;\n  *) exit 1 ;;\nesac\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(fx, 'fleet.json'), JSON.stringify({ self: 'fixture', notify: false }));
  fs.writeFileSync(path.join(fx, 'key'), 'd0c5'.repeat(12) + '\n'); // 48 hex: the pairing key the panel will take
  return home;
}
/** Runs `fn` with a panel on the fixture, and stops the panel after. */
async function withFixturePanel(fn) {
  const home = buildFixture();
  const log = fs.openSync(path.join(out, 'doc-server.log'), 'w');
  // Nothing of the shell's ACP_* (a daemon's ACP_DAEMON, ACP_PORT) reaches it: it is an ordinary panel on its own port.
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('ACP_')));
  const server = spawn(process.execPath, ['server.mjs'], { cwd: repo, stdio: ['ignore', log, log], env: { ...env, HOME: home, ACP_PORT: port, ACP_PREFS: prefs, FLEET_PROC_SCAN: '0' } });
  let died = null;
  server.once('exit', (c) => { died = c; });
  const stop = () => { try { server.kill('SIGTERM'); } catch {} };
  process.once('exit', stop);
  try {
    for (let i = 0; i < 100 && died === null; i++) { if (await fetch(`http://127.0.0.1:${port}/`).then(() => true, () => false)) break; await settle(200); }
    if (died !== null) throw new Error(`the fixture panel exited (${died}); see tools/out/doc-server.log`);
    setUrl();
    await fn();
  } finally { stop(); }
}

const box = (page, sel) => page.evaluate((s) => { const el = document.querySelector(s); if (!el) return null; const b = el.getBoundingClientRect(); return { x: Math.round(b.x), y: Math.round(b.y), r: Math.round(b.right), w: Math.round(b.width), h: Math.round(b.height), shown: getComputedStyle(el).visibility !== 'hidden' && getComputedStyle(el).display !== 'none' }; }, sel);
const stripNames = (page) => page.$$eval('#p-head .docs .doc-row', (els) => els.map((e) => e.dataset.name));
/** The frame of the mockup on show, once it has run its script. */
async function mockText(page) {
  const el = await page.waitForSelector('#doc-body iframe.mock', { timeout: 10_000 });
  const frame = await el.contentFrame();
  const text = await frame.waitForFunction(() => (document.readyState === 'complete' ? document.body.textContent : false), null, { timeout: 5000 }).then((h) => h.jsonValue()).catch(() => null);
  return { el, text };
}

async function docsDesktop() {
  const browser = await launch('chromium');
  const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(url);
  await page.waitForSelector(rowOf(SID[0]), { timeout: 30_000 });
  await settle(600);
  await page.click(rowOf(SID[0]));
  await page.waitForFunction(() => document.body.classList.contains('has-pane'));
  await page.waitForSelector('#p-head .docs .doc-row', { timeout: 10_000 }).catch(() => {});
  await settle(400);
  const names = await stripNames(page);
  check(JSON.stringify(names) === JSON.stringify(['brief.md', 'mock.html', 'long.md', 'notes.txt']), `desktop: the pane head lists the session's documents, newest first (${names.join(', ') || 'none'})`);
  check(await page.evaluate(() => !document.body.classList.contains('has-doc') && getComputedStyle(document.getElementById('doc')).display === 'none'), 'desktop: no document pane until one is opened');

  // The mockup: it runs its own script, which cannot read the panel.
  await page.click('#p-head .doc-row[data-name="mock.html"]');
  const mock = await mockText(page);
  check(mock.text === 'blocked', `desktop: a mockup that reads parent.document is blocked: its body reads "blocked" (${JSON.stringify(mock.text)})`);
  check(await mock.el.getAttribute('sandbox') === 'allow-scripts', 'desktop: the iframe is sandbox="allow-scripts" and nothing more (no allow-same-origin)');
  const mb = await box(page, '#doc-body iframe.mock'), db = await box(page, '#doc-body');
  check(mb && db && mb.w >= db.w - 1 && mb.h >= db.h - 1, `desktop: the mockup fills the pane (${mb?.w}x${mb?.h} in ${db?.w}x${db?.h})`);
  await page.screenshot({ path: path.join(out, 'doc-desktop-mock.png') });

  // Three columns: the list, the session, the document.
  const geo = await page.evaluate(() => { const r = (s) => { const b = document.querySelector(s).getBoundingClientRect(); return { x: Math.round(b.x), r: Math.round(b.right), w: Math.round(b.width) }; }; return { doc: r('#doc'), pane: r('#pane'), main: r('main') }; });
  check(geo.doc.r === 1440 && Math.abs(geo.doc.w - 520) <= 1 && geo.pane.w === 460 && Math.abs(geo.pane.r - geo.doc.x) <= 1 && geo.main.r <= geo.pane.x,
    `desktop: three columns, the document at the right edge (list to ${geo.main.r}, session ${geo.pane.x}-${geo.pane.r}, document ${geo.doc.x}-${geo.doc.r})`);

  // Markdown, made safe.
  await page.click('#p-head .doc-row[data-name="brief.md"]');
  await page.waitForSelector('#doc-body h1', { timeout: 10_000 }).catch(() => {});
  await settle(300);
  check(await page.evaluate(() => document.querySelector('#doc-body h1')?.textContent === 'Brief'), 'desktop: brief.md renders an <h1>');
  check(await page.evaluate(() => document.querySelectorAll('#doc-body img').length === 0 && document.title !== 'pwned' && document.getElementById('doc-body').textContent.includes('<img src=x')), 'desktop: raw HTML in a document is shown as text and never runs');
  check(await page.evaluate(() => document.querySelectorAll('#doc-body a[href^="javascript"]').length === 0 && document.querySelector('#doc-body a[href^="https://example.com"]')?.target === '_blank'), 'desktop: a javascript: link is only its words; an https one opens apart');
  check(await page.evaluate(() => document.querySelectorAll('#doc-body table').length === 1), 'desktop: a table renders');
  check(await page.evaluate(() => { const b = document.getElementById('doc-body'); return b.scrollWidth <= b.clientWidth + 1 && document.querySelector('#doc-body pre').scrollWidth > document.querySelector('#doc-body pre').clientWidth; }), 'desktop: a long code line scrolls inside its block and does not widen the pane');
  await page.screenshot({ path: path.join(out, 'doc-desktop.png') });

  // The raw text behind it.
  await page.click('#doc-head .doc-source');
  check(await page.evaluate(() => document.querySelector('#doc-body pre')?.textContent.startsWith('# Brief') && !document.querySelector('#doc-body h1') && document.querySelector('#doc-head .doc-source').getAttribute('aria-pressed') === 'true'), 'desktop: Source shows the text as written');
  await page.click('#doc-head .doc-source');
  check(await page.evaluate(() => Boolean(document.querySelector('#doc-body h1'))), 'desktop: and Source again brings the rendering back');
  await page.click('#p-head .doc-row[data-name="notes.txt"]');
  await page.waitForFunction(() => document.querySelector('#doc-body pre')?.textContent.includes('plain notes'), null, { timeout: 5000 }).catch(() => {});
  check(await page.evaluate(() => document.querySelector('#doc-body pre')?.textContent.includes('line two')), 'desktop: a .txt shows as text');

  // The width is yours: dragged, kept, bounded, and back to the usual on a double-click.
  const grip = async (to) => { const g = await box(page, '#doc-grip'); await page.mouse.move(g.x + g.w / 2, 450); await page.mouse.down(); await page.mouse.move(to, 450, { steps: 6 }); await page.mouse.up(); await settle(150); };
  const docW = async () => (await box(page, '#doc')).w;
  await grip(820);
  check(Math.abs(await docW() - 620) <= 1 && await page.evaluate(() => localStorage.getItem('fleet.doc.width')) === '620', `desktop: the grip widens the document and keeps the width (${await docW()}, fleet.doc.width=${await page.evaluate(() => localStorage.getItem('fleet.doc.width'))})`);
  await page.reload();
  await page.waitForSelector(rowOf(SID[0]), { timeout: 30_000 });
  await settle(600);
  await page.click(rowOf(SID[0]));
  await page.waitForSelector('#p-head .doc-row', { timeout: 10_000 }).catch(() => {});
  await page.click('#p-head .doc-row[data-name="brief.md"]');
  await settle(300);
  check(Math.abs(await docW() - 620) <= 1, `desktop: after a reload the document opens at that width (${await docW()})`);
  await grip(1430);
  check(await docW() === 360, `desktop: and no narrower than 360 (${await docW()})`);
  await grip(10);
  check(await docW() <= 864, `desktop: nor wider than 60% of the window (${await docW()})`);
  await page.dblclick('#doc-grip');
  check(Math.abs(await docW() - 520) <= 1, `desktop: a double-click on the grip is the usual 520 (${await docW()})`);

  // The session's own grip and fold work with the document beside it: its right edge is the document's left.
  const pg = await box(page, '#p-grip');
  await page.mouse.move(pg.x + pg.w / 2, 450); await page.mouse.down(); await page.mouse.move(pg.x + pg.w / 2 - 50, 450, { steps: 5 }); await page.mouse.up(); await settle(150);
  const wid = await box(page, '#pane'), dd = await box(page, '#doc');
  check(Math.abs(wid.w - 510) <= 1 && wid.r === dd.x, `desktop: the session's grip widens it by what you drag, the document beside it (${wid.w} wide, ends at ${wid.r}, document at ${dd.x})`);
  await page.dblclick('#p-grip');
  const mainR = (await box(page, 'main')).r;
  await page.click('#p-head .p-fold');
  await settle(200);
  const rail = await box(page, '#pane');
  check(rail.w === 44 && rail.r === dd.x && (await box(page, 'main')).r > mainR, `desktop: folded to its strip the session stays beside the document and the list widens (${rail.w} wide, ends at ${rail.r}; list to ${(await box(page, 'main')).r} from ${mainR})`);
  await page.click('#p-rail');
  await settle(200);
  check((await box(page, '#pane')).w === 460, 'desktop: and unfolds to its width');

  // A write while it is open: the push brings the new text in, and the place you were reading stays.
  await page.click('#p-head .doc-row[data-name="long.md"]');
  await page.waitForFunction(() => document.getElementById('doc-body').textContent.includes('Line 160'), null, { timeout: 5000 }).catch(() => {});
  const was = await page.evaluate(() => { const b = document.getElementById('doc-body'); b.scrollTop = 400; return b.scrollTop; });
  const wrote = Date.now();
  fs.writeFileSync(path.join(WORK, 'long.md'), LONG + '\nREVISED LINE\n');
  fs.appendFileSync(path.join(fx, 'home', '.claude', 'fleet-state', `${SID[0]}.docs.jsonl`), JSON.stringify({ p: path.join(WORK, 'long.md'), t: 'Edit', at: wrote }) + '\n');
  const dot = await page.waitForSelector('#p-head .doc-row[data-name="long.md"] .live', { timeout: 3000 }).then(() => true, () => false);
  const came = await page.waitForFunction(() => document.getElementById('doc-body').textContent.includes('REVISED LINE'), null, { timeout: 8000 }).then(() => Date.now() - wrote, () => null);
  check(came !== null && came < 3000, `desktop: a document written while open re-renders on the push, not the 10 s poll (${came === null ? 'never' : came + ' ms'})`);
  const after = await page.evaluate(() => document.getElementById('doc-body').scrollTop);
  check(was >= 300 && Math.abs(after - was) <= 2, `desktop: and it keeps the scroll position (${was} -> ${after})`);
  check(dot, 'desktop: the strip marks a document just written with a dot');
  await settle(2600);
  check(await page.evaluate(() => !document.querySelector('#p-head .doc-row .live')), 'desktop: and the dot goes out after two seconds');
  fs.writeFileSync(path.join(WORK, 'long.md'), LONG);

  // Leaving: Esc closes the document first, the session second; the x closes the document.
  await page.keyboard.press('Escape');
  await settle(300);
  check(await page.evaluate(() => !document.body.classList.contains('has-doc') && document.body.classList.contains('has-pane')), 'desktop: Esc closes the document and leaves the session open');
  check((await box(page, '#pane')).r === 1440, 'desktop: and the session moves back to the edge');
  await page.click('#p-head .doc-row[data-name="brief.md"]');
  await settle(200);
  await page.click('#doc-head .doc-close');
  await settle(200);
  check(await page.evaluate(() => !document.body.classList.contains('has-doc')), 'desktop: the close control closes it');
  await page.click('#p-head .doc-row[data-name="brief.md"]');
  await settle(200);
  await page.keyboard.press('Escape'); await settle(150); await page.keyboard.press('Escape'); await settle(300);
  check(await page.evaluate(() => !document.body.classList.contains('has-doc') && !document.body.classList.contains('has-pane')), 'desktop: a second Esc closes the session');
  check(errors.length === 0, `desktop: no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
  await browser.close();
}

async function docsOverlay() {
  const browser = await launch('chromium');
  const page = await (await browser.newContext({ viewport: { width: 900, height: 800 } })).newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(url);
  await page.waitForSelector(rowOf(SID[0]), { timeout: 30_000 });
  await settle(600);
  await page.click(rowOf(SID[0]));
  await page.waitForSelector('#p-head .doc-row', { timeout: 10_000 }).catch(() => {});
  await settle(300);
  await page.click('#p-head .doc-row[data-name="brief.md"]');
  await page.waitForSelector('#doc-body h1', { timeout: 10_000 }).catch(() => {});
  await settle(300);
  const p = await box(page, '#pane'), d = await box(page, '#doc');
  check(p && d && d.shown && d.x === p.x && d.w === p.w && d.h === p.h, `overlay: at 900 px the document sits over the session in the same box (${d?.x},${d?.w} over ${p?.x},${p?.w})`);
  await page.screenshot({ path: path.join(out, 'doc-overlay.png') });
  await page.click('#doc-head .doc-toggle');
  await settle(250);
  check(await page.evaluate(() => getComputedStyle(document.getElementById('doc')).display === 'none' && document.body.classList.contains('has-doc')), 'overlay: Session in the document head brings the session back (the document stays open behind)');
  check(await page.evaluate(() => /^Documents/.test(document.querySelector('#p-head .doc-toggle')?.textContent ?? '')), 'overlay: and the session head offers Documents');
  await page.click('#p-head .doc-toggle');
  await settle(250);
  check(await page.evaluate(() => getComputedStyle(document.getElementById('doc')).display !== 'none' && Boolean(document.querySelector('#doc-body h1'))), 'overlay: Documents puts the document back, as it was');
  await page.setViewportSize({ width: 1300, height: 800 });
  await settle(300);
  check(await page.evaluate(() => !document.querySelector('#doc-head .doc-toggle') || getComputedStyle(document.querySelector('#doc-head .doc-toggle')).display === 'none'), 'overlay: widened to 1300 px the toggle is gone: both show');
  await page.setViewportSize({ width: 900, height: 800 });
  await settle(300);
  await page.click('#doc-head .doc-toggle');
  await settle(200);
  await page.keyboard.press('Escape');
  await settle(300);
  check(await page.evaluate(() => !document.body.classList.contains('has-pane') && !document.body.classList.contains('has-doc')), 'overlay: with the session in front of the document, Esc closes the session (and the document with it)');
  check(errors.length === 0, `overlay: no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
  await browser.close();
}

async function docsPhone() {
  const browser = await launch('webkit');
  const ctx = await browser.newContext({ ...pw.devices['iPhone 14'], viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  await page.goto(url);
  await page.waitForSelector('#machines .fold-sec', { timeout: 30_000 });
  await settle(600);
  for (const f of await page.$$('.fold-sec[aria-expanded="false"]')) { await f.tap().catch(() => {}); await settle(150); }
  const open = async () => { await page.tap(rowOf(SID[0])); await page.waitForFunction(() => document.body.classList.contains('has-pane')); await settle(350); };
  await open();
  const tap = await page.evaluate(() => Math.min(...[...document.querySelectorAll('#p-head .docs .doc-row')].map((e) => e.getBoundingClientRect().height)));
  check(tap >= 44, `phone: a document in the strip is at least 44 px to tap (${tap})`);
  await page.tap('#p-head .doc-row[data-name="brief.md"]');
  await page.waitForFunction(() => document.body.classList.contains('has-doc'));
  await page.waitForSelector('#doc-body h1', { timeout: 10_000 }).catch(() => {});
  await settle(450); // the slide is 180 ms
  const d = await box(page, '#doc');
  check(d && d.x === 0 && d.w === 390 && d.h >= 800 && d.shown, `phone: the document opens as a full-screen sheet (${d?.x},${d?.y} ${d?.w}x${d?.h})`);
  check(await page.evaluate(() => Boolean(history.state?.fleetDoc) && Boolean(history.state?.fleetPane)), 'phone: opening a document is a history step of its own, on top of the session\'s');
  check(await page.evaluate(() => Boolean(document.querySelector('#doc-head .p-back'))), 'phone: the sheet has a back chevron');
  check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth && document.getElementById('doc-body').scrollWidth <= innerWidth), 'phone: the sheet does not scroll sideways');
  check(await page.evaluate(() => parseFloat(getComputedStyle(document.getElementById('doc-body')).fontSize) >= 15), 'phone: the text is at least 15 px');
  await page.screenshot({ path: path.join(out, 'doc-phone.png') });
  await page.goBack();
  await settle(450);
  check(await page.evaluate(() => !document.body.classList.contains('has-doc') && document.body.classList.contains('has-pane') && history.state?.fleetPane && !history.state?.fleetDoc), 'phone: back closes the document and leaves the session');
  check((await box(page, '#doc')).x >= 389 || !(await box(page, '#doc')).shown, 'phone: and the sheet slides away');
  await page.goBack();
  await settle(450);
  check(await page.evaluate(() => !document.body.classList.contains('has-pane') && history.state === null), 'phone: a second back returns to the queues');

  // The same with the on-screen chevron, and a mockup in the sheet.
  await open();
  await page.tap('#p-head .doc-row[data-name="mock.html"]');
  await page.waitForFunction(() => document.body.classList.contains('has-doc'));
  await settle(450);
  const mock = await mockText(page);
  check(mock.text === 'blocked', `phone: the mockup is sandboxed here too (${JSON.stringify(mock.text)})`);
  const mb = await box(page, '#doc-body iframe.mock'), hb = await box(page, '#doc-head');
  check(mb && hb && mb.x === 0 && mb.w === 390 && mb.y >= hb.y + hb.h - 1 && mb.r === 390 && mb.h > 600, `phone: and it fills the sheet under the head (${mb?.w}x${mb?.h} from y=${mb?.y})`);
  await page.screenshot({ path: path.join(out, 'doc-phone-mock.png') });
  await page.tap('#doc-head .p-back');
  await settle(450);
  check(await page.evaluate(() => !document.body.classList.contains('has-doc') && document.body.classList.contains('has-pane') && !history.state?.fleetDoc), 'phone: the chevron closes the document too, leaving no step behind');
  await page.goBack();
  await settle(450);
  check(await page.evaluate(() => !document.body.classList.contains('has-pane') && history.state === null), 'phone: and one more back is the queues');
  check(errors.length === 0, `phone: no page errors${errors.length ? ': ' + errors.join(' | ') : ''}`);
  await browser.close();
}

/** No Playwright (or it would not run): headless Chrome alone, no install. It pairs itself through the ?k= redirect, so this
 *  takes the queues, and a session too when ACP_SESSION names one (`<machine>:<sessionId>`, opened by the page's #hash). No
 *  assertions: look at the pictures. */
function chromeShots() {
  const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
  if (!fs.existsSync(chrome)) { console.log('playwright not installed and no Google Chrome found; verify by hand at 390×844 (and 1440×900 for the desktop layout)'); return; }
  const shot = (name, hash = '') => {
    const file = path.join(out, name);
    spawnSync(chrome, ['--headless=new', '--disable-gpu', '--user-data-dir=/tmp/acp-shot', '--window-size=390,844', '--hide-scrollbars', '--virtual-time-budget=8000', `--screenshot=${file}`, `${url}${hash}`], { stdio: 'ignore' });
    console.log(`${fs.existsSync(file) ? 'wrote' : 'FAILED'} ${path.relative(process.cwd(), file)}`);
  };
  shot('phone-queues.png');
  if (process.env.ACP_SESSION) shot('phone-session.png', `#${encodeURIComponent(process.env.ACP_SESSION)}`);
}

if (!pw) {
  console.error('\n*** playwright is NOT installed: this run checked nothing. Pictures only (headless Chrome); exiting 2, which is not a pass. ***');
  chromeShots(); process.exit(2);
}
try {
  if (docMode) await withFixturePanel(async () => { await docsDesktop(); await docsOverlay(); await docsPhone(); await phone(); await desktop(); });
  else { await phone(); await desktop(); }
} catch (e) {
  console.error(`\n*** playwright failed: ${String(e.stack ?? e).split('\n').slice(0, 3).join(' | ')}\n*** pictures only (headless Chrome); exiting 1 ***`);
  chromeShots(); process.exit(1);
}
if (skipped.length) { console.error(`\n*** ${skipped.length} check(s) could not run (not enough sessions listed to set them up): ${skipped.join('; ')}. Not a pass. ***`); process.exit(1); }
console.log(failed ? `${failed} check(s) failed` : 'all checks passed; screenshots in tools/out/');
process.exit(failed ? 1 : 0);
