// Satellite (fleet/satellite.mjs): the pure parts on made-up GitHub answers, no network. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { checkState, summarizeChecks, isPromotion, pickDeploys, pickWorkflows, runCell, containedIn, compose, orderRows, placeAgents, guard, createSatellite, span, STALL } from '../fleet/satellite.mjs';

const REPO = 'Clinton-Health-Access-Initiative/chai-expense-tracker';
const T0 = Date.parse('2026-10-24T12:00:00Z');
const iso = (min) => new Date(T0 + min * 60_000).toISOString();

// ---------------------------------------------------------------------------------------------- checks

const run = (name, status, conclusion, min = 0) => ({ __typename: 'CheckRun', name, status, conclusion, startedAt: iso(min), completedAt: iso(min + 1), workflowName: 'gate' });
const ok = (name, min) => run(name, 'COMPLETED', 'SUCCESS', min);
const REQUIRED = ['static', 'db', 'migrations-append-only', 'definer-redefine-guard'];
const green = (min = 0) => REQUIRED.map((n) => ok(n, min));

test('a check run or a commit status becomes one of passed / failed / cancelled / skipped / running / queued', () => {
  assert.equal(checkState(run('x', 'COMPLETED', 'SUCCESS')), 'passed');
  for (const c of ['FAILURE', 'TIMED_OUT', 'STARTUP_FAILURE', 'ACTION_REQUIRED']) assert.equal(checkState(run('x', 'COMPLETED', c)), 'failed', c);
  assert.equal(checkState(run('x', 'COMPLETED', 'CANCELLED')), 'cancelled');
  for (const c of ['SKIPPED', 'NEUTRAL']) assert.equal(checkState(run('x', 'COMPLETED', c)), 'skipped', c);
  assert.equal(checkState(run('x', 'IN_PROGRESS', '')), 'running');
  for (const s of ['QUEUED', 'WAITING', 'PENDING']) assert.equal(checkState(run('x', s, '')), 'queued', s);
  assert.equal(checkState({ __typename: 'StatusContext', context: 'ci', state: 'SUCCESS' }), 'passed');
  assert.equal(checkState({ __typename: 'StatusContext', context: 'ci', state: 'PENDING' }), 'running');
  assert.equal(checkState({ __typename: 'StatusContext', context: 'ci', state: 'FAILURE' }), 'failed');
});

test('four green contexts: passed, 4 of 4; the skipped jobs around them do not count against it', () => {
  const s = summarizeChecks([...green(), run('db-shard', 'COMPLETED', 'SKIPPED'), run('battery — full e2e suite at promotion', 'COMPLETED', 'SKIPPED')]);
  assert.equal(s.state, 'passed');
  assert.equal(s.passed, 4);
  assert.deepEqual(s.failing, []);
});

test('a re-run speaks for its check: the newest of two with the same name', () => {
  // cancelled, then redone and green
  assert.equal(summarizeChecks([run('static', 'COMPLETED', 'CANCELLED', 0), ok('static', 5)]).by.static, 'passed');
  // green, then re-run and red
  assert.equal(summarizeChecks([ok('static', 0), run('static', 'COMPLETED', 'FAILURE', 5)]).by.static, 'failed');
  // the order they are listed in does not matter
  assert.equal(summarizeChecks([run('static', 'COMPLETED', 'FAILURE', 5), ok('static', 0)]).by.static, 'failed');
  // a skipped job of an earlier run does not hide the real one
  assert.equal(summarizeChecks([run('lint', 'COMPLETED', 'SKIPPED', 0), ok('lint', 3)]).by.lint, 'passed');
});

test('the failing check is named by the job that broke, not the gate context that reports it', () => {
  const s = summarizeChecks([...green().slice(1), run('static', 'COMPLETED', 'FAILURE', 3), run('quality', 'COMPLETED', 'FAILURE', 2), ok('lint', 1)]);
  assert.equal(s.state, 'failed');
  assert.deepEqual(s.failing, ['quality', 'static']);
  assert.equal(s.passed, 3);
});

test('running while anything is going; none when nothing has reported; the land job is not a test', () => {
  const going = summarizeChecks([ok('db', 0), run('static', 'IN_PROGRESS', '', 1), run('quality', 'QUEUED', '', 1)]);
  assert.equal(going.state, 'running');
  assert.deepEqual(going.running.sort(), ['quality', 'static']);
  assert.equal(going.passed, 1);
  assert.equal(summarizeChecks([]).state, 'none');
  assert.equal(summarizeChecks(undefined).state, 'none');
  const landing = summarizeChecks([...green(), run('land on green (single pipeline — #3084)', 'IN_PROGRESS', '', 2)]);
  assert.equal(landing.state, 'passed', 'landing is the Land stage, not a test still running');
  assert.equal(landing.land, 'running');
  assert.equal(summarizeChecks([run('land on green (single pipeline — #3084)', 'COMPLETED', 'SKIPPED')]).state, 'none');
});

test('a promotion PR: its battery is read apart', () => {
  const s = summarizeChecks([...green(), run('battery — full e2e suite at promotion (dispatch full-test.yml + block on verdict)', 'IN_PROGRESS', '', 1)]);
  assert.equal(s.battery, 'running');
  assert.equal(s.state, 'running');
});

// ---------------------------------------------------------------------------------------------- promotions and deploys

test('a promotion is the PR that carries staging into main', () => {
  assert.ok(isPromotion({ baseRefName: 'main', headRefName: 'release/2026-10-24-1312', title: 'promote: staging → main (2026-10-24) @ 5a5321fb3' }));
  assert.ok(isPromotion({ baseRefName: 'main', headRefName: 'staging', title: 'anything' }));
  assert.ok(isPromotion({ baseRefName: 'main', headRefName: 'x', title: 'release: 2026-10-22-0043 — the console doors' }));
  assert.ok(!isPromotion({ baseRefName: 'main', headRefName: 'fix/hot', title: 'fix(prod): a hotfix straight into main' }));
  assert.ok(!isPromotion({ baseRefName: 'staging', headRefName: 'release/x', title: 'promote: not into main' }));
});

const job = (name, status, conclusion, sha, startedMin, doneMin) => ({ name, status, conclusion, head_sha: sha, started_at: iso(startedMin), completed_at: doneMin == null ? null : iso(doneMin), html_url: `https://x/${name}/${sha}` });
test('TEST and Prod: the newest deploy that worked, and what is newer than it', () => {
  const runs = [{ id: 4, head_sha: 'd' }, { id: 3, head_sha: 'c' }, { id: 2, head_sha: 'b' }, { id: 1, head_sha: 'a' }];
  const jobs = new Map([
    [4, [job('deploy-prod', 'in_progress', null, 'd', 40), job('deploy-test', 'completed', 'skipped', 'd', 40, 40)]],   // prod going now
    [3, [job('deploy-test', 'completed', 'failure', 'c', 30, 35), job('deploy-prod', 'completed', 'skipped', 'c', 30, 30)]], // a TEST deploy that broke
    [2, [job('deploy-test', 'completed', 'success', 'b', 20, 28), job('deploy-prod', 'completed', 'skipped', 'b', 20, 20)]],
    [1, [job('deploy-prod', 'completed', 'success', 'a', 10, 18), job('deploy-test', 'completed', 'success', 'a', 0, 9)]],
  ]);
  const d = pickDeploys(runs, jobs);
  assert.equal(d.test.ok.sha, 'b');
  assert.equal(d.test.failed.sha, 'c', 'newer than the last good one');
  assert.equal(d.test.active, undefined);
  assert.equal(d.prod.ok.sha, 'a');
  assert.equal(d.prod.active.sha, 'd');
  assert.equal(d.prod.active.startedAt, T0 + 40 * 60_000);
  assert.equal(d.test.ok.at, T0 + 28 * 60_000);
  assert.ok(d.complete);
  // only the newest runs read so far: not complete yet, and nothing is invented
  const early = pickDeploys(runs.slice(0, 2), jobs);
  assert.ok(!early.complete);
  assert.equal(early.test.ok, undefined);
  // a failure older than a deploy that worked is history
  const hist = pickDeploys([{ id: 2, head_sha: 'b' }, { id: 1, head_sha: 'a' }], new Map([[2, [job('deploy-test', 'completed', 'success', 'b', 20, 28)]], [1, [job('deploy-test', 'completed', 'failure', 'a', 0, 9)]]]));
  assert.equal(hist.test.failed, undefined);
});

// ---------------------------------------------------------------------------------------------- inclusion

