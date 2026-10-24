// Fleet, the panel's side: every machine's agents in one snapshot, and starting one anywhere.
//
// This Mac is read in-process (machine.mjs). Each machine in fleet.json is read over SSH: machine.mjs goes on stdin to
// `node --input-type=module -` there, so nothing is installed on it. fleet.json sits beside prefs.json:
//
//   { "self": "Mac", "machines": [ { "name": "server", "ssh": "my-server", "user": "agents" } ],
//     "names": { "claude:me@example.com": "personal", "codex:me@example.com": "codex" }, "epics": [ … ] }
//
// `ssh` is a host or ~/.ssh/config alias; `user` (optional) is who the agents run as there (sudo -u); `names` labels a
// subscription, keyed engine:email; `self` names this machine (its hostname otherwise).

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { execFile, spawn } from 'node:child_process';
import { collect, launch as launchHere, peek as peekHere, send as sendHere, close as closeHere, archive as archiveHere, adopt as adoptHere, fork as forkHere,
  interject as interjectHere, watchHooks, sessionState } from './machine.mjs';
import { DENIED_TOOLS } from '../world.mjs';
import { sshArgs, shq } from './ssh.mjs';

const SCRIPT = new URL('./machine.mjs', import.meta.url);
// A page gets the last read at once, however old (it carries when it was read, and the page asks again soon after an old
// one); while a page has asked in the last WATCH_MS, the panel reads every machine again every WARM_MS behind it.
const WARM_MS = 10_000, WATCH_MS = 90_000;
const SSH_MS = 25_000, COLLECT_MS = 12_000;
// An ssh host never starts with "-" (ssh would take it as an option).
const HOST = /^[\w.@][\w.@-]{0,99}$/, USER = /^[a-z_][\w-]{0,31}$/, NAME = /^[\w .-]{1,40}$/;
const ENGINES = new Set(['claude', 'codex', 'muse', 'fleet']);
const MODELS = new Set(['', 'opus', 'opus[1m]', 'sonnet', 'haiku', 'fable']);

// What each session works on, in a few words, written by haiku from what it was asked: one call for every session whose
// latest ask changed, the same locked-down call World's builds use (no tools, hooks, MCP servers or saved session).
// Where a session is in the path from an idea to production.
const STAGES = ['planning', 'building', 'testing', 'promoting', 'deploying'];
const titleSchema = (epics) => ({ type: 'object', additionalProperties: false, required: ['titles'], properties: { titles: { type: 'array',
  items: { type: 'object', additionalProperties: false, required: ['id', 'title', 'stage', 'epic'], properties: { id: { type: 'string' },
    title: { type: 'string', maxLength: 40 }, stage: { type: 'string', enum: STAGES }, epic: { type: 'string', enum: [...epics, 'Other'] } } } } } });
const TITLE_BATCH = 40;
// One whose title didn't come back (the call failed) is asked again after this long, never sooner.
const RESTAGE_MS = 10 * 60_000;
const PLACEHOLDER = /^[\w.-]+-[0-9a-f]{2}$/; // the name Claude Code makes up for an unnamed session
// Control characters out too: a NUL in a session's text must never reach a command line.
const clean = (v, n) => String(v ?? '').replace(/[<>\x00-\x08\x0b-\x1f\x7f]/g, '').replace(/\s+/g, ' ').trim().slice(0, n);

// The worker each machine keeps for the panel (see worker() below): it reads machine.mjs as base64 on its first line,
// then one JSON request a line ({ id, do, args }) and answers each with one line ({ id, r }), many at a time. A read asked
// for while one runs shares it (a slow machine never piles reads up). Between answers it says when a session's hook file
// changes ({ event }). On end of input it finishes what it started, then exits.
export const WORKER_BOOT = "import readline from 'node:readline'; const ALLOWED = new Set(['collect','launch','peek','send','close','archive','adopt','fork','interject']);"
  + " const running = new Set(); let m = null, reading = null; for await (const line of readline.createInterface({ input: process.stdin, crlfDelay: Infinity })) {"
  + " if (!m) { m = await import('data:text/javascript;base64,' + line); process.stdout.write('{\"ready\":true}\\n');"
  + " try { m.watchHooks?.((ev) => process.stdout.write(JSON.stringify({ event: ev }) + '\\n')); } catch {} continue; }"
  + " let q; try { q = JSON.parse(line); } catch { continue; } if (!ALLOWED.has(q.do)) continue;"
  + " const go = () => Promise.resolve().then(() => m[q.do](q.args ?? {}));"
  + " const job = (q.do === 'collect' ? (reading ??= go().finally(() => { reading = null; })) : go()).catch((e) => ({ ok: false, error: String(e?.message ?? e) }))"
  + ".then((r) => process.stdout.write(JSON.stringify({ id: q.id, r }) + '\\n')); running.add(job); job.finally(() => running.delete(job)); }"
  + " await Promise.allSettled([...running]); process.exit(0);";

