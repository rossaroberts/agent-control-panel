// Satellite, the panel's side: one repo's delivery pipeline as a table flow, from the agent doing the work to production.
//
// A row is a piece of work: an open PR, a PR merged in the last few days, or a branch an agent is on that has no PR yet.
// A column is a stage it passes through:
//
//   Agent      the Fleet session on that PR's branch (the panel already has it: nothing is asked of GitHub)
//   PR         draft, open or merged                                  gh pr list (open, and merged in the window)
//   Checks     the gate's four required contexts, failing one named   the PR's statusCheckRollup
//   Land       queued, landing, conflict, or merged into staging      the merge queue, the `land` job, mergeStateStatus
//   Promotion  is it in the open (or the latest) staging → main PR    that PR's head, and its checks
//   TEST       is it in what the last good `deploy-test` put there    release.yml runs, then their jobs
//   Prod       is it in what the last good `deploy-prod` put there    the same
//
// Everything here only reads GitHub (gh pr list, gh api GETs, one GraphQL query for the merge queue): a guard in front of gh
// refuses anything else, so a bug in here cannot merge, close, comment or dispatch.
//
// "Is this PR in that deploy?" is commit ancestry. Staging only ever takes squash commits, so its history is one straight
// line, and "which staging commits does T contain" is one number: the newest of them that T has (T is a promotion's merge
// commit, so that is the promotion's head, already on the line; otherwise the compare API's merge base). A PR is in T when
// its squash commit is that old or older. That is one list of staging commits plus a lookup per PR, not a compare per pair.
// A commit that is not on the line (a fix merged straight into main) takes one compare per deploy, kept for good: a pair of
// commits never changes its answer.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFile } from 'node:child_process';

export const REPO = 'Clinton-Health-Access-Initiative/chai-expense-tracker';
// The gate's four required contexts (gate.yml pins their names; renaming one bricks every PR).
export const REQUIRED = ['static', 'db', 'migrations-append-only', 'definer-redefine-guard'];
const TTL_MS = 60_000;      // GitHub is read at most this often while a page watches ...
const RETRY_MS = 15_000;    // ... and a read that failed waits this long before the next try
const SNAP_MS = 3_000;      // Fleet's snapshot is usually instant; never let it hold the page up
const DAYS = 3;
const OPEN_FIELDS = 'number,title,headRefName,headRefOid,baseRefName,isDraft,mergeStateStatus,labels,author,createdAt,updatedAt,statusCheckRollup,url';
const MERGED_FIELDS = 'number,title,headRefName,headRefOid,baseRefName,createdAt,mergedAt,mergeCommit,author,url,labels';
const QUEUE_QUERY = 'query($o:String!,$r:String!){repository(owner:$o,name:$r){s:mergeQueue(branch:"staging"){entries(first:50){nodes{position state pullRequest{number}}}} m:mergeQueue(branch:"main"){entries(first:50){nodes{position state pullRequest{number}}}}}}';

const short = (sha) => String(sha ?? '').slice(0, 7);