test('a staging commit is in a deploy when it is as old as the newest staging commit that deploy has', () => {
  const line = new Map(['c4', 'c3', 'c2', 'c1', 'c0'].map((s, i) => [s, i])); // c4 is the newest
  const rep = { m1: 'c2' };                                                   // main's commit m1 holds staging up to c2
  assert.equal(containedIn(line, rep, {}, 'c2', 'm1'), true, 'the newest one it holds');
  assert.equal(containedIn(line, rep, {}, 'c0', 'm1'), true, 'older');
  assert.equal(containedIn(line, rep, {}, 'c3', 'm1'), false, 'newer');
  assert.equal(containedIn(line, rep, {}, 'c4', 'm1'), false);
  assert.equal(containedIn(line, rep, {}, 'c3', 'c3'), true, 'a staging commit holds itself');
  assert.equal(containedIn(line, rep, {}, 'c4', 'c3'), false);
  assert.equal(containedIn(line, rep, {}, 'c0', 'unknown'), false, 'a deploy that could not be placed holds nothing');
  assert.equal(containedIn(line, rep, {}, 'c0', null), false);
  // a commit not on the line (a fix straight into main) is whatever compare said for that pair
  assert.equal(containedIn(line, rep, { 'hotfix>m1': true }, 'hotfix', 'm1'), true);
  assert.equal(containedIn(line, rep, { 'hotfix>m1': false }, 'hotfix', 'm1'), false);
  assert.equal(containedIn(line, rep, {}, 'hotfix', 'm1'), false, 'not asked yet');
});

// ---------------------------------------------------------------------------------------------- the table

const LINE = ['c9', 'c8', 'c7', 'c6', 'c5', 'c4', 'c3', 'c2', 'c1', 'c0'];
const mergedPr = (number, sha, min, extra = {}) => ({ number, title: `change ${number}`, headRefName: `feat/${number}`, baseRefName: 'staging', mergedAt: iso(min), mergeCommit: { oid: sha }, url: `https://x/pull/${number}`, labels: [], author: { login: 'a' }, ...extra });
const openPr = (number, rollup, extra = {}) => ({ number, title: `open ${number}`, headRefName: `feat/${number}`, baseRefName: 'staging', isDraft: false, mergeStateStatus: 'CLEAN', labels: [], author: { login: 'a' }, updatedAt: iso(0), url: `https://x/pull/${number}`, statusCheckRollup: rollup, ...extra });
const promo = (number, head, merge, min, extra = {}) => ({ number, url: `https://x/pull/${number}`, head, merge, mergedAt: merge ? iso(min) : null, ...extra });

function world(over = {}) {
  return {
    repo: REPO, at: T0, took: 1000, calls: 5, days: 3, warnings: [],
    line: LINE,
    open: [],
    merged: [
      mergedPr(10, 'c9', 100), mergedPr(9, 'c7', 90), mergedPr(8, 'c6', 80), mergedPr(7, 'c3', 70), mergedPr(6, 'c1', 60),
    ],
    promos: { open: null, merged: [promo(100, 'c3', 'm1', 75), promo(101, 'c6', 'm2', 85)] },
    deploys: { test: { ok: { sha: 'm2', at: T0 + 88 * 60_000, url: 'u-test' } }, prod: { ok: { sha: 'm1', at: T0 + 77 * 60_000, url: 'u-prod' } }, complete: true },
    queue: [],
    rep: { m1: 'c3', m2: 'c6' },
    pairs: {},
    ...over,
  };
}
const rowOf = (out, n) => out.rows.find((r) => r.number === n);
const sig = (c) => (c ? `${c.s}${c.t ? ' ' + c.t : ''}` : '-');

test('a merged PR walks the pipeline: promotion, TEST, prod, each only where the commit really is', () => {
  const out = compose(world(), undefined, T0 + 120 * 60_000);
  // 10 (c9) and 9 (c7) are newer than every promotion: waiting for the next one, nothing after it
  assert.equal(sig(rowOf(out, 10).cells.promotion), 'wait');
  assert.equal(sig(rowOf(out, 10).cells.test), '-');
  assert.equal(sig(rowOf(out, 9).cells.test), '-');
  // 8 (c6) is the head of promotion 101: promoted, on TEST (m2 holds c6), not yet on prod (m1 holds only up to c3)
  assert.equal(sig(rowOf(out, 8).cells.promotion), 'done #101');
  assert.equal(sig(rowOf(out, 8).cells.test), 'done');
  assert.equal(sig(rowOf(out, 8).cells.prod), 'wait');
  // 7 (c3) is the head of promotion 100, and in prod
  assert.equal(sig(rowOf(out, 7).cells.promotion), 'done #100');
  assert.equal(sig(rowOf(out, 7).cells.prod), 'done');
  assert.ok(rowOf(out, 7).shipped);
  // 6 (c1) is older than promotion 100's head: it went with the first promotion that holds it
  assert.equal(sig(rowOf(out, 6).cells.promotion), 'done #100');
  assert.ok(!rowOf(out, 8).shipped);
  assert.equal(rowOf(out, 7).cells.land.s, 'done');
  assert.equal(rowOf(out, 7).cells.land.t, 'staging');
});

test('the stage headers say where the pipeline is as a whole', () => {
  const out = compose(world(), undefined, T0 + 120 * 60_000);
  const by = Object.fromEntries(out.stages.map((s) => [s.key, s]));
  assert.deepEqual(out.stages.map((s) => s.key), ['agent', 'pr', 'checks', 'land', 'promotion', 'test', 'prod']);
  assert.equal(by.test.t, 'm2'.slice(0, 7));
  assert.equal(by.test.s, 'done');
  assert.equal(by.prod.t, 'm1');
  assert.equal(by.prod.sub, '1 waiting', 'one PR is on TEST and not on prod');
  assert.equal(by.promotion.t, '#101');
  assert.match(by.promotion.sub, /none open · 2 waiting/);
  assert.equal(by.land.subAt, T0 + 100 * 60_000, 'the last landing');
});

test('an open promotion PR: its checks show on every PR it carries, and a failed battery is a failure', () => {
  const battery = 'battery — full e2e suite at promotion (dispatch full-test.yml + block on verdict)';
  const open = { ...promo(102, 'c8', null, 0), statusCheckRollup: [...green(), run(battery, 'COMPLETED', 'FAILURE', 3)] };
  const out = compose(world({ promos: { open, merged: [promo(100, 'c3', 'm1', 75), promo(101, 'c6', 'm2', 85)] } }), undefined, T0 + 120 * 60_000);
  assert.equal(sig(rowOf(out, 9).cells.promotion), `fail #102 ${battery}`);     // c7 is in it
  assert.equal(sig(rowOf(out, 10).cells.promotion), 'wait', 'c9 was merged after it was cut');
  assert.equal(sig(rowOf(out, 8).cells.promotion), 'done #101', 'already in main');
  assert.ok(rowOf(out, 9).attention);
  assert.equal(out.rows[0].number, 9, 'what is blocked rises');
  assert.equal(out.stages.find((s) => s.key === 'promotion').s, 'fail');
  // the same PR with its battery still running
  const going = { ...open, statusCheckRollup: [...green(), run(battery, 'IN_PROGRESS', '', 3)] };
  const out2 = compose(world({ promos: { open: going, merged: [] } }), undefined, T0 + 120 * 60_000);
  assert.equal(sig(rowOf(out2, 9).cells.promotion), 'run #102 battery');
});

test('a deploy still going shows on what is in it; one that failed shows as failed; neither touches older ones', () => {
  const w = world();
  w.deploys.test.active = { sha: 'm3', startedAt: T0 + 110 * 60_000, url: 'u' };
  w.rep.m3 = 'c8';
  w.promos.merged.push(promo(103, 'c8', 'm3', 108));
  const out = compose(w, undefined, T0 + 120 * 60_000);
  assert.equal(sig(rowOf(out, 9).cells.test), 'run deploying', 'c7 is in m3, which is going to TEST');
  assert.equal(sig(rowOf(out, 10).cells.test), '-', 'c9 is newer than m3');
  assert.equal(sig(rowOf(out, 8).cells.test), 'done', 'c6 is already on TEST');
  assert.equal(out.stages.find((s) => s.key === 'test').t, 'm3');
  assert.equal(out.stages.find((s) => s.key === 'test').sub, 'deploying · on it m2');
  w.deploys.test.active = undefined;
  w.deploys.test.failed = { sha: 'm3', at: T0 + 115 * 60_000, url: 'f' };
  const out2 = compose(w, undefined, T0 + 120 * 60_000);
  assert.equal(sig(rowOf(out2, 9).cells.test), 'fail failed');
  assert.ok(rowOf(out2, 9).attention);
  assert.equal(out2.stages.find((s) => s.key === 'test').s, 'fail');
});

