# Fleet, native: a Rust + Swift port of Fleet, Context and Satellite

Status: design, for review. Date: 2026-11-06. Branch: `docs/native-port` (from `feat/fleet` @ 1595bd5).

## 1. Intent

Fleet works, and its design is right. Ross uses it all day: every Claude Code and Codex session he runs, on
the Mac and on redbat, in one list, grouped by area, with the session's real terminal one click away. The
direct terminal link is what made it worth having.

What is wrong is how it feels: a little unwieldy, sessions closing or getting lost, numbers a beat behind,
control that could be sharper. Ross wants the same thing **minimal and blazing fast**: a native Mac app first,
then a native iPhone app.

This is a **port, not a redesign**. The current code (`fleet/*.mjs`, the Fleet parts of `server.mjs`,
`public/fleet.html`, `fleet/context.mjs`, `fleet/satellite.mjs` and their tests) is the specification of
behaviour. Rewrite it in Rust (the always-on side) and Swift (everything you look at). Keep what it does,
make it instant, and make it never lose a session.

### Why it feels slow today (measured from the code, not guessed)

- **Polling, not events.** `fleet/fleet.mjs` re-collects every machine every `WARM_MS = 10 s` while a page
  watches. For redbat that means piping the whole of `machine.mjs` over ssh to `node --input-type=module -`
  and re-reading the last 384 KB of every transcript, on every pass. Everything on screen can be 10 s + an
  ssh round trip old.
- **A long path to the terminal.** Keystrokes go page → WebSocket → Node → a Python pty relay → `claude attach`
  (or `ssh -tt` → `claude attach` on redbat) and back, drawn by xterm.js inside a WKWebView (`app/main.swift`
  is a web-view wrapper).
