// node --test native/bench/ratchet.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { median, ratchet } from './ratchet.mjs';

const B = () => ({ hook: { budget: 5, floor: 1 }, idle: { budget: 60, floor: 20 } });

test('median of three and of an even count', () => {
  assert.equal(median([3, 1, 2]), 2);
  assert.equal(median([4, 1, 2, 3]), 2.5);
});

test('a budget only lowers: median x 1.25 above the budget keeps the budget', () => {
  const { budgets } = ratchet(B(), { hook: 4.5, idle: 10 }); // 4.5 x 1.25 = 5.625 > 5
  assert.equal(budgets.hook.budget, 5);
  const lowered = ratchet(B(), { hook: 2, idle: 30 }).budgets;
  assert.equal(lowered.hook.budget, 2.5);
  assert.equal(lowered.idle.budget, 37.5);
});

test('never below the floor', () => {
  const { budgets } = ratchet(B(), { hook: 0.1, idle: 0 });
  assert.equal(budgets.hook.budget, 1);
  assert.equal(budgets.idle.budget, 20);
});

test('a budget that is over is left alone and reported; a missing median too', () => {
  const r = ratchet(B(), { hook: 6 });
  assert.equal(r.budgets.hook.budget, 5);
  assert.deepEqual(r.skipped, ['hook']);
  assert.equal(r.budgets.idle.budget, 60);
});

test('the argument is not mutated and floors survive', () => {
  const b = B();
  const { budgets } = ratchet(b, { hook: 1, idle: 25 });
  assert.equal(b.hook.budget, 5);
  assert.equal(budgets.idle.floor, 20);
});

test('a hold keeps a budget from ratcheting below it (P2-R6)', () => {
  const b = { idle: { budget: 50, floor: 20, hold: 50 }, hook: { budget: 5, floor: 1 } };
  const { budgets } = ratchet(b, { idle: 30, hook: 2 });
  assert.equal(budgets.idle.budget, 50);
  assert.equal(budgets.idle.hold, 50);
  assert.equal(budgets.hook.budget, 2.5);
});