test('a fix merged straight into main is placed by the compare answers, and has no promotion', () => {
  const w = world();
  w.merged.push(mergedPr(11, 'hot1', 95, { baseRefName: 'main' }), mergedPr(12, 'hot2', 99, { baseRefName: 'main' }));
  w.pairs = { 'hot1>m2': true, 'hot2>m2': false };
  const out = compose(w, undefined, T0 + 120 * 60_000);
  assert.equal(rowOf(out, 11).cells.promotion, null);
  assert.equal(rowOf(out, 11).cells.land.t, 'main');
  assert.equal(sig(rowOf(out, 11).cells.test), 'done');
  assert.equal(sig(rowOf(out, 12).cells.test), 'wait', 'on main, not yet on TEST');
});

test('open PRs: what each stage says, and which rise', () => {
  const w = world({
    open: [
      openPr(20, green()),                                                                             // ready, waits for the gate to land it
      openPr(21, [...green().slice(1), run('static', 'COMPLETED', 'FAILURE', 2), run('quality', 'COMPLETED', 'FAILURE', 1)]), // red
      openPr(22, [...green().slice(1), run('static', 'COMPLETED', 'FAILURE', 2)], { isDraft: true }),    // red, but a draft
      openPr(23, [], { mergeStateStatus: 'DIRTY' }),                                                    // conflicts
      openPr(24, [ok('db', 0), run('static', 'IN_PROGRESS', '', 1)]),                                    // running
      openPr(25, green(), { headRefName: 'feat/crm-5', baseRefName: 'feat/crm-1' }),                    // stacked on another branch
      openPr(26, [...green(), run('land on green (single pipeline — #3084)', 'IN_PROGRESS', '', 3)]),    // landing now
      openPr(27, green(), { labels: [{ name: 'hold' }] }),
    ],
    queue: [{ number: 28, position: 1, state: 'QUEUED', branch: 'staging' }],
  });
  w.open.push(openPr(28, green()));
  const out = compose(w, undefined, T0 + 25 * 60_000); // 25 minutes after the checks finished: nothing has sat long enough to be stuck
  assert.equal(sig(rowOf(out, 20).cells.checks), 'done 4/4');
  assert.equal(sig(rowOf(out, 20).cells.land), 'wait ready');
  assert.equal(sig(rowOf(out, 20).cells.pr), 'run open');
  assert.equal(sig(rowOf(out, 21).cells.checks), 'fail quality +1');
  assert.equal(sig(rowOf(out, 22).cells.pr), 'wait draft');
  assert.equal(sig(rowOf(out, 22).cells.checks), 'fail static');
  assert.equal(sig(rowOf(out, 23).cells.land), 'fail conflict');
  assert.equal(sig(rowOf(out, 23).cells.checks), 'wait');
  assert.equal(sig(rowOf(out, 24).cells.checks), 'run 1/4');
  assert.equal(sig(rowOf(out, 25).cells.land), 'wait stacked');
  assert.equal(sig(rowOf(out, 26).cells.land), 'run landing');
  assert.equal(sig(rowOf(out, 27).cells.land), 'wait hold');
  assert.equal(sig(rowOf(out, 28).cells.land), 'run queued 1');
  for (const n of [20, 22, 24, 25, 26, 27, 28]) assert.ok(!rowOf(out, n).attention, `${n} is not blocked`);
  for (const n of [21, 23]) assert.ok(rowOf(out, n).attention, `${n} is blocked`);
  assert.deepEqual(out.rows.slice(0, 2).map((r) => r.number).sort(), [21, 23]);
  assert.equal(out.stages.find((s) => s.key === 'checks').t, '2 failing');
  assert.equal(out.stages.find((s) => s.key === 'land').t, '1 queued');
  assert.equal(out.counts.attention, 2);
});

test('rows: blocked first, then what is on its way, then what has shipped; newest first inside each', () => {
  const r = (key, at, extra = {}) => ({ key, at, ...extra });
  const rows = orderRows([
    r('old-flight', 1), r('shipped-new', 50, { shipped: true }), r('new-flight', 10), r('blocked-old', 0, { attention: true }),
    r('blocked-new', 5, { attention: true }), r('shipped-old', 20, { shipped: true }),
  ]);
  assert.deepEqual(rows.map((x) => x.key), ['blocked-new', 'blocked-old', 'new-flight', 'old-flight', 'shipped-new', 'shipped-old']);
  // the same activity time never makes the order flicker between reads
  const a = orderRows([r('b', 5), r('a', 5)]), b = orderRows([r('a', 5), r('b', 5)]);
  assert.deepEqual(a.map((x) => x.key), b.map((x) => x.key));
});

test('agents: on their PR, or on a row of their own where the branch has none; counted in the Agent header', () => {
  const w = world({ open: [openPr(20, green())] });
  const ag = (extra) => ({ machine: 'Mac', account: 'default', engine: 'claude', sessionId: 's', title: 'a title', state: 'working', lastAt: T0 + 119 * 60_000, branch: 'feat/20', repoKnown: true, ...extra });
  const agents = { placed: [
    ag({ sessionId: 's1' }),
    ag({ sessionId: 's2', state: 'ready', title: 'idle one', lastAt: T0 }),
    ag({ sessionId: 's3', branch: 'feat/8', title: 'finished the work', state: 'ready' }),  // PR 8 is merged
    ag({ sessionId: 's4', branch: 'feat/new', title: 'no PR yet', state: 'needs-you' }),
    ag({ sessionId: 's5', branch: 'feat/other-repo', repoKnown: false }),                   // could not be placed in this repo
  ], unknown: 3 };
  const out = compose(w, agents, T0 + 120 * 60_000);
  const r20 = rowOf(out, 20);
  assert.equal(r20.cells.agent.s, 'run');
  assert.equal(r20.cells.agent.t, 'a title');
  assert.equal(r20.cells.agent.more, 1, 'a second agent is counted, the busier one is shown');
  assert.equal(rowOf(out, 8).cells.agent.t, 'finished the work');
  const branch = out.rows.find((r) => r.kind === 'branch');
  assert.equal(branch.branch, 'feat/new');
  assert.equal(branch.cells.agent.s, 'need');
  assert.equal(sig(branch.cells.pr), 'wait no PR yet');
  assert.equal(branch.cells.checks, null);
  assert.ok(!out.rows.some((r) => r.branch === 'feat/other-repo'), 'no row for a branch that is not known to be this repo');
  const head = out.stages.find((s) => s.key === 'agent');
  assert.equal(head.t, '1 working · 1 need you · 2 idle');
  assert.equal(head.sub, '3 branch unknown');
  // the busiest agent shows on a row with a needs-you one
  const r = compose(world({ open: [openPr(20, green())] }), { placed: [ag({ state: 'ready' }), ag({ state: 'needs-you', title: 'asks' })], unknown: 0 }, T0);
  assert.equal(rowOf(r, 20).cells.agent.t, 'asks');
});


// ---------------------------------------------------------------------------------------------- the whole repo

