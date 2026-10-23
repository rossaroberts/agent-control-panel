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
import { execFile } from 'node:child_process';
import { collect, launch as launchHere, peek as peekHere, send as sendHere, close as closeHere, archive as archiveHere } from './machine.mjs';
import { DENIED_TOOLS } from '../world.mjs';

const SCRIPT = new URL('./machine.mjs', import.meta.url);
const FRESH_MS = 8000;
const SSH_MS = 25_000;
const HOST = /^[\w.@-]{1,100}$/, USER = /^[a-z_][\w-]{0,31}$/, NAME = /^[\w .-]{1,40}$/;
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
// A working session's stage moves on without a new ask (building, then testing, then promoting): ask again this often.
const RESTAGE_MS = 10 * 60_000;
const clean = (v, n) => String(v ?? '').replace(/[<>]/g, '').replace(/\s+/g, ' ').trim().slice(0, n);

export function createFleet({ dir, quiet = () => {} }) {
  const file = path.join(dir, 'fleet.json');
  const titlesFile = path.join(dir, 'fleet-titles.json');
  let titles = {};
  try { titles = JSON.parse(fs.readFileSync(titlesFile, 'utf8')); } catch {}
  // Sessions you archived: `machine:sessionId` → when. They leave the page; a closed one has nothing else to clear.
  const archivedFile = path.join(dir, 'fleet-archived.json');
  let archived = {};
  try { archived = JSON.parse(fs.readFileSync(archivedFile, 'utf8')); } catch {}
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
    return { machines, names, self, epics, file };
  }

  /** Run machine.mjs on a machine over SSH, with what to do and its arguments in the environment. */
  function overSsh(m, todo, args = {}) {
    const b64 = Buffer.from(JSON.stringify(args)).toString('base64'); // [A-Za-z0-9+/=] only: safe in the remote shell
    const env = `env FLEET_DO=${todo} FLEET_ARGS=${b64}`;
    const cmd = m.user
      ? `sudo -n -u ${m.user} -H ${env} XDG_RUNTIME_DIR=/run/user/$(id -u ${m.user}) node --input-type=module -`
      : `${env} node --input-type=module -`;
    return new Promise((resolve) => {
      const child = execFile('ssh', ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', m.ssh, cmd],
        { timeout: todo === 'launch' ? 150_000 : SSH_MS, maxBuffer: 16_000_000 },
        (err, stdout, stderr) => {
          try { return resolve(JSON.parse(String(stdout))); } catch {}
          resolve({ ok: false, error: String(stderr || err?.message || 'no answer').trim().split('\n').at(-1).slice(0, 300) });
        });
      child.stdin.end(fs.readFileSync(SCRIPT));
    });
  }

  const askHash = (x) => crypto.createHash('sha1').update(`v2\n${x.firstAsk ?? ''}\n${x.lastAsk ?? ''}`).digest('hex').slice(0, 12);
  let writing = null;
  /** Titles for the open sessions whose asks changed since their title was written. Runs in the background, one at a time. */
  function writeTitles(reads) {
    if (writing) return;
    const want = [];
    for (const m of reads) for (const a of m.accounts ?? []) for (const x of a.sessions ?? []) {
      if (!(x.firstAsk || x.lastAsk) || !x.sessionId) continue;
      const key = `${m.name}:${x.sessionId}`, h = askHash(x), t = titles[key];
      if (t?.h !== h || !t.epic || (x.state === 'working' && Date.now() - (t.at ?? 0) > RESTAGE_MS)) want.push({ key, h, x });
    }
    want.sort((a, b) => Number(b.x.live) - Number(a.x.live)); // the open ones first
    if (!want.length) return;
    const batch = want.slice(0, TITLE_BATCH);
    const { epics } = config();
    const prompt = [
      'Each <session> below is a coding agent at work: the folder it is in, what it was first asked, what it was asked last, the',
      'last thing it said, and the step it is on. For each, write a title of 3 to 5 words naming what it is working on now, the way a',
      'to-do list would: specific (the system, feature, issue or PR), plain words, sentence case, no period, no quotes, never the',
      'word "session". When the last ask changed the subject, the title follows the last ask. Then pick its stage: planning',
      '(working out what to do: investigating, designing, writing a plan), building (writing or changing code or data), testing',
      '(running tests or checks, reviewing), promoting (pull requests, merging, landing, promotion to main), or deploying (shipping',
      'to a server, a release, production). Then file it under one epic, by what the work is for:',
      ...epics.map((e) => `- ${e.name}: ${e.about}`),
      '- Other: anything else.',
      'Answer only with the schema, one entry per id.',
      '',
      ...batch.map((b, i) => `<session id="${i}">\nfolder: ${clean(b.x.cwd, 120)}\nfirst asked: ${clean(b.x.firstAsk, 400)}\nlast asked: ${clean(b.x.lastAsk, 400)}\nlast said: ${clean(b.x.said, 300)}\nstep: ${clean(b.x.doing, 200)}\n</session>`),
    ].join('\n');
    const id = crypto.randomUUID();
    quiet(id);
    const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE')));
    const cwd = path.join(dir, 'fleet', 'titles');
    fs.mkdirSync(cwd, { recursive: true });
    writing = new Promise((resolve) => execFile('claude', ['-p', '--model', 'haiku', '--output-format', 'json', '--json-schema', JSON.stringify(titleSchema(epics.map((e) => e.name))),
      '--settings', '{"disableAllHooks": true}', '--no-session-persistence', '--tools', '', '--disallowedTools', ...DENIED_TOOLS,
      '--strict-mcp-config', '--disable-slash-commands', '--session-id', id, '--', prompt], { cwd, env, timeout: 180_000, maxBuffer: 4_000_000 },
    (err, stdout) => {
      let out = null;
      try { out = JSON.parse(String(stdout)); } catch {}
      // A structured answer takes two turns; more, or a refused tool, means it tried something else: throw it away.
      const tried = (Number.isInteger(out?.num_turns) && out.num_turns > 2) || (out?.permission_denials?.length ?? 0) > 0;
      const list = !err && !out?.is_error && !tried ? out?.structured_output?.titles : null;
      if (Array.isArray(list)) {
        for (const t of list) {
          const b = batch[Number(t?.id)];
          if (b && typeof t.title === 'string' && t.title.trim()) titles[b.key] = { h: b.h, title: clean(t.title, 40), stage: STAGES.includes(t.stage) ? t.stage : null,
            epic: [...epics.map((e) => e.name), 'Other'].includes(t.epic) ? t.epic : 'Other', at: Date.now() };
        }
        // Keep a fortnight of them.
        for (const [k, v] of Object.entries(titles)) if (Date.now() - (v.at ?? 0) > 14 * 86_400_000) delete titles[k];
        try { fs.writeFileSync(titlesFile + '.tmp', JSON.stringify(titles)); fs.renameSync(titlesFile + '.tmp', titlesFile); } catch {}
        last = null;
        writing = null;
        resolve();
        if (want.length > batch.length) writeTitles(reads); // the rest, straight after
        return;
      } else console.error('fleet titles:', err?.message ?? out?.result ?? 'no answer');
      writing = null;
      resolve();
    }));
  }

  let last = null, inflight = null, readGen = 0;
  /** After anything that changes a session: the next read starts afresh, and one already under way is not kept. */
  const changed = () => { last = null; inflight = null; readGen++; };
  /** Every machine, read at most every FRESH_MS however many pages ask. */
  async function snapshot() {
    if (last && Date.now() - last.at < FRESH_MS) return last;
    const gen = readGen;
    inflight ??= (async () => {
      const { machines, names, self, epics } = config();
      const reads = await Promise.all([
        collect().then((d) => ({ ...d, name: self, local: true })).catch((e) => ({ name: self, local: true, ok: false, error: String(e?.message ?? e) })),
        ...machines.map((m) => overSsh(m, 'collect').then((d) => ({ ...d, name: m.name, ssh: m.ssh, user: m.user, local: false }))),
      ]);
      for (const m of reads) for (const a of m.accounts ?? []) {
        // An archived session that runs again (resumed) is back on the page.
        a.sessions = (a.sessions ?? []).filter((x) => x.live || !archived[`${m.name}:${x.sessionId ?? x.id}`]);
        for (const x of a.sessions) {
          const t = titles[`${m.name}:${x.sessionId}`];
          Object.assign(x, { title: t?.title ?? null, stage: t?.stage ?? null, epic: t?.epic ?? null });
        }
      }
      writeTitles(reads);
      const fresh = { ok: true, at: Date.now(), machines: reads, names, epics, configured: machines.map((m) => m.name), config: file.replace(os.homedir(), '~') };
      // A close, archive, message or launch that landed while this read ran makes it out of date: hand it back, keep none.
      if (gen === readGen) last = fresh;
      inflight = null;
      return fresh;
    })();
    return inflight;
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

  /** Archive several closed sessions at once (an epic's): each as archive() does; a job record that refuses stays. */
  async function archiveAll(body) {
    const list = Array.isArray(body?.sessions) ? body.sessions.slice(0, 300) : [];
    let done = 0;
    const kept = [];
    for (const x of list) { const r = await archive(x); if (r.ok) done++; else kept.push(r.error); }
    return { ok: true, done, kept };
  }

  return { snapshot, launch, peek, send, close, archive, archiveAll, config };
}
