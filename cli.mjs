// The command line behind `node server.mjs daemon install|remove|status | login | logout | status | orders [on|off]`: it
// installs the panel as a launchd agent, removes it, or says whether it is running; it signs this Mac in to the hub and out
// again; and it says whether the hub's website may place orders on this Mac, or turns that on or off.
// server.mjs hands any argument to `main` and exits with the code it returns.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFile } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { LABEL, plistPath, logPath, install, remove, status } from './daemon.mjs';
import { createHub } from './hub.mjs';
import { workerOf, lastCheck, safeLine } from './update.mjs';
import { describe, setOn } from './web-orders.mjs';

const USAGE = 'usage: node server.mjs daemon install [--program <path>]|remove|status | login [--link <code>] | logout | status | orders [on|off] | open';

/** Where this panel keeps its preferences; ACP_PREFS gives a second panel a folder of its own. The hub file lives beside it. */
export const prefsPath = () => process.env.ACP_PREFS || path.join(os.homedir(), 'Library', 'Application Support', 'Agent Panel', 'prefs.json');
const openHub = () => createHub({ file: path.join(path.dirname(prefsPath()), 'hub.json') });
/** The pairing key, in a file beside prefs.json that only this user can read (made on first use). A browser is given it once,
 *  in the address `node server.mjs open` opens, and keeps it as a cookie: the panel's pages and calls answer only to it. */
export function pairKey(prefs = prefsPath()) {
  const file = path.join(path.dirname(prefs), 'key');
  try { const k = fs.readFileSync(file, 'utf8').trim(); if (/^[0-9a-f]{48}$/.test(k)) return k; } catch {}
  const k = crypto.randomBytes(24).toString('hex');
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, k + '\n', { mode: 0o600 });
  return k;
}
/** The address that pairs a browser with the panel on `port`. */
export const pairUrl = (port, key = pairKey()) => `http://127.0.0.1:${port}/?k=${key}`;
const webOrdersFile = () => path.join(path.dirname(prefsPath()), 'web-orders.json');

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
/** A node path that survives `brew upgrade`: the versioned Cellar binary's link under bin/, when that link points at it. */
export function stableNode(execPath, exists = (p) => { try { return fs.realpathSync(p); } catch { return null; } }) {
  const m = /^(\/opt\/homebrew|\/usr\/local)\/Cellar\/node(?:@\d+)?\/[^/]+\/bin\/node$/.exec(execPath);
  if (!m) return execPath;
  const link = `${m[1]}/bin/node`;
  return exists(link) === exists(execPath) ? link : execPath;
}

/** Signs in with a link when one is given, else (or when the link is spent) with the device sign-in from a terminal: show the
 * code, open the page, and poll at the hub's pace until it ends. */