const wr = (workflow_id, name, branch, status, conclusion, min, extra = {}) => ({ id: workflow_id * 1000 + min, workflow_id, name, head_branch: branch, status, conclusion, event: 'schedule', created_at: iso(min), updated_at: iso(min + 2), run_started_at: iso(min), html_url: `https://x/${name}/${min}`, ...extra });
test('workflows: the last run of each file, gate on staging and on main apart, the quiet ones from their own read', () => {
  const files = ['.github/workflows/gate.yml', '.github/workflows/release.yml', '.github/workflows/full-test.yml', '.github/workflows/i18n-fill.yml', '.github/workflows/docs-gardener.yml'];
  const workflows = [
    { id: 1, name: 'gate', path: files[0] }, { id: 2, name: 'release', path: files[1] }, { id: 3, name: 'full-test', path: files[2] }, { id: 4, name: 'i18n-fill', path: files[3] },
    { id: 5, name: 'docs-gardener', path: files[4] },
    { id: 6, name: 'staging-coordinator', path: '.github/workflows/staging-coordinator.yml' },   // retired: no file
    { id: 7, name: 'Dependabot Updates', path: 'dynamic/dependabot/dependabot-updates' },       // GitHub's own
  ];
  const runs = [ // newest first
    wr(3, 'full-test', 'staging', 'in_progress', null, 50), wr(1, 'gate', 'main', 'completed', 'success', 40), wr(1, 'gate', 'staging', 'completed', 'failure', 35),
    wr(2, 'release', 'main', 'completed', 'success', 30), wr(1, 'gate', 'staging', 'completed', 'success', 10), wr(5, 'docs-gardener', 'staging', 'completed', 'cancelled', 5),
  ];
  const out = pickWorkflows(files, workflows, runs, new Map([[4, wr(4, 'i18n-fill', 'staging', 'completed', 'success', -2000)]]));
  assert.deepEqual(out.map((w) => w.label), ['gate main', 'gate staging', 'release', 'full-test', 'docs-gardener', 'i18n-fill'], 'the pipeline first, then the rest by name');
  const by = Object.fromEntries(out.map((w) => [w.label, w]));
  assert.equal(by['gate staging'].s, 'fail', 'the newest gate run on staging, not the older green one');
  assert.equal(by['gate main'].s, 'done');
  assert.equal(by['full-test'].s, 'run');
  assert.equal(by['full-test'].at, T0 + 50 * 60_000, 'a run still going counts from when it started');
  assert.equal(by['release'].at, T0 + 32 * 60_000, 'a finished one from when it finished');
  assert.equal(by['docs-gardener'].s, 'wait', 'cancelled is neither good news nor bad');
  assert.equal(by['docs-gardener'].t, 'cancelled');
  assert.equal(by['i18n-fill'].s, 'done');
  assert.equal(by['release'].href, 'https://x/release/30');
  // a workflow that has never run says so; gate does not
  const none = pickWorkflows(files, workflows, [], new Map());
  assert.equal(none.find((w) => w.label === 'release').none, true);
  assert.ok(!none.some((w) => w.name === 'gate'));
  assert.equal(runCell({ status: 'completed', conclusion: 'timed_out' }).s, 'fail');
  assert.equal(runCell({ status: 'queued' }).s, 'run');
});

test('every PR is a row: the promotion PRs carry their own state, a PR merged into a stack stops there, bots are marked', () => {
  const battery = 'battery — full e2e suite at promotion (dispatch full-test.yml + block on verdict)';
  const w = world({
    open: [
      openPr(102, [...green(), run(battery, 'COMPLETED', 'FAILURE', 3)], { title: 'promote: staging → main', baseRefName: 'main', headRefName: 'release/x', headRefOid: 'c8', author: { login: 'github-actions[bot]', is_bot: true } }),
      openPr(30, green(), { author: { login: 'dependabot[bot]', is_bot: true } }),
    ],
    merged: [
      mergedPr(10, 'c9', 100), mergedPr(8, 'c6', 80),
      mergedPr(100, 'm1', 75, { title: 'promote: staging → main (a)', baseRefName: 'main', headRefName: 'release/a', headRefOid: 'c3' }),
      mergedPr(101, 'm2', 85, { title: 'promote: staging → main (b)', baseRefName: 'main', headRefName: 'release/b', headRefOid: 'c6' }),
      mergedPr(31, 's1', 70, { baseRefName: 'feat/crm-1' }),
    ],
  });
  const out = compose(w, undefined, T0 + 120 * 60_000);
  const p = rowOf(out, 102);
  assert.ok(p.promo);
  assert.equal(sig(p.cells.checks), `fail ${battery}`);
  assert.ok(p.attention, 'a promotion whose battery failed is blocked');
  assert.equal(p.cells.promotion, null, 'it is the promotion');
  assert.equal(sig(p.cells.land), 'wait');
  assert.ok(rowOf(out, 30).bot);
  assert.ok(!rowOf(out, 10).bot);
  // the merged promotions: on TEST and prod when their head is
  assert.equal(sig(rowOf(out, 101).cells.land), 'done main');
  assert.equal(sig(rowOf(out, 101).cells.test), 'done');
  assert.equal(sig(rowOf(out, 101).cells.prod), 'wait');
  assert.equal(sig(rowOf(out, 100).cells.prod), 'done');
  assert.ok(rowOf(out, 100).shipped);
  // a PR merged into another branch: its landing, and nothing after it
  const stacked = rowOf(out, 31);
  assert.equal(sig(stacked.cells.land), 'done into feat/crm-1');
  assert.deepEqual([stacked.cells.promotion, stacked.cells.test, stacked.cells.prod], [null, null, null]);
  // the promotion PRs are not counted among the PRs waiting on a deploy
  assert.equal(out.stages.find((s) => s.key === 'prod').sub, '1 waiting');
  assert.equal(out.counts.attention, 1);
});

test('the agent column is an extra: rows exist with no agent at all, and sessions never remove one', () => {
  const w = world({ open: [openPr(20, green()), openPr(21, green(), { author: { login: 'someone-else' } })] });
  const none = compose(w, { placed: [], unknown: 0 }, T0);
  const some = compose(w, { placed: [{ machine: 'Mac', state: 'working', title: 't', branch: 'feat/20', repoKnown: true, lastAt: 1 }], unknown: 0 }, T0);
  assert.equal(none.rows.length, some.rows.length);
  assert.equal(rowOf(none, 21).cells.agent, null);
  assert.equal(rowOf(some, 21).cells.agent, null);
  assert.ok(rowOf(some, 20).cells.agent);
});

// ---------------------------------------------------------------------------------------------- stuck

const H = 3_600_000, M = 60_000;
const NOW = T0 + 10 * H;
const ago = (ms) => new Date(NOW - ms).toISOString();
const greenAt = (msAgo) => REQUIRED.map((n) => ({ __typename: 'CheckRun', name: n, status: 'COMPLETED', conclusion: 'SUCCESS', startedAt: ago(msAgo + M), completedAt: ago(msAgo) }));
const redAt = (msAgo, ...names) => [...REQUIRED.slice(1).map((n) => ({ __typename: 'CheckRun', name: n, status: 'COMPLETED', conclusion: 'SUCCESS', startedAt: ago(msAgo + M), completedAt: ago(msAgo) })),
  ...names.map((n) => ({ __typename: 'CheckRun', name: n, status: 'COMPLETED', conclusion: 'FAILURE', startedAt: ago(msAgo + M), completedAt: ago(msAgo) })), { __typename: 'CheckRun', name: 'static', status: 'COMPLETED', conclusion: 'FAILURE', startedAt: ago(msAgo), completedAt: ago(msAgo) }];
const stuckOf = (out, n) => rowOf(out, n).stuck?.why ?? null;
const empty = (over = {}) => world({ open: [], merged: [], promos: { open: null, merged: [] }, deploys: { test: {}, prod: {}, complete: false }, ...over });

test('the time a stage has sat reads as 12m, 3h, 2d', () => {
  assert.equal(span(20_000), 'now');
  assert.equal(span(12 * M), '12m');
  assert.equal(span(3 * H), '3h');
  assert.equal(span(49 * H), '2d');
  assert.equal(span(-5), 'now');
});

test('a draft whose checks are green or nearly, untouched for two hours, is stuck; not before, not on hold, not with many reds', () => {
  const w = empty({ open: [
    openPr(1, greenAt(3 * H), { isDraft: true, updatedAt: ago(3 * H) }),                                        // green and quiet
    openPr(2, greenAt(30 * M), { isDraft: true, updatedAt: ago(30 * M) }),                                      // green, but somebody was just here
    openPr(3, greenAt(3 * H), { isDraft: true, updatedAt: ago(3 * H), labels: [{ name: 'hold' }] }),             // parked on purpose
    openPr(4, redAt(3 * H, 'quality'), { isDraft: true, updatedAt: ago(3 * H) }),                               // one real red: nearly
    openPr(5, redAt(3 * H, 'quality', 'types'), { isDraft: true, updatedAt: ago(3 * H) }),                      // two reds: nearly
    openPr(6, redAt(3 * H, 'quality', 'types', 'lint'), { isDraft: true, updatedAt: ago(3 * H) }),             // three: its author's business
  ] });
  const out = compose(w, undefined, NOW);
  assert.match(stuckOf(out, 1), /^draft 3h, checks green$/);
  assert.equal(stuckOf(out, 2), null);
  assert.equal(stuckOf(out, 3), null);
  assert.match(stuckOf(out, 4), /^draft 3h, checks nearly green \(1 red\)$/);
  assert.match(stuckOf(out, 5), /nearly green \(2 red\)/);
  assert.equal(stuckOf(out, 6), null);
  // how long it has sat, on the cell it sits in
  assert.equal(rowOf(out, 1).cells.pr.since, NOW - 3 * H);
  assert.ok(rowOf(out, 1).cells.pr.stuck);
  assert.ok(!rowOf(out, 2).cells.pr.stuck);
  assert.equal(rowOf(out, 2).cells.pr.since, NOW - 30 * M, 'the age is there for every row, stuck or not');
  assert.equal(rowOf(out, 1).sat.stage, 'draft');
});

