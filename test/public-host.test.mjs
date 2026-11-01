import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs'; import os from 'node:os'; import path from 'node:path';
import { validPublic, publicHost } from '../fleet/public-host.mjs';

test('only a plain dotted hostname is a public host', () => {
  assert.equal(validPublic('Mac.Example-Tailnet.ts.net'), 'mac.example-tailnet.ts.net');
  for (const bad of ['localhost', 'a.b:4747', 'a.b/c', 'a b.c', '-a.b', 'a..b', '', null, 7, 'a.b\r\nx: y']) assert.equal(validPublic(bad), null, String(bad));
});

test('publicHost reads fleet.json beside the prefs file, and ACP_PUBLIC_HOST wins', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-'));
  const prefs = path.join(dir, 'prefs.json');
  assert.equal(publicHost(prefs, {}), null);
  fs.writeFileSync(path.join(dir, 'fleet.json'), JSON.stringify({ public: 'a.example-tailnet.ts.net' }));
  assert.equal(publicHost(prefs, {}), 'a.example-tailnet.ts.net');
  assert.equal(publicHost(prefs, { ACP_PUBLIC_HOST: 'b.example-tailnet.ts.net' }), 'b.example-tailnet.ts.net');
  assert.equal(publicHost(prefs, { ACP_PUBLIC_HOST: 'not a host' }), 'a.example-tailnet.ts.net');
  fs.writeFileSync(path.join(dir, 'fleet.json'), '{bad');
  assert.equal(publicHost(prefs, {}), null);
});
