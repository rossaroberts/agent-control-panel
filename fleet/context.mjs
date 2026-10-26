// Context: every document that shapes how Ross's Claude Code sessions start, gathered for the Context page (/context).
//
// Read-only. The page asks `read({})` for the list and `read({ note })` for one note of the memory folder; each document is
// { id, title, about, path, at, markdown } and the page renders the markdown (public/context-render.mjs, raw HTML never
// shown as HTML). The documents are the config array DOCS below: one entry each, in the order the page lists them; a
// new document is one more entry. Nothing here writes, and nothing here reads outside these paths.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { noteName } from '../public/context-render.mjs';

/** A document longer than this is cut (its tail says so): the page keeps each one in memory. */
export const MAX_DOC = 512 * 1024;
/** The shell functions that start a session: claude, claude-max, claude-assist…, gpt, codex. */
export const START_FN = /^(?:claude|gpt$|codex$)/;

// Where the documents are, from the home folder. The project's memory folder is Claude Code's own (the project's path with
// every other character a dash).
const REPO = 'projects/chai-expense-tracker';
const FM = 'projects/chai-fm';
const memoryDir = (home) => path.join(home, '.claude', 'projects', path.join(home, REPO).replace(/[^A-Za-z0-9]/g, '-'), 'memory');

// ---------------------------------------------------------------- small pure helpers

