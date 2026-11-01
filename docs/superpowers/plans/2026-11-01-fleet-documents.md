# Fleet Documents Pane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every `.md`/`.html` a session writes shows in that session's Documents list the moment it is written and renders in a third pane; sessions can no longer publish claude.ai Artifacts unless allowed.

**Architecture:** The hook script appends one line per document write to a per-session `.docs.jsonl`; the machine worker folds that into the session row and serves file bytes through a new `doc` op (over ssh for remote machines); the page adds a Documents strip and a third column that renders markdown with the vendored `marked` or HTML in a sandboxed iframe. A second hook script denies the `Artifact` tool with an instruction as the reason.

**Tech Stack:** POSIX `sh` + `jq` hooks, Node 20+ ESM worker (`fleet/machine.mjs`), vanilla DOM, `marked` (vendored), `node:test`.

**Spec:** `docs/superpowers/specs/2026-11-01-fleet-mobile-and-documents-design.md`

## Global Constraints

- `fleet/hook.sh` prints nothing, exits 0, and stays one `jq` invocation per event (it runs on every tool call of every session).
- No GET route serves files. Bytes travel only through `POST /api/fleet/doc`, for paths the session's own docs list contains.
- Remote machines run the same `machine.mjs` shipped over ssh: every new read lives in `machine.mjs`, never in `server.mjs`.
- No new polling. Document changes reach pages through the existing `{"op":"fleet"}` push (`fleet.onHook` → `onChange`).
- Repos are public: no estate paths in committed files beyond what the README shows.
- `npm test` stays green.

## Review Focus

1. A path with `..` or a symlink out of the session's folder in `doc()` must be refused even when it is in the docs list (the hook trusts Claude Code's `file_path`) — Task 2 test with a symlink.
2. A `.docs.jsonl` with a truncated last line (a write in flight) must parse the complete lines and skip the broken one — Task 1 test.
3. An HTML document that calls `parent.document` from inside the sandbox must throw, not read the panel — Task 4 verifies with a fixture mockup containing `try{parent.document}catch(e){document.body.textContent='blocked'}`.
4. The Artifact guard must pass `action: "read"`/`"list"`/`"open"`/`"quickstart"` and deny a publish with no `action` field (publish is the default) — Task 4 tests all five.
5. A session with 500 writes to the same file shows one entry, newest time — Task 1 dedupe test.

---

### Task 1: Capture and read documents

**Files:**
- Modify: `fleet/hook.sh`, `fleet/machine.mjs` (`watchHooks` ~459, `collect` where rows are built, `readHook` neighbour)
- Test: `test/fleet-hooks.test.mjs` (hook.sh is already exercised there with `sh` + stdin JSON — follow that pattern), `test/fleet-machine.test.mjs`

**Interfaces:**
- Produces: `<profile>/fleet-state/<session>.docs.jsonl`, one line `{"p":"<abs path>","t":"Write|Edit|MultiEdit|NotebookEdit","at":<ms>}` per qualifying `PostToolUse`; file trimmed to its last 200 lines when it exceeds 64 KB (`tail -n 200 > tmp && mv`). Qualifying: main conversation (`agent_id` empty), `tool_input.file_path // tool_input.notebook_path` ending in `.md .markdown .html .htm .svg .txt .csv .json` (case-insensitive).
- Produces: `export function readDocs(p, sessionId, limit = 50)` in `machine.mjs` → `[{ path, tool, at }]` newest first, deduplicated by `path` (latest `at` wins), skipping unparsable lines. Each session row from `collect` gains `docs: readDocs(p, sessionId)`. `watchHooks` also fires `on({ profile, sessionId, hook: null, docs: true })` on `*.docs.jsonl` changes; `fleet.mjs onHook` treats `docs: true` as a state change (calls `onChange`).