test('an open PR that is green and has not landed in half an hour is stuck; one queued, landing, on hold or in conflict is not', () => {
  const w = empty({ open: [
    openPr(10, greenAt(2 * H)),
    openPr(11, greenAt(10 * M)),
    openPr(12, greenAt(2 * H)),                                                                   // queued below
    openPr(13, [...greenAt(2 * H), { __typename: 'CheckRun', name: 'land on green (single pipeline — #3084)', status: 'IN_PROGRESS', startedAt: ago(M) }]),
    openPr(14, greenAt(2 * H), { labels: [{ name: 'hold' }] }),
    openPr(15, greenAt(2 * H), { mergeStateStatus: 'DIRTY' }),
    openPr(16, greenAt(2 * H), { baseRefName: 'feat/crm-1' }),                                   // stacked: waits for its parent
  ], queue: [{ number: 12, position: 1, state: 'QUEUED', branch: 'staging' }] });
  const out = compose(w, undefined, NOW);
  assert.match(stuckOf(out, 10), /^green 2h, not landed$/);
  assert.equal(rowOf(out, 10).cells.land.since, NOW - 2 * H, 'it has been ready since the checks finished');
  for (const n of [11, 12, 13, 14, 15, 16]) assert.equal(stuckOf(out, n), null, `#${n}`);
  assert.ok(rowOf(out, 15).attention && !rowOf(out, 15).stuck, 'a conflict is blocked, not stuck');
});

test('merged into staging and in no promotion for three hours is stuck, and says for how long', () => {
  const w = empty({ line: ['c3', 'c2', 'c1', 'c0'], merged: [mergedPr(1, 'c3', 0, { mergedAt: ago(2 * H) }), mergedPr(2, 'c2', 0, { mergedAt: ago(3 * H + 10 * M) }), mergedPr(3, 'c1', 0, { mergedAt: ago(2 * 24 * H) })] });
  const out = compose(w, undefined, NOW);
  assert.equal(stuckOf(out, 1), null, 'two hours is not long');
  assert.equal(rowOf(out, 1).sat.stage, 'on staging');
  assert.equal(rowOf(out, 1).cells.promotion.since, NOW - 2 * H);
  assert.equal(stuckOf(out, 2), 'on staging 3h, not in a promotion');
  assert.equal(stuckOf(out, 3), 'on staging 2d, not in a promotion');
  assert.ok(rowOf(out, 3).cells.promotion.stuck);
});

test('in an open promotion that has sat two hours, in main an hour without TEST, on TEST two hours without prod: each is stuck at its stage', () => {
  const open = { ...promo(200, 'c5', null, 0), createdAt: ago(3 * H), statusCheckRollup: greenAt(10 * M).slice(0, 3) };
  const w = empty({
    line: ['c6', 'c5', 'c4', 'c3', 'c2', 'c1', 'c0'],
    merged: [mergedPr(1, 'c5', 0, { mergedAt: ago(5 * H) }), mergedPr(2, 'c3', 0, { mergedAt: ago(9 * H) }), mergedPr(3, 'c1', 0, { mergedAt: ago(20 * H) }), mergedPr(4, 'c0', 0, { mergedAt: ago(30 * H) })],
    promos: { open, merged: [promo(100, 'c3', 'm1', 0, { mergedAt: ago(8 * H) }), promo(99, 'c1', 'm0', 0, { mergedAt: ago(19 * H) })].sort((a, b) => Date.parse(a.mergedAt) - Date.parse(b.mergedAt)) },
    deploys: { test: { ok: { sha: 'm1', at: NOW - 7 * H } }, prod: { ok: { sha: 'm0', at: NOW - 18 * H } }, complete: true },
    rep: { m1: 'c3', m0: 'c1' },
  });
  w.promos.merged.sort((a, b) => Date.parse(a.mergedAt) - Date.parse(b.mergedAt));
  const out = compose(w, undefined, NOW);
  assert.equal(stuckOf(out, 1), 'in promotion 3h, #200 not merged');
  assert.equal(rowOf(out, 1).cells.promotion.since, NOW - 3 * H);
  assert.equal(stuckOf(out, 2), 'on TEST 8h, not in prod', 'in main since its promotion merged, on TEST, not in prod');
  assert.ok(rowOf(out, 2).cells.prod.stuck);
  assert.equal(rowOf(out, 3).sat.stage, 'in prod');
  assert.equal(stuckOf(out, 3), null);
  assert.ok(rowOf(out, 3).shipped);
  // the same commit before TEST has it: in main, not on TEST
  const w2 = { ...w, deploys: { test: { ok: { sha: 'm0', at: NOW - 18 * H } }, prod: { ok: { sha: 'm0', at: NOW - 18 * H } }, complete: true } };
  const out2 = compose(w2, undefined, NOW);
  assert.equal(stuckOf(out2, 2), 'in main 8h, not on TEST');
  assert.ok(rowOf(out2, 2).cells.test.stuck);
  // ... and for less than an hour it is only on its way
  const w3 = { ...w2, promos: { ...w2.promos, merged: w2.promos.merged.map((p) => (p.number === 100 ? { ...p, mergedAt: ago(20 * M) } : p)) } };
  assert.equal(stuckOf(compose(w3, undefined, NOW), 2), null);
  assert.equal(rowOf(compose(w3, undefined, NOW), 2).sat.stage, 'in main');
  // a promotion PR carries what is stuck; it is not itself stuck
  const w4 = { ...w2, merged: [...w2.merged, mergedPr(100, 'm1', 0, { baseRefName: 'main', title: 'promote: x', headRefName: 'release/a', headRefOid: 'c3', mergedAt: ago(8 * H) })] };
  assert.equal(stuckOf(compose(w4, undefined, NOW), 100), null);
});

test('an agent that stopped while its PR goes nowhere: stuck, whatever the checks say (the budget case)', () => {
  const reds = redAt(3 * H, 'lint', 'quality', 'types', 'build');
  const w = empty({ open: [openPr(30, reds, { isDraft: true, updatedAt: ago(2 * M), headRefName: 'feat/budgets', mergeStateStatus: 'DIRTY' })] });
  const ag = (extra) => ({ machine: 'redbat', account: 'gmail', engine: 'claude', title: 'Add budget upload module', state: 'ready', lastAt: NOW - 3 * H, branch: 'feat/budgets', repoKnown: false, ...extra });
  const idle = compose(w, { placed: [ag()], unknown: 0 }, NOW);
  assert.equal(stuckOf(idle, 30), 'agent idle 3h, PR not landed');
  assert.equal(rowOf(idle, 30).cells.agent.t, 'Add budget upload module', 'attached by the branch name, though the folder says nothing of the repo');
  assert.equal(rowOf(idle, 30).cells.agent.s, 'wait');
  // at work, or idle only a few minutes: not stuck
  assert.equal(stuckOf(compose(w, { placed: [ag({ state: 'working' })], unknown: 0 }, NOW), 30), null);
  assert.equal(stuckOf(compose(w, { placed: [ag({ lastAt: NOW - 5 * M })], unknown: 0 }, NOW), 30), null);
  // two agents, one still working: not idle
  assert.equal(stuckOf(compose(w, { placed: [ag(), ag({ state: 'working' })], unknown: 0 }, NOW), 30), null);
  // merged work with an idle agent is finished, not stuck by that
  const m = empty({ line: ['c1', 'c0'], merged: [mergedPr(31, 'c1', 0, { mergedAt: ago(M), headRefName: 'feat/done' })] });
  assert.equal(stuckOf(compose(m, { placed: [ag({ branch: 'feat/done' })], unknown: 0 }, NOW), 31), null);
});

