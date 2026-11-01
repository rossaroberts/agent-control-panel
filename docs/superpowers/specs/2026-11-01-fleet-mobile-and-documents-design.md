# Fleet on the phone, and a Documents pane — design

Date: 2026-11-01. Follows the research at `2026-10-31-fleet-documents-pane-research.md` (issue
IcyDingo/agent-control-panel#22). Ross skipped the design questions ("urgently need a mobile app that
crushes it … fan out and make it work for whatever is fastest performance wise"), so every open
decision below is the author's call, marked **(call)**.

## Intent

Ross runs a fleet of Claude Code sessions across his Mac and redbat and steers them from the Fleet
page. He needs (1) that page on his phone, as an app, as fast as the desktop; (2) a third pane that
shows the markdown or HTML a session wrote, listed per session, so sessions stop publishing
claude.ai Artifacts he never asked for.

## Goals

- A Home Screen app on the phone: full-screen, its own icon, reachable from anywhere, that shows
  every session's state within a second of it changing, lets him read a session and answer it,
  approve or deny a permission prompt, and open the terminal when he must.
- A Documents pane on the desktop and a Documents sheet on the phone: every `.md`/`.html` (and a
  few text kinds) a session writes appears in that session's list the moment it is written, and
  renders on tap.
- Sessions stop publishing Artifacts unless the user has allowed that URL.
- Speed: nothing new polls. Every new piece rides the existing WebSocket push or a hook event.

## Non-goals

- A native iOS app or App Store presence (a Capacitor shell can wrap the same page later).
- Carrying live session data through the hub. The phone talks to the Mac directly.
- Editing documents from the pane.
- Codex sessions' documents (they have no file hooks).

## Anti-goals

- Anything that opens the panel to the LAN or the internet without the pairing cookie.
- A GET route that serves arbitrary files.
- A dependency with a build step. Vendored single files only, as `marked` and `xterm` are.

## Constraints

- Node 20+, no bundler, ES modules, hand-written WebSocket server (`fleet/ws.mjs`).
- The server binds 127.0.0.1 only. The phone reaches it through **Tailscale Serve** (`tailscale
  serve --bg 4747`), which terminates HTTPS on the Mac and proxies to loopback, adding
  `X-Forwarded-Proto: https` with `Host` set to the Mac's `*.ts.net` name. **(call)** Tailscale,
  because the alternative (hub relay) is weeks of work and moves live data off the Mac.
- Both GitHub repos are public: no hostnames, keys or estate paths in committed files beyond what
  the README already shows.
- iOS: Web Push and a persistent icon need an installed (Home Screen) PWA over HTTPS with a
  manifest. A suspended PWA keeps no socket; "Needs you" on a locked phone is a push.
- xterm.js on iOS is weak (maintainers: "a blind spot"). The terminal is available on the phone but
  is never the default view.

## The phone app

**Reach.** `fleet.json` (beside `prefs.json`) gains `"public": "<name>.<tailnet>.ts.net"`; env
`ACP_PUBLIC_HOST` overrides. The server accepts that `Host` and `https://<it>` as an Origin, for pages,
`/api/*` and the WebSocket upgrade alike. The pairing cookie gains `Secure` when the request came
over HTTPS. Nothing else changes: still loopback-bound, still the one-year `acp_k` cookie, still a
per-start token.

**Pairing from the phone.** `node server.mjs pair` prints the pairing URL (public host if set, else
loopback) and draws it as a QR in the terminal. The desktop page's Profile box gets **Pair a phone**,
which shows the same QR on screen. Scanning it once sets the cookie on the phone.

**Install.** `/manifest.webmanifest` (name Fleet, `display: standalone`, icons 192/512 rendered from
`app/icon.svg`), `apple-touch-icon` 180, `apple-mobile-web-app-capable`, `theme-color`, viewport
`viewport-fit=cover`. A service worker at `/sw.js`, registered only over HTTPS: network-first for
navigations with an offline page ("Your Mac is unreachable"), nothing cached from `/api/`, and the
`push`/`notificationclick` handlers.

**Layout under 720 px** (`body.phone`, set by `matchMedia`). Two screens that slide:

1. *Queues.* Top bar: Fleet, the account tally, a **+** that opens the prompt box as a sheet.
   Sections Needs you (always open), Working, Idle, Finished (folded, as today). A row is one
   line: name · state word · age, and the session's last words underneath. Rows are at least
   56 px tall; tapping anywhere on a row opens it; `touchstart` prefetches it (the hover prefetch
   today).
2. *Session.* A header with a back chevron, the name and state word, and a **Terminal** toggle.
   The body is the conversation (the transcript view, never xterm by default). The footer is the
   reply box plus the permission controls when a prompt is open, pinned above the keyboard
   (`visualViewport` sets `--kb`). Safe-area insets respected. Selecting pushes a history entry
   so the phone's back gesture returns to the queues.

Everything else (groups, drag, rename, keyboard shortcuts, Satellite, World, Context) stays
desktop-only and is hidden on the phone.

**Push.** VAPID keys generated once into the prefs dir; the page subscribes when installed and
posts the subscription to `/api/push/subscribe`; the server sends a push (RFC 8291 `aes128gcm`,
RFC 8292 VAPID, implemented on Node's `crypto`, no dependency) wherever it already sends the Mac
notification for a session that comes to need you. Tapping the notification opens
`/?s=<machine>:<sessionId>`, which the page reads to select that session.

## The Documents pane

**Capture.** `fleet/hook.sh` (same file on every machine) keeps, for `PostToolUse` of
`Write|Edit|MultiEdit|NotebookEdit` whose path ends in `.md .markdown .html .htm .svg .txt .csv
.json`, one appended line `{"p":"<abs path>","t":"<tool>","at":<ms>}` in
`<profile>/fleet-state/<session>.docs.jsonl`, trimmed to its last 200 lines past 64 KB. The
existing state file is untouched. **(call)** Any such file counts; no folder convention is needed
for the pane to work, and the estate's conventions (`docs/superpowers/specs/`, `pm/outbox/`) stay
advice in `CLAUDE.md`.

**Read.** The machine worker adds `docs` to each session row (last 50 entries, newest first,
deduplicated by path) and watches `*.docs.jsonl` like it watches state files, so a write reaches
every page as the existing `{"op":"fleet"}` push. A new worker op `doc({profile, sessionId,
path})` returns `{ok, kind: 'md'|'html'|'text', text, at, path}` for a path **that session's docs
list contains** (the only paths it will serve), real-path checked, 1 MiB cap. It runs over the
same ssh line as `peek` for redbat. Route: `POST /api/fleet/doc`.

**Show.** Desktop ≥ 1100 px: a third fixed column `#doc` right of `#pane`, own grip and persisted
width (`fleet.doc.width`, default 520 px, min 360), `main` shrinks by both. 720–1100 px: `#doc`
overlays `#pane`, toggled from the pane head. Phone: a full-screen sheet with a back chevron. The
pane head gains a **Documents** strip: each entry name · age, newest first, a dot while a write is
under way; tap renders. Markdown renders through the vendored `marked` and `renderSafe` from
`public/context-render.mjs`. HTML renders in `<iframe sandbox="allow-scripts" srcdoc>` (no
`allow-same-origin`: a mockup runs its own scripts and cannot read the panel's cookie or token).
A **Source** toggle shows the raw text. A re-render on each push keeps scroll position, debounced
100 ms.

**Stop Artifacts.** A second hook script, `fleet/artifact-guard.sh`, installed for `PreToolUse`
with matcher `Artifact` by `install-hooks.mjs`: read/list/open/quickstart actions pass; a publish
passes if `ACP_ALLOW_ARTIFACT=1` is in the environment or the `url` is a line in
`~/.fleet-panel/artifact-allow.txt`; anything else is denied with the reason: *"Fleet: Artifacts
are off here. Write the document as a file in the repo (research and designs → docs/superpowers/specs/,
outbound drafts → pm/outbox/, reports where the skill says) and give the user its path; the Fleet
Documents pane renders .md and .html a session writes. To publish anyway, the user adds the URL to
~/.fleet-panel/artifact-allow.txt or sets ACP_ALLOW_ARTIFACT=1."* The one standing user-activity
page goes on the allow-list by hand. **(call)** Block, not ask: Claude Code has no documented "ask"
for Artifacts.

## Left to the builder

Exact CSS values, icon rendering sizes beyond those named, the QR library choice (any MIT UMD
single file that runs in Node and the browser), the offline page copy, and test fixtures. Where
the plans name a function, use that name.

## Order of work

Wave 1, in parallel on separate branches off `feat/fleet`: public host + PWA + pairing;
phone layout; documents capture/read/route + artifact guard. Wave 2 after wave 1 merges: the
Documents pane UI (touches `fleet.html` after the phone layout has landed), then push.