- [ ] **Step 1: Failing tests.** In `test/fleet-hooks.test.mjs`: feed `hook.sh` a `PostToolUse` with `tool_name: "Write"`, `tool_input.file_path: "/tmp/x/brief.md"`, `session_id: "s1"`, `CLAUDE_CONFIG_DIR` = temp dir → `fleet-state/s1.docs.jsonl` has one line with `p`, `t: "Write"`, numeric `at`; a `Write` to `/tmp/x/code.ts` appends nothing; a `PostToolUse` with `agent_id: "a1"` appends nothing; `Edit` of `/tmp/x/Page.HTML` appends. In `test/fleet-machine.test.mjs`: `readDocs` on a fixture with three lines for `a.md` (at 1,2,3), one for `b.html`, and a trailing broken line `{"p":"c.md","t":"Wri` → `[{path:'a.md', at:3,…}, {path:'b.html',…}]` (a.md first because newest).
- [ ] **Step 2: Run** `node --test test/fleet-hooks.test.mjs test/fleet-machine.test.mjs` — FAIL.
- [ ] **Step 3: Implement** in `hook.sh`: extend the single `jq` program to emit a second tab-separated field (the docs line, or empty) and append it when non-empty; trim when `wc -c` > 65536. Implement `readDocs` and wire `collect` + `watchHooks`; in `fleet/fleet.mjs` `onHook(machine, ev)` call `onChange` when `ev.docs`.
- [ ] **Step 4: Run** PASS; `npm test` green. **Step 5: Commit** `feat(fleet): a session's documents — the hook keeps every .md/.html write, the worker lists them`.

### Task 2: The `doc` op

**Files:**
- Modify: `fleet/machine.mjs` (new export), `fleet/fleet.mjs` (`ALLOWED` at line 52; a `doc(a)` export routed via `onMachine` like `peek`), `server.mjs` `ACTIONS` (~1842): `'/api/fleet/doc': (b) => fleet.doc(b)`
- Test: `test/fleet-machine.test.mjs`

**Interfaces:**
- Produces: `export async function doc(a, deps = {})` in `machine.mjs` with `a = { profile, sessionId, path }` → `{ ok: true, kind: 'md'|'html'|'text', text, at, path }` or `{ ok: false, error }`. Rules, in order: `path` must equal an entry of `readDocs(p, sessionId)` (else `'Not one of this session\'s documents.'`); `fs.realpathSync(path)` must equal `path` resolved (no symlink, no `..`); `statSync.size <= 1048576` (else `'Larger than 1 MiB.'`); read utf8. `kind`: `.md|.markdown` → `md`; `.html|.htm` → `html`; else `text`. `deps.profiles` seam as in `resume()`.
- Produces: `fleet.doc({ machine, profile, sessionId, path })` → the worker's answer.

- [ ] **Step 1: Failing tests**: a temp profile dir with `fleet-state/s1.docs.jsonl` naming `<tmp>/brief.md` and `<tmp>/link.md` (a symlink to `/etc/hosts`); `doc()` on `brief.md` returns `kind: 'md'` and the text; on `link.md` returns `ok: false`; on `<tmp>/other.md` (exists, not in the list) `ok: false`; on a 2 MiB `big.md` in the list `ok: false` with `/1 MiB/`.
- [ ] **Step 2: Run** FAIL. **Step 3: Implement.** **Step 4: PASS**, `npm test` green. **Step 5: Commit** `feat(fleet): read one of a session's documents, here or over ssh — only paths it wrote`.

### Task 3: Documents strip and third pane (wave 2: after the phone layout merges)

**Files:**
- Modify: `public/fleet.html` (`<aside id="doc">` after `#pane` at ~314; CSS vars `--doc`, `--doc-w`; `renderHead` strip; new `openDoc`, `renderDoc`, `closeDoc`; `applyPaneUi` for the second grip), `server.mjs` fleet page route (load `/vendor/marked.umd.js`; `/context-render.mjs` is already served)

**Interfaces:**
- Consumes: `x.docs` (Task 1), `POST /api/fleet/doc` (Task 2), `renderSafe(marked, markdown, {})` from `/context-render.mjs` (ES module: `import('/context-render.mjs')` once, cached in `let render`).
- Produces: `pane.doc = { path, kind, text, at, source: false } | null`; `openDoc(x, path)`; `#doc` shown when `pane.doc` (`body.has-doc`); width persisted as `fleet.doc.width` (default 520, min 360, max 60% of window); on `{"op":"fleet"}` → if `pane.doc` and the matching docs entry has a newer `at`, refetch and re-render, keeping `#doc-body.scrollTop`.

