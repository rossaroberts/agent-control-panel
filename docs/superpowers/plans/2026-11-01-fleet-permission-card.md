# Fleet Permission Card Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A session that waits for a permission, a question or a plan approval shows a card in its Fleet session view (phone footer, desktop pane) with the same buttons the Board has, and the Fleet page alone is enough for the server to hold the prompt.

**Architecture:** The Board already holds `PermissionRequest` hooks in `pending` (server.mjs `holdPermission`) and answers them through `decide()`, but only while a Board SSE client is connected (`watched()`) and only for the profile on the Board. This plan makes a Fleet WebSocket page count as watching, joins each local session row with its held request, pushes `{"op":"fleet"}` when a request opens or settles, adds `/api/fleet/decide` that answers a request from any local profile, and ports the three Board cards into `fleet.html` in compact form.

**Tech Stack:** Node 20+ ESM, vanilla DOM, `node:test`, Playwright harness `tools/phone-shot.mjs`.

**Spec:** `docs/superpowers/specs/2026-11-01-fleet-mobile-and-documents-design.md` (phone app: "approve or deny a permission prompt"; Session screen footer: "the permission controls when a prompt is open").

## Global Constraints

- Held requests exist only on the Mac's panel: redbat's hooks point at its own 127.0.0.1:4747, where nothing listens. Rows of a remote machine never carry `pending`; the card never shows for them.
- No new polling: a request opening or settling reaches pages as the existing `{"op":"fleet"}` push.
- Payload shapes to `decide()` are the Board's, unchanged: `{behavior:'allow', updatedInput, always, mode?}`, `{behavior:'deny', message?}`, `{behavior:'answer', answers:{[question]: text}}`, `{behavior:'approve-plan', mode}`. Read them in `public/index.html` (`buildPermissionCard`, `buildQuestionCard`, `buildPlanCard`, ~2195–2290) before writing the Fleet card.
- `npm test` stays green (658 at `feat/fleet` b7b2cac; higher after #6 and push merge).
- Repos are public: no estate paths or hostnames in committed files.

## Review Focus

1. A request that expired or was answered in the terminal: `/api/fleet/decide` returns `ok:false` with the Board's "That request is gone" text and the card shows it instead of staying armed — Task 2 test, Task 3 harness.
2. Two pages (phone and desktop) both press Allow: the second gets `ok:false`, nothing is double-sent — Task 2 test (second `decide` on the same id).
3. A Fleet page open with no Board page: `/hook/permission` is held, not answered empty — Task 1 test.
4. A remote machine's row with `waitingFor: 'permission prompt'` has no `pending` and shows the plain "needs your yes" line, never a card — Task 1 test (row with `machine !== local`).
5. `AskUserQuestion` with `multiSelect: true`: the card lets several options be chosen and joins them the way the Board does (read `buildQuestionCard` for the exact join) — Task 3 harness fixture with one multiSelect question.

---

### Task 1: Fleet pages count as watching; rows carry their held request

**Files:**
- Modify: `server.mjs` (`watched` at ~123; `holdPermission`/`settle` at ~437–466; the `/api/fleet/list` handler in `ACTIONS` ~1887; `pages` set near ~1988)
- Test: `test/daemon-server.test.mjs`

**Interfaces:**
- Produces: `watched()` is true when a Fleet WebSocket page is connected (`pages.size > 0`) as well as when an SSE client is.
- Produces: each row returned by `/api/fleet/list` for the local machine gains `pending: { id, open, toolName, toolInput, suggestions, permissionMode, createdAt, deadline } | null`, found by `sessionId` across **all** profiles' entries in `pending` plus `openRequests()` (an open `AskUserQuestion`/`ExitPlanMode` read from the transcript, `id` prefixed `open:`). Remote rows have no `pending` key.
- Produces: `holdPermission` and `settle` each send `'{"op":"fleet"}'` to every socket in `pages` after their existing `broadcast`.

- [ ] **Step 1: Failing tests** in `test/daemon-server.test.mjs` (follow its existing server-start fixture): (a) open one Fleet WS page and no SSE client, POST `/hook/permission` with a `PermissionRequest` body → the response does not arrive within 300 ms (held); then `POST /api/decide` on its id → the hook reply is the allow decision. (b) With a held request for `session_id: 's1'`, `/api/fleet/list` row for `s1` on the local machine has `pending.id` equal to the held id and `pending.toolName`; a row for a remote machine has no `pending`. (c) The Fleet WS page receives `{"op":"fleet"}` once on hold and once on settle.
- [ ] **Step 2: Run** `node --test test/daemon-server.test.mjs` — FAIL.
- [ ] **Step 3: Implement.** The list join lives in `server.mjs` beside `ACTIONS` (fleet.mjs stays free of Board state): after `fleet.list(b)` resolves, map rows where `row.machine === fleet.localName()` (read `fleet/fleet.mjs` for the local machine's name; add a one-line export if none exists). Strip `res`, `timer`, `profileDir` from the pending entry as the snapshot at ~1700 does.
- [ ] **Step 4: Run** PASS; `npm test` green. **Step 5: Commit** `feat(fleet): a Fleet page holds permission prompts too, and each local row knows its held request`.

### Task 2: `/api/fleet/decide`

**Files:**
- Modify: `server.mjs` (`decide(body)` at ~467; `ACTIONS` ~1887; the `/api/decide` branch ~1872)
- Test: `test/daemon-server.test.mjs`

**Interfaces:**
- Produces: `decide(body, { anyProfile = false } = {})` — with `anyProfile`, the `p.profileDir !== configDir` refusal is skipped; everything else (gone, answer validation, open-request paths via `answerOpen`) is unchanged. `POST /api/fleet/decide` = `decide(body, { anyProfile: true })`, and for an `open:` id, the same `answerOpen` path the Board uses.
- Consumes: Task 1's `pending.id`.

- [ ] **Step 1: Failing tests**: hold a request whose hook body carries another profile's config dir (`profileDirOf(e)` reads it; see the test file for how a profile is faked) → `/api/decide` returns the "belongs to a profile that is not on the board" error, `/api/fleet/decide` returns `ok:true` and the hook gets the decision; a second `/api/fleet/decide` on the same id → `ok:false` with `/gone/`.
- [ ] **Step 2: Run** FAIL. **Step 3: Implement.** **Step 4: PASS**, `npm test` green. **Step 5: Commit** `feat(fleet): answer a held request from Fleet, whichever local profile it belongs to`.

### Task 3: The card in `fleet.html`

**Files:**
- Modify: `public/fleet.html` (session view: `renderHead` and the pane body; phone footer `phoneSections`/compose area from PR #6), `tools/phone-shot.mjs` (new `--permission` mode)

**Interfaces:**
- Consumes: `x.pending` (Task 1), `POST /api/fleet/decide` (Task 2), `api()` helper in `fleet.html`.
- Produces: `renderCard(x)` returns a `<section class="card">` or `null`; it is placed above the reply box (phone footer) and at the top of the pane body (desktop). `pane.cardBusy` guards double sends. Three shapes, ported from the Board's builders, compact: **permission** → tool name, one line of its input (`command` | `file_path` | `url` | `description` | first 120 chars of JSON), buttons Allow · Allow and remember (only when `suggestions.length`) · Deny (a reason field folds open on first tap); **question** → each question's text, its options as toggle buttons (`multiSelect` honoured) plus an Other field, one Send; **plan** (`ExitPlanMode`) → Approve · Send back with a feedback field. On `ok:false` the card body shows `error` and the buttons disable. After `ok:true` the card reads "Sent" until the next `{"op":"fleet"}` removes `x.pending`.
- Produces: `tools/phone-shot.mjs --permission`: starts the server, POSTs a `PermissionRequest` for the fixture session to `/hook/permission` (no Origin header), opens the page at 390×844, selects the session, asserts the card is in the footer with an Allow button, presses Allow, asserts the hook request completed with `behavior: 'allow'` and the card left; repeats at 1440×900 asserting the card is at the top of the pane; a multiSelect `AskUserQuestion` fixture asserts two options send as the Board's join.

- [ ] **Step 1: Harness first** (`--permission` mode) — run: FAIL (no card).
- [ ] **Step 2: Markup + CSS**: `.card` uses the pane's existing tokens; buttons at least 44 px tall on the phone; the card sits inside the footer above `--kb` so the keyboard never covers it.
- [ ] **Step 3: JS**: `renderCard`, the three builders, `decideFleet(payload)` → `api('/api/fleet/decide', { id: x.pending.id, ...payload })`.
- [ ] **Step 4: Run** the harness PASS; `npm test` green; desktop + phone screenshots in `tools/out/`.
- [ ] **Step 5: Commit** `feat(fleet): allow, deny, answer and approve from the Fleet page — phone footer and desktop pane`.