- **"Closed" is a guess.** The list infers state from records Fleet does not own (`claude agents --json`, job
  `state.json`, pids, timelines, hook events). When they disagree a live session can read as closed (the
  respawn bug fixed in fork PR #3). Today the board shows 60 + 22 + 15 + 1 + 301 "closed": the pile where
  things get lost.

## 2. Goals

1. **Everything instant.** A change on any machine is on screen as it happens, not on the next poll.
2. **Never lose a session.** Every session's state is truthful and explained. Nothing leaves the board unless
   Ross archives it, or Claude Code / Codex itself says it ended, and then the row says why.
3. **Same board, native.** Everything the Fleet page does today, in a native Mac app with standard Mac UI
   (sidebar, toolbar, menus, keyboard shortcuts), the terminal screen kept as Ross likes it.
4. **Move between sessions fast.** Keyboard-first switching (↑/↓ in the list, ⌘1–⌘9 for "needs you", a ⌘K
   quick switcher by name, ⌘[ / ⌘] back/forward through recently opened sessions). A switch shows the
   session's terminal at once (it is already attached or cached, never a cold attach on click).
5. **Talk to it.** Type or speak into any session; both land instantly. Speech is hold-to-talk.
6. **Context, editable.** The Context page comes over and becomes an editor, not only a reader.
7. **Satellite, faster.** The delivery-pipeline table comes over and takes the same speed gains.
8. **iPhone second**, on the same Swift code and the same hub protocol.

## 3. Non-goals

- **World and the sign-in hub** (`world.mjs`, `hub/`, `hub*.mjs`, `public/world.js`, `index.html`) are not
  ported. After the switch (section 11) they are gone unless Ross restarts the Node panel for them.
- **No redesign** of the board, its groups, its columns or its vocabulary (needs you / working / waiting,
  planning / building / promoting, the epics).
- **No new orchestration.** Fleet starts, steers and reads sessions; it does not plan work or run agents of its
  own.
- **No public exposure.** Nothing listens on a public interface.
- **No Android, no web client** for the new app.

## 4. Anti-goals (fails even if it technically works)

- A session that is alive and shown as closed, hidden, or unreachable. This is the bug the port exists to end.
- Anything slower than today on any path Ross uses: launch, list, open a terminal, type.
- A bug already fixed in the Node code coming back because the port skipped its test.
- The app acting on a session on its own: closing, archiving, stopping, answering a permission, or retrying.
  Only Ross's click or key does that.
- Two sources of truth for state that can disagree on screen.
- A remedy that "fixes" Claude Code's records (rewriting `state.json`, killing pids) instead of reading them.

## 5. What exists today (to port)

| Today | What it does | Ported to |
|---|---|---|
| `fleet/machine.mjs` (1 609 lines) | Per machine: profiles (`~/.claude`, `~/.claude-<name>`) and Codex (`~/.codex`), accounts, plans, limits, sessions, state (`sessionState`, `jobOpen`, `hookState`, `atPrompt`), stage (`stageOf`), branch/worktree (`branchOf`), subagents (`agentsOf`), docs written (`readDocs`), launch / send / close / archive / adopt / resume / revive / fork / interject / tokens / defaults | `fleetd` (Rust), one per machine |
| `fleet/fleet.mjs` | All machines in one snapshot, SSH fan-out, epics, names, push of "needs you" | `fleetd` aggregation + the Mac app's model |
| `fleet/term.mjs`, `fleet/ws.mjs`, `fleet/ssh.mjs` | Live terminals: pty running `claude attach <id>`, over one WebSocket | `fleetd` owns the pty natively; app draws it natively |
| `fleet/held.mjs`, `/api/fleet/decide` | Permission card: allow / deny / answer / approve, hold while watched | `fleetd` + app |
| `fleet/hook.sh`, `fleet/install-hooks.mjs`, `fleet/statusline.mjs` | Hook events and status-line snapshots that feed state and limits | Kept as the feed; posted to the local `fleetd` instead of the panel |
| `fleet/push.mjs`, `pair-codes.mjs`, `qr.mjs`, `public-host.mjs` | Web Push, pairing, public host | Replaced by Tailscale identity + APNs (iPhone phase) |
| `fleet/context.mjs`, `public/context.html` | Context page: start scripts, session defaults, start-up hooks, iron rules (CLAUDE.md), Codex rules (AGENTS.md), memory index + notes, latest handoff, chai-fm rules; read-only | `fleetd` + app, **editable** |
| `fleet/satellite.mjs`, `public/satellite.html` | One repo's PRs through Agent → PR → Checks → Land → Promotion → TEST → Prod, from GitHub reads only, behind a read-only guard on `gh` | `fleetd` + app |
| `public/fleet.html` (2 241 lines) | The board: account meters, machine health, "What should it do?" launcher, Needs you, epic groups, session rows, Documents pane, terminal pane, phone layout | Native Mac app (SwiftUI + AppKit where needed) |
| `fleet.json`, `fleet-titles.json`, `fleet-labels.json`, `fleet-archived.json`, `fleet-seen.json` in `~/Library/Application Support/Agent Panel/` | Machines, account names, epics, renames, labels, archive, seen | Read as-is on first run; same files or a migrated store (builder's call) — never lose Ross's renames and archive |

The tests in `test/fleet-*.test.mjs`, `context.test.mjs`, `profiles.test.mjs`, `push*.test.mjs`,
`pair.test.mjs` are the acceptance list for behaviour: each case either becomes a Rust or Swift test, or is
listed in the port log as deliberately dropped with a reason.

## 6. The shape

```
 Mac                                         redbat (Linux, headless, always on) = THE HUB
 ┌──────────────────────────────┐            ┌────────────────────────────────────┐
 │ Fleet.app (Swift)            │  Tailscale │ fleetd --hub (Rust)                │
 │  board · terminal · Context  │◄──────────►│  merges every machine's sessions   │
 │  · Satellite · speech        │   board,   │  owns renames/labels/archive/seen, │
 │        ▲  fast path:         │   actions  │  epics, account names, Satellite   │
 │        │  Mac sessions +     │            │  sends "needs you" pushes          │
 │        │  Mac terminals      │            │  + its own machine's watcher       │
 │ fleetd (Rust, same binary)   │──deltas───►│                                    │
 └──────────────────────────────┘            └────────────────────────────────────┘
                                                  ▲
                                 iPhone (phase 2) ┘ connects to the hub only, over Tailscale
```

**The hub is on redbat (Ross).** It is always on, so the phone works whether the Mac is awake, asleep or off.

- Every machine runs `fleetd` as a **watcher** (section 6.1). redbat's also runs as the **hub**: it takes every
  watcher's deltas, merges them into the one board, and is the only writer of the shared state (renames,
  labels, archive, seen, epics, account names, Satellite's cache, push subscriptions). It sends the
  "needs you" pushes.
- **Clients talk to the hub** for the board and for every action. A terminal on a redbat session is a pty on
  redbat, streamed by the hub.
- **The Mac fast path (Claude's call).** A Mac round trip to redbat would break the ≤ 100 ms budget for Mac
  sessions, so the Mac app also connects to the Mac's own `fleetd` and takes Mac sessions' live state and Mac
  terminals straight from it, merged with the hub's board by session id. The hub stays the only source of the
  shared state. Both report the same state for a session because the same code decides it.
- **When the hub is unreachable** (redbat down, no network) the Mac app keeps working on the Mac's own
  sessions from the local `fleetd`, shows a plain "hub unreachable" banner, and shows renames and archive as
  last synced. A rename or archive made then is held and sent when the hub is back.
- The Mac's watcher pushes to the hub over Tailscale; if the Mac is asleep its sessions show as
  *unreachable* (section 7), never as gone.

### 6.1 `fleetd` (Rust), one per machine

- **Watches instead of polls.** FSEvents on macOS, inotify on Linux, over each profile's jobs, session
  transcripts, `fleet-status/`, the hook journal and `~/.codex/sessions`. Transcripts are tailed
  incrementally from the last read offset, never re-read from a fixed tail. A slow reconcile pass (for
  example every 30–60 s) catches anything a watch missed; the exact interval is the builder's call.
- **Holds the model in memory** and streams **deltas** to subscribers: a full snapshot on connect, then only
  what changed, each change carrying a sequence number so a client that reconnects asks for "since N".
- **Owns the session state decision** (section 7). The app never re-derives state.
- **Owns terminals.** A pty per attached session running `claude attach <id>` (Codex: its equivalent), kept
  warm for recently opened sessions so switching is instant; scrollback buffered so a reopened terminal
  repaints at once. Detaching never stops the session (as today).
- **Runs the actions** that `machine.mjs` exports today (launch, send, interject, close, archive, adopt,
  resume, revive, fork, decide), same semantics, same guards.
- **Hooks feed it directly.** `hook.sh` posts to the local `fleetd` (Unix socket or loopback HTTP). At the
  switch it is pointed at `fleetd` alone.
- **Installed on each machine.** This is a change from today's "nothing installed on the remote". It is a
  single static binary plus a user service (launchd on the Mac, systemd on redbat, running as the `claude`
  user there, as the current worker does via `sudo -n -u claude`).
- **Satellite reads** move here too: GitHub via GraphQL in one query, conditional requests (ETag /
  If-None-Match) so unchanged reads cost nothing against the rate limit, refreshed often enough to feel
  live (the builder picks the interval; today's is 60 s). The read-only guard stays: nothing in Satellite
  can merge, close, comment or dispatch.

### 6.2 Fleet.app (Swift), Mac first

- **Same board, standard Mac UI.** A sidebar list of sessions (Needs you pinned on top, then the epic groups),
  a toolbar with the launcher, account meters and machine health in the toolbar or a header strip, the
  terminal as the main pane. Native controls, system font sizes the user can change, light and dark.
- **Terminal drawn natively** (a native terminal view, for example SwiftTerm; the builder picks), fed the pty
  bytes from `fleetd`. It can take the whole window.
- **Keyboard first** (goal 4). Every action on a row has a shortcut and a menu item.
- **Speech:** hold a key (or the mic button) to dictate into the focused session's input, using Apple's
  on-device speech recognition; the text appears in the input before sending, so Ross sees what goes in.
- **Launcher presets** come from the start scripts Context already reads (`claude-max`, `gpt`,
  `claude-assist`, …): one click starts a session exactly as the alias would, so it runs the repo's
  session-start ritual and comes up oriented. "What should it do?" stays: account, machine, folder, task,
  Fire.
- **Documents pane** (what a session wrote) and the **permission card** come over.
- **Notifications** for "needs you" use the Mac's own, with the Dock badge, as `app/main.swift` does today.

### 6.3 iPhone (phase 2)

The same SwiftUI views where they fit, the same protocol to the hub on redbat over Tailscale, push via APNs
for "needs you", speech the same way. The phone layout today (two sliding screens, back gesture) is the
reference.

### 6.4 Transport and trust

- Tailscale only. `fleetd` listens on loopback and on its Tailscale address, never on a public interface.
- Who may connect is decided by Tailscale identity (the tailnet's WhoIs for the peer), allow-listed to Ross's
  own devices. The pairing key and one-time pair codes go away for the new app. (Builder: if WhoIs is not
  workable, fall back to a per-device key exchanged once; say so in the plan.)
- The ssh path (`redbat-ts` with `sudo -n -u claude`) is used only to install and update `fleetd` on redbat.

## 7. Session state: one decision, always explained

`fleetd` decides every session's state from every source it has and attaches the **reason**. The app shows the
state and, on hover or in Details, the reason ("live pid 4182, last turn 40 min ago", "Claude Code says
stopped; pid gone since 01:12").

| State | Meaning |
|---|---|
| needs you | a permission, a question or a plan is waiting on Ross |
| working | a turn is running |
| waiting | open and idle at the prompt |
| done | Claude Code / Codex says it finished; the row stays until archived or aged out of the visible window |
| ended | the process is gone, and the record says why (stopped, failed, killed); the row says it, and offers resume / respawn |
| unreachable | its machine cannot be read right now; the row keeps its last known state, greyed, never disappears |

Rules:

- **A live pid is never "closed".** If the record says stopped and a pid is alive (the respawn case), the
  session is *waiting* and the reason says "respawned".
- **Nothing leaves the board on its own.** The current "N closed" fold becomes **Archive**: only Ross's
  archive puts a row there. (Ross) He can archive and unarchive any row, in any state, whenever he likes, one at
  a time or several selected at once; archiving only hides the row and never stops or touches the session. Ended and done rows stay in their group with their reason. A view filter may hide
  done/ended older than a window Ross sets (default: show the last 48 h), and the count of hidden rows is
  always on screen.
- The stage (planning / building / promoting / testing) and subagents carry over from `stageOf` and `agentsOf`
  unchanged.

## 8. Context, editable

The Context documents today (from `DOCS` in `fleet/context.mjs`): start scripts (`~/.zshrc` functions),
session defaults (each profile's `settings.json` / `.claude.json` keys, every machine), start-up hooks, the
project's `CLAUDE.md`, `AGENTS.md`, the memory index and its notes, the latest `HANDOFF_*.md`, chai-fm's
`CLAUDE.md`.

- Every document that is a file opens in a native editor (Markdown with a live preview beside it) and saves
  back to that file. A note in the memory folder can be created and edited the same way.
- **Structured documents are edited as structure, not text.** Start scripts edit as a form (name, model,
  effort, permissions, folder) and write back only that function in `~/.zshrc`, leaving the rest of the file
  byte-for-byte. Session defaults edit only the fixed set of keys `defaults()` reads today, on the machine
  they belong to.
- **Safe saves.** Each save checks the file has not changed since it was opened (hash); if it has, show both
  and let Ross choose. Each save keeps the previous version (a timestamped copy or a local history the builder
  picks). A file inside a git repo shows its uncommitted diff; Fleet never commits.
- **Live.** Context is watched like everything else: an edit made in another editor or by a session shows at
  once.
- Context stays scoped to the paths it reads today plus whatever Ross adds; it never reads or writes
  outside them.

## 9. Speed budgets (acceptance)

Measured on the Mac on a normal day (not under the swap thrash of 2026-10-31):

| Path | Budget |
|---|---|
| App launch to a full board (from a warm `fleetd`) | ≤ 300 ms |
| A state change on the Mac to on screen | ≤ 100 ms |
| A state change on redbat to on screen (over Tailscale) | ≤ 300 ms |
| Switch to a recently opened session's terminal | ≤ 50 ms |
| Open a session's terminal cold | ≤ 500 ms Mac, ≤ 1 s redbat |
| Keystroke echo in the terminal, Mac session | indistinguishable from Terminal.app |
| Satellite table on open | instant from cache, fresh within its interval |
| `fleetd` idle cost | ≈ 0 % CPU, a small, stable memory footprint |

The builder sets up the measurement harness so these are checked, not felt.

## 10. Accounts and machines

Today's `fleet.json` names: `rar` (`~/.claude`), `aol` (`~/.claude-aol`),
`gmail` (`~/.claude-gmail`), `Astra` (Codex, `~/.codex`), plus `muse` (shown on the board today). A new
**Claude Enterprise** login is coming and must show up as its own account the moment its profile folder
exists, with no code change (profiles are discovered as `~/.claude` and `~/.claude-<name>`, as now). Machines:
the Mac and redbat; more by adding to the config.

## 11. The switch

- **One switch, no overlap.** (Ross) When the native app and `fleetd` pass the budgets in section 9 and the
  acceptance list in section 5, they replace the Node panel for Fleet, Context and Satellite in one move. The
  Node panel is stopped; it is not kept running beside the new app.
- On first run the app imports Ross's renames, labels, archive and seen markers from the Node panel's files;
  from then on `fleetd` owns that state in its own store.
- `hook.sh` is pointed at `fleetd` as part of the switch.
- Rollback is restarting the Node panel and pointing `hook.sh` back; nothing in the switch touches Claude
  Code's or Codex's own records, so rollback loses nothing.
- World and the sign-in hub are not ported; after the switch they are gone unless Ross restarts the Node
  panel for them.

## 12. Decided

- Port the existing behaviour; the Node code and its tests are the spec. (Ross)
- The hub on redbat, always on; the Mac app keeps a local fast path. (Ross; the fast path is Claude's call)
- Rust for the `fleetd` that runs on each machine, Swift for everything you look at. (Claude's recommendation;
  Ross agreed.)
- Native Mac app first, then iPhone. (Ross)
- Port Fleet, Context and Satellite; Context becomes editable. World and the hub are out. (Ross)
- The terminal screen stays; the rest of the UI can become standard Mac. (Ross)
- Typing and speaking into a session, both instant. (Ross)
- Tailscale for everything between devices. (Ross, earlier in this conversation)
- A full switch, no side-by-side period. (Ross)
- Archive and unarchive freely, by hand. (Ross)
- Launcher presets are the existing start scripts and run the session-start ritual. (Ross)

## 13. Left to the builder

Rust crates (async runtime, file watching, pty, wire format); the Swift terminal view; the wire protocol
details (framing, deltas, versioning); where app state is stored after import; reconcile intervals; the
measurement harness; how to package and update `fleetd` on each machine; the repo layout.

## 14. Settled after the builder check (Claude's calls, for Ross to check)

- **Hooks never slow a turn.** `hook.sh` posts to each listener fire-and-forget with a short timeout; a
  listener that is down is skipped silently. A Claude turn never waits on Fleet.
- **Codex terminals are not kept warm.** A running `codex resume` would take the thread's one-writer slot
  (`codexOpen`), so Codex sessions attach on open and release on close, with the last scrollback cached so
  they repaint at once. Claude Code `claude attach` terminals and tmux sessions are kept warm as section 6.1
  says. The "instant switch" budget applies to Claude Code sessions; Codex gets the cold-open budget.
- **No guessed reasons.** Where no record says why a session ended (common for Codex and for old rows), the
  row says "ended, reason unknown". It never invents one.
- **Permission holds do not time out** (as today). Any connected client may answer; the first answer wins and
  every other client shows it as answered, by which device, at once. A second answer is refused, not applied.

## 15. Open for Ross

- **Apple Developer account.** Not needed to build and run the Mac app on Ross's own Mac. Needed before the
  iPhone phase (installs that last, APNs push) and for a notarized Mac app. APNs is sent from the hub on
  redbat outbound to Apple, so nothing listens publicly. Does not block the Mac phase.
