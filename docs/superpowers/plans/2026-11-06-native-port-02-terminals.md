# Terminals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `fleetd` owns the pseudo-terminal for every Mac session you open. The app gets the live screen over the socket, warm terminals switch in one frame, and a `fleetd attach` command lets Ross use it in any terminal before the Mac app exists.

**Architecture:** Clients open a second kind of connection on the same socket (`Hello { role: Terminals }`). Over that connection they open, type into, size and close terminals by a number of their own (`ref`), as `fleet/term.mjs` does over its WebSocket. `fleetd` spawns `claude attach <job>` / `codex resume --no-daemon <id>` / `claude` in a folder (trust prompt) on a pty of its own, with no Python relay. Claude Code terminals stay attached for a while after the client lets go ("warm"), with their recent output kept, so reopening is a redraw. Board deltas and terminal bytes use separate connections, so a busy terminal never delays the board.

**Tech Stack:** Rust (existing workspace). `rustix` or `nix` for openpty, setsid and TIOCSCTTY (builder's choice; no `portable-pty`, to stay lean). tokio for the socket side. No new runtime deps on the client.

**Spec:** `docs/superpowers/specs/2026-11-06-fleet-native-port-design.md` §6.1 ("Owns terminals"), §9 rules 7–8, §9.3. Plan 1: `docs/superpowers/plans/2026-11-06-native-port-01-fleetd-core.md` (its Global Constraints and Rulings R1–R22 still bind).

## Global Constraints

- Mac sessions only in this plan. redbat terminals come with the hub (plan 5), as a pty on redbat streamed by the hub. No ssh on any path here.
- Port `fleet/term.mjs` `command()` (local branch) exactly. That means the commands, the argv, the env (strip `CLAUDE*`, `FLEET_*`, `TMUX`, `TMUX_PANE`; set `TERM=xterm-256color`, `COLORTERM=truecolor`, `COLORFGBG=15;0`, `LANG` default `en_US.UTF-8`), `CLAUDE_CONFIG_DIR` for non-default profiles, `CODEX_HOME` for non-`codex` Codex profiles, and the validation regexes (`PROFILE`, `JOB`, `DIR`, `UUID`). Keep `--no-daemon` for Codex and its reason.
- Limits from `term.mjs`: at most 24 terminals in all (`MAX`) and 8 per connection (`PER_PAGE`). cols are clamped to 20–400 (default 100) and rows to 5–200 (default 30).
- One writer per Codex thread: a Codex thread already open in a Fleet terminal, or held by a live Codex process (the plan-1 open set), is refused with `term.mjs`'s words: "It is open in another Fleet terminal already." or the busy reason.
- Closing a terminal ends only the attach. It never stops, archives or answers the session. A terminal is stopped with SIGHUP to its process group, then SIGKILL after 1.5 s, as the PTY relay does.
- Warm pool: only Claude Code `claude attach` terminals are kept warm. Codex and trust-folder terminals end when released (spec §14 Codex ruling). Keep at most 6 warm, each for 10 minutes after release; evict the oldest first. Each keeps a 1 MiB ring of recent output.
- Backpressure: if a terminal connection can't keep up (more than 1 MiB unsent), stop reading every pty of that connection until it drains, so the programs block on output. Never drop bytes and never buffer without bound.
- Wire: `PROTO` becomes 3 (plan 1's final fixes made it 2, adding `run`). `Hello` gains `role: Role { Board, Terminals }`. An older Hello is refused with an `Error` naming proto 3. Board behaviour is unchanged.
- Budgets (added to `native/bench/budgets.json`, same ratchet):
  - keystroke echo through fleetd vs a direct pty, `cat` as the program: added p95 ≤ 1 ms
  - reopen a warm terminal to the first repaint byte: p95 ≤ 16 ms
  - open a cold terminal (stand-in program) to the first byte: p95 ≤ 150 ms
- Read-only invariant from plan 1 holds. Typing into a session is the user's own input, relayed, and the only write `fleetd` makes to any session.
- The repo is public: synthetic fixtures only.

## Review Focus

1. **A warm terminal whose session ended** (job gone, `claude attach` exited) must not be offered for reopen. The reopen gets a clean end with the program's last output, not a hang. Test in Task 3.
2. **The client disconnecting mid-burst** while a pty floods output. No thread or fd leaks, the pty is released to the warm pool or stopped, and other connections are unaffected. Test in Task 4.
3. **Two clients opening the same Claude session.** Both may watch; typing from either reaches it. The second open attaches to the same warm pty rather than starting a second `claude attach`, and the ring is replayed to the newcomer. Test in Task 3.
4. **Resize storms** (a window drag sends 60 sizes a second). Only the latest size per terminal is applied, with no backlog. Test in Task 3.
5. **Non-UTF-8 and partial escape sequences** split across frames pass through byte for byte. `fleetd` never decodes terminal output. Test in Task 2.

---

### Task 1: Wire for terminals, and the session's attach target

**Files:**
- Modify: `native/crates/fleet-model/src/wire.rs`, `native/crates/fleet-model/src/types.rs`
- Modify: `native/crates/fleetd/src/serve.rs` (proto check only), `native/crates/fleetd/src/sessions.rs` (fill the new field)
- Test: `native/crates/fleet-model/tests/wire.rs`, `native/crates/fleetd/tests/serve.rs`

**Interfaces:**
- Produces:
  - `Hello { proto: u16, since: Option<u64>, run: Option<u64>, role: Role }`, with `enum Role { Board, Terminals }` and `PROTO = 3`. Update the golden frame bytes and `native/PROTOCOL.md` (from plan 1's final fixes).
  - New `Frame` variants:
    - `TermOpen { r#ref: u32, target: AttachTarget, cols: u16, rows: u16 }` (client → server)
    - `TermIn { r#ref: u32, data: Vec<u8> }` (client → server, ≤ 64 KiB)
    - `TermSize { r#ref: u32, cols: u16, rows: u16 }` (client → server)
    - `TermClose { r#ref: u32 }` (client → server)
    - `TermOut { r#ref: u32, data: Vec<u8> }` (server → client)
    - `TermEnd { r#ref: u32, error: Option<String> }` (server → client)
  - `enum AttachTarget { Claude { profile: String, job: String }, Codex { profile: String, session_id: String, cwd: String }, Trust { profile: String, dir: String } }`.
  - `Session` gains `job_id: Option<String>` (the roster row's 8-hex `id`, set for background jobs), so a client can build `AttachTarget::Claude` from a board row.

- [ ] **Step 0 (carried from plan 1, Ruling R25):** in `watch.rs` (~2016-2033), a transcript write to a NeedsYou session nudges the roster only when the held roster row's `status` is not `busy`. Add a store test: during a granted long tool run that writes progress lines every 200 ms for 10 s, the roster CLI runs at most twice. Fix the misleading error for a malformed current-proto Hello (`serve.rs` ~1508): it says `expected Hello`.
- [ ] **Step 1:** Write the tests:
  - (a) Every new frame round-trips through `write_frame` and `read_frame`.
  - (b) A proto-2 Hello gets `Error` containing "proto 3".
  - (c) A board connection still gets Snapshot and Delta exactly as before.
  - (d) A `TermIn` over 64 KiB is refused by the reader limit.
  - (e) A background job's Session has `job_id` equal to its roster id; an interactive session's is None.
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3:** Implement. The `--json` output DTO gains `job_id`, and parity ignores it.
- [ ] **Step 4:** Run the tests, `native/check.sh` included. Expected: PASS.
- [ ] **Step 5:** Commit: `fleet-model: proto 3 — terminal frames, roles, job_id`.

### Task 2: The pty and the commands

**Files:**
- Create: `native/crates/fleetd/src/pty.rs`, `native/crates/fleetd/src/attach_cmd.rs`
- Test: `native/crates/fleetd/tests/pty.rs`, `native/crates/fleetd/tests/attach_cmd.rs`

**Interfaces:**
- Produces:
  - `pub fn command(target: &AttachTarget, home: &Path) -> Option<Cmd>`, with `pub struct Cmd { argv: Vec<OsString>, env: Vec<(OsString, OsString)>, cwd: PathBuf }`. This ports `term.mjs` `command()` local branch; it returns None for a bad target or a missing folder.
  - `pub struct Pty`, with:
    - `fn spawn(cmd: &Cmd, cols: u16, rows: u16) -> io::Result<Pty>`. Setup is openpty, then a child in its own session (`setsid`) with the pty as its controlling terminal.
    - `fn reader(&self) -> OwnedFd`
    - `fn write(&self, data: &[u8]) -> io::Result<()>`
    - `fn resize(&self, cols: u16, rows: u16)`
    - `fn stop(&self)`. It sends SIGHUP to the group, waits 1.5 s, then sends SIGKILL.
    - `fn wait(&self) -> Option<ExitStatus>`

- [ ] **Step 1:** Write the tests. `attach_cmd`:
  - (a) For each target kind, the argv, env and cwd match `term.mjs` for the same input. Build a small golden with a Node script calling `createTerms({config}).command(...)` for the local machine, and compare.
  - (b) Bad profile, job, dir and uuid values give None.
  - (c) A missing folder gives None.
- [ ] **Step 2:** Write the tests. `pty`:
  - (a) Spawn `/bin/cat`, write `hello\n`, and read `hello` back. Bytes are echoed raw.
  - (b) Resize, then `stty size` inside the pty prints the new rows and cols.
  - (c) `stop()` kills a child that ignores SIGHUP, and its grandchild, within 2 s.
  - (d) **Review Focus 5:** the bytes `\xff\xfe\x1b[3` and `1m` written in two pieces come out unchanged.
  - (e) No fds are leaked into the child: it sees only 0, 1 and 2 (check `/dev/fd`).
- [ ] **Step 3:** Run the tests. Expected: FAIL.
- [ ] **Step 4:** Implement.
- [ ] **Step 5:** Run the tests. Expected: PASS.
- [ ] **Step 6:** Commit: `fleetd: pty and attach commands (port of term.mjs)`.

### Task 3: The terminal manager and the warm pool

**Files:**
- Create: `native/crates/fleetd/src/terms.rs`
- Test: `native/crates/fleetd/tests/terms.rs`

**Interfaces:**
- Consumes: Tasks 1–2; the plan-1 store's Codex open set (the one-writer rule).
- Produces: `pub struct Terms`, with:
  - `fn open(&self, conn: ConnId, r#ref: u32, target: AttachTarget, cols: u16, rows: u16, out: TermSink) -> Result<(), String>`. Errors use `term.mjs`'s words.
  - `fn input(&self, conn: ConnId, r#ref: u32, data: &[u8])`
  - `fn size(&self, conn: ConnId, r#ref: u32, cols: u16, rows: u16)`
  - `fn close(&self, conn: ConnId, r#ref: u32)`
  - `fn drop_conn(&self, conn: ConnId)`
  - `TermSink` is the bounded per-connection sender used for backpressure.
  - Warm-pool semantics:
    - A released `claude attach` pty goes to the pool, keyed by `(profile, job)`.
    - An open of the same key takes it, replays the ring, and then sends a resize to the client's size to force a redraw. A same-size resize is sent as size−1 then size.
    - Several clients on one key share the pty: output fans out to all of them, and input from any of them goes in.

- [ ] **Step 1:** Write the tests, using a stand-in `claude` script that prints a banner, echoes input, and redraws on SIGWINCH (it prints `REDRAW <cols>x<rows>`):
  - (a) Opening a cold terminal shows the banner.
  - (b) Close, then reopen within 10 min: the ring is replayed, then `REDRAW` arrives. The stand-in ran once, counted from a file it appends to.
  - (c) **Review Focus 3:** two connections open the same job. One process is spawned, both see output, and input from both reaches it.
  - (d) **Review Focus 1:** the stand-in exits while warm. A reopen gets the last output, then `TermEnd`, and is not re-spawned silently. Opening again after that spawns fresh.
  - (e) A seventh released terminal evicts the oldest warm one, which is stopped.
  - (f) Codex: a second open of the same thread is refused with "It is open in another Fleet terminal already."; a thread in the plan-1 open set is refused with its busy reason; a released Codex terminal is stopped, not pooled.
  - (g) The 24/8 limits use `term.mjs`'s words.
  - (h) **Review Focus 4:** 600 resizes in 10 s leave the pty at the last size. The stand-in sees at most one `REDRAW` per 16 ms.
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Each pty has one reader thread (or a tokio task on its fd) writing into its ring and into the sinks. When any sink is full, that pty's reading pauses (backpressure). Resizes coalesce: only the latest pending size is applied.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Commit: `fleetd: terminal manager, warm pool, shared attach`.

### Task 4: Terminal connections on the socket

**Files:**
- Modify: `native/crates/fleetd/src/serve.rs`, `native/crates/fleetd/src/main.rs`
- Test: `native/crates/fleetd/tests/serve_terms.rs`

**Interfaces:**
- Consumes: Task 3 `Terms`.
- Produces: a `role: Terminals` connection takes TermOpen, TermIn, TermSize and TermClose, and gets TermOut and TermEnd. Board and terminal connections are fully independent.

- [ ] **Step 1:** Write the tests:
  - (a) Open, see the banner, type, and see the echo, all over the real socket.
  - (b) **Review Focus 2:** the client drops mid-flood (the stand-in prints 50 MB). Within 2 s the pty is pooled or stopped, the fd count of the daemon returns to baseline, and a board client meanwhile keeps receiving deltas with under 10 ms added latency.
  - (c) A slow terminal client (reads 1 KiB/s) causes no unbounded memory: daemon RSS stays under 64 MB while the stand-in tries to flood.
  - (d) A TermIn for an unknown ref is ignored, not an error.
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Commit: `fleetd: terminal connections on the socket`.

### Task 5: `fleetd attach`, the bench, and the live check

**Files:**
- Create: `native/crates/fleetd/src/attach.rs`
- Modify: `native/crates/fleetd/src/main.rs`, `native/crates/fleetd/src/bench.rs`, `native/bench/run.mjs`, `native/bench/budgets.json`, `native/bench/README.md`
- Test: `native/crates/fleetd/tests/attach.rs`

**Interfaces:**
- Produces:
  - `fleetd attach <profile>/<job>`, `fleetd attach codex/<session-id>` and `fleetd attach --trust <profile> <dir>`. Each puts the local tty into raw mode, relays stdin/stdout through a Terminals connection, and forwards SIGWINCH as TermSize. **Ctrl-]** detaches (as telnet does): it closes the terminal and restores the tty. `fleetd attach --list` prints the board's attachable sessions (profile/job, name, state).
  - Bench modes `term-echo`, `term-warm` and `term-cold`, each with FLEETD_HOME containment like plan 1's bench (I1 rules).

- [ ] **Step 1:** Write the tests:
  - (a) `attach` against the stand-in through a pty harness: the banner shows, typed input echoes, and Ctrl-] exits 0 with the tty restored (check `stty -g` before and after).
  - (b) SIGWINCH reaches the stand-in as `REDRAW`.
  - (c) `--list` shows the fixture sessions.
- [ ] **Step 2:** Run the tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Then add the three budgets to `budgets.json` and the three bench modes to `run.mjs`.
- [ ] **Step 4:** Run the tests and `node native/bench/run.mjs`. Expected: PASS; all budgets green, median-of-3 (Ruling R19). Ratchet.
- [ ] **Step 5:** Live check on Ross's Mac, read-only toward sessions. Reinstall the service with the new build (`install-service`). Then run `fleetd attach --list`, and `fleetd attach <profile>/<job>` on one idle background session. Confirm its screen draws, then detach with Ctrl-] WITHOUT typing anything else. Time the cold open and a warm reopen. Record both, and confirm the session is still listed with the same state afterwards. Do not touch the Node panel on 4747.
- [ ] **Step 6:** Commit: `fleetd: attach command, terminal bench, live check`.
