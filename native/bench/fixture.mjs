// The bench's made-up home: one Claude profile with 400 sessions, a stand-in `claude`, ~600 files in fleet-state/ (the
// real ~/.claude has 582, and the kqueue hot path re-reads that folder per event), and transcripts of realistic size.
// Everything is synthetic: no real names, paths or content.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const SESSIONS = 400, LIVE = 40, HOOK_FILES = 600;
export const sid = (n) => `bbbbbbbb-0000-4000-8000-${String(n).padStart(12, '0')}`;
const jobId = (n) => n.toString(16).padStart(8, '0');
/** `age` is how long ago the session last wrote: the live ones just now, the rest days ago (so only the live ones are on the kqueue hot path, as on a real Mac). */
const line = (type, i, message, age) => JSON.stringify({ isSidechain: false, timestamp: new Date(Date.now() - age + i * 1000).toISOString(), sessionId: 'x', type, message });

/** Bytes of transcript for session n: most are a few dozen KB, eight are 1-5 MB, so the first read shows in startup. */
const sizeOf = (n) => (n < 8 ? (1 + n * 0.55) * 1024 * 1024 : 20_000 + (n * 7919) % 90_000);

function transcript(n, age) {
  const out = [line('user', 0, { role: 'user', content: `Task ${n}: tidy the fixture` }, age)];
  const reply = { role: 'assistant', model: 'claude-fixture-1', stop_reason: 'end_turn', content: [{ type: 'text', text: 'Done with the step.' }],
    usage: { input_tokens: 1000, cache_creation_input_tokens: 200, cache_read_input_tokens: 300 } };
  out.push(line('assistant', 1, reply, age));
  const filler = line('user', 2, { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_f', content: 'x'.repeat(1000) }] }, age);
  let bytes = out.join('\n').length;
  const text = [];
  for (let i = 0; bytes < sizeOf(n); i++) { text.push(filler); bytes += filler.length + 1; }
  return [out[0], ...text, out[1]].join('\n') + '\n';
}

/** Returns { home, profile, cwd, transcriptOf(n) }; `pid` is the process the live rows name (alive for the whole run). */
export function buildHome(pid) {
  const home = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'fleetd-bench-home-')));
  const profile = path.join(home, '.claude-work');
  for (const d of ['fleet-state', 'fleet-status', 'jobs', 'projects']) fs.mkdirSync(path.join(profile, d), { recursive: true });
  fs.mkdirSync(path.join(home, '.local', 'bin'), { recursive: true });
  fs.writeFileSync(path.join(profile, 'settings.json'), '{}');
  const cwd = path.join(home, 'repo');
  fs.mkdirSync(cwd);
  const proj = path.join(profile, 'projects', cwd.replace(/[^A-Za-z0-9]/g, '-'));
  fs.mkdirSync(proj, { recursive: true });
  const rows = [], running = [];
  for (let n = 0; n < SESSIONS; n++) {
    const live = n < LIVE;
    rows.push({ id: jobId(n), sessionId: sid(n), name: `bench ${n}`, kind: 'interactive', cwd, startedAt: Date.now() - 3_600_000,
      ...(live ? { pid, state: 'working', status: 'idle' } : { state: 'done' }) });
    if (live) running.push({ id: jobId(n), sessionId: sid(n) });
    const f = path.join(proj, `${sid(n)}.jsonl`), age = live ? 30_000 : 3 * 86_400_000;
    fs.writeFileSync(f, transcript(n, age));
    if (!live) { const t = new Date(Date.now() - age); fs.utimesSync(f, t, t); }
  }
  // 600 hook files: one per session, then strays from sessions long gone.
  for (let k = 0; k < HOOK_FILES; k++) {
    const id = k < SESSIONS ? sid(k) : `cccccccc-0000-4000-8000-${String(k).padStart(12, '0')}`;
    fs.writeFileSync(path.join(profile, 'fleet-state', `${id}.json`), JSON.stringify({ e: 'Stop', t: null, n: null, r: null, i: null, at: Date.now() - 600_000 }) + '\n');
  }
  fs.writeFileSync(path.join(profile, '.bench-roster.json'), JSON.stringify(rows));
  fs.writeFileSync(path.join(profile, '.bench-active.json'), JSON.stringify(running));
  fs.writeFileSync(path.join(home, '.local', 'bin', 'claude'), `#!/bin/sh
D="\${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
case "$1" in
  agents) case "$*" in *--all*) cat "$D/.bench-roster.json";; *) cat "$D/.bench-active.json";; esac;;
  attach) case "$2" in e*) stty raw -echo; exec cat;; *) printf 'BANNER %s\r\n' "$2"; exec cat;; esac;;
  auth) echo '{"loggedIn":true,"email":"me@example.test"}';;
  *) exit 1;;
esac
`, { mode: 0o755 });
  return { home, profile, cwd, transcriptOf: (n) => path.join(proj, `${sid(n)}.jsonl`) };
}
