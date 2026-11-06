// Writes the synthetic Codex rollouts in fixtures/codex/sessions/YYYY/MM/DD/ and fixtures/codex/auth.json (no real content:
// every line, id and token is made up here; the email is example.test). Each rollout aims at a part of readRollout
// (fleet/machine.mjs): a turn in flight, a finished one, a question to you, an interrupted turn, the early break of its
// backward scan, a task_complete that a later turn hides, the head and tail windows, threads Codex started itself.
// Deterministic; the outputs are committed. Run: node native/parity/make-codex-fixtures.mjs
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures', 'codex');
fs.rmSync(path.join(ROOT, 'sessions'), { recursive: true, force: true });

let clock = Date.UTC(2026, 10, 6, 9, 0, 0);
const at = () => new Date((clock += 2000)).toISOString();
const line = (type, payload) => ({ timestamp: at(), type, payload });
const ev = (payload) => line('event_msg', payload);
const item = (payload) => line('response_item', payload);
let n = 0;
const id = () => `00000000-0000-4000-8000-${String(++n).padStart(12, '0')}`;
const meta = (o = {}) => line('session_meta', { id: id(), timestamp: new Date(clock).toISOString(), cwd: '/work/fixture-app', originator: 'codex_cli_rs', cli_version: '0.0.0-fixture', source: 'cli', ...o });
const ctx = (o = {}) => line('turn_context', { cwd: '/work/fixture-app', model: 'fixture-codex-1', effort: 'high', ...o });
const started = () => ev({ type: 'task_started' });
const ask = (t) => ev({ type: 'user_message', message: t });
const said = (t) => ev({ type: 'agent_message', message: t });
const done = (t) => ev({ type: 'task_complete', last_agent_message: t });
const aborted = () => ev({ type: 'turn_aborted' });
const tokens = (used, size = 200000) => ev({ type: 'token_count', info: { total_token_usage: { input_tokens: used * 2, cached_input_tokens: used, output_tokens: 300 }, last_token_usage: { input_tokens: used }, model_context_window: size },
  rate_limits: { primary: { used_percent: 12 } } });
const call = (name, args) => item({ type: 'function_call', name, arguments: typeof args === 'string' ? args : JSON.stringify(args), call_id: `call_${++n}` });
const out = (t = 'ok') => item({ type: 'function_call_output', call_id: `call_${n}`, output: t });
const msg = (role, t) => item({ type: 'message', role, content: [{ type: role === 'user' ? 'input_text' : 'output_text', text: t }] });
const put = (day, name, lines) => {
  const dir = path.join(ROOT, 'sessions', ...day.split('-'));
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, `rollout-${day}T09-00-00-${name}.jsonl`), lines.map((l) => (typeof l === 'string' ? l : JSON.stringify(l))).join('\n') + '\n');
};