test('a branch with no PR: stuck when its agent stopped and the branch has commits on GitHub that no staging commit is', () => {
  const w = empty({ line: ['s2', 's1', 's0'] });
  const ag = (branch, extra) => ({ machine: 'redbat', account: 'a', engine: 'claude', title: `work on ${branch}`, state: 'ready', lastAt: NOW - 4 * H, branch, repoKnown: false, ...extra });
  const agents = {
    placed: [ag('feat/pushed'), ag('feat/at-staging'), ag('feat/not-pushed', { repoKnown: true }), ag('feat/elsewhere'), ag('feat/busy', { state: 'working' }), ag('feat/old')],
    unknown: 0,
    branches: { 'feat/pushed': { exists: true, sha: 'x9', at: ago(5 * H) }, 'feat/at-staging': { exists: true, sha: 's1', at: ago(5 * H) }, 'feat/not-pushed': { exists: false },
      'feat/busy': { exists: true, sha: 'x8', at: ago(M) }, 'feat/old': { exists: true, sha: 'x1', at: ago(30 * 24 * H) } },
  };
  const out = compose(w, agents, NOW);
  const by = Object.fromEntries(out.rows.filter((r) => r.kind === 'branch').map((r) => [r.branch, r]));
  assert.deepEqual(Object.keys(by).sort(), ['feat/at-staging', 'feat/busy', 'feat/not-pushed', 'feat/old', 'feat/pushed'], 'a branch of unknown folder is a row when GitHub has it; one GitHub does not have, and nothing else vouches for, is not');
  assert.equal(by['feat/pushed'].stuck.why, 'agent idle 4h, commits on the branch, no PR');
  assert.equal(by['feat/pushed'].sat.stage, 'branch');
  assert.equal(by['feat/pushed'].cells.pr.since, NOW - 4 * H);
  assert.equal(by['feat/at-staging'].stuck, null, 'its tip is already on staging');
  assert.equal(by['feat/not-pushed'].stuck, null, 'nothing on GitHub to say there are commits');
  assert.equal(sig(by['feat/not-pushed'].cells.pr), 'wait no PR · not pushed');
  assert.equal(by['feat/busy'].stuck, null, 'still working');
  assert.equal(by['feat/old'].stuck, null, 'a tip older than the window says nothing');
  assert.equal(out.counts.stuck, 1);
});

test('stuck rises to the top: an agent that stopped, then the later the stage the sooner, the longest-stuck first; then what is merely failing', () => {
  const rows = orderRows([
    { key: 'a', at: 100, attention: true }, { key: 'draft', at: 50, stuck: { rank: 6, since: 1 } }, { key: 'staging-new', at: 200, stuck: { rank: 5, since: 30 } },
    { key: 'staging-old', at: 1, stuck: { rank: 5, since: 10 } }, { key: 'idle', at: 5, stuck: { rank: 0, since: 99 } }, { key: 'on-test', at: 5, stuck: { rank: 2, since: 70 } },
    { key: 'd', at: 300 }, { key: 'e', at: 400, shipped: true }, { key: 'f', at: 999, stuck: { rank: 1, since: 60 }, attention: true },
  ]);
  assert.deepEqual(rows.map((r) => r.key), ['idle', 'f', 'on-test', 'staging-old', 'staging-new', 'draft', 'a', 'd', 'e']);
});

test('a draft untouched for a week is parked, not stalled: its age shows, it is not in the Stuck band', () => {
  const w = empty({ open: [openPr(40, greenAt(3 * H), { isDraft: true, updatedAt: ago(8 * 24 * H) }), openPr(41, greenAt(3 * H), { isDraft: true, updatedAt: ago(6 * 24 * H) })] });
  const out = compose(w, undefined, NOW);
  assert.equal(stuckOf(out, 40), null);
  assert.equal(rowOf(out, 40).cells.pr.since, NOW - 8 * 24 * H);
  assert.match(stuckOf(out, 41), /^draft 6d/);
});

test('sessions in the shared checkout show on the PR of the branch it has out, but make no branch of their own and are never "idle" for it', () => {
  const w = empty({ open: [openPr(50, redAt(3 * H, 'quality'), { isDraft: true, updatedAt: ago(M), headRefName: 'feat/shared-branch' })] });
  const ag = (extra) => ({ machine: 'redbat', account: 'a', engine: 'claude', title: 't', state: 'ready', lastAt: NOW - 5 * H, branch: 'feat/shared-branch', repoKnown: true, shared: true, ...extra });
  // all of them in the shared checkout: they are on the row, none counts as idle
  const out = compose(w, { placed: [ag(), ag({ title: 'b' })], unknown: 0 }, NOW);
  assert.equal(rowOf(out, 50).cells.agent.more, 1);
  assert.equal(rowOf(out, 50).cells.agent.sub, 'redbat · shared');
  assert.equal(rowOf(out, 50).stuck, null);
  const head = out.stages.find((s) => s.key === 'agent');
  assert.equal(head.t, 'none on a branch');
  assert.match(head.sub, /2 in shared checkouts/);
  // one with a worktree of its own comes first, and its idleness counts
  const mixed = compose(w, { placed: [ag(), ag({ title: 'own worktree', shared: false })], unknown: 0 }, NOW);
  assert.equal(rowOf(mixed, 50).cells.agent.t, 'own worktree');
  assert.equal(rowOf(mixed, 50).cells.agent.sub, 'redbat');
  assert.equal(rowOf(mixed, 50).stuck.why, 'agent idle 5h, PR not landed');
  // a branch no PR names, with only shared-checkout sessions on it, is not a row
  const none = compose(empty(), { placed: [ag({ branch: 'feat/nobody' })], unknown: 0, branches: { 'feat/nobody': { exists: true, sha: 'q', at: ago(M) } } }, NOW);
  assert.ok(!none.rows.some((r) => r.kind === 'branch'));
});

test('a session in the shared checkout whose own words name one open PR\'s branch is working on that PR', () => {
  const w = empty({ open: [openPr(60, redAt(3 * H, 'lint', 'quality', 'types', 'build'), { isDraft: true, updatedAt: ago(M), headRefName: 'feat/budgets' }), openPr(61, greenAt(M), { headRefName: 'feat/budgets-two', updatedAt: ago(M) })] });
  const ag = (words, extra) => ({ machine: 'redbat', account: 'a', engine: 'claude', title: 'Add budget upload module', state: 'working', lastAt: NOW - M, branch: 'feat/country-ledger-trial-balance-cash', repoKnown: true, shared: true, words, ...extra });
  const said = 'Built and on the real screen: the budgets module is complete on `feat/budgets` (draft PR #5872). What is left: the full suite.';
  const out = compose(w, { placed: [ag(said)], unknown: 0 }, NOW);
  assert.equal(rowOf(out, 60).cells.agent.t, 'Add budget upload module');
  assert.equal(rowOf(out, 60).cells.agent.sub, 'redbat', 'no longer "shared": it is on its PR');
  assert.match(rowOf(out, 60).cells.agent.tip, /its own words name this branch/);
  assert.equal(rowOf(out, 61).cells.agent, null, 'feat/budgets is not feat/budgets-two');
  assert.ok(!out.rows.some((r) => r.kind === 'branch'), 'and the shared checkout\'s branch is not a piece of work');
  // idle, the same sentence puts the PR in the stuck band
  const idle = compose(w, { placed: [ag(said, { state: 'ready', lastAt: NOW - 3 * H })], unknown: 0 }, NOW);
  assert.equal(rowOf(idle, 60).stuck.why, 'agent idle 3h, PR not landed');
  // two branches named, or none: it stays where its checkout says
  const two = compose(w, { placed: [ag('on feat/budgets and also feat/budgets-two')], unknown: 0 }, NOW);
  assert.equal(rowOf(two, 60).cells.agent, null);
  assert.equal(compose(w, { placed: [ag('nothing of the sort')], unknown: 0 }, NOW).rows.some((r) => r.cells.agent), false);
  // a session already in a worktree of its own keeps the branch it reports, whatever it says
  const own = compose(w, { placed: [ag(said, { shared: false, branch: 'feat/budgets-two' })], unknown: 0 }, NOW);
  assert.ok(rowOf(own, 61).cells.agent);
  assert.equal(rowOf(own, 60).cells.agent, null);
});

