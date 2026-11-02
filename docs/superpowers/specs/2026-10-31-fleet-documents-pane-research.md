# Fleet: a documents pane, the app question, and the phone — research

Date: 2026-10-31. Status: research, not a design. Nothing here is built. The design
document comes after Ross answers the questions at the end.

The three asks, in Ross's words: split the Fleet screen into three, the third pane
showing markdown or a web-page mockup; stop sessions publishing claude.ai Artifacts he
did not want, and instead have the session write the right document from the terminal
and have it appear in the sidebar of whichever session made it; make it lightning fast;
then decide whether this should be "an application", and whether it can reach a phone
at the same speed.

## 1. What exists today (verified against the code, branch `feat/fleet`)

### Layout

- Two regions, no tabs. `<main>` on the left holds accounts, machines, the prompt box and
  the session list (`public/fleet.html:283-306`). `<aside id="pane">` on the right holds
  the open session (`fleet.html:307-314`).
- The pane is `position: fixed; right: 0; width: var(--pane)` (`fleet.html:179`), default
  460 px (`fleet.html:28`), dragged by `#p-grip` between 340 px and 80 % of the window
  (`fleet.html:675-682`), persisted as `fleet.pane.width`. `⌘\` folds it to a 44 px rail.
- The list is not a grid column; `body.has-pane main` just shrinks to
  `calc(100% - var(--pane) - 64px)` (`fleet.html:41`).
- The pane toggles between the xterm view `#p-term` and the transcript view `#p-body` +
  `#p-foot` in `renderPane()` (`fleet.html:1057-1105`).
- One responsive breakpoint: under 720 px the pane takes `100vw` and the list hides
  (`fleet.html:269-279`).

### Context page (commit 673480b)

- Not a session tab. It is a fourth top-nav page, `/context`, listing eight fixed
  documents that shape how sessions start: zshrc, hooks, both repos' `CLAUDE.md`, the
  memory index, the newest handoff (`fleet/context.mjs:224-270`). Nothing is keyed to a
  session.
- Markdown is rendered by a vendored `marked` (`public/vendor/marked.umd.js`, 47 KB) through
  `public/context-render.mjs`, which overrides the renderer so raw HTML is escaped, only
  http(s) links survive, and images become `[image: alt]`. The page sets
  `body.innerHTML = renderSafe(...)` under a nonce CSP (`server.mjs:1785`).
- Fleet itself does not load `marked`. It has a hand-rolled `md()` that builds DOM with
  `textContent` (`fleet.html:561-594`).

### Transport and what makes it feel instant

- One WebSocket per tab carries every terminal as binary frames (4-byte ref + pty bytes)
  plus three JSON ops: `fleet` (list changed, re-`load()`), `bye`, `end`
  (`server.mjs:1946,1955`, `fleet/term.mjs:176-200`). Unknown ops are ignored by the page
  (`fleet.html:777-788`), so a new op slots in without breaking anything.
- Hook events from Claude Code patch session state in memory at once and a 50 ms debounced
  `onChange` pushes `{"op":"fleet"}` to every page (`fleet/fleet.mjs:337-378`).
- Everything else polls: list every 10 s, open-session transcript (`peek`) every 2.5 s,
  machines every 10 s while watched, roster every 3 s (`fleet.html:730,1553`,
  `fleet.mjs:25`, `server.mjs:50`).
- The last three terminals stay attached off-screen (`KEEP=3`), WebGL renderer, local echo
  over slow ssh, `setNoDelay`, one persistent ssh ControlMaster per machine
  (`fleet/ssh.mjs`).
- No file watching of any kind exists. No route serves an arbitrary file.

### What the panel knows about files a session writes

- The HTTP hook at `/hook/event` receives the full `PostToolUse` payload including
  `tool_input.file_path` (`server.mjs:279-303`), reduces it with `summarize()` to a
  240-char label, and keeps only the **latest** tool as `s.lastTool` (`server.mjs:222`).
  No history. `Artifact` has no special handling; `summarize` would pick `input.url` or
  `input.file_path`, whichever comes first.
- `fleet/hook.sh` (the file-based hook into `fleet-state/<session>.json`) keeps only
  `{e, t, n, r, i, at}`: event name and tool name, never the path (`fleet/hook.sh:10-13`).
- `machine.mjs:99-100` turns Write/Edit into a step labelled with the basename only;
  `server.mjs:799-804` scans the transcript tail for `file_path` but keeps only the git
  top-level folders (max 4). The full path is dropped everywhere before it reaches a page.