/** The app's routes as docs/generated/routes.md lists them (`- \`/path\``), in order. */
export function routesIn(md) {
  return [...String(md ?? '').matchAll(/^- `(\/[^`\s]*)`/gm)].map((m) => m[1]);
}
/** The pages a change reaches, as routes of the app, for a link onto TEST: a page file is its route (`app/finance/vat/page.tsx`
 *  → `/finance/vat`; a `(group)` folder is dropped, a `[dynamic]` segment and what follows cut, then the nearest route the app
 *  lists); a feature slice (`features/<slice>/…`) is the shortest listed routes with a segment of that name. Shortest first. */
export function pagesOf(files, routes) {
  const known = new Set(routes ?? []), out = new Set();
  const nearest = (segs) => {
    for (let i = segs.length; i >= 1; i--) { const r = '/' + segs.slice(0, i).join('/'); if (known.has(r)) return r; }
    return !segs.length && known.has('/') ? '/' : null;
  };
  for (const f of files ?? []) {
    const page = /^app\/(?:(.*)\/)?page\.tsx$/.exec(f);
    if (page) {
      const segs = (page[1] ?? '').split('/').filter((s) => s && !/^\(.*\)$/.test(s));
      const cut = segs.findIndex((s) => s.startsWith('['));
      const r = nearest(cut >= 0 ? segs.slice(0, cut) : segs);
      if (r) out.add(r);
      continue;
    }
    const slice = /^features\/([^/]+)\//.exec(f)?.[1];
    if (!slice) continue;
    const hits = [...known].filter((r) => !r.includes('[') && r.split('/').slice(1).some((s) => s === slice || s === `${slice}s` || s.startsWith(`${slice}-`)));
    const depth = Math.min(...hits.map((r) => r.split('/').length));
    for (const r of hits) if (r.split('/').length === depth) out.add(r);
  }
  return [...out].sort((a, b) => a.length - b.length || a.localeCompare(b));
}
const MIN = 60_000, HOUR = 3_600_000;
/** The stations of the flow, in order, and the one a row is at: a branch an agent works on with no PR yet is building or
 *  testing it (as its session's tools say); a draft is at PR; an open PR at Checks until they pass, then at Land; then
 *  staging, the promotion, TEST, prod. */
export const STATIONS = [
  { key: 'build', name: 'Building' }, { key: 'tests', name: 'Local tests' }, { key: 'pr', name: 'PR' }, { key: 'checks', name: 'Checks' },
  { key: 'land', name: 'Land' }, { key: 'staging', name: 'Staging' }, { key: 'promotion', name: 'Promotion' }, { key: 'test', name: 'TEST' }, { key: 'prod', name: 'Prod' },
];
export function stationOf(r) {
  const st = r.sat?.stage;
  if (r.kind === 'branch' || st === 'branch') return r.cells?.agent?.stage === 'testing' ? 'tests' : 'build';
  if (st === 'draft') return 'pr';
  if (st === 'open') return r.cells?.checks?.s === 'done' ? 'land' : 'checks';
  if (st === 'queued' || st === 'landing') return 'land';
  if (st === 'on staging') return 'staging';
  if (st === 'in promotion') return 'promotion';
  if (st === 'in main' || st === 'on TEST') return 'test';
  if (st === 'in prod') return 'prod';
  if (st === 'deploying' || st === 'deploy failed') return r.cells?.prod?.s === 'run' || r.cells?.prod?.s === 'fail' ? 'prod' : 'test';
  return null;
}

/** How long something has sat: 12m, 3h, 2d. */
export const span = (t) => { const s = Math.max(0, t) / 1000; return s < 60 ? 'now' : s < 3600 ? `${Math.round(s / 60)}m` : s < 86400 ? `${Math.round(s / 3600)}h` : `${Math.round(s / 86400)}d`; };
// How long a stage may sit before the work is stuck in it. Each is a judgment about this repo's rhythm (the gate lands a green PR
// in minutes; promotions are cut by hand a few times a day; a deploy takes about twenty minutes), so they are all here.
export const STALL = {
  quiet: 2 * HOUR,       // a draft nobody has touched, whose checks are green or nearly
  dormant: 7 * 24 * HOUR, // ... but one untouched for a week is parked, not stalled: its age shows, it is not in the Stuck band
  green: 30 * MIN,       // an open PR green and not landed (the gate lands one within minutes)
  idle: 30 * MIN,        // an agent that stopped, with its PR (or branch) still going nowhere
  staging: 8 * HOUR,     // merged into staging, in no promotion (promotions come a few times a day: over 120 of them the gaps' median was 3 h, three in four within 7 h)
  promotion: 2 * HOUR,   // in a promotion PR that is still open
  main: 1 * HOUR,        // in main, not on TEST (a deploy takes about twenty minutes)
  test: 2 * HOUR,        // on TEST, not in prod
};
const ms = (t) => { const v = Date.parse(t ?? ''); return Number.isFinite(v) && v > 0 && !String(t).startsWith('0001') ? v : null; };

// ------------------------------------------------------------------------------------------------ read-only gh

/** Throws unless these gh arguments only read: a PR or run listing/view, an `api` GET, or a GraphQL `query`. */
export function guard(args) {
  const [a, b] = args;
  // Never --web: that opens a browser on Ross's screen.
  if ((a === 'pr' || a === 'run') && (b === 'list' || b === 'view')) { if (args.some((x) => x === '--web' || x === '-w')) throw new Error('satellite never opens a browser'); return; }
  if (a !== 'api') throw new Error(`satellite only reads GitHub: gh ${args.slice(0, 2).join(' ')} is not allowed`);
  // `gh api` turns into a POST with -f/-F/--input and into anything with -X: so none of them, except GraphQL's own query.
  const graphql = args[1] === 'graphql';
  for (let i = 2; i < args.length; i++) {
    const x = args[i];
    // Every spelling gh takes: -X POST, -XPOST, -X=POST, --method POST, --method=POST, --input file, --input=file, -H with an override.
    if (/^(-X|--method|--input)(=|$)/.test(x) || /^-X./.test(x) || /^--input./.test(x) || (/^(-H|--header)/.test(x) && /x-http-method-override/i.test(`${x} ${args[i + 1] ?? ''}`))) throw new Error('satellite only reads GitHub: no method or body');
    if (/^(-f|-F|--field|--raw-field)(=|$)/.test(x) || /^-[fF]./.test(x)) {
      if (!graphql) throw new Error('satellite only reads GitHub: a REST call takes no fields');
      // The field is in the same word (-fk=v, --field=k=v) or the next one (-f k=v).
      const inWord = /^-[fF]./.test(x) || x.includes('=');
      const v = /^-[fF]./.test(x) ? x.slice(2) : inWord ? x.slice(x.indexOf('=') + 1) : String(args[i + 1] ?? '');
      if (!inWord) i++;
      if (v.startsWith('query=') && (!/^query\s*[({]/.test(v.slice(6)) || /\b(mutation|subscription)\b/i.test(v))) throw new Error('satellite only reads GitHub: a GraphQL query, never a mutation');
    }
  }
}

/** The real gh. launchd gives the panel a PATH with Homebrew on it; the full path is the fallback. */
export function execGh(args, { timeout = 40_000 } = {}) {
  const bin = process.env.SATELLITE_GH || (fs.existsSync('/opt/homebrew/bin/gh') ? '/opt/homebrew/bin/gh' : 'gh');
  return new Promise((resolve, reject) => {
    execFile(bin, args, { timeout, maxBuffer: 64_000_000, env: { ...process.env, GH_PROMPT_DISABLED: '1', NO_COLOR: '1', GH_NO_UPDATE_NOTIFIER: '1' } },
      (err, stdout, stderr) => (err ? reject(new Error(String(stderr || err.message).trim().split('\n').at(-1).slice(0, 300))) : resolve(String(stdout))));
  });
}

// ------------------------------------------------------------------------------------------------ checks

const FAILED = new Set(['FAILURE', 'TIMED_OUT', 'STARTUP_FAILURE', 'ACTION_REQUIRED', 'ERROR']);
/** passed | failed | cancelled | skipped | running | queued, from a check run or a commit status. */
export function checkState(c) {
  if (c.__typename === 'StatusContext' || (c.context && !c.name)) {
    const s = String(c.state ?? '').toUpperCase();
    return s === 'SUCCESS' ? 'passed' : s === 'PENDING' || s === 'EXPECTED' ? 'running' : 'failed';
  }
  const status = String(c.status ?? '').toUpperCase(), done = String(c.conclusion ?? '').toUpperCase();
  if (status !== 'COMPLETED') return status === 'IN_PROGRESS' ? 'running' : 'queued';
  if (done === 'SUCCESS') return 'passed';
  if (FAILED.has(done)) return 'failed';
  if (done === 'CANCELLED') return 'cancelled';
  return 'skipped'; // SKIPPED, NEUTRAL, STALE
}

const LAND = /^land on green/i;
const BATTERY = /^battery/i;
/** What a PR's check rollup says. A re-run adds a second check with the same name: the newest one speaks. The `land` job is
 *  the Land stage's, not a test, so it is read apart. */
export function summarizeChecks(rollup) {
  // A new run of a workflow supersedes the whole of its last one: a job of the old run that finished after the new one began
  // must not speak for a job the new run has not reached (an old `static` failure over a running `quality`, PR #5874).
  const runOf = (c) => { const m = /\/actions\/runs\/(\d+)(?:\/|$)/.exec(c.detailsUrl ?? ''); return m ? Number(m[1]) : null; };
  const newest = new Map(); // workflow → its newest run among these checks
  for (const c of rollup ?? []) { const id = runOf(c), wf = c.workflowName ?? ''; if (id != null && id > (newest.get(wf) ?? 0)) newest.set(wf, id); }
  const current = (c) => { const id = runOf(c); return id == null || id === newest.get(c.workflowName ?? ''); };
  const latest = new Map();
  for (const c of rollup ?? []) {
    const name = c.name ?? c.context;
    if (!name || !current(c)) continue;
    const t = ms(c.startedAt) ?? 0, was = latest.get(name);
    if (!was || t >= was.t) latest.set(name, { name, t, done: ms(c.completedAt), state: checkState(c), url: c.detailsUrl ?? c.targetUrl ?? null });
  }
  const all = [...latest.values()];
  const tests = all.filter((c) => !LAND.test(c.name));
  const by = Object.fromEntries(all.map((c) => [c.name, c.state]));
  const bad = tests.filter((c) => c.state === 'failed' || c.state === 'cancelled');
  // Name the job that broke, not the gate context that only reports it: `quality` before `static`.
  bad.sort((x, y) => Number(REQUIRED.includes(x.name)) - Number(REQUIRED.includes(y.name)));
  const running = tests.filter((c) => c.state === 'running' || c.state === 'queued');
  const required = REQUIRED.map((name) => ({ name, state: by[name] ?? 'none' }));
  // The gate's contexts are made as its jobs reach them: until all four are there, nothing has passed ("0 of 4, passed" is
  // a gate that has not reported yet).
  const missing = tests.length > 0 && required.some((r) => r.state === 'none');
  const state = bad.length ? 'failed' : running.length || missing ? 'running' : tests.length ? 'passed' : 'none';
  const battery = tests.find((c) => BATTERY.test(c.name));
  return {
    state, by, required, total: tests.length,
    passed: required.filter((r) => r.state === 'passed' || r.state === 'skipped').length,
    failing: bad.map((c) => c.name), running: running.map((c) => c.name),
    land: all.find((c) => LAND.test(c.name))?.state ?? null,
    landUrl: all.find((c) => LAND.test(c.name))?.url ?? null,
    battery: battery?.state ?? null,
    url: bad[0]?.url ?? null,
    // Jobs that really broke (the gate context that reports them does not count twice): "two reds" is two of these.
    reds: bad.filter((c) => !REQUIRED.includes(c.name)).length || bad.length,
    // When the last of them finished, for a run that is all green.
    doneAt: state === 'passed' ? Math.max(0, ...tests.map((c) => c.done ?? 0)) || null : null,
  };
}

const GLYPH = { passed: '✓', skipped: '–', running: '●', queued: '●', failed: '✕', cancelled: '✕', none: '·' };
const tipOf = (sum) => sum.required.map((r) => `${GLYPH[r.state]} ${r.name}`).join('\n')
  + (sum.failing.length ? `\nfailing: ${sum.failing.join(', ')}` : '') + (sum.running.length ? `\nrunning: ${sum.running.join(', ')}` : '');

// ------------------------------------------------------------------------------------------------ promotions and deploys

/** A promotion is the PR that carries staging into main (ship cuts a release/* branch for it). */
export function isPromotion(pr) {
  return pr.baseRefName === 'main' && (pr.headRefName === 'staging' || /^(release|promote)\//i.test(pr.headRefName ?? '') || /^(promote|release)\b\s*[:(]/i.test(pr.title ?? ''));
}

/** The post-merge proof of staging: gate's own runs on the staging branch (ship promotes only a head one of them proved
 *  green). The newest green one, how many reds have come since it (and since when), and one still running. */
export function stagingProof(gateRuns) {
  const runs = [...(gateRuns ?? [])].sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at));
  const done = (r) => r.status === 'completed';
  const green = runs.find((r) => done(r) && r.conclusion === 'success') ?? null;
  const reds = runs.filter((r) => done(r) && ['failure', 'timed_out', 'startup_failure'].includes(r.conclusion) && (!green || Date.parse(r.created_at) > Date.parse(green.created_at)));
  const cell = (r) => (r ? { sha: r.head_sha, at: ms(r.updated_at) ?? ms(r.created_at), url: r.html_url ?? null } : null);
  return { green: cell(green), reds: reds.length, red: cell(reds[0] ?? null), redSince: reds.length ? ms(reds.at(-1).run_started_at) ?? ms(reds.at(-1).created_at) : null,
    running: cell(runs.find((r) => !done(r)) ?? null), total: runs.length };
}

/** TEST and Prod from release.yml's runs (newest first) and their jobs (`deploy-test`, `deploy-prod`; the one that did
 *  not run is "skipped"). For each: the newest job that worked (ok), and what is newer than it: still going (active) or
 *  broken (failed). Both need only the newest ones, so the caller fetches jobs a few runs at a time until `complete`. */
export function pickDeploys(runs, jobsOf) {
  const out = { test: {}, prod: {}, complete: false };
  for (const run of runs) {
    for (const j of jobsOf.get(run.id) ?? []) {
      const env = j.name === 'deploy-test' ? 'test' : j.name === 'deploy-prod' ? 'prod' : null;
      if (!env || out[env].ok) continue; // anything older than a deploy that worked is history
      const info = { sha: j.head_sha || run.head_sha, runId: run.id, url: j.html_url ?? run.html_url ?? null,
        at: ms(j.completed_at) ?? ms(run.updated_at), startedAt: ms(j.started_at) ?? ms(run.created_at) };
      if (j.conclusion === 'success') out[env].ok = info;
      else if (j.status && j.status !== 'completed') out[env].active ??= info;
      else if (['failure', 'cancelled', 'timed_out'].includes(j.conclusion)) out[env].failed ??= info;
    }
  }
  out.complete = Boolean(out.test.ok && out.prod.ok);
  return out;
}

// ------------------------------------------------------------------------------------------------ workflows

const WORKFLOW_ORDER = ['gate', 'ship', 'release', 'full-test', 'nightly-canary', 'promotion-watchdog'];
const RUN_FIELDS = '[.workflow_runs[] | {id,name,event,status,conclusion,head_branch,head_sha,created_at,updated_at,run_started_at,html_url,workflow_id}]';
/** A workflow run as a stage cell: ✓ ✕ ● ·. A cancelled or skipped one is neither good nor bad news. */
export function runCell(r) {
  const done = r.status === 'completed', c = String(r.conclusion ?? '');
  const s = !done ? 'run' : c === 'success' ? 'done' : ['failure', 'timed_out', 'startup_failure', 'action_required'].includes(c) ? 'fail' : 'wait';
  return { s, t: !done ? r.status?.replace('_', ' ') : s === 'wait' ? c : '', at: !done ? ms(r.run_started_at) ?? ms(r.created_at) : ms(r.updated_at) ?? ms(r.created_at),
    href: r.html_url ?? null, branch: r.head_branch ?? null, event: r.event ?? null };
}
/** The last run of every workflow file on staging. `runs` are the newest runs on staging and main (newest first), `extra` the
 *  last run of a workflow they hold none of (a nightly job a busy week has pushed out of them). Gate is read twice, because
 *  its run on staging (the post-merge proof) and its run on main (a promotion's) are different news. */
export function pickWorkflows(files, workflows, runs, extra = new Map()) {
  const out = [];
  for (const w of workflows ?? []) {
    if (!(files ?? []).includes(w.path)) continue; // a workflow whose file is gone, or one GitHub makes itself (Dependabot)
    const mine = (runs ?? []).filter((r) => r.workflow_id === w.id);
    const picks = w.name === 'gate' ? ['staging', 'main'].map((b) => [b, mine.find((r) => r.head_branch === b)]) : [[null, mine[0] ?? extra.get(w.id) ?? null]];
    for (const [branch, r] of picks) {
      if (!r) { if (w.name !== 'gate') out.push({ key: w.name, name: w.name, label: w.name, s: 'wait', t: 'no runs', at: null, href: null, none: true }); continue; }
      out.push({ key: branch ? `${w.name}:${branch}` : w.name, name: w.name, label: branch ? `${w.name} ${branch}` : w.name, ...runCell(r) });
    }
  }
  const rank = (x) => { const i = WORKFLOW_ORDER.indexOf(x.name); return i < 0 ? WORKFLOW_ORDER.length : i; };
  return out.sort((a, b) => rank(a) - rank(b) || a.label.localeCompare(b.label));
}

// ------------------------------------------------------------------------------------------------ agents

const gitCache = new Map();
/** The branch and remotes of the checkout a folder is in (a worktree's `.git` is a file that points at its own folder). */
export function gitInfo(cwd) {
  const hit = gitCache.get(cwd);
  if (hit && Date.now() - hit.at < 10_000) return hit.v;
  let v = null, dir = cwd;
  for (let i = 0; i < 8 && !v; i++) {
    const dot = path.join(dir, '.git');
    let st = null;
    try { st = fs.statSync(dot); } catch {}
    if (st) {
      let gitdir = dot, common = dot;
      try {
        if (st.isFile()) {
          const m = /^gitdir:\s*(.+)$/m.exec(fs.readFileSync(dot, 'utf8'));
          if (m) {
            gitdir = path.resolve(dir, m[1].trim());
            try { common = path.resolve(gitdir, fs.readFileSync(path.join(gitdir, 'commondir'), 'utf8').trim()); } catch { common = gitdir; }
          }
        }
        let head = '', cfg = '';
        try { head = fs.readFileSync(path.join(gitdir, 'HEAD'), 'utf8').trim(); } catch {}
        try { cfg = fs.readFileSync(path.join(common, 'config'), 'utf8'); } catch {}
        const b = /^ref:\s*refs\/heads\/(.+)$/.exec(head);
        v = { branch: b ? b[1] : null, worktree: st.isFile(), remotes: [...cfg.matchAll(/^\s*url\s*=\s*(\S+)/gm)].map((m) => m[1]) };
      } catch { v = null; }
      break;
    }
    const up = path.dirname(dir);
    if (up === dir) break;
    dir = up;
  }
  gitCache.set(cwd, { at: Date.now(), v });
  return v;
}

const NOT_WORK = new Set(['main', 'master', 'staging', 'HEAD']);
const AGENT_RANK = { 'needs-you': 0, working: 1, capped: 2, ready: 3 };

/** The open sessions of Fleet's snapshot that are on a branch of this repo (`shared`: started in the repo's own checkout, so
 *  the branch is only the one that checkout has out). A session's `branch` is used when Fleet gives
 *  one; on this Mac the branch is read from the folder's own git files. A session on another machine without a branch
 *  cannot be placed, and is only counted (`unknown`) when its folder looks like this repo's. */
export function placeAgents(snap, { repo = REPO, home = os.homedir() } = {}) {
  const placed = [];
  let unknown = 0;
  const name = repo.split('/').at(-1).toLowerCase();
  const slug = repo.toLowerCase();
  for (const m of snap?.machines ?? []) {
    if (m.ok === false) continue;
    for (const a of m.accounts ?? []) for (const x of a.sessions ?? []) {
      if (x.state === 'closed' || !x.state) continue;
      // Where it works: the folder its tools go into (a worktree of its own, often), else where it was started.
      const cwd = String(x.workdir ?? x.cwd ?? '');
      const abs = cwd.startsWith('~/') ? path.join(home, cwd.slice(2)) : cwd;
      const nameIn = abs.toLowerCase().split(path.sep).some((p) => p === name);
      let branch = typeof x.branch === 'string' && x.branch ? x.branch : null;
      // Is it this repo? true or false where the checkout says (its remote), else the folder's name, else nobody knows (null).
      let known = nameIn ? true : null;
      // A folder that is the repo's own checkout (not a worktree made for the work) is shared by every session started in it:
      // the branch it has out says what the checkout is on, not what each of those sessions is doing. Elsewhere that is told
      // by the path (a worktree lives under a "worktrees" folder, or has a folder of its own).
      let shared = nameIn && !/worktrees?/i.test(abs);
      if (m.local && abs) {
        const g = gitInfo(abs);
        if (g) {
          if (g.remotes.length) known = g.remotes.some((u) => u.toLowerCase().replace(/\.git$/, '').endsWith(slug));
          branch ??= g.branch;
          shared = known === true && !g.worktree;
        }
      }
      if (known === false) continue; // a checkout of some other repo, whatever its folder is called
      if (!branch) { if (known) unknown++; continue; }
      if (NOT_WORK.has(branch)) continue;
      placed.push({ machine: m.name, account: x.sub ?? a.id ?? null, engine: a.engine, sessionId: x.sessionId ?? null, title: x.title ?? x.name ?? null,
        state: x.state, stage: x.stage ?? null, lastAt: x.lastAt ?? x.startedAt ?? null, branch, repoKnown: known === true, shared,
        ...(shared ? { words: [x.said, x.doing, x.running?.step, x.running?.cmd].filter((v) => typeof v === 'string').join('\n').slice(0, 2000) } : {}) });
    }
  }
  return { placed, unknown };
}

const byRank = (a, b) => (AGENT_RANK[a.state] ?? 9) - (AGENT_RANK[b.state] ?? 9) || (b.lastAt ?? 0) - (a.lastAt ?? 0);
const AGENT_CELL = { 'needs-you': 'need', working: 'run', capped: 'fail', ready: 'wait' };

// ------------------------------------------------------------------------------------------------ composing the table

/** Does T hold the commit x? On the staging line: x is as old as the newest staging commit T has, or older. Off it: the
 *  compare answer gathered for that pair. T is a commit sha; `line` maps staging shas to their place (0 = newest). */
export function containedIn(line, rep, pairs, x, T) {
  if (!T || !x) return false;
  if (line.has(x)) {
    const at = line.has(T) ? line.get(T) : rep?.[T] != null && line.has(rep[T]) ? line.get(rep[T]) : Infinity;
    return line.get(x) >= at;
  }
  return pairs?.[`${x}>${T}`] === true;
}

/** Stuck first (the way Fleet puts what needs you first): an agent that stopped, then green work not landed, then the later
 *  the stage the more it matters (on TEST, in main, in a promotion, on staging), then drafts; the longest-stuck first inside
 *  each. Then failing or blocked; then what is still on its way; then what has shipped, newest activity first. */
export function orderRows(rows) {
  const lane = (r) => (r.stuck ? 0 : r.attention ? 1 : r.shipped ? 3 : 2);
  const stuckFirst = (a, b) => (a.stuck.rank ?? 9) - (b.stuck.rank ?? 9) || (a.stuck.since ?? 0) - (b.stuck.since ?? 0);
  return [...rows].sort((a, b) => lane(a) - lane(b) || (lane(a) === 0 ? stuckFirst(a, b) : (b.at ?? 0) - (a.at ?? 0)) || String(a.key).localeCompare(String(b.key)));
}

/** The table, from what GitHub said (`raw`, see gather) and the agents on branches. Pure: no clock but `now`, no I/O. */
export function compose(raw, agents = { placed: [], unknown: 0 }, now = Date.now()) {
  const line = new Map((raw.line ?? []).map((s, i) => [s, i]));
  const has = (x, T) => containedIn(line, raw.rep, raw.pairs, x, T);
  const D = raw.deploys ?? { test: {}, prod: {} };
  const promos = raw.promos ?? { open: null, merged: [] };
  const openPromo = promos.open ? { ...promos.open, sum: summarizeChecks(promos.open.statusCheckRollup) } : null;
  const lastPromo = promos.merged.at(-1) ?? null;
  const queue = new Map((raw.queue ?? []).map((q) => [q.number, q]));
  // A session started in the shared checkout reports the branch that checkout has out, whatever it is doing in a worktree. When
  // its own words name exactly one open PR's branch (verbatim, with its slash), that is where it is working.
  const openHeads = [...new Set((raw.open ?? []).map((p) => p.headRefName))].filter((b) => b.includes('/') && b.length >= 8);
  const named = (text) => openHeads.filter((b) => new RegExp(`(^|[^\\w/.-])${b.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}($|[^\\w/-])`).test(text));
  const placed = agents.placed.map((a) => {
    const hit = a.shared && a.words ? named(a.words) : [];
    return hit.length === 1 ? { ...a, branch: hit[0], shared: false, inferred: true } : a;
  });
  const byBranch = new Map();
  for (const a of placed) { if (!byBranch.has(a.branch)) byBranch.set(a.branch, []); byBranch.get(a.branch).push(a); }
  // A session in a worktree of its own comes before one in the shared checkout; then the busier first.
  for (const l of byBranch.values()) l.sort((a, b) => Number(Boolean(a.shared)) - Number(Boolean(b.shared)) || byRank(a, b));
  const shown = new Set(); // the agents that ended up on a row: the Agent header counts these
  const take = (branch) => { const l = byBranch.get(branch) ?? []; for (const a of l) shown.add(a); return l; };

  const agentCell = (list) => {
    if (!list?.length) return null;
    const a = list[0];
    return { s: AGENT_CELL[a.state] ?? 'wait', t: a.title || 'agent', key: a.sessionId ? `${a.machine}:${a.sessionId}` : null, sub: a.shared ? `${a.machine} · shared` : a.machine, more: list.length - 1, stage: a.stage ?? null,
      tip: list.map((x) => `${x.machine}${x.account ? ' · ' + x.account : ''}: ${x.title ?? 'agent'} (${x.state})${x.shared ? ', in the shared checkout' : ''}${x.inferred ? ', its own words name this branch' : ''}`).join('\n') };
  };
  const checksCell = (sum) => {
    if (sum.state === 'none') return { s: 'wait', t: '' };
    if (sum.state === 'failed') return { s: 'fail', t: sum.failing.slice(0, 1).concat(sum.failing.length > 1 ? [`+${sum.failing.length - 1}`] : []).join(' '), tip: tipOf(sum), href: sum.url };
    if (sum.state === 'running') return { s: 'run', t: `${sum.passed}/${REQUIRED.length}${sum.battery === 'running' ? ' battery' : ''}`, tip: tipOf(sum) };
    return { s: 'done', t: `${sum.passed}/${REQUIRED.length}`, tip: tipOf(sum) };
  };
  const labelsOf = (pr) => (pr.labels ?? []).map((l) => l.name ?? l);
  const landOpen = (pr, sum) => {
    const q = queue.get(pr.number);
    if (q) return { s: 'run', t: q.position ? `queued ${q.position}` : 'queued', tip: `in the ${q.branch} merge queue` };
    if (sum.land === 'running' || sum.land === 'queued') return { s: 'run', t: 'landing' };
    if (!['staging', 'main'].includes(pr.baseRefName)) return { s: 'wait', t: 'stacked', tip: `into ${pr.baseRefName}, not into staging` };
    if (pr.mergeStateStatus === 'DIRTY') return { s: 'fail', t: 'conflict', tip: `conflicts with ${pr.baseRefName}: update the branch` };
    if (labelsOf(pr).includes('hold')) return { s: 'wait', t: 'hold' };
    if (sum.land === 'failed' || sum.land === 'cancelled') return { s: 'fail', t: 'land failed', tip: 'the gate\'s land job failed: open the run', href: sum.landUrl };
    if (!pr.isDraft && sum.state === 'passed') return { s: 'wait', t: 'ready', tip: 'every check passed; the gate lands it' };
    return { s: 'wait', t: '' };
  };
  // What the open promotion PR says of itself, as a cell.
  const promoState = (p) => {
    const sum = p.sum;
    if (sum.state === 'failed') return { s: 'fail', t: `#${p.number} ${sum.failing[0]}`.trim(), tip: tipOf(sum), href: p.url };
    if (sum.state === 'passed') return { s: 'run', t: `#${p.number} landing`, tip: tipOf(sum), href: p.url };
    return { s: 'run', t: `#${p.number}${sum.battery === 'running' ? ' battery' : ` ${sum.passed}/${REQUIRED.length}`}`, tip: tipOf(sum), href: p.url };
  };
  const deployCell = (env, x) => {
    const d = D[env] ?? {};
    if (d.ok && has(x, d.ok.sha)) return { s: 'done', t: '', at: d.ok.at, href: d.ok.url };
    if (d.active && has(x, d.active.sha)) return { s: 'run', t: 'deploying', at: d.active.startedAt, href: d.active.url };
    if (d.failed && has(x, d.failed.sha)) return { s: 'fail', t: 'failed', at: d.failed.at, href: d.failed.url };
    return null;
  };
  // The flow's frontier: a stage is only drawn as waiting when the one before it is done; further on stays blank.
  const downstream = (x, promoted) => {
    const test = deployCell('test', x) ?? (promoted ? { s: 'wait', t: '' } : null);
    const prod = deployCell('prod', x) ?? (test?.s === 'done' ? { s: 'wait', t: '' } : null);
    return { test, prod };
  };

  const rows = [];
  const onBranch = new Set();
  // How long the agents on a branch have been idle (all of them stopped), or 0 while any is at work.
  // Sessions in the shared checkout say nothing of this branch's work, so they do not count.
  const idleFor = (all) => { const ags = all.filter((a) => !a.shared); return ags.length && ags.every((a) => a.state === 'ready' && a.lastAt) ? now - Math.max(...ags.map((a) => a.lastAt)) : 0; };
  const LATE_RANK = { 'on TEST': 2, 'in main': 3, 'in promotion': 4, 'on staging': 5 };
  /** The stage a row sits at (`sat`: its name, since when, which cell it is drawn in) and, past its limit or for the other
   *  reasons in `why` ({ t, rank }), why it is stuck. */
  const settle = (row, sat, why = []) => {
    row.sat = sat ? { stage: sat.stage, since: sat.since } : null;
    const late = sat?.limit && sat.since && now - sat.since >= sat.limit ? { t: `${sat.stage} ${span(now - sat.since)}, ${sat.note}`, rank: LATE_RANK[sat.stage] ?? 5 } : null;
    const reasons = [late, ...why].filter(Boolean).sort((x, y) => x.rank - y.rank);
    row.stuck = reasons.length ? { why: reasons.map((r) => r.t).join(' · '), since: sat?.since ?? now, rank: reasons[0].rank, cell: sat?.cell ?? 'pr' } : null;
    const c = sat?.cell && row.cells[sat.cell];
    if (c && sat.since) row.cells[sat.cell] = { ...c, since: sat.since, ...(row.stuck ? { stuck: true } : {}) };
    return row;
  };
  for (const pr of raw.open ?? []) {
    onBranch.add(pr.headRefName);
    const ags = take(pr.headRefName);
    const sum = summarizeChecks(pr.statusCheckRollup);
    const upd = ms(pr.updatedAt) ?? 0, hold = labelsOf(pr).includes('hold'), q = queue.get(pr.number), landing = sum.land === 'running' || sum.land === 'queued';
    const landable = ['staging', 'main'].includes(pr.baseRefName); // a stacked PR waits for its parent, not for the gate
    const green = sum.state === 'passed', nearly = green || (sum.state === 'failed' && sum.reds <= 2);
    const greenSince = sum.doneAt ?? upd;
    const sat = { stage: pr.isDraft ? 'draft' : q ? 'queued' : landing ? 'landing' : 'open', since: upd, cell: pr.isDraft ? 'pr' : 'land' };
    const why = [], idle = idleFor(ags);
    if (!hold && landable) {
      if (pr.isDraft && nearly && now - upd >= STALL.quiet && now - upd < STALL.dormant) why.push({ t: `draft ${span(now - upd)}, checks ${green ? 'green' : `nearly green (${sum.reds} red)`}`, rank: 6 });
      if (!pr.isDraft && green && !q && !landing && pr.mergeStateStatus !== 'DIRTY' && now - greenSince >= STALL.green) { why.push({ t: `green ${span(now - greenSince)}, not landed`, rank: 1 }); sat.since = greenSince; }
      if (idle >= STALL.idle && !q && !landing) why.push({ t: `agent idle ${span(idle)}, PR not landed`, rank: 0 });
    }
    rows.push(settle({
      key: `pr:${pr.number}`, kind: 'pr', state: pr.isDraft ? 'draft' : 'open', number: pr.number, title: pr.title, branch: pr.headRefName, base: pr.baseRefName,
      url: pr.url, author: pr.author?.login ?? null, bot: Boolean(pr.author?.is_bot), promo: isPromotion(pr), labels: labelsOf(pr).filter((l) => l === 'hold' || l.startsWith('risk:')),
      at: Math.max(upd, ...ags.map((a) => a.lastAt ?? 0)),
      cells: { agent: agentCell(ags), pr: { s: pr.isDraft ? 'wait' : 'run', t: pr.isDraft ? 'draft' : 'open', at: upd }, checks: checksCell(sum),
        land: landOpen(pr, sum), promotion: null, test: null, prod: null },
      draft: pr.isDraft, checksFailed: sum.state === 'failed', conflict: pr.mergeStateStatus === 'DIRTY',
    }, sat, why));
  }
  for (const pr of raw.merged ?? []) {
    onBranch.add(pr.headRefName);
    const ags = take(pr.headRefName);
    const sha = pr.mergeCommit?.oid ?? null, promoPr = isPromotion(pr);
    const viaStaging = pr.baseRefName === 'staging' && !promoPr, stacked = !['staging', 'main'].includes(pr.baseRefName);
    let promotion = null, test = null, prod = null;
    const promo = viaStaging ? promos.merged.find((p) => has(sha, p.head)) ?? null : null;
    if (viaStaging) {
      if (promo) promotion = { s: 'done', t: `#${promo.number}`, at: ms(promo.mergedAt), href: promo.url };
      else if (openPromo && has(sha, openPromo.head)) promotion = promoState(openPromo);
      else promotion = { s: 'wait', t: '' };
    }
    // A promotion PR is its own row: what it carried is on TEST and prod when its head is. A PR merged into another
    // branch (a stack) travels with that branch's PR, so it has no later stages of its own.
    if (promoPr) ({ test, prod } = downstream(pr.headRefOid, true));
    else if (!stacked) ({ test, prod } = downstream(sha, Boolean(promo) || !viaStaging));
    // Where it sits now, and since when: in main from its promotion's merge (or its own, for a fix straight into main).
    const mergedAt = ms(pr.mergedAt), inMain = promoPr ? mergedAt : promo ? ms(promo.mergedAt) : !viaStaging && !stacked ? mergedAt : null;
    let sat = null;
    const why = [];
    // On staging and in no promotion: ship promotes the newest head the staging proof (gate on staging) found green, so a
    // commit behind a red streak is not going anywhere until that is fixed; it says so.
    const proof = raw.proof;
    if (viaStaging && !promo && !(openPromo && has(sha, openPromo.head)) && proof?.reds && !(proof.green && has(sha, proof.green.sha))) {
      why.push({ t: `staging proof red ×${proof.reds}${proof.redSince ? `, ${span(now - proof.redSince)}` : ''}`, rank: 4 });
    }
    if (!stacked) {
      if (prod?.s === 'done') sat = { stage: 'in prod', since: inMain ?? mergedAt };
      else if (prod?.s === 'run' || test?.s === 'run') sat = { stage: 'deploying', since: (prod?.s === 'run' ? prod : test).at, cell: prod?.s === 'run' ? 'prod' : 'test' };
      else if (prod?.s === 'fail' || test?.s === 'fail') sat = { stage: 'deploy failed', since: (prod?.s === 'fail' ? prod : test).at, cell: prod?.s === 'fail' ? 'prod' : 'test' };
      else if (test?.s === 'done') sat = { stage: 'on TEST', since: inMain, cell: 'prod', limit: STALL.test, note: 'not in prod' };
      else if (inMain != null) sat = { stage: 'in main', since: inMain, cell: 'test', limit: STALL.main, note: 'not on TEST' };
      else if (openPromo && has(sha, openPromo.head)) sat = { stage: 'in promotion', since: ms(openPromo.createdAt), cell: 'promotion', limit: STALL.promotion, note: `#${openPromo.number} not merged` };
      else if (viaStaging) sat = { stage: 'on staging', since: mergedAt, cell: 'promotion', limit: STALL.staging, note: 'not in a promotion' };
      if (promoPr && sat) delete sat.limit; // a promotion is the carrier: what it carries is what is stuck
    }
    rows.push(settle({
      key: `pr:${pr.number}`, kind: 'pr', state: 'merged', number: pr.number, title: pr.title, branch: pr.headRefName, base: pr.baseRefName, url: pr.url,
      author: pr.author?.login ?? null, bot: Boolean(pr.author?.is_bot), promo: promoPr, labels: [], at: Math.max(mergedAt ?? 0, ...ags.map((a) => a.lastAt ?? 0)),
      pages: promoPr ? [] : pagesOf(raw.files?.[pr.number], raw.routes),
      cells: { agent: agentCell(ags), pr: { s: 'done', t: 'merged', at: mergedAt }, checks: { s: 'done', t: '' },
        land: { s: 'done', t: viaStaging ? 'staging' : stacked ? `into ${pr.baseRefName}` : 'main', at: mergedAt }, promotion, test, prod },
    }, sat, why));
  }
  // A branch an agent is on that has no PR yet: of this repo by its folder, or because GitHub has a branch of that name.
  const windowStart = (raw.at ?? now) - ((raw.days ?? DAYS) + 1) * 86_400_000;
  for (const [branch, list] of byBranch) {
    const info = agents.branches?.[branch];
    // Only a session in a worktree of its own makes a branch a piece of work: the shared checkout is on whatever it is on.
    if (onBranch.has(branch) || !list.some((a) => !a.shared) || !(list.some((a) => a.repoKnown) || info?.exists)) continue;
    take(branch);
    // Commits that nothing has carried on: the branch is on GitHub and its tip is not a staging commit.
    const ahead = Boolean(info?.exists && info.sha && !line.has(info.sha) && (ms(info.at) ?? 0) > windowStart), idle = idleFor(list);
    const since = idle ? now - idle : Math.max(...list.map((a) => a.lastAt ?? 0));
    const why = ahead && idle >= STALL.idle ? [{ t: `agent idle ${span(idle)}, commits on the branch, no PR`, rank: 0 }] : [];
    rows.push(settle({ key: `br:${branch}`, kind: 'branch', state: 'branch', number: null, title: branch, branch, base: null, url: `https://github.com/${raw.repo}/tree/${branch}`,
      author: null, labels: [], at: Math.max(...list.map((a) => a.lastAt ?? 0)),
      cells: { agent: agentCell(list), pr: { s: 'wait', t: info && !info.exists ? 'no PR · not pushed' : 'no PR yet' }, checks: null, land: null, promotion: null, test: null, prod: null } },
    { stage: 'branch', since, cell: 'pr' }, why));
  }
  for (const r of rows) {
    r.shipped = r.state === 'merged' && r.cells.prod?.s === 'done';
    // Failing or blocked, and someone is waiting on it: a draft that is red is its author's business still.
    const broke = ['checks', 'land', 'promotion', 'test', 'prod'].some((k) => r.cells[k]?.s === 'fail');
    r.attention = broke && !r.draft;
  }

  // The header of each stage: where the whole pipeline is, not any one row.
  const open = rows.filter((r) => r.kind === 'pr' && r.state !== 'merged'), merged = rows.filter((r) => r.state === 'merged');
  const ag = [...shown].filter((a) => !a.shared), sharedN = [...shown].filter((a) => a.shared).length;
  const n = (list, f) => list.filter(f).length;
  const lastLanded = Math.max(0, ...merged.filter((r) => r.base === 'staging').map((r) => r.cells.land.at ?? 0));
  const deployHead = (env) => {
    const d = D[env] ?? {};
    const waiting = merged.filter((r) => !r.promo && r.cells[env]?.s === 'wait').length;
    const on = d.ok ? `on it ${short(d.ok.sha)}` : '';
    if (d.active) return { s: 'run', t: short(d.active.sha), at: d.active.startedAt, href: d.active.url, sub: ['deploying', on].filter(Boolean).join(' · ') };
    if (d.failed) return { s: 'fail', t: short(d.failed.sha), at: d.failed.at, href: d.failed.url, sub: ['failed', on].filter(Boolean).join(' · ') };
    if (d.ok) return { s: 'done', t: short(d.ok.sha), at: d.ok.at, href: d.ok.url, sub: waiting ? `${waiting} waiting` : '' };
    return { s: 'wait', t: 'unknown', sub: '' };
  };
  const stages = [
    { key: 'agent', name: 'Agent', ...(ag.length
      ? { s: n(ag, (a) => a.state === 'needs-you') ? 'need' : n(ag, (a) => a.state === 'working') ? 'run' : 'wait',
          t: [n(ag, (a) => a.state === 'working') && `${n(ag, (a) => a.state === 'working')} working`, n(ag, (a) => a.state === 'needs-you') && `${n(ag, (a) => a.state === 'needs-you')} need you`,
            n(ag, (a) => a.state === 'ready') && `${n(ag, (a) => a.state === 'ready')} idle`].filter(Boolean).join(' · '),
          sub: [sharedN && `${sharedN} in shared checkouts`, agents.unknown && `${agents.unknown} branch unknown`].filter(Boolean).join(' · ') }
      : { s: 'wait', t: 'none on a branch', sub: [sharedN && `${sharedN} in shared checkouts`, agents.unknown && `${agents.unknown} branch unknown`].filter(Boolean).join(' · ') }) },
    { key: 'pr', name: 'PR', t: `${n(open, (r) => r.state === 'open')} open · ${n(open, (r) => r.state === 'draft')} draft`, sub: `${merged.length} merged` },
    { key: 'checks', name: 'Checks', ...(n(open, (r) => r.checksFailed)
      ? { s: 'fail', t: `${n(open, (r) => r.checksFailed)} failing` } : n(open, (r) => r.cells.checks?.s === 'run') ? { s: 'run', t: `${n(open, (r) => r.cells.checks?.s === 'run')} running` } : { s: 'done', t: 'all green' }),
      sub: n(open, (r) => r.conflict) ? `${n(open, (r) => r.conflict)} conflict` : '' },
    { key: 'land', name: 'Land', ...(queue.size ? { s: 'run', t: `${queue.size} queued` } : n(open, (r) => r.cells.land?.t === 'landing') ? { s: 'run', t: `${n(open, (r) => r.cells.land?.t === 'landing')} landing` } : { s: 'wait', t: 'queue empty' }),
      subAt: lastLanded || null, sub: lastLanded ? 'landed' : '' },
    { key: 'promotion', name: 'Promotion', ...(openPromo ? { ...promoState(openPromo), sub: 'open' } : lastPromo ? { s: 'done', t: `#${lastPromo.number}`, at: ms(lastPromo.mergedAt), href: lastPromo.url, sub: `none open · ${n(merged, (r) => r.cells.promotion?.s === 'wait')} waiting` } : { s: 'wait', t: 'none open', sub: '' }) },
    { key: 'test', name: 'TEST', ...deployHead('test') },
    { key: 'prod', name: 'Prod', ...deployHead('prod') },
  ];
  // The station each piece of work is at, from an agent at work to production (the flow drawn above the table).
  for (const r of rows) r.station = stationOf(r);
  // Yours: a PR you opened, or one of your Fleet sessions works on it (an agent cell). The rest is everyone else's.
  for (const r of rows) r.mine = Boolean((raw.me && r.author === raw.me) || r.cells?.agent || r.kind === 'branch');
  return { ok: true, repo: raw.repo, at: raw.at, took: raw.took, calls: raw.calls, days: raw.days, warnings: raw.warnings ?? [], workflows: raw.workflows ?? [], me: raw.me ?? null, stations: STATIONS, proof: raw.proof ?? null,
    stages, rows: orderRows(rows), counts: { open: open.length, merged: merged.length, shipped: rows.filter((r) => r.shipped).length, attention: rows.filter((r) => r.attention && !r.stuck).length, stuck: rows.filter((r) => r.stuck).length } };
}

