# Mac App (board + terminals) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native Mac app, `Fleet.app`, that shows the same board as today's Fleet page, live from `fleetd`, with each session's real terminal one keystroke away. It opens to the last board instantly and switches between warm terminals in one frame.

**Architecture:** `fleetd` gains the board's metadata, all read-only:
- titles, epics, labels and archive from the Node panel's files;
- account names and plan limits;
- machine health.

`FleetKit` is a Swift package with no UI. It decodes the postcard wire (pinned by plan 1's golden bytes), keeps the board model (snapshot, deltas, run ids, and resync on a gap) and runs both connections. `FleetApp` is SwiftUI + AppKit: a sidebar board and a terminal pane drawn with SwiftTerm. Actions (launch, close, archive, permission answers, speech) are plan 4. Typing into a terminal works here, because it is the terminal.

**Tech Stack:**
- Rust: the existing `fleetd`.
- Swift 6.3 / Xcode 26.6 (installed), SwiftPM, macOS 15+.
- SwiftTerm (MIT) is the only third-party package.
- The app bundle is built by a script, like `app/build.sh`, with an ad-hoc signature. No Xcode project.

**Spec:** `docs/superpowers/specs/2026-11-06-fleet-native-port-design.md` §6.2, §7, §9 (rules 7–8, budgets), §10. Earlier plans:
- `…-01-fleetd-core.md` (Rulings R1–R25)
- `…-02-terminals.md` (Rulings P2-R1–R6)

Wire contract: `native/PROTOCOL.md`.

## Global Constraints

- **Same board, standard Mac UI.**
  - Sessions are grouped like `public/fleet.html`: "Needs you" first, oldest wait first; then the epics in `fleet.json` order; then groups the user made; then Other.
  - State words come from `fleet.html` (`WORD`): needs you · working · at limit · waiting. "closed" becomes **done** or **ended**, with the reason shown on hover and in the header.
  - Row order inside a group follows `fleet.html` `ORDER`, then most recent first.
  - The quiet monospace look is kept for rows: SF Mono / system monospace, small type, no gradients or shadows. Window chrome, sidebar, toolbar and menus are standard AppKit/SwiftUI.
- **Nothing leaves the board on its own** (spec §7).
  - Archived rows (from `fleet-archived.json`) are not shown.
  - Done or ended rows older than 48 h are folded behind an always-visible "N older" count per group. All other rows show.
- **Read-only.** `fleetd` reads `fleet.json`, `fleet-titles.json`, `fleet-labels.json` and `fleet-archived.json` from `~/Library/Application Support/Agent Panel/` and never writes them. The Node panel owns them until the switch (plan 8).
- **Speed budgets** (spec §9.3), measured by the app's bench mode and gated like plan 1's bench:
  - launch to the last board on screen ≤ 50 ms
  - launch to the live board ≤ 200 ms (warm `fleetd`)
  - switch to a warm terminal ≤ 16 ms
  - a delta to on screen ≤ 16 ms after it arrives
  - app idle ≈ 0 % CPU; ≤ 150 MB resident with the board open
- **Wire.** The Swift decoder must pass every golden in `native/crates/fleet-model/tests/golden/` (decode == expected value; encode == golden bytes for client frames). Any field added to `Session` or a new frame is a PROTO bump, and its goldens are added in the same commit.
- **Reconnect.** Follow PROTOCOL.md exactly: subscribe with `run` and `since`, treat a seq gap as resync, accept a Snapshot at any time, and back off from 50 ms up to 2 s on reconnect. Show "live", "reconnecting…" or "fleetd not running" in the toolbar.
- **Test setup.** No Node and no web view in the app. The app never starts `fleetd` itself; it tells the user how to install it if the socket is absent.
- **Public repo.** Fixtures are synthetic only.

## Review Focus

1. **A seq gap or a `run` change mid-session.** The board must resync from a fresh Snapshot, with no duplicate or ghost rows. Test in Task 3.
2. **`fleetd` restarting while a terminal is open.** The pane shows "reconnecting…", then reattaches the same session (warm on the daemon side), without the window freezing. Test in Task 6.
3. **A burst of 500 deltas in 1 s.** The UI coalesces to at most one render per frame and never falls behind. Test in Task 5.
4. **A session whose title, epic or label changes** while it is selected keeps its selection and terminal. Test in Task 5.
5. **Missing or corrupt Node-panel files** (`fleet.json` absent, `fleet-titles.json` half-written). `fleetd` serves the board without that metadata and never crashes. The app shows sessions under "Other" with their raw names. Test in Task 1.

---

### Task 1: `fleetd` board metadata (titles, epics, labels, archive)

**Files:**
- Create: `native/crates/fleet-read/src/panelmeta.rs`
- Modify: `native/crates/fleet-model/src/types.rs` (`Session` gains `title: Option<String>`, `epic: Option<String>`, `archived: bool`, `account: Option<String>`), `native/crates/fleetd/src/sessions.rs`, `native/crates/fleetd/src/watch.rs`, `native/crates/fleet-model/src/wire.rs` (PROTO 4; new frame `Meta { epics: Vec<String>, groups: Vec<String>, accounts: Vec<Account> }`), the goldens, `native/PROTOCOL.md`
- Test: `native/crates/fleet-read/tests/panelmeta.rs`, `native/crates/fleetd/tests/store.rs`

**Interfaces:**
- Produces:
  - `pub struct PanelMeta { self_name: Option<String>, names: HashMap<String,String>, epics: Vec<Epic>, titles: HashMap<String, Titled>, labels: HashMap<String, Label>, archived: HashSet<String> }`
  - `pub fn read_panel_meta(dir: &Path) -> PanelMeta`. A missing or corrupt file counts as empty, as in Review Focus 5.
  - The key is `"<machine>:<sessionId>"`, as `fleet.mjs` uses.
  - `title`/`epic` follow `fleet.mjs` line ~324: `label.title ?? own ?? titled.title ?? name`, and `label.group ?? titled.epic`.
  - `account` comes from `fleet.json` `names["claude:<email>"]` or `names["codex:<email>"]`, else the email's local part.
  - `pub struct Account { name: String, engine: Engine, profile: String, limits: Vec<Limit> }`, where `Limit { window: String, used_pct: u8, resets_at: Option<i64> }` comes from the status-line snapshots (plan 1 `statusfile.rs`) and Codex token_count events. Port `fleet.mjs`/`machine.mjs` limits: `claudeProfile` and `codexAccount` limits.
  - The watcher watches that directory (FSEvents) and re-sends `Meta` and the affected Sessions on change.

- [ ] **Step 1:** Write the tests.
  - (a) Over a fixture dir, the title/epic precedence matches `fleet.mjs` for label, own, titled and name. Build a golden by running the Node rule on the same fixture, with a small exported helper if needed (R2: export only).
  - (b) Archived keys are flagged.
  - (c) **Review Focus 5:** an absent dir, a truncated `fleet-titles.json` and non-JSON `fleet.json` each give an empty PanelMeta and no panic.
  - (d) Account naming.
  - (e) Changing `fleet-labels.json` produces a Delta for that session within 50 ms.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement it, bump PROTO to 4, and add the goldens for the new fields and `Meta`.
- [ ] **Step 4:** Run them and `native/check.sh`. Expected: PASS. Parity ignores the new fields.
- [ ] **Step 5:** Commit: `fleetd: board metadata — titles, epics, labels, archive, accounts (proto 4)`.

### Task 2: `fleetd` machine health

**Files:**
- Create: `native/crates/fleetd/src/health.rs`
- Modify: `wire.rs` (a `Health { cpu_pct: u8, mem_pct: u8, disk_pct: u8, dev: u16, db: u16 }` field in `Meta`, or its own frame; the builder chooses, PROTO stays 4 if done within Task 1's bump, else 5), the goldens, PROTOCOL.md
- Test: `native/crates/fleetd/tests/health.rs`

**Interfaces:**
- Produces machine health the way `machine.mjs` `machineStatus` computes it: memory from `vm_stat`/sysctl on macOS (no subprocess per sample if sysctl serves), disk from `statfs("/")`, CPU from `host_statistics` load. `dev` counts listening dev servers and `db` counts Supabase/Postgres containers, both ported as Node counts them; the builder reads `machineStatus` and documents any deviation.
- Sampling: every 5 s while at least one Board client is connected, and never while none is (idle stays idle).

- [ ] **Step 1:** Write the tests.
  - (a) Values are in 0–100 and plausible against `vm_stat`/`df` read in the test.
  - (b) No sampling happens with zero clients, measured as CPU time over 30 s with no client ≤ the idle budget.
  - (c) A Health update reaches a connected client within 6 s.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run them. Expected: PASS.
- [ ] **Step 5:** Commit: `fleetd: machine health`.

### Task 3: FleetKit — wire decoding and the board model

**Files:**
- Create: `native/app/Package.swift` (products: library `FleetKit`, executable `FleetApp`; dependency SwiftTerm, pinned to an exact version tag at least two weeks old), `native/app/Sources/FleetKit/{Postcard.swift,Frames.swift,Board.swift}`
- Test: `native/app/Tests/FleetKitTests/{GoldenTests.swift,BoardTests.swift}`

**Interfaces:**
- Produces:
  - `struct PostcardReader` (varint, zigzag, strings, Option, Vec, enum index) and `PostcardWriter`.
  - `enum Frame` (every server and client frame of the current PROTO), with `static func decode(_ bytes: Data) throws -> Frame` and `func encode() -> Data`.
  - `@MainActor final class Board: ObservableObject` with:
    - `func apply(_ frame: Frame) -> BoardChange`, where `BoardChange` is `.applied` or `.needsResync`;
    - `var sessions: [SessionKey: Session]`, `meta`, `health`, `run`, `seq`;
    - `func groups(now: Date) -> [Group]`, the grouping and order from Global Constraints, folding old done/ended rows.

- [ ] **Step 1:** Write the tests.
  - (a) Every golden file decodes to the expected value. Read the hex files from `../crates/fleet-model/tests/golden/` by relative path. Client frames encode to the golden bytes.
  - (b) **Review Focus 1:** Snapshot, then Deltas, then a Delta with a seq gap gives `.needsResync`. A Snapshot with a new run replaces everything, with no ghosts.
  - (c) The grouping order, Needs you ordering, folding (> 48 h done/ended) and archived-hidden checks against a fixture board. Write the expected groups from reading `fleet.html`'s rules, and cite its lines in the test.
- [ ] **Step 2:** Run `cd native/app && swift test`. Expected: FAIL.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run `swift test`. Expected: PASS.
- [ ] **Step 5:** Commit: `FleetKit: postcard wire and the board model`.

### Task 4: FleetKit — connections

**Files:**
- Create: `native/app/Sources/FleetKit/{Socket.swift,BoardClient.swift,TermClient.swift}`
- Test: `native/app/Tests/FleetKitTests/ClientTests.swift`, against a real `fleetd serve` over a fixture home, started by the test with `FLEETD_HOME`/`FLEETD_SOCK` (reuse plan 1's bench fixture builder through a small CLI flag, or a committed fixture).

**Interfaces:**
- Produces:
  - `actor BoardClient`:
    - `start(socket: URL)` drives the Board model on the main actor.
    - It reconnects with backoff from 50 ms up to 2 s.
    - It sends Hello with run and since.
    - It publishes `status: .live / .reconnecting / .absent`.
  - `actor TermClient`, one Terminals connection per app:
    - `open(ref, target, cols, rows)`, `input(ref, Data)` (splits into ≤ 64 KiB frames), `size`, `close`;
    - per-ref output streams (`AsyncStream<Data>`) and end events;
    - on reconnect, reopens the refs that were open.
  - Uses `Network.framework` `NWConnection` over a Unix endpoint, or raw sockets with a DispatchSource; the builder chooses for latency.

- [ ] **Step 1:** Write the tests.
  - (a) Connect and get the Snapshot. A hook write in the fixture arrives as a Delta in < 20 ms.
  - (b) Kill and restart the fixture `fleetd` (by its pid). The client reconnects, gets a new run, and resyncs.
  - (c) The term client opens the stand-in, gets its banner, types, and sees the echo. 200 KiB of input arrives intact, split into ≤ 64 KiB frames.
  - (d) An absent socket gives `.absent` status and no busy loop: CPU time over 5 s is ≈ 0.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run them. Expected: PASS.
- [ ] **Step 5:** Commit: `FleetKit: board and terminal connections`.

### Task 5: The app — board window

**Files:**
- Create: `native/app/Sources/FleetApp/{FleetApp.swift,BoardView.swift,RowView.swift,Toolbar.swift,BoardCache.swift}`, `native/app/build.sh` (bundle at `~/Applications/Fleet.app`, bundle id `dev.fleet.app`, ad-hoc codesign, icon reused from `app/icon.svg`)
- Test: `native/app/Tests/FleetKitTests/CoalesceTests.swift`; the bench mode, run by `native/app/bench.sh`

**Interfaces:**
- Produces:
  - A `NavigationSplitView`:
    - **Sidebar:** the groups. Each row shows a state dot and word, title, machine, account, age, effort and context. Hover shows the reason.
    - **Toolbar:** account meters from `Meta.accounts`, machine health, and the connection status.
  - The Dock badge shows the Needs-you count.
  - `BoardCache` writes the board to `~/Library/Application Support/Fleet/board.cache` at most once per second, and the app reads it at launch before connecting. Rows from the cache are marked "from <time>" until the first Snapshot.
  - Rendering is coalesced to at most one update per display frame (CADisplayLink/CVDisplayLink, or a main-actor throttle).
  - `FleetApp --bench <out.json>` launches, measures the Global-Constraint budgets with `os_signpost` and timestamps, writes them, and quits.

- [ ] **Step 1:** Write the tests.
  - (a) **Review Focus 3:** feeding 500 deltas in 1 s to the coalescer produces ≤ 61 renders, and the final state equals the last delta's.
  - (b) **Review Focus 4:** a selected session whose title and epic change stays selected (selection by SessionKey).
  - (c) BoardCache round-trips and survives a truncated file.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement it. Then `native/app/build.sh`, and launch `~/Applications/Fleet.app` against the live `fleetd`: the board shows Ross's sessions grouped as the Fleet page groups them. Take a screenshot (`screencapture -l <windowid>`) into the report folder; it is gitignored, because it holds real session names.
- [ ] **Step 4:** Run the tests and `native/app/bench.sh` (median of 3). Expected: PASS; last board ≤ 50 ms, live ≤ 200 ms.
- [ ] **Step 5:** Commit: `FleetApp: the board window`.

### Task 6: The app — terminal pane and keyboard

**Files:**
- Create: `native/app/Sources/FleetApp/{TerminalPane.swift,Switcher.swift,KeyCommands.swift}`
- Test: `native/app/Tests/FleetKitTests/HistoryTests.swift`; bench additions

**Interfaces:**
- Produces:
  - **Layout (Ruling P3-R8; Ross: "the terminal pane is the key"):**
    - The window becomes a narrow left rail, about 320–420 pt, that the user can resize and that remembers its width.
    - The rail replaces the wide table rows with compact two-line rows. Line 1 is the state dot and the title. Line 2 is the state word, age and account, truncated, never overlapping.
    - The terminal is the main area and fills the rest of the window.
  - The main area: a slim header strip (title, state word, reason) above a SwiftTerm `TerminalView` fed by `TermClient`.
  - Keys go to the pty. The pty is resized on view resize, debounced to one per frame.
  - The pane can take the whole window (⌘⇧F), as "Expand" does today.
  - App-side warm views: the last 6 opened Claude terminals keep their `TerminalView` alive, so a switch is a view swap and the daemon's warm pool is the second layer. Codex terminals close when left, per plan 2.
  - Keyboard:
    - ↑/↓ in the sidebar;
    - ⌘1–⌘9 jump to the Nth "Needs you";
    - ⌘K opens the quick switcher: fuzzy match on title, name and job, filtering within one frame per keystroke;
    - ⌘[ and ⌘] go back and forward through recently opened sessions;
    - Esc returns focus to the sidebar.
  - Each key is also in a menu with its shortcut.
  - **Review Focus 2:** when `fleetd` restarts, the pane shows "reconnecting…" and reattaches its ref on reconnect.

- [ ] **Step 1:** Write the tests.
  - (a) The back/forward history model: push, back, forward, and dedupe of consecutive entries.
  - (b) The quick-switcher ranking on a fixture: exact prefix > word start > subsequence.
  - (c) A bench-mode switch between two warm terminals ≤ 16 ms (median of 3).
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run the tests and the bench. Expected: PASS.
- [ ] **Step 5:** Live check, read-only toward sessions:
  - Rebuild and launch `Fleet.app`.
  - Open one idle background session's terminal, and confirm it draws.
  - Switch between it and a second idle one with ⌘[ and ⌘], and time it (the bench log).
  - Type nothing into either.
  - Quit, and confirm with `fleetd snapshot --json` that both sessions' states are unchanged.
  - Don't touch the Node panel on 4747.
- [ ] **Step 6:** Commit: `FleetApp: terminal pane, switcher, keyboard`.