- The ssh worker's op allow-list is `ALLOWED = new Set(['collect','launch','peek',...])`
  (`fleet.mjs:52`); any file read for a remote session must be a new op on that worker.

### What steers sessions toward Artifacts

- Nothing in `chai-fm/CLAUDE.md`, memory, or settings says "publish an artifact". The
  pressure comes from the `Artifact` tool's own description in the Claude Code system
  prompt, which tells the model to publish any finished work meant for other people.
- The one legitimate standing artifact: one skill in the chai-fm estate republishes
  a locally written HTML table to one fixed `claude.ai/code/artifact/…`
  URL. Everything else is the model's own initiative.
- Where the estate already expects documents: `pm/outbox/` (37 dated drafts,
  `YYYY-MM-DD-<slug>.md`), per-city `docs/` and `runbooks/`, `erpx-mcp/audit/runs/` for
  generated reports, `design/project/` for HTML and JSX templates.

### The app and the phone

- "Agent Panel.app" is already an application: a WKWebView wrapper (`app/main.swift:84`)
  that starts or reuses `node server.mjs`, restarts it when it dies, badges the Dock from
  the page title, shims `Notification` onto `UNUserNotificationCenter`, and adds a File
  menu. Built with `swiftc -O`, ad-hoc signed, macOS 14+ (`app/build.sh`).
- The server binds `127.0.0.1` only (`server.mjs:1963`), accepts only
  `127.0.0.1:PORT`/`localhost:PORT` as Host (`:75`), needs the `acp_k` pairing cookie
  (HttpOnly, `SameSite=Strict`, one year) and a per-start token injected into the HTML.
  Four separate things each stop a phone reaching it today.
- The hub (`universe.cashegg.com`) carries World counts and orders over SSE. No session
  list, no terminals, no transcript. It is not a relay and was never meant as one.
- Tailscale is not installed on this Mac. No spec, plan or CSS targets mobile beyond the
  one 720 px breakpoint.
- Port 3047 is a Next.js dev server, not the panel.

## 2. Findings from outside the repo

