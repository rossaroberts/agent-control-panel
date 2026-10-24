// World counting the tokens of the other machines and of Codex (fleet/machine.mjs tokens → world.setMachine): each
// machine's count replaces its last (never added twice), lands on this Mac's local day, and only from World's start.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createWorld, dayKey } from '../world.mjs';

const NOW = Date.UTC(2026, 9, 24, 12, 0, 0);
const world = () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'world-machines-'));
  const w = createWorld({ file: path.join(dir, 'world.json'), configDirs: [], gitPlace: () => null, run: async () => ({ err: true, stdout: '' }), now: () => NOW });
  w.open();
  return { w, file: path.join(dir, 'world.json') };
};
const slot = (ms) => Math.floor(ms / 300_000);

test('another machine\'s tokens count on the day they were used, once however often it reports', () => {
  const { w, file } = world();
  const since = w.startedAt();
  const count = { ok: true, since, slotMs: 300_000, buckets: { [slot(NOW + 60_000)]: 1000, [slot(NOW + 3_600_000)]: 500 } };
  assert.equal(w.summary().today.tokens, 0);
  assert.equal(w.setMachine('redbat', count), true);
  assert.equal(w.summary().today.tokens, 1500);
  assert.equal(w.setMachine('redbat', count), false, 'the same count again changes nothing');
  assert.equal(w.summary().today.tokens, 1500);
  assert.equal(w.setMachine('redbat', { ...count, buckets: { ...count.buckets, [slot(NOW + 7_200_000)]: 250 } }), true);
  assert.equal(w.summary().today.tokens, 1750, 'a later count replaces the earlier one');
  w.setMachine('Mac Codex', { ok: true, since, slotMs: 300_000, buckets: { [slot(NOW + 120_000)]: 40 } });
  assert.equal(w.summary().today.tokens, 1790, 'machines add up');
  assert.equal(JSON.parse(fs.readFileSync(file, 'utf8')).machines.redbat.buckets[slot(NOW + 60_000)], 1000, 'kept in world.json');
});

test('a count from before World started, or for another start, is not taken', () => {
  const { w } = world();
  const since = w.startedAt();
  assert.equal(w.setMachine('redbat', { ok: true, since: since - 1, slotMs: 300_000, buckets: { [slot(NOW)]: 10 } }), false);
  assert.equal(w.setMachine('redbat', { ok: false, error: 'no answer' }), false);
  w.setMachine('redbat', { ok: true, since, slotMs: 300_000, buckets: { [slot(NOW - 86_400_000)]: 99, [slot(NOW + 60_000)]: 7 } });
  assert.equal(w.summary().today.tokens, 7, 'what came before World started counts for nothing');
  assert.equal(dayKey(NOW + 60_000), w.summary().today.day);
});