/** `~/…` for a path under the home folder. */
export const tilde = (p, home) => (typeof p === 'string' && home && (p === home || p.startsWith(home + path.sep)) ? '~' + p.slice(home.length) : p);
/** A markdown code span that holds `v` whatever it holds, or a dash when there is nothing. */
export function code(v) {
  if (v == null || v === '') return '—';
  const s = String(v).replace(/\s+/g, ' ');
  const fence = '`'.repeat(Math.max(0, ...(s.match(/`+/g) ?? []).map((r) => r.length)) + 1);
  return `${fence}${s.startsWith('`') || s.endsWith('`') ? ` ${s} ` : s}${fence}`;
}
/** A fence longer than any run of backticks in the text, so the text can't close it. */
export const fenceFor = (text) => '`'.repeat(Math.max(2, ...(String(text).match(/`+/g) ?? []).map((r) => r.length)) + 1);
const oneLine = (v, n) => String(v ?? '').replace(/[\x00-\x1f\x7f]+/g, ' ').replace(/\s+/g, ' ').trim().slice(0, n);

/** A file as text, at most `max` bytes (a longer one ends with a note saying so), with its modified time; null if there is no such file. */
export function readFile(file, max = MAX_DOC) {
  let fd;
  try {
    const st = fs.statSync(file);
    if (!st.isFile()) return null;
    fd = fs.openSync(file, 'r');
    const buf = Buffer.alloc(Math.min(st.size, max));
    fs.readSync(fd, buf, 0, buf.length, 0);
    let text = buf.toString('utf8');
    if (st.size > max) text += `\n\n---\n\n_Cut here: the first ${Math.round(max / 1024)} KB of ${Math.round(st.size / 1024)} KB._\n`;
    return { text, at: Math.round(st.mtimeMs) };
  } catch { return null; } finally { if (fd !== undefined) try { fs.closeSync(fd); } catch {} }
}

// ---------------------------------------------------------------- 1. start scripts (the shell functions in ~/.zshrc)

/** The functions of a shell file: top-level `name() {…}` and `function name {…}`, each with the comment lines directly above it
 *  and its text. The end is found by counting braces outside quotes and comments (not understood: heredocs, and a quote
 *  inside `$( )` inside double quotes). A function that never closes runs to the end of the file. */
export function zshFunctions(text) {
  const lines = String(text ?? '').split('\n');
  const starts = [];
  let at = 0;
  for (let i = 0; i < lines.length; i++) { starts.push(at); at += lines[i].length + 1; }
  const head = /^(?:function\s+([\w.:-]+)\s*(?:\(\s*\))?|([\w.:-]+)\s*\(\s*\))\s*(\{|$)/;
  const out = [];
  for (let i = 0; i < lines.length;) {
    const m = head.exec(lines[i]);
    let open = m && (m[3] === '{' ? starts[i] + m[0].length - 1 : -1);
    if (m && open === -1) { // the brace is on the next line
      const j = i + 1;
      if (j < lines.length && /^\s*\{/.test(lines[j])) open = starts[j] + lines[j].indexOf('{'); else { i++; continue; }
    }
    if (!m) { i++; continue; }
    // from the opening brace: depth, outside single quotes, double quotes, escapes and # comments
    let depth = 0, state = '', end = text.length;
    for (let k = open; k < text.length; k++) {
      const ch = text[k];
      if (state === "'") { if (ch === "'") state = ''; continue; }
      if (state === '"') { if (ch === '\\') k++; else if (ch === '"') state = ''; continue; }
      if (state === '#') { if (ch === '\n') state = ''; continue; }
      if (ch === '\\') { k++; continue; }
      if (ch === "'" || ch === '"') { state = ch; continue; }
      if (ch === '#' && (k === 0 || /[\s;&|]/.test(text[k - 1]))) { state = '#'; continue; } // a word's first character: not `${#x}`, not zsh's `(#i)`
      if (ch === '{') depth++;
      else if (ch === '}' && --depth === 0) { end = k + 1; break; }
    }
    // the last line it reaches
    let last = i;
    while (last + 1 < lines.length && starts[last + 1] < end) last++;
    const comment = [];
    for (let j = i - 1; j >= 0 && /^\s*#/.test(lines[j]); j--) comment.unshift(lines[j].replace(/^\s*#+ ?/, '').trimEnd());
    out.push({ name: m[1] ?? m[2], comment, body: lines.slice(i, last + 1).join('\n') });
    i = last + 1;
  }
  return out;
}

/** The start functions as markdown: a heading each, its comment as text, the function in a zsh block. */
export function startScriptsMarkdown(fns) {
  const list = fns.filter((f) => START_FN.test(f.name));
  if (!list.length) return '_No start functions (claude…, gpt, codex) in this file._\n';
  return list.map((f) => {
    const fence = fenceFor(f.body);
    return `## ${f.name}\n\n${f.comment.length ? f.comment.join('\n') + '\n\n' : ''}${fence}zsh\n${f.body}\n${fence}\n`;
  }).join('\n');
}

// ---------------------------------------------------------------- 2. session defaults (profile settings, every machine)

/** The profile defaults of every machine as one table: `results` is [{ name, r }] (fleet.profileDefaults()), `logins` the
 *  fleet.json corrections (`machine:profile` → email). A machine that did not answer is named after the table. */
export function defaultsMarkdown(results, logins = {}) {
  const rows = [], down = [];
  let at = 0;
  for (const { name, r } of results) {
    if (!r?.ok) { down.push({ name, why: oneLine(r?.error, 160) }); continue; }
    for (const p of r.profiles ?? []) {
      at = Math.max(at, p.at ?? 0);
      rows.push([name, p.id, logins[`${name}:${p.id}`] ?? p.email, p.model, p.modelEffort, p.effort, p.defaultMode, p.tui].map(code));
    }
  }
  const head = ['machine', 'profile', 'account', 'model', 'fable-5-1 effort', 'effort', 'mode', 'tui'];
  let md = rows.length ? `| ${head.join(' | ')} |\n|${head.map(() => ' --- ').join('|')}|\n${rows.map((r) => `| ${r.join(' | ')} |`).join('\n')}\n`
    : '_No Claude Code profile was read._\n';
  md += '\n`fable-5-1 effort` is `modelSettings["claude-fable-5-1"].effortLevel`, `effort` the profile\'s own `effortLevel`, `mode` is `permissions.defaultMode`. Only these keys are read.\n';
  for (const d of down) md += `\n**${code(d.name)} was unreachable**${d.why ? `: ${code(d.why)}` : ''}. Its profiles are not in the table.\n`;
  return { markdown: md, at: at || null };
}

// ---------------------------------------------------------------- 3. start-up routine (the project's hooks)

// The order a session meets its hooks.
const EVENTS = ['SessionStart', 'UserPromptSubmit', 'PreToolUse', 'PermissionRequest', 'PostToolUse', 'Notification', 'PreCompact', 'Stop', 'SubagentStop', 'SessionEnd'];
// A hook this project has, in a plain sentence: the first whose pattern matches its command. Others get a sentence from their event.
export const HOOK_NOTES = [
  [/guard-destructive-bash/, 'Stops a short list of irreversible shell commands before they run; every other command passes without a word.'],
  [/\[resume\]/, 'Prints a reminder into the new session: read the newest HANDOFF file and run `npm run catchup` (just the catchup when there is no handoff), and a warning when fan-out mode is on.'],
  [/docs:check/, 'When a turn ends with changed markdown, docs or migrations, runs the docs check and prints a note if it fails; it never blocks, and it is skipped in fan-out mode.'],
];
const EVENT_SAYS = {
  SessionStart: 'Runs when a session starts or resumes; what it prints goes into the session.',
  UserPromptSubmit: 'Runs when you send a message, before Claude sees it.',
  PreToolUse: 'Runs before a tool call, and can block it.',
  PermissionRequest: 'Runs when a tool call asks for permission, and can answer it.',
  PostToolUse: 'Runs after a tool call has finished.',
  Notification: 'Runs when Claude Code wants your attention.',
  PreCompact: 'Runs before the conversation is compacted.',
  Stop: 'Runs each time Claude finishes a turn.',
  SubagentStop: 'Runs when a subagent finishes.',
  SessionEnd: 'Runs when a session ends.',
};
/** One plain sentence for a hook. */
export function hookSays(event, matcher, command) {
  const note = HOOK_NOTES.find(([re]) => re.test(command ?? ''));
  if (note) return note[1];
  const base = EVENT_SAYS[event] ?? `Runs on ${event}.`;
  return matcher && matcher !== '*' ? `${base.replace(/\.$/, '')}, for ${matcher} only.` : base;
}
/** The hooks of a settings file as markdown: a section per event, in the order a session meets them, each hook with its matcher,
 *  its command in a code block and what it does. */
export function hooksMarkdown(settings) {
  const hooks = settings?.hooks && typeof settings.hooks === 'object' && !Array.isArray(settings.hooks) ? settings.hooks : {};
  const events = [...EVENTS.filter((e) => e in hooks), ...Object.keys(hooks).filter((e) => !EVENTS.includes(e))];
  if (!events.length) return '_This project has no hooks._\n';
  return events.map((event) => {
    const groups = Array.isArray(hooks[event]) ? hooks[event] : [];
    const body = groups.flatMap((g) => (Array.isArray(g?.hooks) ? g.hooks : []).map((h) => {
      const command = typeof h?.command === 'string' ? h.command : typeof h?.url === 'string' ? h.url : typeof h?.prompt === 'string' ? h.prompt : JSON.stringify(h ?? null);
      const matcher = typeof g.matcher === 'string' && g.matcher ? g.matcher : '';
      const fence = fenceFor(command);
      return `**Matcher:** ${matcher ? code(matcher) : 'none (every time)'}${h?.type && h.type !== 'command' ? ` · **type:** ${code(h.type)}` : ''}\n\n${fence}sh\n${command}\n${fence}\n\n${hookSays(event, matcher, command)}\n`;
    }));
    return `## ${event}\n\n${body.join('\n') || '_Nothing set._\n'}`;
  }).join('\n');
}

// ---------------------------------------------------------------- 6. memory: the index, its notes, the sync state

/** A memory note's front matter (`---` … `---`): its name and description, and the rest. */
export function frontmatter(text) {
  const m = /^---[ \t]*\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)/.exec(text);
  if (!m) return { meta: {}, body: text };
  const meta = {};
  for (const k of ['name', 'description']) {
    const v = new RegExp(`^${k}:[ \\t]*(.*)$`, 'm').exec(m[1])?.[1]?.trim();
    if (!v) continue;
    let s = v;
    if (v.startsWith('"')) { try { s = JSON.parse(v); } catch { s = v.replace(/^"|"$/g, ''); } } else if (v.startsWith("'")) s = v.replace(/^'|'$/g, '');
    meta[k] = oneLine(s, 400);
  }
  return { meta, body: text.slice(m[0].length) };
}

const asMs = (v) => {
  const n = typeof v === 'number' ? v : typeof v === 'string' && v.trim() ? (/^\d+(\.\d+)?$/.test(v.trim()) ? Number(v) : Date.parse(v)) : NaN;
  if (!Number.isFinite(n) || n <= 0) return null;
  return Math.round(n < 1e11 ? n * 1000 : n); // seconds or milliseconds
};
const stateOf = (v) => (v === true ? 'ok' : v === false ? 'failed' : typeof v === 'string' ? oneLine(v, 24) : null);
/** How often the sync says it runs ("5 min", "30 s", "1 h"), in ms; null when it does not say. */
const everyMs = (v) => {
  const m = /^\s*(\d+(?:\.\d+)?)\s*(s|sec|secs|seconds?|m|min|mins|minutes?|h|hr|hrs|hours?)\s*$/i.exec(typeof v === 'string' ? v : '');
  return m ? Math.round(Number(m[1]) * ({ s: 1000, m: 60_000, h: 3_600_000 }[m[2][0].toLowerCase()])) : null;
};
/** What ~/.memory-sync/status.json says, as plain fields for one line: { ok (true, false or null), at (ms), every (ms), machines
 *  [{ name, state }], trouble [names of memories that are not ok or carry a warning], note }. It is another program's file: every
 *  field is checked and cut. */
export function syncStatus(j) {
  if (!j || typeof j !== 'object' || Array.isArray(j)) return null;
  const list = Array.isArray(j.machines) ? j.machines.map((x) => (x && typeof x === 'object'
      ? { name: oneLine(x.name ?? x.machine ?? x.host, 40), state: stateOf(x.ok ?? x.state ?? x.status) } : { name: oneLine(x, 40), state: null }))
    : j.machines && typeof j.machines === 'object' ? Object.entries(j.machines).map(([name, v]) => ({ name: oneLine(name, 40), state: stateOf(v && typeof v === 'object' ? (v.ok ?? v.state ?? v.status) : v) }))
    : [];
  const trouble = (Array.isArray(j.memories) ? j.memories : []).filter((x) => x && typeof x === 'object' && (x.ok === false || oneLine(x.warn, 10))).map((x) => oneLine(x.name, 40)).filter(Boolean).slice(0, 8);
  return { ok: typeof j.ok === 'boolean' ? j.ok : null, at: asMs(j.lastSync ?? j.last_sync ?? j.at), every: everyMs(j.every),
           machines: list.filter((x) => x.name).slice(0, 12), trouble, note: oneLine(j.note, 200) || null };
}

// ---------------------------------------------------------------- the documents (config)

/** The documents, in the order the page lists them. An entry has an id, a title, one line of what it is, and either `file`
 *  (a markdown file shown as it is; `optional`: no entry when it isn't there) or `build(c)` returning
 *  { path, at, markdown, status? } (or null for no entry). `c` is { home, p(path under home), fleet }. */
export const DOCS = [
  { id: 'scripts', title: 'Start scripts', about: 'The shell functions that start a session: model, effort, permissions, folder.',
    build: (c) => {
      const f = readFile(c.p('.zshrc'));
      if (!f) return missing('~/.zshrc');
      return { path: '~/.zshrc', at: f.at, markdown: startScriptsMarkdown(zshFunctions(f.text)) };
    } },
  { id: 'defaults', title: 'Session defaults', about: 'What each Claude Code profile starts with, on every machine.',
    build: async (c) => {
      const d = defaultsMarkdown(await c.fleet.profileDefaults(), c.fleet.config().logins ?? {});
      return { path: '~/.claude*/settings.json on each machine', at: d.at, markdown: d.markdown };
    } },
  { id: 'startup', title: 'Start-up routine', about: 'The hooks that run as a session starts, calls a tool and stops.',
    build: (c) => {
      const file = c.p(REPO, '.claude', 'settings.json');
      const f = readFile(file);
      if (!f) return missing(tilde(file, c.home));
      let s = null;
      try { s = JSON.parse(f.text); } catch { return { path: tilde(file, c.home), at: f.at, markdown: '_The settings file could not be read as JSON._\n' }; }
      return { path: tilde(file, c.home), at: f.at, markdown: hooksMarkdown(s) };
    } },
  { id: 'iron', title: 'Iron rules', about: 'The project\'s CLAUDE.md: loaded into every session in chai-expense-tracker.', file: [REPO, 'CLAUDE.md'] },
  { id: 'codex', title: 'Codex rules', about: 'AGENTS.md: what Codex reads first in chai-expense-tracker.', file: [REPO, 'AGENTS.md'], optional: true },
  { id: 'memory', title: 'Memory index', about: 'MEMORY.md: one line per note; a link opens the note here.',
    build: (c) => {
      const file = path.join(memoryDir(c.home), 'MEMORY.md');
      const f = readFile(file);
      if (!f) return missing(tilde(file, c.home));
      return { path: tilde(file, c.home), at: f.at, markdown: f.text, notes: true, status: memoryStatus(c.p('.memory-sync', 'status.json')) };
    } },
  { id: 'handoff', title: 'Latest handoff', about: 'The newest HANDOFF_*.md of the project: where the last session left off.',
    build: (c) => {
      const dir = c.p(REPO, 'docs', 'plans', 'active');
      let best = null;
      try {
        for (const n of fs.readdirSync(dir)) {
          if (!/^HANDOFF_.*\.md$/.test(n)) continue;
          const at = fs.statSync(path.join(dir, n)).mtimeMs;
          if (!best || at > best.at) best = { n, at };
        }
      } catch {}
      if (!best) return missing(tilde(path.join(dir, 'HANDOFF_*.md'), c.home));
      const file = path.join(dir, best.n), f = readFile(file);
      return f ? { path: tilde(file, c.home), at: f.at, markdown: f.text } : missing(tilde(file, c.home));
    } },
  { id: 'chaifm', title: 'chai-fm rules', about: 'The CLAUDE.md of chai-fm, the other repo sessions work in.', file: [FM, 'CLAUDE.md'] },
];

const missing = (p) => ({ path: p, at: null, missing: true, markdown: `_Not found:_ ${code(p)}\n` });

/** ~/.memory-sync/status.json as syncStatus() says it: null when there is no such file; { unreadable: true } when it isn't JSON. */
function memoryStatus(file) {
  const f = readFile(file, 64 * 1024);
  if (!f) return null;
  try { return syncStatus(JSON.parse(f.text)) ?? { unreadable: true }; } catch { return { unreadable: true }; }
}

/** The Context page's reads. `fleet` gives the profile defaults of every machine and fleet.json's logins. */
export function createContext({ home = os.homedir(), fleet }) {
  const c = { home, fleet, p: (...parts) => path.join(home, ...parts) };

  async function one(d) {
    let r;
    try {
      if (d.file) {
        const file = c.p(...d.file), f = readFile(file);
        if (!f && d.optional) return null;
        r = f ? { path: tilde(file, home), at: f.at, markdown: f.text } : missing(tilde(file, home));
      } else r = await d.build(c);
    } catch (e) { r = { path: null, at: null, error: true, markdown: `_Could not be read:_ ${code(oneLine(e?.message ?? e, 200))}\n` }; }
    return r && { id: d.id, title: d.title, about: d.about, ...r };
  }

  /** One note of the memory folder, by name: a plain `name.md` that is a file in that folder itself (a link out of it is refused). */
  function note(name) {
    const n = noteName(String(name ?? ''));
    if (!n) return { ok: false, error: 'That is not a note of the memory folder.' };
    const dir = memoryDir(home), file = path.join(dir, n);
    try {
      if (path.dirname(fs.realpathSync(file)) !== fs.realpathSync(dir)) return { ok: false, error: 'That is not a note of the memory folder.' };
    } catch { return { ok: false, error: `There is no note called ${n}.` }; }
    const f = readFile(file);
    if (!f) return { ok: false, error: `There is no note called ${n}.` };
    const { meta, body } = frontmatter(f.text);
    return { ok: true, note: { name: n, title: meta.name || n.replace(/\.md$/, ''), about: meta.description ?? '', path: tilde(file, home), at: f.at, markdown: body } };
  }

  return {
    /** `{ note }` → that note; else every document. */
    async read(body) {
      if (body?.note != null) return note(body.note);
      return { ok: true, at: Date.now(), documents: (await Promise.all(DOCS.map(one))).filter(Boolean) };
    },
    note,
  };
}
