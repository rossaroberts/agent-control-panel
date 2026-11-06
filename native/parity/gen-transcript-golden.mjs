// Writes golden/transcript.json: Node's readTranscript over each fixture in fixtures/transcripts/, once whole and once
// after each of 3 appended chunks (cut at seeded-random bytes, so mid-line), and golden/stage.json: Node's stageOfTool and
// stageOf over a table of tool uses. The Rust Tail and stage_of are checked against them
// (native/crates/fleet-read/tests/transcript.rs, native/crates/fleet-model/tests/stage.rs).
// Deterministic (seeded cuts, fixed mtimes); the outputs are committed. Run: node native/parity/gen-transcript-golden.mjs
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { readTranscript, stageOf, stageOfTool } from '../../fleet/machine.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.join(HERE, 'fixtures', 'transcripts');
const MTIME_S = 1_793_932_000; // each read's file mtime is MTIME_S + its index, in whole seconds

// The checkout the workdir fixtures point into: a repo, a worktree whose .git is a file, Claude Code's worktrees folder.
// The Rust test makes the same tree.
const WORK = '/tmp/fleet-transcript-fixture';
function makeWork() {
  const put = (f, s) => { fs.mkdirSync(path.dirname(f), { recursive: true }); fs.writeFileSync(f, s); };
  put(`${WORK}/.git/HEAD`, 'ref: refs/heads/fixture-main\n');
  put(`${WORK}/wt2/.git`, 'gitdir: ../gitdirs/wt2\n');
  put(`${WORK}/gitdirs/wt2/HEAD`, 'ref: refs/heads/fixture-wt2\n');
  fs.mkdirSync(`${WORK}/.claude/worktrees/wt1`, { recursive: true });
  fs.mkdirSync(`${WORK}/src`, { recursive: true });
}

// mulberry32
function rng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Two cut points, so three chunks. A cut just before a newline would leave a whole line with no newline at the end,
 *  which Node parses and the Rust tail (rightly) holds back until its newline: move it past the newline. */
function cutsOf(buf, rand) {
  const pick = () => { let c = 1 + Math.floor(rand() * (buf.length - 2)); if (buf[c] === 0x0a) c += 1; return c; };
  let a = pick(), b = pick();
  while (b === a) b = pick();
  return [Math.min(a, b), Math.max(a, b)];
}

/** The fields a Session or TailFacts uses, with Node's undefined as null. */
function norm(t) {
  const r = t.running ?? null;
  return {
    model: t.model ?? null, effort: t.effort ?? null, contextTokens: t.contextTokens ?? null, lastAt: t.lastAt ?? null,
    said: t.said ?? null, doing: t.doing ?? null, lastAsk: t.lastAsk ?? null, firstAsk: t.firstAsk ?? null,
    turnOver: t.turnOver ?? null,
    running: r && { step: r.step ?? null, since: r.since ?? null, tool: r.tool ?? null, cmd: r.cmd ?? null },
    limitHit: t.limitHit ? { text: t.limitHit.text ?? null, at: t.limitHit.at ?? null } : null,
    stage: t.stage ?? null, steps: t.steps, workdir: t.workdir ?? null,
    queued: (t.queued ?? []).map((q) => ({ text: q.text, at: q.at ?? null })), writtenAt: t.writtenAt,
  };
}

makeWork();
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'fleet-transcript-golden-'));
const rand = rng(20261106);
const fixtures = [];
for (const name of fs.readdirSync(FIXTURES).filter((f) => f.endsWith('.jsonl')).sort()) {
  const buf = fs.readFileSync(path.join(FIXTURES, name));
  const file = path.join(tmp, name);
  const read = (i) => { fs.utimesSync(file, MTIME_S + i, MTIME_S + i); return norm(readTranscript(file)); };
  fs.writeFileSync(file, buf);
  const whole = read(0);
  const cuts = cutsOf(buf, rand);
  const bounds = [0, ...cuts, buf.length];
  const chunks = [];
  fs.writeFileSync(file, '');
  for (let i = 0; i < 3; i++) {
    fs.appendFileSync(file, buf.subarray(bounds[i], bounds[i + 1]));
    chunks.push(read(i + 1));
  }
  fixtures.push({ name, size: buf.length, cuts, whole, chunks });
}
fs.rmSync(tmp, { recursive: true, force: true });

