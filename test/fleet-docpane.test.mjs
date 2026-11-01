// The Documents pane lives in public/fleet.html, which a test cannot drive (tools/phone-shot.mjs --doc does, in a browser).
// What a test can hold is what must never change by accident: a mockup runs in a sandbox that cannot reach the panel, the
// page's script still parses as the classic script it is served as, and the pane's wiring is in the markup.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const HTML = fs.readFileSync(new URL('../public/fleet.html', import.meta.url), 'utf8');
const SCRIPT = HTML.slice(HTML.indexOf('<script>\nconst TOKEN') + '<script>'.length, HTML.indexOf('</script>', HTML.indexOf('<script>\nconst TOKEN')));

test('a mockup runs in sandbox="allow-scripts" and nothing more: never allow-same-origin', () => {
  // (the page's comments say it: what counts is every sandbox the page sets)
  const sandboxes = [...HTML.matchAll(/sandbox\s*[:=]\s*['"]([^'"]*)['"]/g)].map((m) => m[1]);
  assert.ok(sandboxes.length >= 1 && sandboxes.every((v) => v === 'allow-scripts'), `allow-same-origin would give a mockup the panel's origin, its token and its cookie (found: ${sandboxes.join(' | ')})`);
  const frames = [...SCRIPT.matchAll(/h\('iframe',\s*\{([^}]*)\}/g)].map((m) => m[1]);
  assert.equal(frames.length, 1, 'one place makes a document frame');
  assert.match(frames[0], /sandbox:\s*'allow-scripts'/);
  assert.match(frames[0], /srcdoc:/);
});

test('the page script parses as a classic script', () => {
  assert.ok(SCRIPT.length > 10_000);
  assert.doesNotThrow(() => new vm.Script(SCRIPT, { filename: 'fleet.html' }));
});

test('the document pane is in the markup beside the session, closed to start with', () => {
  assert.match(HTML, /<aside id="pane" hidden[^>]*>[\s\S]*<\/aside>\s*<aside id="doc" hidden[^>]*>\s*<div id="doc-grip"[^>]*><\/div>\s*<div id="doc-head"><\/div>\s*<div id="doc-body"><\/div>\s*<\/aside>/);
});

test('its markdown comes from the vendored marked and renderSafe, not from a copy', () => {
  assert.match(SCRIPT, /import\('\/context-render\.mjs'\)/);
  assert.match(SCRIPT, /\/vendor\/marked\.umd\.js/);
  assert.match(SCRIPT, /renderSafe\(window\.marked/);
});

test('the phone\'s back gesture has a step for the document on top of the session\'s', () => {
  assert.match(SCRIPT, /fleetDoc: p/);
  assert.match(SCRIPT, /pane\.doc && !history\.state\?\.fleetDoc\) \{ closeDoc\(\{ step: false \}\)/);
});
