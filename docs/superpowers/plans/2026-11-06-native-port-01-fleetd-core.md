# fleetd core (Mac) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust `fleetd` on the Mac that knows every Claude Code and Codex session's truthful state the instant it changes, serves it as a snapshot plus deltas on a local socket, matches `fleet/machine.mjs` collect on the same inputs, and proves its speed against the Node panel with a bench that gates every PR.

**Architecture:** A Cargo workspace under `native/`. `fleet-model` is pure types plus the one state decision. `fleet-read` holds the readers: profiles, hook files, roster, incremental transcript tails and Codex rollouts. `fleetd` is the binary: a file-event watcher feeds a store, and the store emits sequenced deltas to socket subscribers. Node stays the oracle: a parity harness runs `machine.mjs` and `fleetd` over the same fixture homes and diffs them.

**Tech Stack:** Rust stable (rustup, user install), tokio, notify (macOS FSEvents backend), serde + serde_json (reading), postcard (wire), memchr. Node 20+ (oracle and bench drivers only, already present).

**Spec:** `docs/superpowers/specs/2026-11-06-fleet-native-port-design.md` (sections 6.1, 7, 9, 10). Roadmap: `docs/superpowers/plans/2026-11-06-native-port-00-roadmap.md`.

## Global Constraints

- Nothing in `fleetd` writes to Claude Code's or Codex's records (`jobs/`, `projects/`, `sessions/`, `fleet-state/`, `.claude.json`). It only reads.
- `fleetd` never closes, stops, archives or answers anything on its own (spec section 4).
- A live pid is never shown as ended. If the roster says `stopped`/`failed` beside a live pid, the state is `Waiting` with the reason `respawned` (spec section 7).
- Never guess a reason. With no record, the reason is `ended, reason unknown`.
- Profiles are discovered, not configured: `~/.claude` (id `default`) and `~/.claude-<name>` matching `^\.claude-[\w.-]{1,40}$`, each holding `settings.json`, `.claude.json` or `projects/` (`machine.mjs` `claudeProfiles`). Codex is `~/.codex`. A new `~/.claude-enterprise` appears with no code change.
- Transcripts are never re-read from a fixed tail after the first read. Reads continue from the last offset.
- No web view, Node, Python or ssh on any `fleetd` hot path.
- Idle means idle: no busy timers. The only periodic work is the reconcile pass, every 30 s.
- Socket: `~/Library/Application Support/Fleet/fleetd.sock` on macOS, `$XDG_RUNTIME_DIR/fleetd.sock` (else `~/.local/state/fleet/fleetd.sock`) on Linux, mode 0600. Overridable with `FLEETD_SOCK`. `FLEETD_HOME` overrides the home folder, for fixtures.
- Budgets, from spec 9.3 tightened for "even faster", enforced by `native/bench/budgets.json`:
  - hook event written → delta on socket: p95 ≤ 5 ms
  - snapshot on connect, 400 sessions: ≤ 5 ms
  - transcript append → delta: p95 ≤ 5 ms
  - `fleetd` idle: 0.0 % CPU over 60 s; RSS ≤ 30 MB
- The ratchet: a budget only moves down. `bench --ratchet` sets each to max(p95 × 1.25, floor), never higher than before.
- The repo is public. No emails, hostnames or tokens in fixtures, commits or logs.

## Review Focus

1. **A respawned job** (`state: stopped`, live pid, hook file says `SessionStart`) must read `Waiting · respawned`, never ended. This is the anti-goal the port exists for. Test in Task 3.
2. **A transcript truncated, rotated or replaced** while being tailed (size < offset, or inode changed) must re-read cleanly, not panic or freeze stale. Test in Task 4.
3. **A half-written JSONL line** at end of file (a write in flight) must be held until its newline arrives, not parsed as garbage or dropped. Test in Task 4.
4. **A burst of 200 hook writes in 1 s** must coalesce to the final state per session without falling behind, and without a trailing debounce that adds latency to a single event. Test in Task 7.
5. **The roster command failing or hanging** (`claude agents` > 8 s, non-zero exit) must keep the last good roster and mark that profile's sessions' reasons `roster stale since <t>`, never drop them. Test in Task 5.

---

### Task 1: Workspace and the Node "before" bench