async function login(link) {
  const hub = openHub();
  if (hub.status().loggedIn) {
    const handle = hub.status().account?.handle;
    console.log(`Already signed in as @${handle}. This Mac stays linked to @${handle}; to switch, run \`node server.mjs logout\` and then the line again.`);
    return 0;
  }
  if (link) {
    const out = await hub.linkDeviceLogin(link);
    if (out === 'ok') {
      const s = hub.status();
      console.log(`Linked to @${s.account?.handle}. Your galaxy: ${s.url}/@${s.account?.handle}`);
      return 0;
    }
    if (out === 'error') { console.error('The hub could not be reached. Run the line again in a moment.'); return 1; }
    console.log('That link was already used or has run out; signing in the usual way.');
  }
  const r = await hub.startDeviceLogin();
  if (!r.ok) { console.error(r.error); return 1; }
  console.log(`Open ${r.verificationUri} and enter ${r.userCode}`);
  // A browser is a courtesy; without one the line above is enough. Only a web address is handed to `open`, which would
  // as gladly start an app or open a file for anything else the hub sent.
  if (/^https?:\/\//.test(String(r.verificationUri))) execFile('open', [r.verificationUri], () => {});
  for (;;) {
    const p = hub.status().pending;
    await wait((p?.interval ?? 5) * 1000);
    const outcome = await hub.pollDeviceLogin();
    if (outcome === 'ok') {
      const a = hub.status().account;
      console.log(`Signed in as @${a?.handle}. Your galaxy is at ${a?.x}, ${a?.y}.`);
      return 0;
    }
    if (outcome === 'expired') { console.error('That code ran out. Run login again for a new one.'); return 1; }
    if (outcome === 'denied') { console.error('That sign-in was refused on the hub.'); return 1; }
    if (outcome === 'error') { console.error('The sign-in could not be completed. Is the hub reachable?'); return 1; }
  }
}

/** One line for the hub: signed in, waiting for an approval, or neither. */
function hubLine() {
  const s = openHub().status();
  if (s.loggedIn) return `Signed in as @${s.account?.handle} (${s.url})`;
  if (s.pending) return `Waiting for ${s.pending.userCode} to be approved`;
  return `Not signed in (${s.url})`;
}

/** One line for the last update check, or null when none was recorded or it says nothing we know. */
function checkLine(last) {
  const what = { error: last?.error, current: 'up to date', off: 'updates are off', held: 'a release was held back',
    switched: `switched to ${String(last?.sha).slice(0, 7)}` }[last?.outcome];
  return what ? safeLine(`Last update check ${last.at}: ${what}`) : null;
}

/** Runs one command and returns the exit code: 0 done, 1 launchctl or the hub refused, 2 the command is not one of ours. */
export async function main(argv) {
  const [cmd, sub] = argv;
  if (cmd === 'login' && (argv.length === 1 || (argv.length === 3 && argv[1] === '--link'))) return login(argv[2] ?? null);
  if (cmd === 'logout' && argv.length === 1) {
    const r = await openHub().logout();
    console.log(r.ok ? 'Signed out.' : r.error);
    return r.ok ? 0 : 1;
  }
  if (cmd === 'orders' && (argv.length === 1 || (argv.length === 2 && ['on', 'off'].includes(sub)))) {
    if (sub) setOn(webOrdersFile(), sub === 'on');
    console.log(describe(webOrdersFile()));
    return 0;
  }
  if (cmd === 'status' && argv.length === 1) {
    const w = workerOf(path.dirname(fileURLToPath(import.meta.url)));
    if (w) console.log(`Worker ${w.sha.slice(0, 7)}`);
    const last = w ? checkLine(lastCheck(w.dir)) : null;
    if (last) console.log(last);
    console.log(hubLine());
    console.log(describe(webOrdersFile()));
    return main(['daemon', 'status']);
  }
  if (cmd === 'open' && argv.length === 1) {
    // Pairs the browser this opens in: the key goes once in the address, and stays as a cookie. The address is printed too,
    // for a browser on another profile or a machine that `open` cannot reach.
    const url = pairUrl(Number(process.env.ACP_PORT ?? 4747));
    console.log(url);
    execFile('open', [url], () => {});
    return 0;
  }
  if (cmd !== 'daemon' || !['install', 'remove', 'status'].includes(sub)) { console.error(USAGE); return 2; }
  const port = Number(process.env.ACP_PORT ?? 4747);
  try {
    if (sub === 'install') {
      // --program names the worker install's launcher, which the service runs instead of `node server.mjs`.
      const program = argv[2] === '--program' && argv[3] ? argv[3] : null;
      if (argv.length > (program ? 4 : 2)) { console.error(USAGE); return 2; }
      const repo = program ? path.dirname(program) : path.dirname(fileURLToPath(import.meta.url));
      // process.execPath is the resolved binary, which under Homebrew is a versioned Cellar path that a `brew upgrade`
      // deletes. install.sh passes the node on PATH as ACP_NODE, a link that keeps working across upgrades; without it, the
      // keg's own link (/opt/homebrew/bin/node, or /usr/local/bin/node) stands in for the versioned path when it is that node.
      const node = process.env.ACP_NODE || stableNode(process.execPath);
      const { label } = await install({ port, node, repo, pathEnv: process.env.PATH ?? '', program });
      console.log(`Background service ${label} installed (log: ${logPath(port)})`);
    } else if (sub === 'remove') {
      const { removed } = await remove({ port });
      console.log(removed ? 'Background service removed' : 'No background service was installed');
    } else {
      const { pid } = await status({ port });
      console.log(pid ? `Running (pid ${pid})` : fs.existsSync(plistPath(port)) ? 'Installed, not running' : 'Not installed');
    }
    return 0;
  } catch (err) {
    console.error(err.message);
    return 1;
  }
}
