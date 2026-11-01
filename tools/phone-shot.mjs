// Looks at the Fleet page the way a phone does (390x844, touch) and the way a desktop does (1440x900), writes
// screenshots to tools/out/, and checks the phone's two screens: queues -> tap a row -> session -> back gesture -> queues.
//
//   ACP_PORT=4799 ACP_PREFS=/tmp/acp-phone/prefs.json node server.mjs &
//   NODE_PATH=/opt/homebrew/lib/node_modules ACP_PORT=4799 ACP_PREFS=/tmp/acp-phone/prefs.json node tools/phone-shot.mjs
//
// Read-only: it opens a session to look at it and never types into one. Playwright is not a dependency of this repo; where
// it is not installed (exit 2) or fails (exit 1) the pictures are still taken by headless Chrome, but the run is not a pass.
// (ESM ignores NODE_PATH, hence createRequire.)
import { createRequire } from 'node:module';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
import { spawnSync } from 'node:child_process';
let pw = null;
try { pw = require('playwright'); } catch {}

const here = path.dirname(fileURLToPath(import.meta.url)), out = path.join(here, 'out');
fs.mkdirSync(out, { recursive: true });
const port = process.env.ACP_PORT ?? '4747';
const prefs = process.env.ACP_PREFS ?? path.join(process.env.HOME, '.config/agent-control-panel/prefs.json');
const key = fs.readFileSync(path.join(path.dirname(prefs), 'key'), 'utf8').trim();
const url = `http://127.0.0.1:${port}/?k=${key}`;
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
try { await phone(); await desktop(); } catch (e) {
  console.error(`\n*** playwright failed: ${String(e.stack ?? e).split('\n').slice(0, 3).join(' | ')}\n*** pictures only (headless Chrome); exiting 1 ***`);
  chromeShots(); process.exit(1);
}
if (skipped.length) { console.error(`\n*** ${skipped.length} check(s) could not run (not enough sessions listed to set them up): ${skipped.join('; ')}. Not a pass. ***`); process.exit(1); }
console.log(failed ? `${failed} check(s) failed` : 'all checks passed; screenshots in tools/out/');
process.exit(failed ? 1 : 0);