**Files:**
- Create: `native/Cargo.toml` (workspace: `crates/fleet-model`, `crates/fleet-read`, `crates/fleetd`), `native/rust-toolchain.toml` (channel `stable`), `native/.gitignore` (`target/`)
- Create: `native/crates/{fleet-model,fleet-read,fleetd}/Cargo.toml` + `src/lib.rs` / `src/main.rs` stubs
- Create: `native/bench/node-before.mjs`, `native/bench/budgets.json`, `native/bench/README.md`
- Create: `native/bench/results/` (gitignored except `before.json`)

**Interfaces:**
- Produces: `native/bench/results/before.json`: `{ "measuredAt": iso, "machine": "Mac", "node": { "hookToSnapshotMs": {p50,p95,max}, "snapshotMs": {p50,p95}, "idleCpuPct": n, "rssMb": n } }`

- [ ] **Step 1:** Install Rust for the user only: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal`, then `rustup target add aarch64-apple-darwin x86_64-unknown-linux-musl`. Verify with `~/.cargo/bin/cargo --version`.
- [ ] **Step 2:** Create the workspace and stubs. `cargo build` in `native/` succeeds and `cargo test` runs 0 tests.
- [ ] **Step 3:** Write `node-before.mjs`. It starts its own panel (`ACP_PORT=4797`, `ACP_PREFS` in a temp dir, never 4747) over a fixture home built like `test/fleet-machine.test.mjs`'s `home()`. It then measures:
  - (a) Write a hook file with a new `at`, then poll `GET /api/fleet` every 5 ms until that session's state flips. Record the time. Repeat 50×.
  - (b) The latency of `GET /api/fleet`, 50×.
  - (c) CPU and RSS of the server pid, sampled over 60 s with `ps -o %cpu=,rss=` every second.

  It writes `results/before.json`.
- [ ] **Step 4:** Run `node native/bench/node-before.mjs`. Expected: `before.json` is written, with `hookToSnapshotMs.p95` in the thousands (the 10 s warm cycle). Commit `before.json`.
- [ ] **Step 5:** Write `budgets.json` with the four budgets from Global Constraints, each `{ "budget": n, "floor": n }`. The floors are 1 ms, 1 ms, 1 ms and 0 % / 15 MB.
- [ ] **Step 6:** Commit: `native: workspace and the Node before-bench`.

### Task 2: The model types and `hook_state`

**Files:**
- Create: `native/crates/fleet-model/src/{lib.rs,types.rs,hook.rs}`
- Test: `native/crates/fleet-model/tests/hook.rs`

**Interfaces:**
- Produces (in `types.rs`, all `serde::{Serialize, Deserialize}`, `Clone, Debug, PartialEq`):
  - `enum Engine { Claude, Codex, Muse }`
  - `enum State { NeedsYou, Working, Waiting, Capped, Done, Ended, Unreachable }`
  - `enum HookSays { Ready, Working, NeedsYou, Capped, Closed }`
  - `struct HookEvent { e: String, t: Option<String>, n: Option<String>, r: Option<String>, i: Option<bool>, at: i64 }` (field names exactly as `hook.sh` writes them)
  - `struct SessionKey { machine: String, profile: String, session_id: String }`
  - `struct Session { key: SessionKey, engine: Engine, name: Option<String>, cwd: Option<String>, state: State, reason: String, live: bool, pid: Option<u32>, stage: Option<String>, model: Option<String>, effort: Option<String>, context_tokens: Option<u64>, last_at: Option<i64>, said: Option<String>, doing: Option<String>, last_ask: Option<String>, first_ask: Option<String>, started_at: Option<i64>, hook_at: Option<i64>, written_at: Option<i64> }`
- Produces (in `hook.rs`): `pub fn hook_state(h: &HookEvent) -> Option<HookSays>`

- [ ] **Step 1:** Write `tests/hook.rs` as a table test that ports every branch of `machine.mjs` `hookState` (lines 441–455):
  - `SessionStart` → `Ready`; `SessionStart` with `r="compact"` → `None`
  - `PreToolUse` with `t="AskUserQuestion"` or `"ExitPlanMode"` → `NeedsYou`; any other tool → `Working`
  - `PostToolUseFailure` with `i=true` → `Ready`
  - `Notification`: `n` of `permission_prompt`, `elicitation_dialog` or `elicitation_url_dialog` → `NeedsYou`; `idle_prompt` → `Ready`; any other `n` → `None`
  - `StopFailure`: `r="rate_limit"` → `Capped`; anything else → `Ready`
  - `PostCompact`: `r="manual"` → `Ready`; anything else → `Working`
  - `UserPromptSubmit`, `PostToolUse`, `PermissionDenied`, `ElicitationResult`, `PreCompact` → `Working`
  - `PermissionRequest`, `Elicitation` → `NeedsYou`; `Stop` → `Ready`; `SessionEnd` → `Closed`; an unknown event → `None`
- [ ] **Step 2:** `cargo test -p fleet-model`. Expected: FAIL (unresolved `hook_state`).
- [ ] **Step 3:** Implement the types and `hook_state`.
- [ ] **Step 4:** `cargo test -p fleet-model`. Expected: PASS.
- [ ] **Step 5:** Commit: `fleet-model: types and hook_state`.

### Task 3: The one state decision, with reasons

**Files:**
- Create: `native/crates/fleet-model/src/decide.rs`
- Create: `native/parity/gen-decide-golden.mjs`, `native/parity/golden/decide.json`
- Test: `native/crates/fleet-model/tests/decide.rs`

**Interfaces:**
- Consumes: Task 2 types.
- Produces:
  - `struct RosterRow { id: Option<String>, session_id: Option<String>, pid: Option<u32>, pid_alive: bool, state: Option<String>, status: Option<String>, waiting_for: Option<String>, active: bool, kind: Option<String>, name: Option<String>, cwd: Option<String>, started_at: Option<i64> }`
  - `struct TailFacts { last_at: Option<i64>, written_at: Option<i64>, limit_hit_at: Option<i64>, turn_over: Option<bool> }`
  - `pub fn decide(r: &RosterRow, t: &TailFacts, h: Option<&HookEvent>, now: i64) -> (State, bool /*live*/, String /*reason*/)`

- [ ] **Step 1:** Write `gen-decide-golden.mjs`. It imports `sessionState` from `../../fleet/machine.mjs` and enumerates a case grid:
  - roster `state` × `status` × `pid` × `active` × `waitingFor`
  - hook `e` / `t` / `n` / `r` / `i`, with hook `at` relative to `writtenAt` at −20 s, −10 s and +0
  - `now` quiet at 1 s, 5 s and 90 s
  - `limitHit` present or absent

  It writes `golden/decide.json` as `[{ input, node: { state, live } }]`. Run it: `node native/parity/gen-decide-golden.mjs`.
- [ ] **Step 2:** Write `tests/decide.rs`:
  - (a) **Golden parity.** For every case, `decide` maps to Node's result. `needs-you`→`NeedsYou`, `working`→`Working`, `ready`→`Waiting`, `capped`→`Capped`, `closed`→`Done` when roster `state=="done"`, otherwise `Ended`. `live` must be equal.
  - (b) **The deliberate divergence.** Node says `closed` for `state:"stopped"` with a live pid. Rust says `Waiting`, `live=true`, reason `respawned`. List these cases in an `EXPECTED_DIVERGENCE` set and fail on any other mismatch.
  - (c) **Reasons are never empty**, and a no-record `Ended` is exactly `ended, reason unknown`.
  - (d) **Review Focus 1:** `stopped` + live pid + hook `SessionStart` → `Waiting · respawned`.
- [ ] **Step 3:** `cargo test -p fleet-model decide`. Expected: FAIL.
- [ ] **Step 4:** Implement `decide`. Port `sessionState` (machine.mjs 548–575) branch for branch, with the respawn rule first. Reasons, in exact words:
  - `respawned`
  - `waiting on you: <waitingFor or hook event>`
  - `limit hit at <hh:mm>`
  - `working: hook <e>` or `working: roster busy`
  - `idle at prompt`
  - `finished`
  - `stopped by Claude Code`
  - `failed`
  - `ended, reason unknown`
- [ ] **Step 5:** `cargo test -p fleet-model`. Expected: PASS.
- [ ] **Step 6:** Commit: `fleet-model: decide() with reasons, golden parity vs sessionState`.

### Task 4: Incremental transcript tail

**Files:**
- Create: `native/crates/fleet-read/src/transcript.rs`
- Modify: `fleet/machine.mjs:285` (export `readTranscript`, nothing else, for the golden generator)
- Create: `native/parity/gen-transcript-golden.mjs`, `native/parity/fixtures/transcripts/*.jsonl` (synthetic, no real content)
- Test: `native/crates/fleet-read/tests/transcript.rs`

**Interfaces:**
- Consumes: `TailFacts` (Task 3).
- Produces: `pub struct Tail { path: PathBuf, offset: u64, inode: u64, partial: Vec<u8>, /* summary */ }` with:
  - `pub fn open(path: &Path) -> io::Result<Tail>`. The first read takes the last 384 KiB plus the first 256 KiB (for `first_ask`), as `machine.mjs` does.
  - `pub fn advance(&mut self) -> io::Result<bool>`. It reads only bytes past `offset` and returns whether the summary changed.
  - `pub fn facts(&self) -> TailFacts`
  - `pub fn summary(&self) -> &TranscriptSummary { model, effort, context_tokens, last_at, said, doing, last_ask, first_ask, turn_over, running, limit_hit, stage, steps }`

- [ ] **Step 1:** Write the fixtures. Write `gen-transcript-golden.mjs`: for each fixture it calls `readTranscript`, and also writes the fixture in 3 random append chunks with `readTranscript` after each. It writes `golden/transcript.json`.
- [ ] **Step 2:** Write `tests/transcript.rs`:
  - (a) For each fixture and each chunk point, `open` plus repeated `advance` gives the same summary fields as Node.
  - (b) **Review Focus 3:** a fixture ending mid-line. Nothing is parsed from the partial line until its newline arrives in a later `advance`.
  - (c) **Review Focus 2:** truncate the file to 0 and rewrite it, then replace it via rename (new inode). `advance` re-opens and matches a fresh `open`.
  - (d) An `advance` after 1 appended line reads ≤ that line's bytes (assert through a read counter).
- [ ] **Step 3:** `cargo test -p fleet-read transcript`. Expected: FAIL.
- [ ] **Step 4:** Implement it.
  - Keep the summary as "newest wins" fields, updated per new line in file order. This is the forward equivalent of `readTranscript`'s backward scan.
  - `stage` is recomputed from a ring of the last 12 tool uses, porting `stageOf` (machine.mjs 210–245) to `fleet-model/src/stage.rs`, with its own golden from the exported `stageOf`.
  - Lines are split with `memchr`.
- [ ] **Step 5:** `cargo test -p fleet-read`. Expected: PASS.
- [ ] **Step 6:** Commit: `fleet-read: incremental transcript tail with golden parity`.

### Task 5: Profiles, accounts, hook files and the roster

**Files:**
- Create: `native/crates/fleet-read/src/{profiles.rs,hookfile.rs,roster.rs}`
- Test: `native/crates/fleet-read/tests/{profiles.rs,roster.rs}`

**Interfaces:**
- Produces:
  - `struct Profile { id: String, dir: PathBuf, plain: bool, config: PathBuf }`
  - `pub fn claude_profiles(home: &Path) -> Vec<Profile>`
  - `pub fn account(p: &Profile) -> Option<String>` (the email from `oauthAccount.emailAddress` in `config`)
  - `pub fn read_hook(p: &Profile, session_id: &str) -> Option<HookEvent>`. The session id must match `^[\w-]{8,80}$`. Files over 64 KiB are refused.
  - `pub trait RosterSource { fn read(&self, p: &Profile) -> Result<Vec<RosterRow>, RosterError>; }`
  - `struct CliRoster`. It runs `claude agents --json --all` with `CLAUDE_CONFIG_DIR=<dir>` (unset for `plain`), with an 8 s timeout and an env stripped of `CLAUDE*`/`FLEET_*`, as `baseEnv` does. `pid_alive` comes from `kill(pid, 0)`.
  - `struct FileRoster`. It reads `<dir>/jobs/*/state.json` plus pid liveness.

- [ ] **Step 1:** Write the tests:
  - (a) Discovery over a temp home with `.claude`, `.claude-aol`, `.claude-enterprise`, and a decoy `.claude-x` holding none of the marker files. Expect ids `default`, `aol` and `enterprise`, and not `x`.
  - (b) `account` reads the fixture email.
  - (c) `read_hook` refuses `../x` and files of 65 537 bytes.
  - (d) `CliRoster` against a stand-in `claude` script on `PATH` that prints a fixed JSON. Rows parse and `pid_alive` is right for the test's own pid and for pid 999 999.
  - (e) **Review Focus 5:** the stand-in sleeps 9 s. `read` returns `Err(Timeout)` within 8.5 s.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement the readers.
- [ ] **Step 4:** Run them. Expected: PASS.
- [ ] **Step 5:** **The file-roster spike.** `fleetd roster-parity` runs both sources over the real `~/.claude*` once a minute for 10 minutes. It logs every row where they differ to `native/bench/results/roster-parity.jsonl` and prints the timings. Run it. If there are zero diffs, `FileRoster` becomes the default (`FLEETD_ROSTER=cli` keeps the CLI). Otherwise `CliRoster` stays the default and the diffs go in the PR body. Expected timing: `CliRoster` ~170 ms per profile (measured 2026-11-06), `FileRoster` < 1 ms.
- [ ] **Step 6:** Commit: `fleet-read: profiles, hook files, roster (cli + files), parity spike`.

### Task 6: Codex and Muse sessions

**Files:**
- Create: `native/crates/fleet-read/src/codex.rs`
- Modify: `fleet/machine.mjs` (export `readRollout`, `recentRollouts`, `codexAccount` for the golden)
- Create: `native/parity/gen-codex-golden.mjs`, `native/parity/fixtures/codex/sessions/YYYY/MM/DD/*.jsonl`
- Test: `native/crates/fleet-read/tests/codex.rs`

**Interfaces:**
- Produces:
  - `pub fn recent_rollouts(codex_home: &Path, within_ms: i64, now: i64) -> Vec<PathBuf>` (newest first, at most 200, only threads where `thread_source` is null or `"user"`)
  - `pub fn codex_session(f: &Path, open: bool) -> Session`
  - `pub fn codex_account(codex_home: &Path) -> Option<String>`

- [ ] **Step 1:** Write the golden generator and the tests. Every fixture rollout's `Session` must match Node's `readRollout` on state, said, doing, model, last_at, context_tokens. Threads started by an agent or a reviewer are excluded.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement it, reading rollouts through the same `Tail` mechanism as Task 4, so appends are incremental too. A Codex session with no record of why it ended gets `Ended · ended, reason unknown`. Muse (`museHome`, machine.mjs 870) is ported the same way in this file, if `~/.muse` exists.
- [ ] **Step 4:** Run them. Expected: PASS.
- [ ] **Step 5:** Commit: `fleet-read: codex and muse sessions`.

### Task 7: Watcher, store and sequenced deltas

**Files:**
- Create: `native/crates/fleetd/src/{store.rs,watch.rs}`
- Test: `native/crates/fleetd/tests/store.rs`

**Interfaces:**
- Consumes: Tasks 2–6.
- Produces:
  - `struct Store`
    - `fn snapshot(&self) -> (u64 /*seq*/, Vec<Session>)`
    - `fn since(&self, seq: u64) -> Option<Delta>`. It returns `None` when `seq` is older than the 4 096-delta ring, and the caller then sends a snapshot.
    - `fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Delta>`
  - `struct Delta { seq: u64, upsert: Vec<Session>, remove: Vec<SessionKey> }`. Only sessions whose serialized form changed are included.
  - `fn watch(home: &Path, store: Arc<Store>) -> Result<WatchHandle>`. It uses notify's FSEvents backend with no latency (`kFSEventStreamCreateFlagNoDefer`, latency 0) on:
    - each profile's `fleet-state/`, `jobs/` and `projects/`
    - `~/.codex/sessions/`
  - **Event routing:**
    - A hook file change re-decides that one session.
    - A transcript change advances that one `Tail`.
    - A `jobs/` change re-reads that profile's roster. With `CliRoster`, at most one run is in flight per profile. If another change arrives during the run, exactly one follow-up run starts after it. There is no fixed debounce.
  - The reconcile pass runs every 30 s: a full re-read that emits only real differences.

- [ ] **Step 1:** Write the tests, over a temp home with a stand-in `claude`:
  - (a) Connect, then write a hook file. A delta with that session's new state arrives within 50 ms (a CI-safe bound; the bench holds the real 5 ms budget).
  - (b) Append a transcript line. A delta arrives carrying the new `said`.
  - (c) **Review Focus 4:** 200 hook writes in 1 s, for 10 sessions. The last delta per session has the final state, the deltas total ≤ 220, and the single-write latency from (a) is unchanged after the burst.
  - (d) The reconcile pass on an unchanged home emits no delta.
  - (e) `since(seq)` after 5 000 deltas returns `None`.
  - (f) Roster failure: the stand-in exits 1. Sessions stay, with reason prefixed `roster stale since`.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run them. Expected: PASS.
- [ ] **Step 5:** Commit: `fleetd: FSEvents watcher, store, sequenced deltas`.

### Task 8: The socket protocol and `fleetd` commands

**Files:**
- Create: `native/crates/fleet-model/src/wire.rs`, `native/crates/fleetd/src/{serve.rs,main.rs}`
- Test: `native/crates/fleetd/tests/serve.rs`

**Interfaces:**
- Produces:
  - **Wire.** Each frame is a `u32` little-endian length followed by postcard bytes of `enum Frame`:
    - `Hello { proto: u16, since: Option<u64> }` (client → server)
    - `Snapshot { seq: u64, machine: String, sessions: Vec<Session> }`
    - `Delta(Delta)`
    - `Error { message: String }`

    `PROTO = 1`. `pub fn write_frame` and `pub fn read_frame` (async, tokio) live in `wire.rs`.
  - **Commands:**
    - `fleetd serve`: the Unix socket at the Global-Constraints path, mode 0600
    - `fleetd snapshot [--json]`: one snapshot, printed (the parity and debug path)
    - `fleetd watch`: prints each delta as one JSON line
    - `fleetd roster-parity` (from Task 5)
    - `fleetd install-service` / `uninstall-service`: write and load or unload `~/Library/LaunchAgents/dev.fleet.fleetd.plist` (RunAtLoad, KeepAlive, logs to `~/Library/Logs/Fleet/fleetd.log`)

- [ ] **Step 1:** Write the tests:
  - (a) Hello with `since: None` returns a Snapshot.
  - (b) Hello with a current `since` returns only the deltas after it.
  - (c) A wrong `proto` returns `Error` and closes the connection.
  - (d) The socket file mode is 0600.
  - (e) Two clients both receive the same delta.
- [ ] **Step 2:** Run them. Expected: FAIL.
- [ ] **Step 3:** Implement the protocol and commands.
- [ ] **Step 4:** Run them. Expected: PASS.
- [ ] **Step 5:** Commit: `fleetd: socket protocol, serve/snapshot/watch, launchd service`.

### Task 9: Collect parity over fixture homes

**Files:**
- Create: `native/parity/collect-parity.mjs`
- Test: it is the test; wired into `native/check.sh`

**Interfaces:**
- Consumes: `fleetd snapshot --json` (Task 8). Node `FLEET_DO=collect node --input-type=module - < fleet/machine.mjs`, run as `test/fleet-machine.test.mjs` runs it.

- [ ] **Step 1:** Write `collect-parity.mjs`. It builds the fixture homes from `test/fleet-machine.test.mjs`, copying its `home()` builder rather than importing it, plus the Task 3–6 fixtures. For each home it:
  - runs Node collect and `FLEETD_HOME=<home> fleetd snapshot --json`
  - normalizes both to `{ profile, sessionId, engine, state, live, stage, model, contextTokens, lastAt, said, doing }`
  - diffs them, allowing only Task 3's `EXPECTED_DIVERGENCE` cases

  It exits 1 on any other difference and prints it.
- [ ] **Step 2:** Run `node native/parity/collect-parity.mjs`. Expected: `parity: N homes, M sessions, 0 unexpected differences`.
- [ ] **Step 3:** Write `native/check.sh`: `cargo fmt --check && cargo clippy -- -D warnings && cargo test && node native/parity/collect-parity.mjs && node native/bench/run.mjs`.
- [ ] **Step 4:** Commit: `native: collect parity vs machine.mjs`.

### Task 10: The bench gate and the ratchet

**Files:**
- Create: `native/bench/run.mjs`
- Modify: `native/bench/README.md`

**Interfaces:**
- Consumes: `fleetd serve` (Task 8), `budgets.json` and `before.json` (Task 1).
- Produces: `results/latest.json` with the same shape as `before.json` under the key `"fleetd"`. Exit 1 if any p95 is over its budget.

- [ ] **Step 1:** Write `run.mjs`. It starts `fleetd serve` on a temp socket over a fixture home with 400 sessions, connects, and measures:
  - hook write → delta (200 samples)
  - transcript append → delta (200)
  - snapshot on connect (50)
  - idle CPU and RSS over 60 s

  It compares each against `budgets.json`, then prints a before/after table against `before.json`. `--ratchet` lowers budgets as Global Constraints say and rewrites `budgets.json`.
- [ ] **Step 2:** Run `cargo build --release && node native/bench/run.mjs`. Expected: every budget green. Record the before/after table in the PR body.
- [ ] **Step 3:** Run `node native/bench/run.mjs --ratchet`, then commit the tightened `budgets.json`.
- [ ] **Step 4:** Live check on the real Mac. Run `fleetd install-service`, then `fleetd watch`, while one of Ross's live sessions takes a turn. Expected: deltas appear as the turn runs, and its states agree with the Fleet page on 4747. Note any disagreement in the PR body, with its reason string.
- [ ] **Step 5:** Commit: `native: bench gate and ratchet`. Open a draft PR to `feat/fleet` on the fork (`--repo rossaroberts/agent-control-panel`), with the before/after table.