test('read(): the budget session on redbat, on a branch the folder does not name, ends up in the stuck band with its agent', async () => {
  const gh = fakeGh();
  const inner = gh.exec;
  const hours = (n) => Date.now() - n * 3_600_000;
  const exec = async (args) => {
    const a = args.join(' ');
    if (a.includes('--state open')) { gh.log.push(args); return JSON.stringify([openPr(5872, redAt(3 * H, 'lint', 'quality'), { isDraft: true, headRefName: 'feat/budgets', updatedAt: new Date(hours(1)).toISOString() })]); }
    if (a.includes('/branches/feat/uploads')) { gh.log.push(args); return JSON.stringify({ sha: 'zzz', at: new Date(hours(6)).toISOString() }); }
    if (a.includes('/branches/feat/gone')) { gh.log.push(args); throw new Error('gh: Not Found (HTTP 404)'); }
    return inner(args);
  };
  const sess = (title, branch, cwd) => ({ sessionId: title, title, state: 'ready', cwd, branch, lastAt: hours(3) });
  const snap = { machines: [{ name: 'redbat', local: false, ok: true, accounts: [{ engine: 'claude', id: 'gmail', sessions: [
    sess('Add budget upload module', 'feat/budgets', '~/dev/chai-budget'),
    sess('Upload helper', 'feat/uploads', '~/dev/chai-uploads'),
    sess('Somebody else\'s repo', 'feat/gone', '~/dev/other-thing'),
  ] }] }] };
  const sat = createSatellite({ repo: REPO, exec, snapshot: async () => snap });
  const out = await sat.read();
  assert.ok(out.ok, out.error);
  const budget = out.rows.find((r) => r.number === 5872);
  assert.equal(budget.cells.agent.t, 'Add budget upload module');
  assert.equal(budget.cells.agent.sub, 'redbat');
  assert.match(budget.stuck.why, /agent idle 3h, PR not landed/);
  const up = out.rows.find((r) => r.branch === 'feat/uploads');
  assert.ok(up, 'a branch GitHub has is a row though its folder says nothing');
  assert.match(up.stuck.why, /agent idle 3h, commits on the branch, no PR/);
  assert.ok(!out.rows.some((r) => r.branch === 'feat/gone'), 'a branch GitHub has never heard of, from a folder that does not name the repo, is not ours');
  assert.deepEqual(out.rows.slice(0, 2).map((r) => r.key).sort(), ['br:feat/uploads', 'pr:5872']);
  assert.equal(out.counts.stuck, 2);
  // asking again within the minute does not ask GitHub about the branches again
  const asked = gh.log.length;
  await sat.read();
  assert.equal(gh.log.length, asked);
  // every call, the branch lookups too, was a read
  for (const args of gh.log) assert.doesNotThrow(() => guard(args));
});

// ---------------------------------------------------------------------------------------------- agents on branches

function checkouts() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'satellite-'));
  const w = (f, text) => { fs.mkdirSync(path.dirname(f), { recursive: true }); fs.writeFileSync(f, text); };
  const url = (u) => `[core]\n\tbare = false\n[remote "origin"]\n\turl = ${u}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n`;
  // the main checkout, with a subfolder
  w(path.join(root, 'tracker/.git/HEAD'), 'ref: refs/heads/test/permissions-battery\n');
  w(path.join(root, 'tracker/.git/config'), url(`https://github.com/${REPO}.git`));
  fs.mkdirSync(path.join(root, 'tracker/app/deep'), { recursive: true });
  // a linked worktree: its .git is a file, its HEAD and commondir live under the main .git
  w(path.join(root, 'tracker/.git/worktrees/wt1/HEAD'), 'ref: refs/heads/ops/image-pins\n');
  w(path.join(root, 'tracker/.git/worktrees/wt1/commondir'), '../..\n');
  w(path.join(root, 'wt/.git'), `gitdir: ${path.join(root, 'tracker/.git/worktrees/wt1')}\n`);
  // a detached HEAD
  w(path.join(root, 'detached/.git/HEAD'), '0123456789abcdef0123456789abcdef01234567\n');
  w(path.join(root, 'detached/.git/config'), url(`git@github.com:${REPO}.git`));
  // another repo, in a folder that is not named like this one
  w(path.join(root, 'other/.git/HEAD'), 'ref: refs/heads/feat/fleet\n');
  w(path.join(root, 'other/.git/config'), url('https://github.com/someone/other-repo.git'));
  // the same name, another repo's remote
  w(path.join(root, 'lookalike/chai-expense-tracker/.git/HEAD'), 'ref: refs/heads/feat/x\n');
  w(path.join(root, 'lookalike/chai-expense-tracker/.git/config'), url('https://github.com/fork/chai-expense-tracker-fork.git'));
  // on staging itself
  w(path.join(root, 'onstaging/.git/HEAD'), 'ref: refs/heads/staging\n');
  w(path.join(root, 'onstaging/.git/config'), url(`https://github.com/${REPO}`));
  return root;
}
const sess = (cwd, extra = {}) => ({ sessionId: `s-${Math.random()}`, title: 't', state: 'ready', cwd, lastAt: 1, ...extra });
const snapOf = (mac, redbat = []) => ({ machines: [
  { name: 'Mac', local: true, ok: true, accounts: [{ engine: 'claude', id: 'default', sessions: mac }] },
  { name: 'redbat', local: false, ok: true, accounts: [{ engine: 'claude', id: 'gmail', sessions: redbat }] },
] });

test('on this Mac the branch is read from the folder\'s git files, worktrees and subfolders included', () => {
  const root = checkouts();
  const { placed, unknown } = placeAgents(snapOf([
    sess(path.join(root, 'tracker')), sess(path.join(root, 'tracker/app/deep')), sess(path.join(root, 'wt')),
    sess(path.join(root, 'detached')), sess(path.join(root, 'other')), sess(path.join(root, 'lookalike/chai-expense-tracker')), sess(path.join(root, 'onstaging')),
  ]), { repo: REPO });
  assert.deepEqual(placed.map((a) => a.branch), ['test/permissions-battery', 'test/permissions-battery', 'ops/image-pins']);
  assert.ok(placed.every((a) => a.repoKnown && a.machine === 'Mac'));
  assert.deepEqual(placed.map((a) => a.shared), [true, true, false], 'the main checkout and its subfolders are shared; a linked worktree is the work\'s own');
  assert.equal(unknown, 1, 'the detached HEAD is this repo\'s but on no branch');
});

test('closed sessions are not agents; a "~/" folder is this Mac\'s home; a branch Fleet gives wins', () => {
  const root = checkouts();
  const snap = snapOf([sess('~/tracker'), sess('~/tracker', { state: 'closed' }), sess('~/tracker', { branch: 'given/by-fleet' })]);
  const { placed } = placeAgents(snap, { repo: REPO, home: root });
  assert.deepEqual(placed.map((a) => a.branch), ['test/permissions-battery', 'given/by-fleet']);
});

test('another machine: only a branch Fleet gives places it; without one it is counted when its folder looks like this repo', () => {
  const { placed, unknown } = placeAgents(snapOf([], [
    sess('~/dev/chai-expense-tracker', { branch: 'feat/remote' }),
    sess('~/dev/chai-expense-tracker'),
    sess('~/dev/chai-expense-tracker'),
    sess('~/dev/fleet'),
    sess('~/dev/fleet', { branch: 'feat/fleet' }),
  ]), { repo: REPO });
  assert.deepEqual(placed.map((a) => [a.machine, a.branch, a.repoKnown]), [['redbat', 'feat/remote', true], ['redbat', 'feat/fleet', false]]);
  assert.equal(unknown, 2);
  // elsewhere the path says whether a folder is the repo's own checkout or a worktree made for the work
  const where = (cwd) => placeAgents(snapOf([], [sess(cwd, { branch: 'feat/x' })]), { repo: REPO }).placed[0].shared;
  assert.equal(where('~/dev/chai-expense-tracker'), true);
  assert.equal(where('~/dev/chai-expense-tracker/app'), true);
  assert.equal(where('~/dev/chai-expense-tracker/.claude/worktrees/memo-notes'), false);
  assert.equal(where('~/dev/chai-budget'), false);
  assert.equal(where('~/dev/chai-worktrees/crm-1'), false);
  assert.equal(placeAgents(null).placed.length, 0);
  assert.equal(placeAgents({ machines: [{ name: 'x', ok: false }] }).placed.length, 0);
});

// ---------------------------------------------------------------------------------------------- read-only

test('gh is only ever asked to read', () => {
  for (const ok of [['pr', 'list', '-R', 'a/b'], ['pr', 'view', '5'], ['run', 'list'], ['api', 'repos/a/b/commits?per_page=100', '--jq', '.[]'],
    ['api', 'graphql', '-f', 'query=query($o:String!){repository(owner:$o){id}}', '-f', 'o=x']]) assert.doesNotThrow(() => guard(ok), ok.join(' '));
  for (const no of [['pr', 'merge', '5'], ['pr', 'close', '5'], ['pr', 'comment', '5'], ['pr', 'create'], ['workflow', 'run', 'ship'], ['run', 'rerun', '1'], ['issue', 'create'],
    ['api', 'repos/a/b/issues', '-f', 'title=x'], ['api', 'repos/a/b/issues', '-X', 'POST'], ['api', 'repos/a/b', '--method', 'DELETE'], ['api', 'repos/a/b', '--input', 'x.json'],
    ['api', 'graphql', '-f', 'query=mutation { mergePullRequest(input: {}) { clientMutationId } }'], ['api', 'graphql', '-f', 'query={ viewer { login } } mutation { x }']]) {
    assert.throws(() => guard(no), /only reads GitHub/, no.join(' '));
  }
});

