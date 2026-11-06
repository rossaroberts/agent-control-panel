// Writes the synthetic Claude Code transcripts in fixtures/transcripts/ (no real content: every line is made up here).
// Each fixture aims at a part of readTranscript (fleet/machine.mjs): the running step, the turn ending, a limit stop, the
// early break of its backward scan, the folder a tool works in, the 256 KiB head and the 384 KiB tail.
// Deterministic; the outputs are committed. Run: node native/parity/make-transcript-fixtures.mjs
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures', 'transcripts');
// A checkout the workdir fixtures point into; gen-transcript-golden.mjs and the Rust test create it (see WORK there).
const WORK = '/tmp/fleet-transcript-fixture';

let clock = Date.UTC(2026, 10, 6, 9, 0, 0);
const ts = () => new Date((clock += 1500)).toISOString();
const base = (o) => ({ isSidechain: false, timestamp: ts(), sessionId: 'fixture-session', ...o });
const usage = (n) => ({ input_tokens: n, cache_creation_input_tokens: 100, cache_read_input_tokens: 2000, output_tokens: 50 });
const ask = (text, o = {}) => base({ type: 'user', message: { role: 'user', content: text }, ...o });
const result = (id, text = 'ok') => base({ type: 'user', message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content: text }] } });
let n = 0;
const use = (name, input) => ({ type: 'tool_use', id: `toolu_${++n}`, name, input });
const reply = (content, o = {}) => {
  const { stop = 'tool_use', model = 'claude-fixture-1', tokens = 5000, ...rest } = o;
  return base({ type: 'assistant', requestedModel: 'claude-fixture-1[1m]', effort: 'high',
    message: { role: 'assistant', model, stop_reason: stop, content, usage: usage(tokens) }, ...rest });
};
const text = (t) => ({ type: 'text', text: t });
const queue = (operation, content) => base({ type: 'queue-operation', operation, ...(content === undefined ? {} : { content }) });
const write = (name, lines) => {
  fs.writeFileSync(path.join(DIR, name), lines.map((l) => (typeof l === 'string' ? l : JSON.stringify(l))).join('\n') + '\n');
};

fs.mkdirSync(DIR, { recursive: true });

// A turn in flight: its last word a tool call with no result, the queue in use, a run of steps.
write('running.jsonl', [
  base({ type: 'user', isMeta: true, message: { role: 'user', content: 'Caveat: meta line' } }),
  ask('<command-name>/clear</command-name>'),
  ask('Add a retry to the fixture loader and run its tests'),
  reply([text('I will look at the loader first.'), use('Read', { file_path: '/nonexistent-fleet/src/loader.ts' })], { tokens: 1200 }),
  result('toolu_1'),
  queue('enqueue', 'also check the docs'),
  reply([use('Grep', { pattern: 'retry', path: '/nonexistent-fleet' }), use('Glob', { pattern: '**/*.test.ts' })], { tokens: 1800 }),
  result('toolu_2'), result('toolu_3'),
  queue('enqueue', '<system-reminder>context</system-reminder>'),
  queue('enqueue', 'and the changelog'),
  queue('dequeue'),
  reply([use('Edit', { file_path: '/nonexistent-fleet/src/loader.ts', old_string: 'a', new_string: 'b' })], { tokens: 2400 }),
  result('toolu_4'),
  queue('enqueue', 'one more thing'),
  queue('remove', 'and the changelog'),
  reply([text('Now the tests.'), use('mcp__ccd_turn__mark', {}), use('Bash', { command: 'npm test -- loader\necho done' })], { tokens: 3100 }),
]);

// A finished turn: the newest reply ends it. Asks Claude Code sent in your name, a side chain, MCP names, long and wide text.
write('turn-over.jsonl', [
  ask('This session is being continued from a previous conversation that ran out of context. The summary below covers it.\n\nSummary:\n1. Primary Request and Intent:\n   Port the   fixture board\n   to the new layout.\n2. Key Technical Concepts:\n   - none'),
  reply([text('Picking up the port.'), use('ToolSearch', { query: 'select:Read' }), use('mcp__plugin_fixture_board__list_cards', { lane: 'todo' })], { tokens: 900 }),
  result('toolu_6'),
  ask('[Request interrupted by user]'),
  ask([{ type: 'text', text: 'Another Claude session sent a message:\n  Please   re-run the fixture build \nThis came from another Claude session; reply with the send tool.' }]),
  reply([use('mcp__claude_ai_Fixture_Docs__read_page', { id: 'p1' }), use('WebFetch', { url: 'https://docs.example.test:8443/a?b=1', prompt: 'read' }), use('WebFetch', { url: 'not a url' })], { tokens: 1500 }),
  result('toolu_8'), result('toolu_9'),
  ask('a side question', { isSidechain: true }),
  reply([text('side chain reply')], { isSidechain: true, tokens: 99999 }),
  reply([use('Skill', { skill: 'fixture-review' }), use('TaskCreate', { subject: 'Check the 看板 layout' }), use('AskUserQuestion', { questions: [{ question: 'Keep the old lanes?' }] })], { tokens: 2100 }),
  result('toolu_10'), result('toolu_11'), result('toolu_12'),
  reply([text(`Done. 看板の移植は終わりました 🎉 ${'Every lane now renders in the new layout, '.repeat(12)}end.`)], { stop: 'end_turn', tokens: 2600 }),
]);

