// Context, the page that shows the documents shaping how a session starts: the markdown that is made safe to show, the shell
// functions found in ~/.zshrc, the settings read out of a profile (and nothing else of it), the hooks in words, the memory
// folder's notes, and the page and its API on a real server. Run with `npm test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { esc, noteName, linkOf, renderMarkdown, renderSafe } from '../public/context-render.mjs';
import { zshFunctions, startScriptsMarkdown, defaultsMarkdown, hooksMarkdown, hookSays, frontmatter, syncStatus, createContext, code, fenceFor, DOCS } from '../fleet/context.mjs';
import { settingsSummary } from '../fleet/machine.mjs';

// The vendored UMD build, as the page loads it (a global there; here the module it exports).
const marked = (() => {
  const module = { exports: {} };
  new Function('module', 'exports', fs.readFileSync(new URL('../public/vendor/marked.umd.js', import.meta.url), 'utf8'))(module, module.exports);
  return module.exports;
})();

// ---------------------------------------------------------------- showing markdown safely

test('esc turns the five characters that matter into entities', () => {
  assert.equal(esc(`<a href="x" onclick='y'>&</a>`), '&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;&lt;/a&gt;');
  assert.equal(esc(null), '');
  assert.equal(esc(5), '5');
});

/** Every tag in a piece of html, with its attribute names and values. */
function tagsOf(html) {
  return [...html.matchAll(/<(\/?)([a-zA-Z][\w-]*)([^>]*)>/g)].map(([, close, name, rest]) => ({
    close: Boolean(close), name: name.toLowerCase(),
    attrs: [...rest.matchAll(/\s([\w:-]+)(?:=("[^"]*"|'[^']*'|[^\s>]+))?/g)].map(([, k, v]) => [k.toLowerCase(), (v ?? '').replace(/^["']|["']$/g, '')]),
  }));
}
const TAGS = new Set(['p', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'ul', 'ol', 'li', 'blockquote', 'pre', 'code', 'em', 'strong', 'del', 'br', 'hr', 'a', 'span',
  'table', 'thead', 'tbody', 'tr', 'th', 'td', 'input']);