// ------------------------------------------------------------------------------------------------ the GitHub reads

export function createSatellite({ repo = REPO, snapshot = async () => null, days = DAYS, stagingBranch = 'staging', exec = execGh, now = Date.now, testUrl = null } = {}) {
  const [owner, name] = repo.split('/');
  // These never change once known, so they are kept for the life of the panel.
  const jobsCache = new Map(); // finished run id → its jobs
  const repCache = new Map();  // sha → the newest staging commit it contains
  const pairCache = new Map(); // `x>t` → does t hold x
  let wfMeta = null;           // the workflow files and their ids: read at most hourly
  const wfExtra = new Map();   // workflow id → its last run, for the ones too quiet to be in the newest runs (ten minutes)
  const branchCache = new Map(); // branch → { at, info }: does GitHub have it, and where is its tip (a minute)
  const filesCache = new Map(); // `<pr>:<merge sha>` → the files a merged PR changed (a promise of them): they never change
  let routes = null, routesAt = 0; // the app's routes (docs/generated/routes.md on staging): read hourly
  // A few `gh` processes at a time for the per-PR reads; the rest wait their turn.
  let inPool = 0; const queueOfPool = [];
  const pooled = (fn) => new Promise((ok, no) => { const go = () => { inPool++; fn().then(ok, no).finally(() => { inPool--; queueOfPool.shift()?.(); }); }; if (inPool < 6) go(); else queueOfPool.push(go); });
  let raw = null, rawAt = 0, inflight = null, failure = null, lastTry = 0, me = null;

  /** One full read of GitHub. The calls it made are counted, and the guard sees every one. */
  async function gather() {
    const started = now();
    let calls = 0;
    const warnings = [];
    const call = async (args, opts) => { guard(args); calls++; return exec(args, opts); };
    const json = async (args, opts) => JSON.parse(await call(args, opts));
    const soft = async (p, fallback, label) => { try { return await p; } catch (e) { warnings.push(`${label}: ${String(e?.message ?? e).slice(0, 160)}`); return fallback; } };

    const since = new Date(started - (days + 1) * 86_400_000);
    // Staging's line: its commits in the window, newest first (three pages of a hundred at most).
    const readLine = async () => {
      const shas = [], at = now();
      for (let page = 1; page <= 3; page++) {
        const part = await json(['api', `repos/${repo}/commits?sha=${stagingBranch}&per_page=100&since=${since.toISOString()}&page=${page}`, '--jq', '[.[].sha]']);
        shas.push(...part);
        if (part.length < 100) break;
        if (page === 3) warnings.push('staging has more than 300 commits in the window: the oldest rows may read as not deployed');
      }
      return { shas, at };
    };

    // TEST and Prod: the newest release runs, and their jobs a few at a time until each has a deploy that worked.
    const jobsOf = new Map();
    const jobsFor = async (run) => {
      if (jobsOf.has(run.id)) return;
      const jobs = jobsCache.get(run.id) ?? await json(['api', `repos/${repo}/actions/runs/${run.id}/jobs?per_page=20`, '--jq', '[.jobs[] | {name,status,conclusion,started_at,completed_at,head_sha,html_url}]']);
      jobsOf.set(run.id, jobs);
      if (run.status === 'completed') jobsCache.set(run.id, jobs); // a run still going is asked again next time
    };
    const readDeploys = async () => {
      const runs = await soft(json(['api', `repos/${repo}/actions/workflows/release.yml/runs?per_page=10`, '--jq', '[.workflow_runs[] | {id,status,conclusion,head_sha,created_at,updated_at,html_url}]']), [], 'deploys');
      let found = { test: {}, prod: {}, complete: false };
      for (let want = Math.min(3, runs.length); want > 0;) {
        await soft(Promise.all(runs.slice(0, want).map(jobsFor)), null, 'deploy jobs');
        found = pickDeploys(runs.slice(0, want), jobsOf);
        if (found.complete || want >= runs.length) break;
        want = Math.min(runs.length, want + 3);
      }
      return found;
    };

    // Every workflow file: its last run on staging or main. Two reads of the newest runs cover the busy ones; a quiet one
    // (a nightly job, a PR-closed hook) is asked for by itself, at most every ten minutes. Gate's runs on staging are kept
    // apart: they are the staging proof (stagingProof).
    let proof = null;
    const readWorkflows = async () => {
      if (!wfMeta || now() - wfMeta.at > 3_600_000) {
        const [files, list] = await Promise.all([
          json(['api', `repos/${repo}/contents/.github/workflows?ref=${stagingBranch}`, '--jq', '[.[] | select(.name | test("[.]ya?ml$")) | .path]']),
          json(['api', `repos/${repo}/actions/workflows?per_page=100`, '--jq', '[.workflows[] | {id,name,path,state}]']),
        ]);
        wfMeta = { files, list, at: now() };
      }
      const [a, b] = await Promise.all([stagingBranch, 'main'].map((br) => json(['api', `repos/${repo}/actions/runs?branch=${br}&per_page=100`, '--jq', RUN_FIELDS])));
      const runs = [...a, ...b].sort((x, y) => Date.parse(y.created_at) - Date.parse(x.created_at));
      const gateId = wfMeta.list.find((w) => /\/gate\.ya?ml$/.test(w.path ?? ''))?.id;
      if (gateId != null) proof = stagingProof(runs.filter((r) => r.workflow_id === gateId && r.head_branch === stagingBranch));
      const seen = new Set(runs.map((r) => r.workflow_id));
      await Promise.all(wfMeta.list.filter((w) => wfMeta.files.includes(w.path) && !seen.has(w.id)).map(async (w) => {
        const was = wfExtra.get(w.id);
        if (was && now() - was.at < 600_000) return;
        const r = await json(['api', `repos/${repo}/actions/workflows/${w.id}/runs?per_page=1`, '--jq', RUN_FIELDS]);
        wfExtra.set(w.id, { at: now(), run: r[0] ?? null });
      }));
      return pickWorkflows(wfMeta.files, wfMeta.list, runs, new Map([...wfExtra].filter(([, v]) => v.run).map(([id, v]) => [id, v.run])));
    };

    // Everything that does not need another answer goes at once: the two PR lists are the slow ones (about four seconds).
    const linePromise = readLine(), deploysPromise = readDeploys(), workflowsPromise = soft(readWorkflows(), [], 'workflows');
    linePromise.catch(() => {}); deploysPromise.catch(() => {}); // seen below, but not if a PR list throws first
    const [open, mergedAll, queue] = await Promise.all([
      json(['pr', 'list', '-R', repo, '--state', 'open', '--limit', '100', '--json', OPEN_FIELDS]),
      json(['pr', 'list', '-R', repo, '--state', 'merged', '--search', `merged:>=${since.toISOString().slice(0, 10)}`, '--limit', '300', '--json', MERGED_FIELDS]),
      soft(queueEntries(call), [], 'merge queue'),
    ]);
    let lineRead = await linePromise;
    // A PR that landed (or a release branch cut) after the line was read is not on it: read it again, once.
    const newest = [...mergedAll.filter((pr) => pr.baseRefName === 'staging').map((pr) => pr.mergeCommit?.oid), ...open.filter(isPromotion).map((pr) => pr.headRefOid)].filter(Boolean);
    if (newest.some((sha) => !lineRead.shas.includes(sha)) && mergedAll.some((pr) => Date.parse(pr.mergedAt) > lineRead.at - 120_000)) lineRead = await readLine();
    const line = lineRead.shas;
    const onLine = new Set(line);
    const deploys = await deploysPromise, workflows = await workflowsPromise;

    // Promotions: staging → main PRs, the open one and the ones merged in the window (oldest first).
    const promoOf = (pr) => ({ number: pr.number, url: pr.url, title: pr.title, createdAt: pr.createdAt ?? null, head: pr.headRefOid, merge: pr.mergeCommit?.oid ?? null, mergedAt: pr.mergedAt ?? null, statusCheckRollup: pr.statusCheckRollup });
    let merged = mergedAll.filter(isPromotion).sort((a, b) => Date.parse(a.mergedAt) - Date.parse(b.mergedAt));
    if (!merged.length) {
      const older = await soft(json(['pr', 'list', '-R', repo, '--base', 'main', '--state', 'merged', '--limit', '10', '--json', MERGED_FIELDS]), [], 'last promotion');
      merged = older.filter(isPromotion).sort((a, b) => Date.parse(a.mergedAt) - Date.parse(b.mergedAt)).slice(-1);
    }
    const openPromo = open.filter(isPromotion).sort((a, b) => Date.parse(b.createdAt) - Date.parse(a.createdAt))[0] ?? null;
    const promos = { open: openPromo ? promoOf(openPromo) : null, merged: merged.map(promoOf) };
    const promoBySha = new Map();
    for (const p of [...promos.merged, ...(promos.open ? [promos.open] : [])]) { promoBySha.set(p.head, p); if (p.merge) promoBySha.set(p.merge, p); }

    // The newest staging commit that `sha` contains. A promotion's own commits are answered from its head; anything else
    // asks GitHub's compare for the merge base with staging (the answer is large, so it is asked once per sha, ever).
    const repOf = async (sha) => {
      if (!sha) return null;
      if (onLine.has(sha)) return sha;
      if (repCache.has(sha)) return repCache.get(sha);
      const p = promoBySha.get(sha);
      let r = null;
      if (p && onLine.has(p.head)) r = p.head;
      else {
        const base = await soft(call(['api', `repos/${repo}/compare/${sha}...${stagingBranch}?per_page=1`, '--jq', '.merge_base_commit.sha'], { timeout: 90_000 }), null, `ancestry of ${short(sha)}`);
        r = base ? String(base).trim() : null;
      }
      if (r) repCache.set(sha, r);
      return r;
    };
    const deploySha = ['test', 'prod'].flatMap((e) => ['ok', 'active', 'failed'].map((k) => deploys[e][k]?.sha)).filter(Boolean);
    await Promise.all([...new Set([promos.open?.head, promos.merged.at(-1)?.head, promos.merged.at(-1)?.merge, ...deploySha].filter(Boolean))].map(repOf));

    // Merged PRs whose commit is not on staging's line (fixes straight into main): one compare per deploy, kept for good.
    const work = mergedAll.filter((pr) => Date.parse(pr.mergedAt) >= started - days * 86_400_000);
    // The pages each merged PR touched, for the links onto TEST: its files (asked once per PR, ever; a few at a time, and
    // this read waits only so long for them — the rest come with the next) against the app's routes (hourly).
    const routesPromise = soft((async () => {
      if (routes && now() - routesAt < 3600_000) return routes;
      const b64 = await call(['api', `repos/${repo}/contents/docs/generated/routes.md?ref=${stagingBranch}`, '--jq', '.content']); // --jq prints the string raw
      routes = routesIn(Buffer.from(String(b64).replace(/\s/g, ''), 'base64').toString('utf8')); routesAt = now();
      return routes;
    })(), routes ?? [], 'routes');
    const fileKey = (pr) => `${pr.number}:${pr.mergeCommit?.oid ?? ''}`;
    const filesOf = (pr) => {
      const k = fileKey(pr);
      if (!filesCache.has(k)) {
        const p = pooled(() => soft(json(['api', `repos/${repo}/pulls/${pr.number}/files?per_page=100`, '--jq', '[.[].filename]']), null, `files of #${pr.number}`));
        filesCache.set(k, p);
        p.then((v) => { if (!Array.isArray(v)) filesCache.delete(k); }); // a read that failed is asked again next time
      }
      return filesCache.get(k);
    };
    const paged = work.filter((pr) => !isPromotion(pr)).sort((a, b) => Date.parse(b.mergedAt) - Date.parse(a.mergedAt)).slice(0, 60);
    const deadline = new Promise((ok) => setTimeout(() => ok(undefined), 12_000).unref?.());
    const filesStarted = paged.map((pr) => [pr.number, filesOf(pr)]); // started now, gathered at the end: the other reads go on meanwhile
    for (const k of [...filesCache.keys()]) if (!paged.some((pr) => fileKey(pr) === k)) filesCache.delete(k);
    const off = work.filter((pr) => !isPromotion(pr) && ['staging', 'main'].includes(pr.baseRefName)).map((pr) => pr.mergeCommit?.oid).filter((sha) => sha && !onLine.has(sha));
    await Promise.all(off.flatMap((x) => [...new Set(deploySha)].map(async (t) => {
      const k = `${x}>${t}`;
      if (pairCache.has(k)) return;
      const status = await soft(call(['api', `repos/${repo}/compare/${x}...${t}?per_page=1`, '--jq', '.status'], { timeout: 90_000 }), null, `ancestry of ${short(x)}`);
      if (status) pairCache.set(k, ['ahead', 'identical'].includes(String(status).trim()));
    })));

    const files = Object.fromEntries((await Promise.all(filesStarted.map(async ([n, p]) => [n, await Promise.race([p, deadline])]))).filter(([, v]) => Array.isArray(v)));
    const routeList = await routesPromise;
    // Whose GitHub this is (gh's own login): asked once.
    me ??= await soft(call(['api', 'user', '--jq', '.login']).then((v) => String(v).trim() || null), null, 'who you are');
    return {
      ok: true, repo, at: started, took: now() - started, calls, days, warnings, me,
      open,
      merged: work,
      promos, line, deploys, queue, workflows, proof,
      files, routes: routeList,
      rep: Object.fromEntries(repCache), pairs: Object.fromEntries(pairCache),
    };
  }

  /** Who is in the merge queues (staging's and main's): the only thing here that is not REST. A GraphQL query, read-only. */
  async function queueEntries(call) {
    const out = JSON.parse(await call(['api', 'graphql', '-f', `query=${QUEUE_QUERY}`, '-f', `o=${owner}`, '-f', `r=${name}`]));
    const entries = [];
    for (const [key, branch] of [['s', 'staging'], ['m', 'main']]) {
      for (const e of out?.data?.repository?.[key]?.entries?.nodes ?? []) entries.push({ number: e.pullRequest?.number, position: e.position ?? null, state: e.state ?? null, branch });
    }
    return entries.filter((e) => e.number);
  }

  /** A branch on GitHub: its tip commit and when that was made, or { exists: false }. Asked only for the branches agents are on
   *  that have no PR, and kept a minute. */
  async function branchInfo(branch) {
    const hit = branchCache.get(branch);
    if (hit && now() - hit.at < 60_000) return hit.info;
    const args = ['api', `repos/${repo}/branches/${branch.split('/').map(encodeURIComponent).join('/')}`, '--jq', '{sha: .commit.sha, at: .commit.commit.committer.date}'];
    let info;
    try { guard(args); const out = JSON.parse(await exec(args)); info = { exists: true, sha: out.sha, at: out.at }; } catch (e) {
      if (!/not found|404/i.test(String(e?.message ?? e))) throw e; // a network error is not an answer
      info = { exists: false };
    }
    branchCache.set(branch, { at: now(), info });
    return info;
  }

  /** One read at a time; a page never waits on it except for the very first. */
  function refresh() {
    if (inflight) return inflight;
    lastTry = now();
    inflight = gather().then((r) => { raw = r; rawAt = now(); failure = null; },
      (e) => { failure = String(e?.message ?? e).slice(0, 300); console.error('satellite:', failure); }).finally(() => { inflight = null; });
    return inflight;
  }

  /** The pipeline, as of the last read of GitHub (at most a minute old while a page watches) and the agents as of now.
   *  `fresh` asks for another read of GitHub behind this answer. */
  async function read({ fresh = false } = {}) {
    const t = now();
    if (!raw) await refresh();
    else if (fresh) { // the button: a read now, and this answer waits for it (within reason)
      if (!inflight && t - lastTry >= 5000) refresh(); // a page pressing twice gets one read
      if (inflight) await Promise.race([inflight, new Promise((ok) => setTimeout(ok, 30_000).unref?.())]);
    } else if (!inflight && t - lastTry >= RETRY_MS && t - rawAt >= TTL_MS) refresh();
    if (!raw) return { ok: false, error: `GitHub could not be read: ${failure ?? 'no answer'}` };
    let snap = null;
    try { snap = await Promise.race([Promise.resolve().then(snapshot), new Promise((ok) => setTimeout(() => ok(null), SNAP_MS).unref?.())]); } catch {}
    const agents = placeAgents(snap, { repo });
    // The branches agents are on that no PR names: is each on GitHub, and what is at its tip? (Not waited for long.)
    const named = new Set([...(raw.open ?? []), ...(raw.merged ?? [])].map((pr) => pr.headRefName));
    const orphans = [...new Set(agents.placed.filter((a) => !named.has(a.branch)).map((a) => a.branch))].slice(0, 12);
    agents.branches = {};
    if (orphans.length) {
      await Promise.race([Promise.all(orphans.map(async (b) => { try { const i = await branchInfo(b); if (i) agents.branches[b] = i; } catch {} })),
        new Promise((ok) => setTimeout(ok, SNAP_MS + 1_000).unref?.())]);
    }
    const out = compose(raw, agents, t);
    out.testUrl = (typeof testUrl === 'function' ? testUrl() : testUrl) || null;
    if (failure) out.stale = failure;
    if (inflight) out.refreshing = true;
    return out;
  }

  return { read, gather };
}