- **Claude Code hooks** carry `tool_input.file_path` on `PostToolUse` for `Write|Edit`,
  and `PreToolUse` can return a deny decision with a reason the model sees. The matcher is
  by tool name; path filtering happens in the script.
  ([docs](https://docs.claude.com/en/docs/claude-code/hooks))
- **Turning Artifacts off**: the permissions docs allow `Tool(param:value)` deny rules and
  per-scope deny lists, but no page names an Artifacts kill-switch. A deny rule on the
  `Artifact` tool name has to be tested on this build, not assumed.
  ([settings](https://docs.claude.com/en/docs/claude-code/settings),
  [what's new w25](https://code.claude.com/docs/en/whats-new/2026-w25)). See §4 for the
  verified answer once the docs check returns.
- **Tailscale Serve** is the standard way to put a loopback server behind an HTTPS
  `*.ts.net` URL reachable from a phone on the tailnet. Needs MagicDNS + HTTPS certs on in
  the admin console; first request is slow while the cert issues; one open report of
  `ERR_SSL_PROTOCOL_ERROR` on an iPhone.
  ([Open WebUI guide](https://docs.openwebui.com/ecosystem/computer/phone-and-remote/tailscale),
  [issue 19147](https://github.com/tailscale/tailscale/issues/19147))
- **iOS Web Push** works only for a Home Screen (installed) PWA over HTTPS with a manifest,
  since iOS 16.4; no install banner; background sockets are not kept alive, so "Needs you"
  on a locked phone must go through push.
  ([MagicBell guide](https://www.magicbell.com/blog/pwa-ios-limitations-safari-support-complete-guide),
  [PWA 2026](https://slightfuture.com/webdev/progressive-web-apps-2026/))
- **xterm.js on iOS** is the weak point: maintainers call mobile "a blind spot"; copy, arrow
  keys and the on-screen keyboard leaking through overlays are open or app-level
  workarounds. ([xterm.js #3727](https://github.com/xtermjs/xterm.js/issues/3727),
  [atterm PR 90](https://github.com/attson/atterm/pull/90))
- **Markdown speed**: `marked` parses a 10,000-word document in roughly 3 ms in one 2026
  guide; no credible benchmark separates it from `markdown-it`. For documents of the size
  a session writes, the parser is not where time goes; the fetch round-trip and the
  re-render strategy are.
  ([PkgPulse](https://www.pkgpulse.com/guides/best-markdown-parsing-libraries-2026))

## 3. Recommendation (my call unless Ross overrides)

### 3a. The third pane: "Documents"

- **Column, not tab.** List | Session | Document, since Ross said split into three. The
  document pane is a second fixed `<aside>` to the right of `#pane`, with its own grip and
  persisted width; `main` shrinks by both. Under ~1100 px the document pane overlays the
  session pane with a toggle; under 720 px it is a full-screen sheet (same pattern the
  session pane already uses).
- **Sidebar list inside the session pane head**: a "Documents" strip listing every file
  the open session wrote or edited with `.md`, `.html` (and `.svg`, `.csv`, `.json` as
  plain text), newest first, with a dot while the file is still being written. Clicking
  one opens it in the third pane. The list persists across the session's life, so a
  document written at 09:00 is still there at 17:00.
- **Rendering**: markdown through the already-vendored `marked` + `renderSafe` (reuse
  `context-render.mjs` as a module; fleet.html's inline script would need a `type=module`
  shim or a small loader). HTML mockups through a sandboxed `<iframe srcdoc>` with
  `sandbox="allow-scripts"` and no `allow-same-origin`, so a mockup can run its own JS but
  cannot touch the panel's cookie or token. Serve the file bytes through a new POST op,
  never a GET route, so the panel keeps its "no arbitrary file URL" property.
- **Speed path** (why this will feel instant):
  - The hook already lands the `Write` payload in-process within milliseconds of the write.
    Keep the full path in a new per-session `docs[]` ring (cap 50) instead of only
    `lastTool`.
  - Broadcast `{op:'doc', session, path, at}` on the existing WebSocket. The page adds the
    row at once and, if that document is open (or the pane is empty), fetches content.
  - Content fetch is a new `doc` op on the machine worker (local: direct read; remote: over
    the persistent ssh line, ~0.2 s Mac / 1.3 s redbat, same as `peek`). Cap 1 MB. Path
    must be under the session's cwd or its git top-level, realpath-checked like
    `context.mjs:298-309` does for memory notes.
  - Re-render on each push is a full `marked` parse: single-digit ms at these sizes. If a
    session streams a long document in many edits, debounce 100 ms and keep scroll
    position. No incremental parser needed.
  - Zero polling. The pane never asks "did anything change".

### 3b. Stopping unwanted Artifacts and teaching sessions the new habit

The instruction change should be enforced where it matters, not written where it gets
buried. Three layers, cheapest first:

1. **A `PreToolUse` hook on `Artifact`** (in the panel's hook set, so every profile gets it)
   that denies `publish` unless the `url` is on an allow-list (the standing user-activity
   page) or `ACP_ALLOW_ARTIFACT=1` is set in the session's environment. The deny reason
   is the instruction, delivered at the exact moment the model reaches for the tool:
   "Write the file into the repo (docs/ or pm/outbox/ by estate convention) — the Fleet
   Documents pane shows it. Do not publish." This costs zero context, cannot be
   compacted away, and the panel can log every blocked publish so Ross sees the habit
   die off.
2. **One line in `chai-fm/CLAUDE.md` § How we report**: "Deliverables are files in the
   repo, not claude.ai Artifacts; the Fleet Documents pane renders `.md` and `.html` from
   any session." Plus a `[[fleet-documents-pane]]` memory note so sessions on other
   repos pick it up.
3. **`permissions.deny: ["Artifact"]`** as the blunt fallback if the hook route proves
   fragile. Verify on this build first (see §4).

Where documents go: no new folder convention is required for the pane to work, since it
lists whatever the session wrote. A light convention helps tidiness: research and designs
to `docs/superpowers/specs/` (already the habit in this repo), outbound drafts to
`pm/outbox/`, throwaway mockups to `$CLAUDE_JOB_DIR/tmp` or `<repo>/.scratch/` (gitignored).

### 3c. "Should we make this an application?"

It already is one on the Mac, and the WKWebView wrapper is the right shape: the whole
interface is the web page, the shell adds Dock badge, notifications, menu and server
lifecycle. Rewriting it in SwiftUI/Electron/Tauri would buy nothing Fleet needs and would
lose the no-build-step iteration speed the repo relies on. Recommendation: keep it;
add a `manifest.json` + service worker so the same page installs as a PWA on the phone.
Do not build a native iOS app in this round.

### 3d. The phone, at the same speed

- **Reach**: Tailscale on the Mac and the phone, `tailscale serve --bg 4747`, giving
  `https://<mac>.<tailnet>.ts.net`. Server changes: accept that hostname in `HOSTS` and
  `ORIGINS` (from an `ACP_PUBLIC_HOST` env), keep binding to loopback (Serve proxies from
  the tailnet, so nothing opens on the LAN), and let the pairing flow run once from the
  phone (`node server.mjs open` prints a URL; a QR code of it on the Profile box is a
  small addition). `SameSite=Strict` is fine since every request is same-site.
- **Speed**: the tailnet hop adds a few tens of ms on Wi-Fi; the WebSocket and hook-push
  design carries over unchanged. The list and transcript already arrive as a cached
  snapshot. What does not carry over is xterm on a phone screen with an on-screen
  keyboard. Mobile should lead with the three things that work well in a column: the
  Needs-you list, the transcript view with the answer box, and the Documents pane. The
  terminal stays available behind a tap but is not the default.
- **"Needs you" when the phone is locked**: Web Push from an installed PWA (iOS 16.4+).
  The server already knows the moment a permission prompt or question arrives; a VAPID
  push on that event is a small addition. Without push, a locked phone learns nothing
  until it is unlocked, because iOS does not keep the socket alive.
- **Order**: Tailscale Serve + host allow-list first (an afternoon, immediately useful,
  even with today's layout since the 720 px breakpoint already makes the pane full-width).
  Then the mobile column layout and PWA manifest. Push last.

## 4. Verified against the Claude Code docs

- **Artifacts can be switched off** four ways: the Artifacts row in `/config`,
  `"enableArtifact": false` in any settings scope (project/local override a managed
  `true`), `CLAUDE_CODE_DISABLE_ARTIFACT=1`, or `Artifact` in `permissions.deny`.
  There is no documented "ask before publishing"; a new artifact publishes without a
  prompt. ([artifacts](https://code.claude.com/docs/en/artifacts.md),
  [settings](https://code.claude.com/docs/en/settings.md))
- **Parameter-matched deny rules** (`Tool(param:value)`) work on any built-in tool's
  top-level input field, exact match or `*`. Whether `Artifact` exposes `action` or `url`
  as a matchable field is not documented; `claude --verbose` shows the real names.
  ([permissions](https://code.claude.com/docs/en/permissions.md))
  Consequence for §3b: the all-or-nothing switch is trivial; the allow-list for the one
  standing user-activity page needs either a `PreToolUse` hook (decision + reason, which
  also carries the instruction) or a verified `Artifact(url:…)` rule. Hook first.
- **`PostToolUse` on `Write`** delivers `session_id`, `cwd`, `tool_input.file_path`,
  `tool_response.filePath`, `tool_use_id`, `duration_ms`. Edit/MultiEdit shapes are not
  spelled out ("depends on the tool"), so the panel hook should read
  `tool_input.file_path ?? tool_response.filePath` and tolerate either.
  ([hooks](https://code.claude.com/docs/en/hooks.md))
- **Hook-injected instructions**: `SessionStart` and `UserPromptSubmit` hooks can add
  `additionalContext` (cap 10,000 chars, wrapped in a system reminder, saved to the
  transcript, replayed on resume). This is the right channel for the one-paragraph
  "documents are files, the Fleet pane shows them" rule across every repo, instead of
  editing each `CLAUDE.md`. The panel's hook set already runs on `SessionStart`.
- **Remote Control already covers the single-session phone case**: a session linked to
  `claude.ai/code/<id>` syncs its transcript to the Claude mobile app, accepts replies
  from the phone, and pushes permission prompts and questions. What it does not give is
  the fleet view (every session on every machine), the documents pane, or terminals.
  So the phone work in §3d is about Fleet, not about talking to one session.
  ([remote-control](https://code.claude.com/docs/en/remote-control.md))

## 5. Questions only Ross can answer

1. Third pane as a true third column, or as a toggle that replaces the terminal inside the
   session pane when the screen is narrow? (I assume column on wide, overlay under 1100 px.)
2. Which files count as documents: anything the session writes with `.md`/`.html`, or
   only files under a named folder? (I assume anything, with a filter chip for the noisy
   ones like `BOARD.md` regenerations.)
3. Block Artifact publishing outright except the standing user-activity page, or make it
   ask? (I assume block + allow-list.)
4. Is Tailscale acceptable on the work Mac and phone, or must the phone route go through
   something else (the hub as a relay would be a much larger build and carries live data
   off the Mac)?
5. On the phone, is read-and-answer (transcript, Needs you, documents, permission
   prompts) enough for the first cut, with the terminal second?

## 6. Sizes

- Documents pane + hook ring + WS op + worker op: a project with a written design.
  About two to three days.
- Artifact PreToolUse hook + CLAUDE.md line + memory note: a small change. Half a day,
  including a test in `test/`.
- Tailscale Serve + host allow-list + QR pairing: a small change. Half a day plus
  installing Tailscale.
- Mobile column layout + PWA manifest + Web Push: a project with a written design.
  Two to three days.