const ATTRS = { a: ['href', 'target', 'rel', 'title', 'data-note'], code: ['class'], pre: ['class'], span: ['class'], input: ['type', 'checked', 'disabled'], th: ['align'], td: ['align'], ol: ['start'] };
/** The html is something the page may put in itself: only these tags, only these attributes, links only to http(s) or this page. */
function assertInert(html) {
  for (const t of tagsOf(html)) {
    assert.ok(TAGS.has(t.name), `a <${t.name}> came out: ${html}`);
    for (const [k, v] of t.attrs) {
      assert.ok((ATTRS[t.name] ?? []).includes(k), `<${t.name} ${k}> came out: ${html}`);
      assert.ok(!k.startsWith('on'), 'an event handler came out');
      if (k === 'href') assert.match(v, /^(https?:\/\/|#[\w/.%-]+$)/, `an unsafe href came out: ${v}`);
    }
  }
}

const EVIL = [
  '# A note', '',
  '<script>window.__pwned = 1</script>', '',
  '<img src=x onerror="window.__pwned = 2">', '',
  'Inline <img src=x onerror=window.__pwned=3> and <b onmouseover="window.__pwned=4">bold</b> and <iframe src="javascript:alert(1)"></iframe> in a line.', '',
  '[click me](javascript:window.__pwned=5)', '',
  '[mixed case](JaVaScRiPt:alert(1)) and [entity](java&#115;cript:alert(1)) and [tab](java&#9;script:alert(1))', '',
  '[data](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==) and [vb](vbscript:msgbox(1)) and [file](file:///etc/passwd)', '',
  '<javascript:alert(1)> and <https://ok.example/a>', '',
  '[ref link][r]', '', '[r]: javascript:alert(1)', '',
  '![pixel](https://evil.example/p.png "t") and ![](javascript:alert(1))', '',
  '[../up](../../etc/passwd) and [abs](/etc/passwd) and [rel](docs/generated/doctypes.md) and [proto-relative](//evil.example/x)', '',
  '```html\n<script>alert(1)</script>\n```', '',
  '`<img src=x onerror=alert(1)>`', '',
  '| h <script> |\n| --- |\n| <img src=x onerror=alert(1)> |', '',
].join('\n');

test('a script, an <img onerror> and a javascript: link in a document come out inert', () => {
  for (const notes of ['', 'memory']) {
    const html = renderMarkdown(marked, EVIL, { notes });
    assertInert(html);
    assert.ok(!/<script/i.test(html), 'a <script> came out');
    assert.ok(!/<img/i.test(html), 'an <img> came out');
    assert.ok(!/<iframe/i.test(html), 'an <iframe> came out');
    assert.ok(!/href="javascript/i.test(html), 'a javascript: href came out');
    assert.ok(!/\bon\w+=/i.test(tagsOf(html).map((t) => t.attrs.map(([k]) => k).join(' ')).join(' ')), 'an on… attribute came out');
    // what was written is still readable, as text
    assert.ok(html.includes('&lt;script&gt;window.__pwned = 1&lt;/script&gt;'), 'the script is shown as text');
    assert.ok(html.includes('&lt;img src=x onerror='), 'the img is shown as text');
    assert.ok(html.includes('click me'), 'the link text stays');
    // the only links: the ones to the web
    const links = tagsOf(html).filter((t) => t.name === 'a' && !t.close).map((t) => t.attrs.find(([k]) => k === 'href')[1]);
    assert.deepEqual(links, ['https://ok.example/a']);
  }
});

test('raw html is escaped in a block and inline, whatever it is', () => {
  const block = renderMarkdown(marked, '<div onclick="x()">hi</div>');
  assert.equal(block, '<pre class="raw"><code>&lt;div onclick=&quot;x()&quot;&gt;hi&lt;/div&gt;</code></pre>\n');
  const inline = renderMarkdown(marked, 'a <span style="color:red">b</span> c');
  assert.ok(inline.includes('&lt;span style=&quot;color:red&quot;&gt;b&lt;/span&gt;'), inline);
  assertInert(block + inline);
});

test('links: http(s) open in a new tab, only the memory index and notes may link to a note, nothing else is a link', () => {
  const web = renderMarkdown(marked, '[a](https://example.com/x?y=1&z="2") [b](http://example.com) [c](HTTPS://EXAMPLE.com/Q)');
  assert.equal(tagsOf(web).filter((t) => t.name === 'a' && !t.close).length, 3);
  assert.ok(web.includes('target="_blank" rel="noopener noreferrer"'));
  assert.ok(!web.includes('z="2"'), 'a quote in a url is escaped');
  assertInert(web);
  // a note link: only where the document has notes
  assert.equal(renderMarkdown(marked, '[n](project_x.md)', { notes: '' }), '<p>n</p>\n');
  assert.equal(renderMarkdown(marked, '[n](project_x.md)', { notes: 'memory' }), '<p><a href="#memory/project_x.md" data-note="project_x.md">n</a></p>\n');
  // never a path
  for (const bad of ['../x.md', '/etc/passwd.md', 'a/b.md', '..%2fx.md', '%2e%2e/x.md', 'x.md/', 'x.txt', '.hidden.md', 'a..b.md', 'x.md#frag', 'http://x.md']) {
    const html = renderMarkdown(marked, `[n](${bad})`, { notes: 'memory' });
    assert.ok(!html.includes('data-note'), `${bad} became a note link: ${html}`);
  }
  // an image is its words, and is never loaded
  assert.equal(renderMarkdown(marked, '![logo](https://evil.example/p.png)'), '<p><span class="noimg">[image: logo]</span></p>\n');
});

test('noteName and linkOf', () => {
  assert.equal(noteName('project_fleet_panel_2026_10_24.md'), 'project_fleet_panel_2026_10_24.md');
  assert.equal(noteName('MEMORY.md'), 'MEMORY.md');
  assert.equal(noteName('a-b.c.md'), 'a-b.c.md');
  for (const bad of ['', '..', '../a.md', 'a/../b.md', '/a.md', 'a/b.md', 'a\\b.md', '.a.md', 'a.md ', 'a.txt', 'a.md\0', null, 5, 'x'.repeat(200) + '.md']) assert.equal(noteName(bad), null, String(bad));
  assert.deepEqual(linkOf('https://a.example/b', ''), { web: 'https://a.example/b' });
  assert.deepEqual(linkOf('x.md', 'memory'), { note: 'x.md' });
  assert.equal(linkOf('x.md', ''), null);
  for (const bad of ['javascript:alert(1)', 'data:text/html,x', 'mailto:a@b.c', '//a.example', '/a', 'a', '', undefined]) assert.equal(linkOf(bad, 'memory'), null, String(bad));
});

test('renderSafe falls back to the text itself if marked throws', () => {
  const broken = { Marked: class { constructor() { throw new Error('no'); } } };
  assert.equal(renderSafe(broken, '<b>x</b>'), '<pre class="raw"><code>&lt;b&gt;x&lt;/b&gt;</code></pre>');
});

test('code spans and fences hold whatever they are given', () => {
  assert.equal(code('a|b'), '`a|b`');
  assert.equal(code('a`b'), '``a`b``');
  assert.equal(code('`a`'), '`` `a` ``');
  assert.equal(code(null), '—');
  assert.equal(fenceFor('x'), '```');
  assert.equal(fenceFor('a ``` b'), '````');
});

// ---------------------------------------------------------------- 1. the shell functions

const ZSHRC = `# Start SSH agent
export PATH="/opt/homebrew/bin:$PATH"

# not about the function below (a blank line between)

# Personal assistant — no permission prompts
claude-assist() {
  (cd ~/Dropbox/Assistant && command claude --dangerously-skip-permissions "$@")
}

# Max: braces } in { quotes, \${PATH}, \${#arr[@]} and a comment with a } in it
claude-max() {
  local m='{'   # an open brace in a comment {
  echo "close } here" "\${#PATH}" '}' \\}
  if [ -n "$m" ]; then
    (cd ~/projects/x && command claude --model "m[1m]" "$@")
  fi
}

# the twin
function gpt {
  command codex -c 'model_reasoning_effort="ultra"' "$@"
}

ll() { ls -l; }
claudex()
{
  echo next-line brace
}
codex() { command codex "$@"; }
mygpt() { echo no; }
# trailing comment, no function
`;

test('zshFunctions finds every top-level function with the comment directly above it, to its real end', () => {
  const fns = zshFunctions(ZSHRC);
  assert.deepEqual(fns.map((f) => f.name), ['claude-assist', 'claude-max', 'gpt', 'll', 'claudex', 'codex', 'mygpt']);
  assert.deepEqual(fns[0].comment, ['Personal assistant — no permission prompts']);
  assert.deepEqual(fns[1].comment, ['Max: braces } in { quotes, ${PATH}, ${#arr[@]} and a comment with a } in it']);
  assert.ok(fns[1].body.startsWith('claude-max() {') && fns[1].body.endsWith('\n}'), fns[1].body);
  assert.ok(fns[1].body.includes('--model "m[1m]"') && !fns[1].body.includes('function gpt'));
  assert.equal(fns[2].body.split('\n').at(-1), '}');
  assert.deepEqual(fns[2].comment, ['the twin']);
  assert.deepEqual(fns[3].comment, []);
  assert.equal(fns[4].body, 'claudex()\n{\n  echo next-line brace\n}');
});

test('startScriptsMarkdown: claude…, gpt and codex only, a heading each, the comment as text, the function in a zsh block', () => {
  const md = startScriptsMarkdown(zshFunctions(ZSHRC));
  assert.deepEqual([...md.matchAll(/^## (.+)$/gm)].map((m) => m[1]), ['claude-assist', 'claude-max', 'gpt', 'claudex', 'codex']);
  assert.ok(md.includes('## claude-assist\n\nPersonal assistant — no permission prompts\n\n```zsh\nclaude-assist() {\n'), md);
  assert.ok(!md.includes('ll()') && !md.includes('mygpt'));
  assert.equal(startScriptsMarkdown([]), '_No start functions (claude…, gpt, codex) in this file._\n');
  // a function that holds a fence of its own can't end the block early
  const tricky = startScriptsMarkdown(zshFunctions('claude-x() {\n  echo "```"\n}\n'));
  assert.ok(tricky.includes('````zsh\n') && tricky.endsWith('\n````\n'), tricky);
});

test('zshFunctions on the real kind of file: an unclosed function runs to the end, an empty file has none', () => {
  assert.equal(zshFunctions('').length, 0);
  const fns = zshFunctions('claude-open() {\n  echo "never closed"\n');
  assert.equal(fns.length, 1);
  assert.ok(fns[0].body.includes('never closed'));
});

// ---------------------------------------------------------------- 2. profile defaults

test('settingsSummary reads those keys and nothing else', () => {
  const s = settingsSummary({
    model: 'claude-fable-5-1[1m]', effortLevel: 'low', tui: 'fullscreen', permissions: { defaultMode: 'bypassPermissions', allow: ['Bash(rm:*)'] },
    modelSettings: { 'claude-fable-5-1': { effortLevel: 'xhigh', autoCompactWindow: 400000 }, 'claude-opus-5': { effortLevel: 'max' } },
    env: { ANTHROPIC_API_KEY: 'sk-ant-SECRET', GITHUB_TOKEN: 'ghp_SECRET' }, hooks: { Stop: [{ hooks: [{ command: 'curl https://x/?t=SECRET' }] }] },
    apiKeyHelper: '/bin/get-secret', statusLine: { command: 'SECRET' },
  }, { oauthAccount: { emailAddress: 'me@example.com', organizationName: 'SECRET Org' }, primaryApiKey: 'sk-SECRET', userID: 'u-SECRET' });
  assert.deepEqual(s, { email: 'me@example.com', model: 'claude-fable-5-1[1m]', modelEffort: 'xhigh', effort: 'low', defaultMode: 'bypassPermissions', tui: 'fullscreen' });
  assert.ok(!JSON.stringify(s).includes('SECRET'));
});

test('settingsSummary: absent keys are null, a value that is not a plain word is dropped, a boolean tui is its word', () => {
  assert.deepEqual(settingsSummary({}, null), { email: null, model: null, modelEffort: null, effort: null, defaultMode: null, tui: null });
  assert.deepEqual(settingsSummary(null, undefined), { email: null, model: null, modelEffort: null, effort: null, defaultMode: null, tui: null });
  const odd = settingsSummary({ model: { a: 1 }, effortLevel: 'x\ny', tui: false, permissions: 'bypassPermissions', modelSettings: 'x' }, { oauthAccount: { emailAddress: 'not an email' } });
  assert.deepEqual(odd, { email: null, model: null, modelEffort: null, effort: null, defaultMode: null, tui: 'false' });
  const long = settingsSummary({ model: 'm'.repeat(150) }, null);
  assert.equal(long.model.length, 80);
});

test('defaultsMarkdown: one table across machines; one that did not answer is named, the rest still show', () => {
  const mac = { ok: true, profiles: [{ id: 'default', email: 'a@x.com', model: 'm', modelEffort: 'xhigh', effort: 'low', defaultMode: 'bypassPermissions', tui: 'fullscreen', at: 5 }, { id: 'aol', email: null, model: null, modelEffort: null, effort: null, defaultMode: null, tui: null, at: 9 }] };
  const d = defaultsMarkdown([{ name: 'Mac', r: mac }, { name: 'redbat', r: { ok: false, error: 'redkat: Connection `refused`\nsecond' } }], { 'Mac:default': 'real@x.com' });
  const rows = d.markdown.split('\n').filter((l) => l.startsWith('| `'));
  assert.equal(rows.length, 2);
  assert.ok(rows[0].includes('`Mac` | `default` | `real@x.com`'), 'the fleet.json login stands in for the file\'s');
  assert.ok(rows[1].includes('| `aol` | — | — |'), rows[1]);
  assert.ok(d.markdown.includes('| machine | profile | account | model | fable-5-1 effort | effort | mode | tui |'));
  assert.match(d.markdown, /\*\*`redbat` was unreachable\*\*: ``redkat: Connection `refused` second``\./);
  assert.equal(d.at, 9);
  assert.match(defaultsMarkdown([{ name: 'Mac', r: { ok: false, error: 'x' } }]).markdown, /No Claude Code profile was read/);
});

// ---------------------------------------------------------------- 3. the hooks

test('hooksMarkdown: a section per event in the order a session meets them, matcher, command in a block, one sentence', () => {
  const md = hooksMarkdown({ hooks: {
    Stop: [{ hooks: [{ type: 'command', command: '[ -f .claude/FANOUT ] && exit 0; npm run -s docs:check' }] }],
    PreToolUse: [{ matcher: 'Bash', hooks: [{ type: 'command', command: '"$CLAUDE_PROJECT_DIR/.claude/hooks/guard-destructive-bash.sh"' }] }],
    SessionStart: [{ hooks: [{ type: 'command', command: 'echo "[resume] hi"' }] }],
    Custom: [{ matcher: '*', hooks: [{ type: 'http', url: 'http://127.0.0.1:4747/hook/event' }] }],
  } });
  assert.deepEqual([...md.matchAll(/^## (.+)$/gm)].map((m) => m[1]), ['SessionStart', 'PreToolUse', 'Stop', 'Custom']);
  assert.ok(md.includes('## PreToolUse\n\n**Matcher:** `Bash`\n\n```sh\n"$CLAUDE_PROJECT_DIR/.claude/hooks/guard-destructive-bash.sh"\n```\n\nStops a short list of irreversible shell commands'), md);
  assert.ok(md.includes('## SessionStart\n\n**Matcher:** none (every time)\n\n```sh\necho "[resume] hi"\n```\n\nPrints a reminder into the new session'));
  assert.ok(md.includes('**type:** `http`') && md.includes('http://127.0.0.1:4747/hook/event'));
  assert.equal(hooksMarkdown({}), '_This project has no hooks._\n');
  assert.equal(hooksMarkdown(null), '_This project has no hooks._\n');
  assert.equal(hooksMarkdown({ hooks: { Stop: 'x' } }).includes('Nothing set'), true);
});

test('hookSays: a hook it knows in its own words, any other from its event', () => {
  assert.match(hookSays('Stop', '', 'npm run docs:check'), /docs check/);
  assert.equal(hookSays('PreToolUse', 'Edit', 'x.sh'), 'Runs before a tool call, and can block it, for Edit only.');
  assert.equal(hookSays('Mystery', '', 'x'), 'Runs on Mystery.');
  assert.equal(hookSays('SessionEnd', '*', 'x'), 'Runs when a session ends.');
});

// ---------------------------------------------------------------- 6. memory

test('frontmatter: name and description, then the body', () => {
  const f = frontmatter('---\nname: project_x\ndescription: "Ross\'s thing — \\"quoted\\""\nmetadata:\n  type: project\n---\n\nBody here.\n');
  assert.deepEqual(f.meta, { name: 'project_x', description: 'Ross\'s thing — "quoted"' });
  assert.equal(f.body, '\nBody here.\n');
  assert.deepEqual(frontmatter('No front matter\n---\nstill none'), { meta: {}, body: 'No front matter\n---\nstill none' });
  assert.deepEqual(frontmatter('---\nunclosed\n').meta, {});
});

test('syncStatus: another program\'s file, checked and cut', () => {
  const s = syncStatus({ lastSync: '2026-10-26T10:42:46Z', host: 'Mac', ok: true, machines: ['Mac', 'redbat'], every: '5 min', note: 'shared',
    memories: [{ name: 'a', ok: true, warn: '' }, { name: 'b', ok: false }, { name: 'c', ok: true, warn: 'index long' }] });
  assert.deepEqual(s, { ok: true, at: Date.parse('2026-10-26T10:42:46Z'), every: 300_000, machines: [{ name: 'Mac', state: null }, { name: 'redbat', state: null }], trouble: ['b', 'c'], note: 'shared' });
  assert.equal(syncStatus({ lastSync: 1_793_011_394 }).at, 1_793_011_394_000);
  assert.equal(syncStatus({ lastSync: 1_793_011_394_000 }).at, 1_793_011_394_000);
  assert.deepEqual(syncStatus({ ok: 'yes', machines: { Mac: true, redbat: { ok: false }, x: 'late' }, note: 'a\nb'.repeat(200) }).machines.map((m) => `${m.name}:${m.state}`), ['Mac:ok', 'redbat:failed', 'x:late']);
  assert.equal(syncStatus({ ok: 'yes' }).ok, null);
  assert.equal(syncStatus({ note: 'x'.repeat(500) }).note.length, 200);
  assert.equal(syncStatus({ every: 'whenever' }).every, null);
  assert.equal(syncStatus({ every: '2 h' }).every, 7_200_000);
  for (const bad of [null, 5, 'x', [], undefined]) assert.equal(syncStatus(bad), null);
});

// ---------------------------------------------------------------- the documents on disk

/** A home with every document in it. */
function world() {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-context-'));
  const w = (rel, text, at) => { const f = path.join(home, rel); fs.mkdirSync(path.dirname(f), { recursive: true }); fs.writeFileSync(f, text); if (at) fs.utimesSync(f, at / 1000, at / 1000); return f; };
  const mem = path.join('.claude', 'projects', path.join(home, 'projects', 'chai-expense-tracker').replace(/[^A-Za-z0-9]/g, '-'), 'memory');
  w('.zshrc', ZSHRC, 1_700_000_000_000);
  w('projects/chai-expense-tracker/CLAUDE.md', '# Iron rules\n\nBe good.\n', 1_700_000_001_000);
  w('projects/chai-expense-tracker/AGENTS.md', '# Codex\n', 1_700_000_002_000);
  w('projects/chai-expense-tracker/.claude/settings.json', JSON.stringify({ hooks: { Stop: [{ hooks: [{ type: 'command', command: 'npm run docs:check' }] }] }, env: { TOKEN: 'sk-SECRET' } }), 1_700_000_003_000);
  w('projects/chai-expense-tracker/docs/plans/active/HANDOFF_OLD.md', '# old', 1_700_000_000_000);
  w('projects/chai-expense-tracker/docs/plans/active/HANDOFF_NEWEST.md', '# newest', 1_700_000_009_000);
  w('projects/chai-expense-tracker/docs/plans/active/HANDOFF_MID.md', '# mid', 1_700_000_005_000);
  w('projects/chai-expense-tracker/docs/plans/active/NOT_A_HANDOFF.md', '# not', 1_700_000_099_000);
  w('projects/chai-fm/CLAUDE.md', '# chai-fm rules\n', 1_700_000_004_000);
  w(path.join(mem, 'MEMORY.md'), '# Memory\n\n- [A note](project_a.md) — one line\n- [Out](../../../../etc/passwd)\n', 1_700_000_006_000);
  w(path.join(mem, 'project_a.md'), '---\nname: project_a\ndescription: The A note\n---\n\nBody of A. [back](MEMORY.md)\n', 1_700_000_007_000);
  fs.mkdirSync(path.join(home, 'secret'));
  w('secret/outside.md', 'OUTSIDE', 1);
  fs.symlinkSync(path.join(home, 'secret', 'outside.md'), path.join(home, mem, 'linked.md'));
  fs.mkdirSync(path.join(home, mem, 'adir.md'));
  return { home, mem: path.join(home, mem), w };
}
const FLEET = { profileDefaults: async () => [{ name: 'Mac', r: { ok: true, profiles: [{ id: 'default', email: 'a@x.com', model: 'm', at: 1_700_000_010_000 }] } }, { name: 'redbat', r: { ok: false, error: 'redkat: no route' } }], config: () => ({ logins: {} }) };

test('the documents come in the config order, each with id, title, what it is, where from, when, and its markdown', async () => {
  const { home } = world();
  const r = await createContext({ home, fleet: FLEET }).read({});
  assert.equal(r.ok, true);
  assert.deepEqual(r.documents.map((d) => d.id), ['scripts', 'defaults', 'startup', 'iron', 'codex', 'memory', 'handoff', 'chaifm']);
  assert.deepEqual(DOCS.map((d) => d.id), r.documents.map((d) => d.id));
  for (const d of r.documents) {
    assert.ok(d.title && d.about && d.path && typeof d.markdown === 'string' && d.markdown.length, d.id);
    assert.ok(Number.isFinite(d.at), `${d.id} has no modified time`);
  }
  const by = Object.fromEntries(r.documents.map((d) => [d.id, d]));
  assert.equal(by.scripts.path, '~/.zshrc');
  assert.equal(by.scripts.at, 1_700_000_000_000);
  assert.ok(by.scripts.markdown.includes('## claude-max'));
  assert.ok(by.defaults.markdown.includes('`redbat` was unreachable'));
  assert.equal(by.defaults.at, 1_700_000_010_000);
  assert.ok(by.startup.markdown.startsWith('## Stop\n'));
  assert.ok(!JSON.stringify(by.startup).includes('SECRET'), 'the settings file is read for its hooks only');
  assert.equal(by.iron.markdown, '# Iron rules\n\nBe good.\n');
  assert.equal(by.iron.path, '~/projects/chai-expense-tracker/CLAUDE.md');
  assert.equal(by.memory.notes, true);
  assert.equal(by.memory.status, null);
  assert.equal(by.handoff.path, '~/projects/chai-expense-tracker/docs/plans/active/HANDOFF_NEWEST.md', 'the newest by modified time, not by name');
  assert.equal(by.handoff.markdown, '# newest');
  assert.equal(by.chaifm.markdown, '# chai-fm rules\n');
  assert.ok(Math.abs(r.at - Date.now()) < 5000);
});

test('no AGENTS.md: no Codex entry; any other missing document says so; a status file is shown beside the index', async () => {
  const { home, w } = world();
  fs.rmSync(path.join(home, 'projects/chai-expense-tracker/AGENTS.md'));
  fs.rmSync(path.join(home, 'projects/chai-fm/CLAUDE.md'));
  w('.memory-sync/status.json', JSON.stringify({ lastSync: '2026-10-26T10:42:46Z', ok: false, machines: ['Mac'], note: 'redbat unreachable' }));
  const r = await createContext({ home, fleet: FLEET }).read({});
  assert.deepEqual(r.documents.map((d) => d.id), ['scripts', 'defaults', 'startup', 'iron', 'memory', 'handoff', 'chaifm']);
  const fm = r.documents.find((d) => d.id === 'chaifm');
  assert.equal(fm.missing, true);
  assert.match(fm.markdown, /Not found/);
  const mem = r.documents.find((d) => d.id === 'memory');
  assert.equal(mem.status.ok, false);
  assert.equal(mem.status.note, 'redbat unreachable');
  w('.memory-sync/status.json', 'not json{');
  assert.deepEqual((await createContext({ home, fleet: FLEET }).read({})).documents.find((d) => d.id === 'memory').status, { unreadable: true });
});

test('a document that cannot be built is its own message, not an error for the rest', async () => {
  const { home } = world();
  const r = await createContext({ home, fleet: { profileDefaults: async () => { throw new Error('boom'); }, config: () => ({}) } }).read({});
  const d = r.documents.find((x) => x.id === 'defaults');
  assert.equal(d.error, true);
  assert.match(d.markdown, /boom/);
  assert.equal(r.documents.length, 8);
});

test('a note: only a plain name.md that is a file of the memory folder itself', async () => {
  const { home } = world();
  const c = createContext({ home, fleet: FLEET });
  const ok = await c.read({ note: 'project_a.md' });
  assert.equal(ok.ok, true);
  assert.deepEqual({ name: ok.note.name, title: ok.note.title, about: ok.note.about }, { name: 'project_a.md', title: 'project_a', about: 'The A note' });
  assert.equal(ok.note.markdown, '\nBody of A. [back](MEMORY.md)\n');
  assert.ok(ok.note.path.endsWith('/memory/project_a.md') && ok.note.path.startsWith('~/.claude/projects/'));
  assert.equal((await c.read({ note: 'MEMORY.md' })).ok, true);
  for (const bad of ['../secret/outside.md', '../../x.md', '/etc/passwd', path.join(home, 'secret', 'outside.md'), 'a/b.md', '..', '', 'x.txt', '.hidden.md', 'a..md', 'linked.md', 'adir.md', 5, {}]) {
    const r = await c.read({ note: bad });
    assert.equal(r.ok, false, `${JSON.stringify(bad)} was read`);
    assert.ok(!JSON.stringify(r).includes('OUTSIDE'));
  }
  assert.match((await c.read({ note: 'nope.md' })).error, /no note called nope\.md/);
});

// ---------------------------------------------------------------- the page and its API, on a real server

const SERVER = fileURLToPath(new URL('../server.mjs', import.meta.url));
const freePort = () => new Promise((r) => { const s = net.createServer().listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => r(port)); }); });

test('/context is a page for a paired browser only, held by its own policy; /api/context takes the token and reads the documents', async (t) => {
  const { home } = world();
  // the Mac's own profile: its settings carry a secret that must never leave
  fs.mkdirSync(path.join(home, '.claude'), { recursive: true });
  fs.writeFileSync(path.join(home, '.claude', 'settings.json'), JSON.stringify({ model: 'claude-fable-5-1[1m]', env: { KEY: 'sk-LEAK' }, permissions: { defaultMode: 'plan' } }));
  fs.writeFileSync(path.join(home, '.claude.json'), JSON.stringify({ oauthAccount: { emailAddress: 'me@example.com' }, primaryApiKey: 'sk-LEAK' }));
  const panel = path.join(home, 'panel');
  fs.mkdirSync(panel);
  fs.writeFileSync(path.join(panel, 'prefs.json'), JSON.stringify({ profile: 'default' }));
  const bin = path.join(home, 'bin');
  fs.mkdirSync(bin);
  fs.writeFileSync(path.join(bin, 'claude'), '#!/bin/sh\ncase "$1" in agents) echo "[]";; auth) echo "{}";; *) exit 1;; esac\n', { mode: 0o755 });
  const port = await freePort(), base = `http://127.0.0.1:${port}`;
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE')));
  const child = spawn(process.execPath, [SERVER], { env: { ...env, HOME: home, PATH: `${bin}:${env.PATH}`, ACP_PORT: String(port), ACP_PREFS: path.join(panel, 'prefs.json') }, stdio: ['ignore', 'pipe', 'pipe'] });
  let log = '';
  child.stdout.on('data', (d) => { log += d; }); child.stderr.on('data', (d) => { log += d; });
  t.after(() => new Promise((r) => { if (child.exitCode !== null) return r(); child.once('exit', r); child.kill(); }));
  const up = Date.now() + 10_000;
  while (!log.includes(base)) { if (Date.now() > up || child.exitCode !== null) throw new Error('server did not start: ' + log); await new Promise((r) => setTimeout(r, 50)); }
  const paired = { cookie: `acp_k=${fs.readFileSync(path.join(panel, 'key'), 'utf8').trim()}` };

  // an unpaired browser gets the pairing page, not the token
  assert.match(await (await fetch(base + '/context')).text(), /not paired/);
  const res = await fetch(base + '/context', { headers: paired });
  const html = await res.text();
  assert.equal(res.status, 200);
  const token = /const TOKEN = '([0-9a-f]{48})'/.exec(html)?.[1];
  assert.ok(token, 'the page carries the token');
  const csp = res.headers.get('content-security-policy');
  const nonce = /nonce-([A-Za-z0-9+/=]+)/.exec(csp)?.[1];
  assert.ok(nonce && html.includes(`<script type="module" nonce="${nonce}">`), 'one nonce, in the policy and on the page\'s script');
  assert.ok(!html.includes('__ACP_'));
  assert.match(csp, /default-src 'none'/);
  assert.match(csp, /script-src 'self' 'nonce-/);
  assert.ok(!/script-src[^;]*unsafe/.test(csp), 'no inline script but the page\'s own');
  assert.match(csp, /frame-ancestors 'none'/);
  assert.equal(res.headers.get('x-frame-options'), 'DENY');
  assert.notEqual(nonce, /nonce-([A-Za-z0-9+/=]+)/.exec((await fetch(base + '/context', { headers: paired })).headers.get('content-security-policy'))[1], 'a new nonce each time');

  // what the page loads
  const lib = await fetch(base + '/vendor/marked.umd.js');
  assert.equal(lib.status, 200);
  assert.match(lib.headers.get('content-type'), /javascript/);
  assert.match(await lib.text(), /marked v18/);
  assert.match(await (await fetch(base + '/context-render.mjs')).text(), /export function renderMarkdown/);

  // the API: the token, this origin, the cookie
  const post = (body, h = {}) => fetch(base + '/api/context', { method: 'POST', headers: { 'content-type': 'application/json', origin: base, 'x-acp-token': token, ...paired, ...h }, body: JSON.stringify(body) });
  assert.equal((await post({}, { 'x-acp-token': 'nope' })).status, 403);
  assert.equal((await post({}, { origin: 'http://evil.example' })).status, 403);
  assert.equal((await post({}, { cookie: '' })).status, 403);
  const r = await (await post({})).json();
  assert.equal(r.ok, true);
  assert.deepEqual(r.documents.map((d) => d.id), ['scripts', 'defaults', 'startup', 'iron', 'codex', 'memory', 'handoff', 'chaifm']);
  const defaults = r.documents.find((d) => d.id === 'defaults');
  assert.ok(defaults.markdown.includes('`me@example.com`') && defaults.markdown.includes('`plan`'), defaults.markdown);
  assert.ok(!JSON.stringify(r).includes('sk-LEAK'), 'a secret of a profile never reaches the page');
  assert.equal((await (await post({ note: 'project_a.md' })).json()).ok, true);
  const bad = await post({ note: '../../etc/passwd' });
  assert.equal(bad.status, 422);
  assert.equal((await bad.json()).ok, false);
});