// stageOfTool: every rule, and the commands each STAGE_OF_RUN pattern names.
const tools = [
  ['Edit', {}], ['MultiEdit', {}], ['Write', {}], ['NotebookEdit', {}], ['apply_patch', {}],
  ['Read', {}], ['Grep', {}], ['Glob', {}], ['WebFetch', {}], ['WebSearch', {}], ['ExitPlanMode', {}], ['LSP', {}],
  ['mcp__fixture__tool', {}], ['Skill', { skill: 'verify-gate' }], ['Skill', { skill: 'code-review' }],
  ['Skill', { skill: 'pre-review-x' }], ['Skill', { skill: 'brainstorm' }], ['Skill', {}], ['Agent', { prompt: 'x' }],
  ['Task', {}], ['AskUserQuestion', {}], ['TodoWrite', {}], ['Bash', {}], ['Bash', { command: 'ls -la' }],
  ...[
    'gh workflow run ci.yml', './deploy.sh prod', 'bash deploy/web.sh', 'gh release.yml', 'ship.yml', 'kubectl apply -f x',
    'docker compose -f a.yml up -d', 'docker compose down',
    'gh pr create --fill', 'gh pr merge 3', 'gh pr ready', 'gh pr edit 3', 'gh pr update-branch', 'gh pr checks 3', 'git push origin x',
    'gh run watch 1', 'gh run view 1', 'merge-queue status', 'merge queue',
    'npm test', 'npm run test', 'npm t', 'npm run test:unit', 'npm testing', 'vitest run', 'npx jest', 'npx playwright test', 'pytest -q',
    'npm run smoke', 'smoke:api', 'npm run typecheck', 'tsc --noEmit', 'npm run lint', 'knip', 'verify-gate', 'npm run check:types',
    'npm run docs:check', 'node --test', 'NPM TEST',
    'npm i', 'npm install x', 'npm ci', 'npm run db:reset', 'db:rebuild', 'new:migration add', 'npm run build', 'git commit -m x',
    'git add .', 'git rebase main', 'git merge main', 'git status', 'echo pytest', 'npm installer', 'cat lint.txt',
  ].map((command) => ['Bash', { command }]),
  ['shell', { command: ['npm', 'test'] }], ['exec_command', { cmd: 'git push' }], ['shell', { cmd: 'ls' }],
  ['Bash', { command: 'git status', cmd: 'git push' }],
].map(([name, input]) => ({ name, input, stage: stageOfTool(name, input) ?? null }));
// stageOf: newest first; the first that ships, promotes, tests or builds wins, else planning if anything looked.
const seqs = [
  [], [['Agent', {}]], [['Read', {}], ['Agent', {}]], [['Agent', {}], ['Read', {}]],
  [['Read', {}], ['Bash', { command: 'npm test' }], ['Edit', {}]], [['Edit', {}], ['Bash', { command: 'npm test' }]],
  [['Grep', {}], ['Glob', {}], ['Bash', { command: 'git push' }]], [['TodoWrite', {}], ['Skill', { skill: 'x' }]],
  [['Bash', { command: 'ls' }], ['Bash', { command: './deploy.sh' }], ['Bash', { command: 'git push' }]],
].map((uses) => ({ uses, stage: stageOf(uses) ?? null }));

const g = path.join(HERE, 'golden');
fs.mkdirSync(g, { recursive: true });
fs.writeFileSync(path.join(g, 'transcript.json'), JSON.stringify({ mtimeS: MTIME_S, fixtures }, null, 1) + '\n');
fs.writeFileSync(path.join(g, 'stage.json'), JSON.stringify({ tools, seqs }, null, 1) + '\n');
console.log(`${fixtures.length} fixtures, ${tools.length} tools, ${seqs.length} sequences -> ${path.relative(process.cwd(), g)}`);