// A turn in flight: its last word a tool call.
put('2026-11-06', 'working', [
  meta(), ctx(), msg('user', '# AGENTS.md instructions for /work/fixture-app'), msg('user', '<environment_context>cwd</environment_context>'),
  started(), ask('Add a retry to the fixture loader and run its tests'), msg('user', 'Add a retry to the fixture loader and run its tests'),
  said('I will read the loader first.'), call('shell', { command: ['bash', '-lc', 'cat src/loader.ts\nwc -l src/loader.ts'] }), out(), tokens(4200),
  said('Now patching it.'), call('apply_patch', {}), out(), tokens(6100),
]);
// A finished turn: task_complete says what it did.
put('2026-11-06', 'finished', [
  meta(), ctx(), started(), ask('Rename the config key'), call('shell', { cmd: 'git grep -n oldKey' }), out(), said('Renamed in three files.'), tokens(9000), done('Renamed oldKey to newKey in three files; tests pass.'),
]);
// A question to you that nothing has answered.
put('2026-11-06', 'asking', [
  meta(), ctx({ model: 'fixture-codex-2', effort: 'low' }), started(), ask('Pick a branch name for the work'), call('shell', { command: 'git branch' }), out('main'),
  call('request_user_input', { questions: [{ question: 'Which branch should I use?' }] }), tokens(3000),
]);
// A question left standing for days.
put('2026-11-02', 'asking-stale', [
  meta(), ctx(), started(), ask('Pick a branch name'), call('request_user_input', { questions: [{ question: 'Which branch?' }] }), tokens(2000),
]);
// The same question, answered.
put('2026-11-06', 'answered', [
  meta(), ctx(), started(), ask('Pick a branch name for the work'), call('request_user_input', { question: 'Which branch?' }), out('release'), said('Using release.'), tokens(3100),
]);
// An interrupted turn: idle, and no question stands.
put('2026-11-06', 'aborted', [
  meta(), ctx(), started(), ask('Run the slow suite'), call('request_user_input', { prompt: 'Continue?' }), aborted(), tokens(2000),
]);
// A one-turn `codex exec`: closed once its process lets go.
put('2026-11-06', 'exec', [
  meta({ source: 'exec', originator: 'codex_exec' }), ctx(), started(), ask('Summarise the changelog'), said('Three changes.'), tokens(1500), done('Summarised.'),
]);
// Threads Codex started itself: not yours.
put('2026-11-06', 'subagent', [meta({ thread_source: 'subagent' }), ctx(), started(), ask('Look into the build'), said('Looking.'), tokens(800)]);
put('2026-11-06', 'review', [meta({ thread_source: 'review' }), ctx(), started(), ask('Review the diff'), said('Reviewing.'), tokens(900)]);
put('2026-11-05', 'user-thread', [meta({ thread_source: 'user' }), ctx(), started(), ask('Explain the retry logic'), said('It backs off twice.'), tokens(1700), done('Explained.')]);
// The backward scan stops at the newest lines: the older tool call is never seen, and its doing is null.
put('2026-11-05', 'break', [
  meta(), ctx({ model: 'fixture-codex-old' }), started(), ask('First request'), call('shell', { command: 'echo older' }), out(), said('older reply'), tokens(500), done('older done'),
  ctx(), ask('Second request'), started(), said('Working on the second.'), tokens(7000),
]);
// A task_complete the next turn hides: its message is no longer what it said.
put('2026-11-05', 'hidden-done', [
  meta(), ctx(), started(), ask('Do the first thing'), said('Early words.'), tokens(1000), done('First thing done.'), ask('Now the second'), started(),
]);
// Only a tool call with no name or command: nothing to say it is doing; the older call is.
put('2026-11-05', 'nameless', [
  meta(), ctx(), started(), ask('Poke around'), call('shell', { command: ['ls', '-la'] }), out(), call('', {}), out(), tokens(1200),
]);
// Odd arguments and lines: arguments that are not JSON, a bare number line, an unparsable line, a line with no timestamp.
put('2026-11-05', 'odd', [
  meta(), ctx({ model: '', effort: 'minimal' }), ctx(), started(), ask('Odd lines'), '{"timestamp": "2026-11-06T', 42, 'not json', { type: 'event_msg', payload: { type: 'agent_message', message: 'No timestamp here.' } },
  call('shell', 'not json at all'), out(), tokens(2500, 0),
]);
// First line is not a session_meta: the id is the file's name.
put('2026-11-04', 'no-meta', [ctx(), started(), ask('No meta line'), said('Fine.'), tokens(600), done('Fine.')]);
// Old: written before the window.
put('2026-11-02', 'old', [meta(), ctx(), started(), ask('Old thread'), said('Old.'), tokens(100), done('Old.')]);
// Longer than the 384 KiB tail: the first ask is only in the head, and the end decides the rest.
{
  const pad = 'x'.repeat(900);
  const lines = [meta(), ctx(), started(), ask('The opening request in the head only')];
  for (let i = 0; i < 340; i++) lines.push(call('shell', { command: `echo ${i} ${pad}` }), out(`line ${i} ${pad}`));
  lines.push(started(), ask('The newest request in the tail'), said('Working on the newest.'), call('shell', { command: 'npm test' }), tokens(88000));
  put('2026-11-06', 'big', lines);
}

// The id token: header.payload.signature with a made-up payload, as Codex stores it. No real person.
const b64u = (o) => Buffer.from(JSON.stringify(o)).toString('base64url');
const jwt = `${b64u({ alg: 'none', typ: 'JWT' })}.${b64u({ email: 'fixture@example.test', 'https://api.openai.com/auth': { chatgpt_plan_type: 'plus' } })}.${Buffer.from('not-a-signature').toString('base64url')}`;
fs.writeFileSync(path.join(ROOT, 'auth.json'), JSON.stringify({ tokens: { id_token: jwt, access_token: 'fixture-access', refresh_token: 'fixture-refresh' } }, null, 1) + '\n');
console.log(`${fs.readdirSync(path.join(ROOT, 'sessions'), { recursive: true }).filter((f) => f.endsWith('.jsonl')).length} rollouts`);