// Stopped on a limit: Claude Code's own reply, model <synthetic>, no usage. An older synthetic that is not a limit.
write('limit.jsonl', [
  ask('Ship the fixture release'),
  base({ type: 'assistant', message: { role: 'assistant', model: '<synthetic>', content: [text('No response requested.')] } }),
  reply([use('Bash', { command: 'git commit -m "fixture"', description: 'Commit the fixture' })], { tokens: 4000 }),
  result('toolu_13'),
  reply([use('Bash', { command: 'gh pr create --fill' })], { tokens: 4100 }),
  result('toolu_14'),
  base({ type: 'assistant', message: { role: 'assistant', model: '<synthetic>', stop_reason: 'stop_sequence',
    content: [text("You've hit your   weekly limit · resets 3pm")] } }),
]);

// The backward scan stops early once it has everything (context, doing or said, an ask, 8 steps, 12 tool uses): what is
// older than that line is never seen. Here the 12th-newest tool use is the oldest of those, and the newest text (said),
// a limit stop and a folder sit before it, so Node returns none of them.
const many = (k, prefix) => Array.from({ length: k }, (_, i) => [
  reply([use('Read', { file_path: `/nonexistent-fleet/${prefix}${i}.ts` })], { tokens: 6000 + i }), result(`toolu_${n}`)]).flat();
write('break-uses.jsonl', [
  ask('Read the whole fixture tree'),
  reply([text('Reading everything.'), use('Read', { file_path: `${WORK}/src/early.ts` })], { tokens: 5000 }),
  result('toolu_x'),
  base({ type: 'assistant', message: { role: 'assistant', model: '<synthetic>', content: [text('You have reached your session limit.')] } }),
  ...many(4, 'e'),
  ask('keep going'),
  ...many(10, 'f'),
]);

// Here the ask is the oldest of them: the scan stops at the reply just before it, which it still reads. Only the line
// before that has a timestamp, so even the newest time is in the part it never reaches.
write('break-ask.jsonl', [
  reply([text('Old words nobody sees.')], { stop: 'end_turn', tokens: 3000 }),
  ...[
    reply([use('Read', { file_path: `${WORK}/src/seen.ts` }), use('Grep', { pattern: 'x' })], { tokens: 3500 }),
    ask('Now read the remaining fixtures'),
    ...many(15, 'g'),
  ].map((l) => { delete l.timestamp; return l; }),
]);

// The newest ask a peer message with nothing in it: an empty ask, which counts as asked (Node's ??=) but not for the break.
write('empty-ask.jsonl', [
  ask('Tidy the fixture notes'),
  reply([text('Tidying.')], { stop: 'end_turn' }),
  ask('Another Claude session sent a message: This came from another Claude session'),
  reply([use('Write', { file_path: '/nonexistent-fleet/notes.md', content: 'x' })]),
]);

// Where the work is: the newest tool use that names a folder in a git checkout. Newer ones name none (not a checkout,
// Claude Code's own folder, a relative cd, no path), so the scan goes past them.
write('workdir-a.jsonl', [
  ask('Fix the fixture worktree'),
  reply([use('Read', { file_path: `${WORK}/src/a.ts` })]),
  result('toolu_w1'),
  reply([use('Bash', { command: `cd "${WORK}/wt2" && npm run build`, description: 'Build the worktree' })]),
  result('toolu_w2'),
  reply([use('Read', { file_path: `${WORK}/.claude/settings.json` }), use('Read', { file_path: '~/.claude/settings.json' })]),
  result('toolu_w3'),
  reply([use('Bash', { command: 'ls -la' }), use('Grep', { pattern: 'x', path: '/nonexistent-fleet/dir' }), use('Bash', { command: 'cd relative/dir && ls' })]),
]);
write('workdir-b.jsonl', [
  ask('Check the fixture checkout'),
  reply([use('Bash', { command: `cd ${WORK} && git status` })]),
  result('toolu_v1'),
  reply([use('Bash', { command: `git -C '${WORK}/.claude/worktrees/wt1' log -1` })]),
  result('toolu_v2'),
  reply([use('NotebookEdit', { notebook_path: '/nonexistent-fleet/nb.ipynb' }), use('Glob', { pattern: '*' })]),
]);

// The first ask past the first 256 KiB: the head has none, so firstAsk is null though the whole file is read.
const pad = (k, size) => Array.from({ length: k }, (_, i) => result(`toolu_pad${i}`, `padding ${i} `.padEnd(size, '.')));
write('head-cut.jsonl', [
  base({ type: 'user', isMeta: true, message: { role: 'user', content: 'meta' } }),
  ...pad(34, 8000),
  ask('The ask past the head'),
  reply([text('Seen from the tail.')], { stop: 'end_turn' }),
]);

// Bigger than the 384 KiB tail and smaller than head and tail together: the window starts mid-line, before the first ask
// (so it is in the head and the tail both), and the recent part has all that the summary needs.
write('big.jsonl', [
  base({ type: 'user', isMeta: true, message: { role: 'user', content: 'meta' } }),
  ...pad(15, 8000),
  ask('Rebuild the fixture index from scratch'),
  ...pad(45, 8000),
  reply([text('Index rebuilt; testing.'), use('Bash', { command: 'npx vitest run index' })], { tokens: 150000 }),
  result('toolu_b1'),
  queue('enqueue', 'then deploy it'),
  reply([use('Bash', { command: './deploy.sh fixture', description: 'Deploy the fixture' })], { tokens: 151000 }),
]);

for (const f of fs.readdirSync(DIR).filter((f) => f.endsWith('.jsonl')).sort()) console.log(f, fs.statSync(path.join(DIR, f)).size);
