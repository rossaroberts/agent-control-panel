import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path'; import net from 'node:net';
import { spawn } from 'node:child_process';
import { pairUrl, main } from '../cli.mjs';
import { loadQr, qrLines } from '../fleet/qr.mjs';

const KEY = 'a'.repeat(48);

test('the pairing address is https on the public host, else the loopback', () => {
  assert.equal(pairUrl({ publicHost: 'mac.example-tailnet.ts.net', port: 4747, key: KEY }), `https://mac.example-tailnet.ts.net/?k=${KEY}`);
  assert.equal(pairUrl({ publicHost: null, port: 4747, key: KEY }), `http://127.0.0.1:4747/?k=${KEY}`);
});

test('the vendored QR encoder runs in Node and encodes the pairing address', () => {
  const qrcode = loadQr();
  const qr = qrcode(0, 'M');
  qr.addData(pairUrl({ publicHost: 'mac.example-tailnet.ts.net', port: 4747, key: KEY }));
  qr.make();
  assert.ok(qr.getModuleCount() > 21, `modules ${qr.getModuleCount()}`);
  assert.equal(typeof qr.isDark(0, 0), 'boolean');
});

test('the vendored QR encoder ships with its MIT licence, and the server lists it', () => {
  const lic = fs.readFileSync(new URL('../public/vendor/LICENSE-qrcode', import.meta.url), 'utf8');
  assert.match(lic, /MIT License/);
  assert.match(lic, /Kazuhiko Arase/);
  assert.match(fs.readFileSync(new URL('../server.mjs', import.meta.url), 'utf8'), /'qrcode\.js': 'text\/javascript/);
});

test('the terminal drawing packs two module rows into one line and keeps a quiet zone', () => {
  const qrcode = loadQr();
  const qr = qrcode(0, 'M');
  qr.addData('https://mac.example-tailnet.ts.net/');
  qr.make();
  const n = qr.getModuleCount();
  const lines = qrLines(qr);
  assert.equal(lines.length, Math.ceil((n + 4) / 2));
  for (const l of lines) { assert.equal([...l].length, n + 4); assert.match(l, /^[▀▄█ ]+$/u); }
  assert.match(lines[0], /^ +$/, 'the first line is quiet zone');
  // the top-left finder pattern starts at module (0,0), which is dark: row 2 of the drawing is (0,*) over (1,*)
  assert.equal([...lines[1]][2], '█');
});

async function free() { return new Promise((r) => { const s = net.createServer().listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => r(port)); }); }); }

test('node server.mjs pair prints the address and a QR', async () => {
  const prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  fs.writeFileSync(path.join(prefs, 'fleet.json'), JSON.stringify({ public: 'mac.example-tailnet.ts.net' }));
  const out = await new Promise((resolve) => {
    const c = spawn(process.execPath, [new URL('../server.mjs', import.meta.url).pathname, 'pair'], { env: { ...process.env, ACP_PREFS: path.join(prefs, 'prefs.json') } });
    let o = ''; c.stdout.on('data', (d) => { o += d; }); c.stderr.on('data', (d) => { o += d; }); c.on('exit', () => resolve(o));
  });
  const key = fs.readFileSync(path.join(prefs, 'key'), 'utf8').trim();
  assert.ok(out.includes(`https://mac.example-tailnet.ts.net/?k=${key}`), out);
  assert.match(out, /[▀▄█]/u);
});

test('pair without a public host says how to set one and still prints the loopback address', async () => {
  const prefs = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const port = await free();
  const out = await new Promise((resolve) => {
    const c = spawn(process.execPath, [new URL('../server.mjs', import.meta.url).pathname, 'pair'], { env: { ...process.env, ACP_PREFS: path.join(prefs, 'prefs.json'), ACP_PORT: String(port), ACP_PUBLIC_HOST: '' } });
    let o = ''; c.stdout.on('data', (d) => { o += d; }); c.stderr.on('data', (d) => { o += d; }); c.on('exit', () => resolve(o));
  });
  assert.match(out, /"public"/);
  assert.match(out, new RegExp(`http://127\\.0\\.0\\.1:${port}/\\?k=[0-9a-f]{48}`));
});