- [ ] **Step 1: Verification harness**: extend `tools/phone-shot.mjs` (from the phone plan) with a `--doc` mode: with a fixture session (write `<tmp>/mock.html` containing the `parent.document` probe from Review Focus 3 and `<tmp>/brief.md`, and a hand-made `fleet-state/<sid>.docs.jsonl`), open the page at 1440×900, select the session, click the `mock.html` entry, assert the iframe's body text is `blocked`; click `brief.md`, assert an `<h1>` rendered; at 390×844 assert the doc opens as a full-screen sheet and the back chevron returns.
- [ ] **Step 2: Markup + CSS**: `#doc` fixed right column (`right: 0; width: var(--doc)`), `#pane` shifts left by `var(--doc)` when `body.has-doc` at ≥ 1100 px; 720–1100 px `#doc` overlays `#pane` (same box) with a `Documents`/`Session` toggle in `#p-head`; phone: `#doc` is a sheet like `#pane` with a back chevron. `#doc-head`: name · age · `Source` toggle · close. `#doc-body`: rendered content; `iframe.mock { width: 100%; height: 100%; border: 0 }`.
- [ ] **Step 3: JS**: `renderHead` adds `h('div', { class: 'docs' }, x.docs.map(d => h('button', { class: 'doc-row', onclick: () => openDoc(x, d.path) }, basename(d.path), ' ', when(d.at))))` (newest first; a `·` dot when `d.at > Date.now() - 2000`). `openDoc` POSTs `/api/fleet/doc`, sets `pane.doc`, `renderDoc()`. `renderDoc`: `md` → `#doc-body.innerHTML = render(window.marked, text)`; `html` → `h('iframe', { class: 'mock', sandbox: 'allow-scripts', srcdoc: text })`; `text` → `<pre>`; `source: true` → `<pre>` of text. On WS `fleet` op (line ~782) after `load()`, call `refreshDoc()`.
- [ ] **Step 4: Run** the harness — all assertions pass; desktop screenshot shows three columns. `npm test` green.
- [ ] **Step 5: Commit** `feat(fleet): a Documents pane — what a session wrote, rendered the moment it lands`.

### Task 4: Artifact guard hook

**Files:**
- Create: `fleet/artifact-guard.sh`
- Modify: `fleet/install-hooks.mjs` (install for `PreToolUse` with `matcher: "Artifact"`, same idempotence/backup rules as `hook.sh`; `--remove` takes it out), `README.md` (one paragraph under the hooks section)
- Test: `test/fleet-hooks.test.mjs`

**Interfaces:**
- Produces: `fleet/artifact-guard.sh` reads the hook JSON on stdin. Pass (print nothing, exit 0) when `tool_input.action` ∈ {`read`,`list`,`open`,`quickstart`,`pin`,`unpin`,`delete`}, or `ACP_ALLOW_ARTIFACT=1`, or `tool_input.url` equals a non-empty line of `$HOME/.fleet-panel/artifact-allow.txt`. Otherwise print `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"<spec text>"}}` and exit 0. The reason text is the spec's, verbatim.
- Produces: `install-hooks.mjs` adds `{ matcher: 'Artifact', hooks: [{ type: 'command', command: "sh '<dir>/artifact-guard.sh'", timeout: 5 }] }` to `PreToolUse`; `OURS` recognises both scripts.

- [ ] **Step 1: Failing tests**: five cases from Review Focus 4 plus the allow-list file and the env var; assert exact JSON for a deny and empty stdout for a pass; `installHooks()` pure function output contains the Artifact matcher once after two runs.
- [ ] **Step 2: Run** FAIL. **Step 3: Implement.** **Step 4: PASS**, `npm test` green. **Step 5: Commit** `feat(fleet): no unasked Artifacts — a PreToolUse guard that says where the file goes instead`.