// ---------------------------------------------------------------------------------------------- the reads, with a made-up gh

const wfRun = (workflowId, branch, conclusion, min) => ({ id: workflowId * 100 + min, status: 'completed', conclusion, head_branch: branch, event: 'workflow_dispatch', workflow_id: workflowId, created_at: iso(min), updated_at: iso(min + 1), run_started_at: iso(min), html_url: `https://x/runs/${workflowId}` });
function fakeGh(state = {}) {
  const log = [];
  const exec = async (args) => {
    log.push(args);
    const a = args.join(' ');
    if (state.fail) throw new Error('gh: not logged in');
    if (a.includes('--state open')) return JSON.stringify([openPr(20, green())]);
    if (a.includes('--state merged')) return JSON.stringify([
      mergedPr(10, 'c9', 100, { mergedAt: new Date(Date.now() - (state.recent ? 30_000 : 3600_000)).toISOString() }), mergedPr(8, 'c6', 80, { mergedAt: new Date(Date.now() - 7200_000).toISOString() }),
      { number: 101, title: 'promote: staging → main', headRefName: 'release/x', baseRefName: 'main', headRefOid: 'c6', mergeCommit: { oid: 'm2' }, mergedAt: new Date(Date.now() - 7000_000).toISOString(), url: 'u101', labels: [] },
    ]);
    if (args[1] === 'graphql') return JSON.stringify({ data: { repository: { s: { entries: { nodes: [{ position: 1, state: 'QUEUED', pullRequest: { number: 20 } }] } }, m: null } } });
    if (a.includes('workflows/release.yml/runs')) return JSON.stringify([{ id: 2, status: 'completed', head_sha: 'm2' }, { id: 1, status: 'completed', head_sha: 'm1' }]);
    if (a.includes('/actions/runs/2/jobs')) return JSON.stringify([job('deploy-test', 'completed', 'success', 'm2', 0, 5), job('deploy-prod', 'completed', 'skipped', 'm2', 0, 0)]);
    if (a.includes('/actions/runs/1/jobs')) return JSON.stringify([job('deploy-prod', 'completed', 'success', 'm1', 0, 5), job('deploy-test', 'completed', 'skipped', 'm1', 0, 0)]);
    if (a.includes('/contents/.github/workflows')) return JSON.stringify(['.github/workflows/gate.yml', '.github/workflows/release.yml', '.github/workflows/nightly-canary.yml']);
    if (a.includes('/actions/workflows?per_page')) return JSON.stringify([
      { id: 1, name: 'gate', path: '.github/workflows/gate.yml', state: 'active' }, { id: 2, name: 'release', path: '.github/workflows/release.yml', state: 'active' },
      { id: 3, name: 'nightly-canary', path: '.github/workflows/nightly-canary.yml', state: 'active' }, { id: 4, name: 'staging-coordinator', path: '.github/workflows/staging-coordinator.yml', state: 'active' }]);
    if (a.includes('/actions/runs?branch=staging')) return JSON.stringify([wfRun(1, 'staging', 'failure', 30)]);
    if (a.includes('/actions/runs?branch=main')) return JSON.stringify([wfRun(2, 'main', 'success', 20), wfRun(1, 'main', 'success', 10)]);
    if (a.includes('/actions/workflows/3/runs')) return JSON.stringify([wfRun(3, 'staging', 'success', 5)]);
    if (a.includes('/commits?')) return JSON.stringify(LINE);
    if (a.includes('/compare/m1...staging')) return 'c3\n';
    throw new Error(`unexpected gh call: ${a}`);
  };
  return { exec, log };
}

test('one read of GitHub: a handful of calls, the right rows, and a second read within the minute asks nothing', async () => {
  const gh = fakeGh();
  const sat = createSatellite({ repo: REPO, exec: gh.exec, snapshot: async () => snapOf([]) });
  const out = await sat.read();
  assert.ok(out.ok, out.error);
  assert.equal(out.calls, gh.log.length);
  assert.ok(out.calls <= 14, `${out.calls} calls`);
  assert.deepEqual(out.rows.map((r) => r.number).sort((a, b) => a - b), [8, 10, 20, 101], 'every PR is a row, the promotion PR too');
  assert.ok(rowOf(out, 101).promo);
  assert.deepEqual(out.workflows.map((w) => [w.label, w.s]), [['gate main', 'done'], ['gate staging', 'fail'], ['release', 'done'], ['nightly-canary', 'done']], 'a workflow whose file is gone is not listed; a quiet one is asked for by itself');
  assert.equal(sig(rowOf(out, 20).cells.land), 'run queued 1', 'the merge queue answered');
  assert.equal(sig(rowOf(out, 8).cells.test), 'done');
  assert.equal(sig(rowOf(out, 8).cells.prod), 'wait', 'prod holds m1, whose newest staging commit is c3 (found by compare)');
  assert.equal(out.stages.find((s) => s.key === 'prod').t, 'm1');
  const before = gh.log.length;
  const again = await sat.read();
  assert.ok(again.ok);
  assert.equal(gh.log.length, before, 'served from the last read');
  // every call was a read
  for (const args of gh.log) assert.doesNotThrow(() => guard(args));
});

test('what never changes is not asked twice: finished runs\' jobs, and a commit\'s place on the line', async () => {
  const gh = fakeGh();
  const sat = createSatellite({ repo: REPO, exec: gh.exec, snapshot: async () => null });
  const first = await sat.gather();
  const second = await sat.gather();
  assert.ok(second.calls < first.calls, `${first.calls} then ${second.calls}`);
  const asked = (re) => gh.log.filter((a) => re.test(a.join(' '))).length;
  assert.equal(asked(/actions\/runs\/\d+\/jobs/), 2, 'each finished run\'s jobs once');
  assert.equal(asked(/compare\//), 1, 'the merge base of m1 once');
});

test('a failed first read says so', async () => {
  const gh = fakeGh({ fail: true });
  const quiet = console.error; console.error = () => {};
  try {
    const bad = await createSatellite({ repo: REPO, exec: gh.exec, snapshot: async () => null }).read();
    assert.equal(bad.ok, false);
    assert.match(bad.error, /not logged in/);
  } finally { console.error = quiet; }
});

test('a minute on, the next read refreshes behind the answer; one that fails keeps the good rows, marked stale', async () => {
  const state = {}, gh = fakeGh(state);
  let t = Date.now();
  const sat = createSatellite({ repo: REPO, exec: gh.exec, snapshot: async () => null, now: () => t });
  const settle = () => new Promise((r) => setTimeout(r, 30));
  const first = await sat.read();
  const calls = () => gh.log.length;
  const c0 = calls();
  t += 20_000; await sat.read({ fresh: true }); await settle();
  assert.ok(calls() > c0, 'a page that asks for a fresh read gets one (behind its answer)');
  const c1 = calls();
  t += 3_000; await sat.read({ fresh: true }); await settle();
  assert.equal(calls(), c1, 'but not again within a few seconds');
  t += 30_000; await sat.read(); await settle();
  assert.equal(calls(), c1, 'under a minute since the last read: none');
  t += 61_000; await sat.read(); await settle();
  assert.ok(calls() > c1, 'over a minute: read again');
  state.fail = true;
  const quiet = console.error; console.error = () => {};
  try {
    t += 61_000;
    const served = await sat.read(); // the old rows at once ...
    assert.ok(served.ok);
    await settle();
    const later = await sat.read(); // ... and the next one knows the refresh failed
    assert.ok(later.ok);
    assert.equal(later.rows.length, first.rows.length);
    assert.match(later.stale, /not logged in/);
  } finally { console.error = quiet; }
});

test('a PR that landed after the staging line was read makes the line be read once more', async () => {
  const gh = fakeGh({ recent: true });
  const inner = gh.exec;
  let lineReads = 0;
  const exec = async (args) => {
    if (args.join(' ').includes('/commits?')) { gh.log.push(args); return JSON.stringify(++lineReads === 1 ? LINE.slice(1) : LINE); } // the first answer lacks c9, the newest
    return inner(args);
  };
  const out = await createSatellite({ repo: REPO, exec, snapshot: async () => null }).gather();
  assert.equal(lineReads, 2);
  assert.equal(out.line.length, LINE.length);
});

test('a snapshot that never answers does not hold the page up', async () => {
  const gh = fakeGh();
  const sat = createSatellite({ repo: REPO, exec: gh.exec, snapshot: () => new Promise(() => {}) });
  const t = Date.now();
  const out = await sat.read();
  assert.ok(out.ok);
  assert.ok(Date.now() - t < 6_000);
  assert.equal(out.stages[0].t, 'none on a branch');
});