export function createFleet({ dir, quiet = () => {} }) {
  const file = path.join(dir, 'fleet.json');
  const titlesFile = path.join(dir, 'fleet-titles.json');
  let titles = {};
  try { titles = JSON.parse(fs.readFileSync(titlesFile, 'utf8')); } catch {}
  // Sessions you archived: `machine:sessionId` → when. They leave the page; a closed one has nothing else to clear.
  const archivedFile = path.join(dir, 'fleet-archived.json');
  let archived = {};
  try { archived = JSON.parse(fs.readFileSync(archivedFile, 'utf8')); } catch {}
  // Your own name and group for a session, over Haiku's title and epic: `machine:sessionId` → { title, group }.
  const labelsFile = path.join(dir, 'fleet-labels.json');
  let labels = {};
  try { labels = JSON.parse(fs.readFileSync(labelsFile, 'utf8')); } catch {}
  const host = os.hostname().replace(/\.local$/, '');

  function config() {
    let c = {};
    try { c = JSON.parse(fs.readFileSync(file, 'utf8')); } catch {}
    const machines = (Array.isArray(c.machines) ? c.machines : [])
      .filter((m) => NAME.test(m?.name ?? '') && HOST.test(m?.ssh ?? '') && (m.user == null || USER.test(m.user)))
      .map((m) => ({ name: m.name, ssh: m.ssh, user: m.user ?? null }));
    const names = c.names && typeof c.names === 'object' ? Object.fromEntries(Object.entries(c.names).filter(([, v]) => typeof v === 'string').map(([k, v]) => [k, v.slice(0, 40)])) : {};
    // Epics: what the work is for. Haiku files each session under one by its `about`; `test` is where its TEST site is.
    const epics = (Array.isArray(c.epics) ? c.epics : []).filter((e) => NAME.test(e?.name ?? '')).slice(0, 12)
      .map((e) => ({ name: e.name, about: String(e.about ?? '').slice(0, 300), test: /^https:\/\/[\w.-]+(\/[\w./?=&%-]*)?$/.test(e.test ?? '') ? e.test : null }));
    const self = typeof c.self === 'string' && NAME.test(c.self) ? c.self : host; // what this machine is called on the page
    // The account a profile's login really is, where its config file says another (`machine:profile` → email).
    const logins = c.logins && typeof c.logins === 'object'
      ? Object.fromEntries(Object.entries(c.logins).filter(([k, v]) => /^[\w .-]{1,40}:[\w.-]{1,40}$/.test(k) && /^[^\s@]+@[^\s@]+$/.test(String(v))).map(([k, v]) => [k, String(v)])) : {};
    return { machines, names, self, epics, logins, file };
  }

  // Each machine has one worker for the panel's life: through the machine's open connection, one `node` that has
  // machine.mjs loaded (sent once, as the first line) and answers a request a line, many at a time. A call costs a round
  // trip, not a login, a sudo, a node start and the script sent again. It starts again after a drop, or when machine.mjs
  // changes here; a request it never got goes the one-shot way instead.
  const workers = new Map();
  const asUser = (m) => (m.user ? `sudo -n -u ${m.user} -H env XDG_RUNTIME_DIR=/run/user/$(id -u ${m.user})` : 'env');
  function worker(m) {
    const mtime = fs.statSync(SCRIPT).mtimeMs;
    let w = workers.get(m.name);
    if (w && !w.dead && !w.stuck && w.mtime === mtime && w.ssh === m.ssh && w.user === m.user) return w;
    if (w) retire(w);
    const child = spawn('ssh', [...sshArgs(), m.ssh, `${asUser(m)} node --input-type=module -e ${shq(WORKER_BOOT)}`], { stdio: ['pipe', 'pipe', 'pipe'] });
    w = { name: m.name, child, mtime, ssh: m.ssh, user: m.user, pending: new Map(), next: 1, dead: false, stuck: false, err: '' };
    w.ready = new Promise((resolve) => { w.isReady = resolve; });
    const late = setTimeout(() => { if (!w.dead) { w.isReady(false); child.kill(); } }, 30_000);
    let buf = '';
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (d) => {
      buf += d;
      for (let i; (i = buf.indexOf('\n')) >= 0;) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 1);
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        if (msg.ready) { clearTimeout(late); w.isReady(true); continue; }
        if (msg.event) { onHook(m.name, msg.event); continue; }
        const p = w.pending.get(msg.id);
        if (p) { w.pending.delete(msg.id); clearTimeout(p.timer); p.resolve(msg.r); }
      }
    });
    child.stderr.on('data', (d) => { w.err = (w.err + d).slice(-2000); });
    const gone = () => {
      if (w.dead) return;
      w.dead = true; clearTimeout(late); w.isReady(false);
      const why = w.err.trim().split('\n').at(-1) || 'the connection dropped';
      for (const p of w.pending.values()) { clearTimeout(p.timer); p.resolve({ ok: false, error: `${m.name}: ${why.slice(0, 200)}. It may or may not have happened: look before trying again.` }); }
      w.pending.clear();
      if (workers.get(m.name) === w) workers.delete(m.name);
    };
    child.on('exit', gone); child.on('error', gone);
    child.stdin.on('error', () => {});
    child.stdin.write(fs.readFileSync(SCRIPT).toString('base64') + '\n');
    workers.set(m.name, w);
    return w;
  }
  /** A worker that is replaced: what it is still doing finishes (its input ends, so it exits after), then it goes. */
  function retire(w) {
    if (workers.get(w.name) === w) workers.delete(w.name);
    if (!w.pending.size) { w.child.kill(); return; }
    w.child.stdin.end();
    setTimeout(() => w.child.kill(), 160_000).unref?.();
  }
  /** Run machine.mjs's `todo` on a machine: through its worker, else (an action it never got) the one-shot way. */
  async function overSsh(m, todo, args = {}) {
    const ms = todo === 'launch' || todo === 'adopt' || todo === 'fork' ? 150_000 : todo === 'send' ? 90_000 : todo === 'collect' ? COLLECT_MS : SSH_MS;
    const w = worker(m);
    const up = await Promise.race([w.ready, new Promise((ok) => setTimeout(() => ok(false), ms).unref?.())]);
    if (up && !w.dead) {
      return new Promise((resolve) => {
        const id = w.next++;
        const timer = setTimeout(() => {
          w.pending.delete(id);
          // No answer in time: the next call goes to a new worker on a fresh channel; this one finishes what it has.
          w.stuck = true;
          resolve({ ok: false, error: `${m.name} did not answer in ${ms / 1000}s.${todo === 'collect' || todo === 'peek' ? '' : ' It may or may not have happened: look before trying again.'}` });
        }, ms);
        w.pending.set(id, { resolve, timer });
        w.child.stdin.write(JSON.stringify({ id, do: todo, args }) + '\n');
      });
    }
    // A read waits for the next one rather than starting a second connection.
    if (todo === 'collect') return { ok: false, error: `${m.name}: ${w.err.trim().split('\n').at(-1) || 'no connection'}`.slice(0, 200) };
    return oneShot(m, todo, args, ms);
  }
  /** The one-shot way: machine.mjs on stdin to a `node` of its own, what to do and its arguments in the environment. */
  function oneShot(m, todo, args, ms) {
    const b64 = Buffer.from(JSON.stringify(args)).toString('base64'); // [A-Za-z0-9+/=] only: safe in the remote shell
    const cmd = `${asUser(m)} FLEET_DO=${todo} FLEET_ARGS=${b64} node --input-type=module -`;
    return new Promise((resolve) => {
      const child = execFile('ssh', [...sshArgs(), m.ssh, cmd], { timeout: ms, maxBuffer: 16_000_000 },
        (err, stdout, stderr) => {
          try { return resolve(JSON.parse(String(stdout))); } catch {}
          resolve({ ok: false, error: String(stderr || err?.message || 'no answer').trim().split('\n').at(-1).slice(0, 300) });
        });
      child.stdin.on('error', () => {}); // ssh that exits before it has read the script (host down) must not stop the panel
      child.stdin.end(fs.readFileSync(SCRIPT));
    });
  }
  // Open every machine's connection and worker now, so the first read finds them up; and watch this Mac's hook files.
  setTimeout(() => { for (const m of config().machines) worker(m); }, 0).unref?.();
  try { watchHooks((ev) => onHook(config().self, ev)); } catch {}

  const askHash = (x) => crypto.createHash('sha1').update(`v3\n${x.firstAsk ?? ''}\n${x.lastAsk ?? ''}`).digest('hex').slice(0, 12);
  let writing = null;
  const asked = {}; // key → when haiku was last asked for it: one that came back without a title waits before it is asked again
  /** Titles for the open sessions whose asks changed since their title was written. Runs in the background, one at a time. */
  function writeTitles(reads) {
    if (writing) return;
    const want = [];
    for (const m of reads) for (const a of m.accounts ?? []) for (const x of a.sessions ?? []) {
      if (!(x.firstAsk || x.lastAsk) || !x.sessionId) continue;
      const key = `${m.name}:${x.sessionId}`, h = askHash(x), t = titles[key];
      if (Date.now() - (asked[key] ?? 0) < RESTAGE_MS) continue;
      // Once per session, ever: a title and a group for one that has none yet. Its stage comes from its tools now, and a
      // title once written stays, so asking again would only spend your tokens.
      if (!t?.title || !t.epic) want.push({ key, h, x });
    }
    want.sort((a, b) => Number(b.x.live) - Number(a.x.live)); // the open ones first
    if (!want.length) return;
    const batch = want.slice(0, TITLE_BATCH);
    for (const b of batch) asked[b.key] = Date.now();
    const { epics } = config();
    const prompt = [
      'Each <session> below is a coding agent at work: the folder it is in, whether it is working now, what it was first asked,',
      'what it was asked last, the last thing it said, and its last steps (oldest first). "(continued)" marks a session carried on',
      'from an earlier one: what follows is what that work is for. For each, write a title of 3 to 6 words naming what it is',
      'working on NOW, the way a to-do list would: specific (the system, feature, issue or PR number), plain words, sentence case,',
      'no period, no quotes, never the words "session", "agent" or "continued". The last ask, its last words and its last steps',
      'say what it does now; the first ask only says where it began. Then pick its stage from what its last steps do: planning',
      '(working out what to do: investigating, designing, writing a plan), building (writing or changing code or data), testing',
      '(running tests or checks, reviewing), promoting (pull requests, merging, landing, promotion to main), or deploying (shipping',
      'to a server, a release, production). Then file it under one epic, by what the work is for:',
      ...epics.map((e) => `- ${e.name}: ${e.about}`),
      '- Other: anything else.',
      'Answer only with the schema, one entry per id.',
      '',
      ...batch.map((b, i) => `<session id="${i}">\nfolder: ${clean(b.x.cwd, 120)}\nnow: ${b.x.state ?? ''}\nfirst asked: ${clean(b.x.firstAsk, 500)}\nlast asked: ${clean(b.x.lastAsk, 500)}\nlast said: ${clean(b.x.said, 400)}\nlast steps:\n${(b.x.steps ?? [b.x.doing].filter(Boolean)).map((st) => `- ${clean(st, 140)}`).join('\n')}\n</session>`),
    ].join('\n');
    const id = crypto.randomUUID();
    quiet(id);
    const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE')));
    const cwd = path.join(dir, 'fleet', 'titles');
    fs.mkdirSync(cwd, { recursive: true });
    const done = () => { writing = null; };
    writing = new Promise((resolve) => {
      try {
        const child = execFile('claude', ['-p', '--model', 'sonnet', '--output-format', 'json', '--json-schema', JSON.stringify(titleSchema(epics.map((e) => e.name))),
          '--settings', '{"disableAllHooks": true}', '--no-session-persistence', '--tools', '', '--disallowedTools', ...DENIED_TOOLS,
          '--strict-mcp-config', '--disable-slash-commands', '--session-id', id, '--', prompt], { cwd, env, timeout: 180_000, maxBuffer: 4_000_000 },
        (err, stdout) => {
          let out = null;
          try { out = JSON.parse(String(stdout)); } catch {}
          // It has no tools: a refused one means it tried something else, so throw the answer away.
          const tried = (out?.permission_denials?.length ?? 0) > 0;
          // The schema's answer, or the same answer given as its text.
          const list = !err && !out?.is_error && !tried ? out?.structured_output?.titles ?? titlesIn(out?.result) : null;
          if (Array.isArray(list)) {
            for (const t of list) {
              const b = batch[Number(t?.id)];
              // A title, once written, stays: only its stage and epic move on.
              if (b && typeof t.title === 'string' && t.title.trim()) titles[b.key] = { h: b.h, title: titles[b.key]?.title ?? clean(t.title, 40), stage: STAGES.includes(t.stage) ? t.stage : null,
                epic: [...epics.map((e) => e.name), 'Other'].includes(t.epic) ? t.epic : 'Other', at: Date.now() };
            }
            // Keep a fortnight of them.
            for (const [k, v] of Object.entries(titles)) if (Date.now() - (v.at ?? 0) > 14 * 86_400_000) delete titles[k];
            try { fs.writeFileSync(titlesFile + '.tmp', JSON.stringify(titles)); fs.renameSync(titlesFile + '.tmp', titlesFile); } catch {}
            done(); resolve();
            if (want.length > batch.length) writeTitles(reads); // the rest, straight after
            return;
          }
          console.error('fleet titles:', String(err?.message ?? out?.result ?? 'no answer').slice(0, 300));
          done(); resolve();
        });
        child.stdin?.on('error', () => {});
        child.stdin?.end(); // nothing comes on stdin: it would wait for it otherwise
      } catch (e) { console.error('fleet titles:', e?.message ?? e); done(); resolve(); }
    });
  }
  /** The titles in an answer given as text: the JSON in it, fenced or not. */
  function titlesIn(text) {
    const s = String(text ?? ''), a = s.indexOf('{'), z = s.lastIndexOf('}');
    if (a < 0 || z < a) return null;
    try { const v = JSON.parse(s.slice(a, z + 1)); return Array.isArray(v?.titles) ? v.titles : null; } catch { return null; }
  }

  let last = null, inflight = null, readGen = 0, askedAt = 0, warm = null, restUntil = 0;
  /** After anything that changes a session: the next read starts afresh, and one already under way is not kept. */
  const changed = () => { last = null; inflight = null; readGen++; };
  const good = new Map(); // machine, or machine:engine:account → its last good read
  const doubt = new Map(); // machine:engine:account → reads in a row that listed nothing
  /** A read that failed keeps the last good one, marked stale: one missed read must not take sessions (and an open
   *  terminal) off the page. */
  function steady(reads) {
    return reads.map((m) => {
      if (m.ok === false) return good.has(m.name) ? { ...structuredClone(good.get(m.name)), stale: m.error ?? 'no answer' } : m;
      for (const a of m.accounts ?? []) {
        const k = `${m.name}:${a.engine}:${a.id}`;
        const was = good.get(k) ?? [];
        if (a.error && !(a.sessions ?? []).length && good.has(k)) Object.assign(a, { sessions: structuredClone(was), stale: a.error });
        // A list with no rows at all where there were some (Claude Code's daemon restarting, say) is not believed at once:
        // the last read stands, marked stale, for two more reads. (Closing sessions keeps their rows, as closed.)
        else if (!a.error && !(a.sessions ?? []).length && was.length && (doubt.set(k, (doubt.get(k) ?? 0) + 1), doubt.get(k) <= 2)) {
          Object.assign(a, { sessions: structuredClone(was), stale: 'listed no sessions just now: showing the last read' });
        } else if (!a.error) { doubt.delete(k); good.set(k, structuredClone(a.sessions ?? [])); }
      }
      good.set(m.name, structuredClone(m));
      return m;
    });
  }
  /** Every machine, as of the last read, at once: while a page looks, the panel reads again every WARM_MS behind it, so a
   *  page never waits on a read. Only the first (and the next after a change you made) waits for one. */
  function snapshot() {
    askedAt = Date.now();
    if (!warm) {
      warm = setInterval(() => {
        if (Date.now() - askedAt > WATCH_MS) { clearInterval(warm); warm = null; return; }
        if ((!last || Date.now() - last.at >= WARM_MS) && Date.now() >= restUntil) read().catch(() => {});
      }, 1000);
      warm.unref?.();
    }
    if (!last) return read();
    if (Date.now() - last.at >= WARM_MS && Date.now() >= restUntil) read().catch(() => {});
    return last;
  }
  /** A session's name, group and stage: yours, else the one it has in Claude Code (the app's, claude.ai's, /rename's), else
   *  a written one, but only where Claude Code's is a placeholder (its folder and two characters, "chai-expense-tracker-54").
   *  (A Codex session's "name" is only the app that ran it.) */
  function named(machine, engine, x) {
    const t = titles[`${machine}:${x.sessionId}`], l = labels[`${machine}:${x.sessionId}`];
    const own = engine === 'claude' && typeof x.name === 'string' && x.name.trim() && !PLACEHOLDER.test(x.name.trim()) ? x.name.trim() : null;
    return Object.assign(x, { title: l?.title ?? own ?? t?.title ?? x.name ?? null, stage: x.stage ?? t?.stage ?? null, epic: l?.group ?? t?.epic ?? null });
  }
  /** Groups you made, after the epics in fleet.json. */
  const groupsMade = (epics) => [...new Set(Object.values(labels).map((l) => l.group).filter((g) => g && !epics.some((e) => e.name === g)))].sort();
  /** Why a Codex thread must not be resumed in a terminal here: it runs somewhere (the Codex app), by the last read. */
  function codexBusy(machine, sessionId) {
    if (!last || Date.now() - last.at > 60_000) { soon(); return 'Checking it is not open in Codex: try again in a few seconds.'; }
    const x = last.machines.find((m) => m.name === machine)?.accounts?.flatMap((a) => (a.engine === 'codex' ? a.sessions ?? [] : [])).find((s) => s.sessionId === sessionId);
    return x?.live ? 'It is open in Codex (the app or a terminal): finish there, or close it there first.' : null;
  }
  // ---------------------------------------------------------------- live, between reads
  // A hook event (a session's state as Claude Code says it, the moment it changes): that session changes on the page at
  // once, the pages are told, and you get a notification when it now needs you. A session the last read doesn't have (just
  // started) brings a read a moment later.
  const listeners = new Set();
  let soonT = null, tellT = null;
  function onHook(machine, { sessionId, hook }) {
    const x = last?.machines.find((m) => m.name === machine)?.accounts?.flatMap((a) => (a.engine === 'claude' ? a.sessions ?? [] : [])).find((y) => y.sessionId === sessionId);
    if (!x || !x.live) { soon(); return; }
    // The last read's job state, without its word on waiting or busy: the hook is newer than that.
    const r = { pid: x.pid, active: x.rs?.active ?? x.live, state: x.rs?.state === 'blocked' ? null : x.rs?.state ?? null };
    const { state, live } = sessionState(r, { writtenAt: x.writtenAt }, hook);
    if (state === x.state && live === x.live) return;
    Object.assign(x, { state, live, hookAt: hook.at });
    looked();
  }
  function soon() {
    if (soonT) return;
    soonT = setTimeout(() => { soonT = null; read().catch(() => {}); }, 800);
    soonT.unref?.();
  }
  /** After a read or a hook event: a notification for each session that now needs you, and the pages told. */
  const was = new Map(); // machine:session → its state when last looked at
  function looked() {
    if (!last) return;
    const first = !was.size;
    for (const m of last.machines) for (const a of m.accounts ?? []) for (const x of a.sessions ?? []) {
      const k = `${m.name}:${x.sessionId ?? x.id}`, before = was.get(k);
      if (!first && x.live && x.state === 'needs-you' && before !== undefined && before !== 'needs-you') notify(m.name, x);
      was.set(k, x.live ? x.state : 'closed');
    }
    clearTimeout(tellT);
    tellT = setTimeout(() => { for (const fn of listeners) try { fn(); } catch {} }, 50);
  }
  /** Pages hear when something changed (server.mjs passes it on over their sockets). */
  function onChange(fn) { listeners.add(fn); return () => listeners.delete(fn); }
  // A Mac notification when a session comes to need you: once per session a minute at most; "notify": false in fleet.json
  // turns them off.
  const notified = new Map();
  function notify(machine, x) {
    let off = false;
    try { off = JSON.parse(fs.readFileSync(file, 'utf8')).notify === false; } catch {}
    const k = `${machine}:${x.sessionId ?? x.id}`;
    if (off || process.platform !== 'darwin' || Date.now() - (notified.get(k) ?? 0) < 60_000) return;
    notified.set(k, Date.now());
    const what = clean(typeof x.waitingFor === 'string' ? x.waitingFor : x.doing ?? 'It waits for you', 160) || 'It waits for you';
    execFile('osascript', ['-e', 'on run argv', '-e', 'display notification (item 1 of argv) with title (item 2 of argv) subtitle (item 3 of argv)', '-e', 'end run',
      what, 'Fleet · needs you', clean(`${x.title ?? x.name ?? 'A session'} · ${machine}`, 120)], { timeout: 5000 }, () => {});
  }

  // Every session ever seen, so none just disappears: one that a machine that answered no longer reports stays on the
  // page, closed, as it was last seen, until you archive it. machine:session → what it was.
  const seenFile = path.join(dir, 'fleet-seen.json');
  let seen = {};
  try { seen = JSON.parse(fs.readFileSync(seenFile, 'utf8')); } catch {}
  let seenDirty = false, seenWritten = 0;
  const KEPT = ['engine', 'id', 'sessionId', 'name', 'title', 'cwd', 'startedAt', 'lastAt', 'model', 'via', 'web', 'effort', 'kind', 'stage', 'turnOver', 'branch'];
  function ledger(reads) {
    for (const m of reads) {
      const reported = new Set();
      for (const a of m.accounts ?? []) for (const x of a.sessions ?? []) {
        const k = `${m.name}:${x.sessionId ?? x.id}`;
        reported.add(k);
        const rec = { acct: `${a.engine}:${a.id}`, ...Object.fromEntries(KEPT.map((f) => [f, x[f] ?? null])) };
        if (JSON.stringify(seen[k]?.rec) !== JSON.stringify(rec)) { seen[k] = { rec, at: Date.now() }; seenDirty = true; }
        else seen[k].at = Date.now();
      }
      if (m.ok === false || m.stale) continue; // a machine that didn't answer shows its last read anyway
      for (const [k, { rec }] of Object.entries(seen)) {
        if (!k.startsWith(`${m.name}:`) || reported.has(k) || archived[k]) continue;
        const a = (m.accounts ?? []).find((y) => `${y.engine}:${y.id}` === rec.acct);
        if (a) a.sessions = [...(a.sessions ?? []), { ...rec, acct: undefined, live: false, state: 'closed', unlisted: true }];
      }
    }
    // An archived one long gone is forgotten.
    for (const [k, v] of Object.entries(seen)) if (archived[k] && Date.now() - v.at > 30 * 86_400_000) { delete seen[k]; seenDirty = true; }
    if (seenDirty && Date.now() - seenWritten > 20_000) {
      try { fs.writeFileSync(seenFile + '.tmp', JSON.stringify(seen)); fs.renameSync(seenFile + '.tmp', seenFile); seenDirty = false; seenWritten = Date.now(); } catch {}
    }
  }

  /** One read of every machine; reads asked for while one runs share it. */
  async function read() {
    const gen = readGen;
    if (inflight) return inflight;
    const mine = (async () => {
      const { machines, names, self, epics, logins } = config();
      const reads = steady(await Promise.all([
        collect().then((d) => ({ ...d, name: self, local: true })).catch((e) => ({ name: self, local: true, ok: false, error: String(e?.message ?? e) })),
        ...machines.map((m) => overSsh(m, 'collect').then((d) => ({ ...d, name: m.name, ssh: m.ssh, user: m.user, local: false }))),
      ]));
      ledger(reads);
      for (const m of reads) for (const a of m.accounts ?? []) {
        const real = a.engine === 'claude' && logins[`${m.name}:${a.id}`];
        if (real && a.account) a.account = { ...a.account, email: real, says: a.account.email };
        // An archived session that runs again (resumed) is back on the page.
        a.sessions = (a.sessions ?? []).filter((x) => x.live || !archived[`${m.name}:${x.sessionId ?? x.id}`]);
        for (const x of a.sessions) named(m.name, a.engine, x);
      }
      try { writeTitles(reads); } catch (e) { console.error('fleet titles:', e?.message ?? e); }
      // What the page shows of each session: what it was asked and its steps are for the titles only (half the bytes).
      const shown = reads.map((m) => ({ ...m, accounts: (m.accounts ?? []).map((a) => ({ ...a, sessions: (a.sessions ?? []).map(({ firstAsk, lastAsk, steps, ...x }) =>
        ({ ...x, said: typeof x.said === 'string' ? x.said.slice(0, 400) : x.said, running: x.running && { step: x.running.step, since: x.running.since, tool: x.running.tool } })) })) }));
      const fresh = { ok: true, at: Date.now(), machines: shown, names, epics, groups: groupsMade(epics), configured: machines.map((m) => m.name), config: file.replace(os.homedir(), '~') };
      // A close, archive, message or launch that landed while this read ran makes it out of date: hand it back, keep none.
      if (gen === readGen) { last = fresh; looked(); }
      return fresh;
    })().catch((e) => { restUntil = Date.now() + 10_000; throw e; }).finally(() => { if (inflight === mine) inflight = null; });
    inflight = mine;
    return mine;
  }

  /** Start an agent on a machine. Checked here and again on the machine. */
  async function launch(body) {
    const where = String(body?.machine ?? '');
    const engine = String(body?.engine ?? '');
    if (!ENGINES.has(engine)) return { ok: false, error: 'Pick Claude, Codex, Muse or the fleet.' };
    const profile = String(body?.profile ?? '');
    if (engine !== 'fleet' && engine !== 'muse' && !/^[\w.-]{1,40}$/.test(profile)) return { ok: false, error: 'Pick an account.' };
    const model = String(body?.model ?? '');
    if (!MODELS.has(model)) return { ok: false, error: 'Pick a model from the list.' };
    const cwd = String(body?.cwd ?? '').trim();
    if (cwd.length > 1000) return { ok: false, error: 'That folder path is too long.' };
    const effort = String(body?.effort ?? '');
    if (effort && !['low', 'medium', 'high', 'xhigh', 'max'].includes(effort)) return { ok: false, error: 'Pick an effort from the list.' };
    const args = { engine, profile, model, effort, cwd, prompt: String(body?.prompt ?? ''), name: String(body?.name ?? '') };
    let r;
    if (where === config().self) r = await launchHere(args);
    else {
      const m = config().machines.find((x) => x.name === where);
      if (!m) return { ok: false, error: `There is no machine called ${where} in fleet.json.` };
      r = await overSsh(m, 'launch', args);
    }
    changed(); // the next snapshot reads afresh, so the new agent shows
    return r;
  }

  /** Run one of machine.mjs's actions where the session lives: here, or over SSH. */
  async function onMachine(where, todo, args, here) {
    if (where === config().self) return here(args);
    const m = config().machines.find((x) => x.name === where);
    return m ? overSsh(m, todo, args) : { ok: false, error: `There is no machine called ${where} in fleet.json.` };
  }
  const SESSION = /^[0-9a-f-]{36}$|^[0-9a-z-]{20,60}$/i;
  /** A session's conversation up close. */
  function peek(body) {
    const args = { engine: body?.engine === 'codex' ? 'codex' : 'claude', profile: String(body?.profile ?? ''), sessionId: String(body?.sessionId ?? '') };
    if (!/^[\w.-]{1,40}$/.test(args.profile) || !SESSION.test(args.sessionId)) return { ok: false, error: 'Pick a session.' };
    return onMachine(String(body?.machine ?? ''), 'peek', args, peekHere);
  }
  /** A message to a running background job. */
  async function send(body) {
    const args = { profile: String(body?.profile ?? ''), sessionId: String(body?.sessionId ?? ''), text: String(body?.text ?? '') };
    if (!/^[\w.-]{1,40}$/.test(args.profile) || !/^[0-9a-f-]{36}$/.test(args.sessionId)) return { ok: false, error: 'Pick a session.' };
    const r = await onMachine(String(body?.machine ?? ''), 'send', args, sendHere);
    changed();
    return r;
  }

  /** Close an open session where it runs. */
  async function close(body) {
    const args = { profile: String(body?.profile ?? ''), sessionId: String(body?.sessionId ?? '') };
    if (!/^[\w.-]{1,40}$/.test(args.profile) || !/^[0-9a-f-]{36}$/.test(args.sessionId)) return { ok: false, error: 'Pick a session.' };
    const r = await onMachine(String(body?.machine ?? ''), 'close', args, closeHere);
    changed();
    return r;
  }
  /** Archive a closed session: off the page, and a background job's record removed. */
  async function archive(body) {
    const machine = String(body?.machine ?? ''), key = String(body?.sessionId ?? body?.id ?? '');
    if (!/^[\w.-]{1,80}$/.test(key)) return { ok: false, error: 'Pick a session.' };
    if (body?.engine === 'claude' && /^[0-9a-f]{8}$/.test(String(body?.id ?? ''))) {
      const r = await onMachine(machine, 'archive', { profile: String(body?.profile ?? ''), id: String(body.id) }, archiveHere);
      if (!r.ok) return r;
    }
    archived[`${machine}:${key}`] = Date.now();
    try { fs.writeFileSync(archivedFile + '.tmp', JSON.stringify(archived)); fs.renameSync(archivedFile + '.tmp', archivedFile); } catch {}
    changed();
    return { ok: true };
  }

  /** Your name or group for a session; an empty one puts Haiku's back. */
  function label(body) {
    const machine = String(body?.machine ?? ''), sid = String(body?.sessionId ?? '');
    if (!NAME.test(machine) || !/^[0-9a-f-]{36}$|^[\w-]{8,60}$/i.test(sid)) return { ok: false, error: 'Pick a session.' };
    const key = `${machine}:${sid}`, l = { ...labels[key] };
    if (typeof body.title === 'string') { const t = clean(body.title, 60); if (t) l.title = t; else delete l.title; }
    if (typeof body.group === 'string') {
      const g = clean(body.group, 40);
      if (g && !NAME.test(g)) return { ok: false, error: 'A group name uses letters, digits, spaces, dots and dashes.' };
      if (g) l.group = g; else delete l.group;
    }
    if (Object.keys(l).length) labels[key] = l; else delete labels[key];
    try { fs.writeFileSync(labelsFile + '.tmp', JSON.stringify(labels)); fs.renameSync(labelsFile + '.tmp', labelsFile); } catch {}
    // On the page at once: the last read gets it now (a read under way names its sessions as it ends, so it has it too).
    if (last) {
      const m = last.machines.find((y) => y.name === machine);
      for (const a of m?.accounts ?? []) for (const x of a.sessions ?? []) if (x.sessionId === sid) named(machine, a.engine, x);
      last.groups = groupsMade(config().epics);
    }
    return { ok: true };
  }

  /** Move an idle session into the background, conversation kept, so it opens as a terminal here. */
  async function adopt(body) {
    const args = { profile: String(body?.profile ?? ''), sessionId: String(body?.sessionId ?? '') };
    if (!/^[\w.-]{1,40}$/.test(args.profile) || !/^[0-9a-f-]{36}$/.test(args.sessionId)) return { ok: false, error: 'Pick a session.' };
    const r = await onMachine(String(body?.machine ?? ''), 'adopt', args, adoptHere);
    changed();
    return r;
  }

  /** Continue a session as a copy in the background, the original untouched. */
  async function fork(body) {
    const args = { profile: String(body?.profile ?? ''), sessionId: String(body?.sessionId ?? '') };
    if (!/^[\w.-]{1,40}$/.test(args.profile) || !/^[0-9a-f-]{36}$/.test(args.sessionId)) return { ok: false, error: 'Pick a session.' };
    const r = await onMachine(String(body?.machine ?? ''), 'fork', args, forkHere);
    changed();
    return r;
  }

  /** Stop the command a session's current step runs, so what you queued reaches it now (machine.mjs interject). */
  async function interject(body) {
    const args = { profile: String(body?.profile ?? ''), sessionId: String(body?.sessionId ?? '') };
    if (!/^[\w.-]{1,40}$/.test(args.profile) || !/^[0-9a-f-]{36}$/.test(args.sessionId)) return { ok: false, error: 'Pick a session.' };
    const r = await onMachine(String(body?.machine ?? ''), 'interject', args, interjectHere);
    soon();
    return r;
  }

  /** Archive several closed sessions at once (an epic's): each as archive() does; a job record that refuses stays. */
  async function archiveAll(body) {
    const list = Array.isArray(body?.sessions) ? body.sessions.slice(0, 300) : [];
    let done = 0;
    const kept = [];
    for (const x of list) { const r = await archive(x); if (r.ok) done++; else kept.push(r.error); }
    return { ok: true, done, kept };
  }

  return { snapshot, launch, peek, send, close, archive, archiveAll, label, adopt, fork, interject, config, codexBusy, onChange };
}
