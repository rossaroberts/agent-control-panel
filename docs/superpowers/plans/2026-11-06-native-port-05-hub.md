# Hub on redbat (network listener, Linux build, redbat install, `--hub` merge) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** redbat's sessions appear on the same board as the Mac's, served by an always-on hub on redbat, so the board works with the Mac asleep.

**Architecture:**
- `fleetd` gains a TCP listener beside its Unix socket. It speaks the same protocol over the same framing, and binds only loopback and the machine's Tailscale IPv4. Every peer is authenticated before a byte of it is read: Tailscale WhoIs for tailnet peers, and a same-uid check or a key for loopback.
- `fleetd` builds static for x86_64 Linux (musl, linked by `rust-lld` from the Mac). It runs on redbat as a systemd system unit with `User=claude`.
- `fleetd serve --hub` is the watcher plus one Board client per configured machine, over that listener. Each remote's board is mirrored and projected into the hub's own `Store` (sessions are already keyed by machine), so the hub serves one merged board with no new frames. A machine that cannot be read keeps its rows, as `Unreachable` with the last state in the reason.

**Tech Stack:** Rust (existing workspace, tokio, postcard); `socket2` for TCP keepalive; the `tailscale` CLI for WhoIs (no new Rust deps for it); systemd on Ubuntu 24.04.

**Spec:** `docs/superpowers/specs/2026-11-06-fleet-native-port-design.md` §6, §6.4, §7 (unreachable), §9, §11, §14. Lanes: `docs/superpowers/plans/2026-11-06-native-port-lanes.md` (binding). Earlier rulings: plans 1–3 (R1–R24, P2-R1–R6, P3-R1–R7) bind. Wire contract: `native/PROTOCOL.md`.

## Global Constraints

- Work only in this worktree, branch `native/hub`. Cargo is `~/.cargo/bin/cargo`. Commit on `native/hub`; never push or merge.
- **The wire is lane A's.** No change to `Frame`, `Session`, `Meta`, `Role`, `PROTO` or the goldens. This lane adds one section to `native/PROTOCOL.md`, "Network", and nothing else there.
- **Never `0.0.0.0`, never a public interface.** The TCP listener binds `127.0.0.1:<port>` and the Tailscale IPv4 (from `tailscale ip -4`, which must lie in `100.64.0.0/10` and be assigned to a local interface), and nothing else. No IPv6.
- **The port is 4780** (`DEFAULT_PORT`), overridable by `--port N` or the config's `port`.
- **Off by default.** `fleetd serve` with no flag and no config `listen` is byte-for-byte today's behaviour. The Mac's installed LaunchAgent is not touched by this lane, ever.
- **Authenticated before read.** No byte of a TCP peer is read as a frame until it is allowed. Refusal is one `Error { message: "not allowed: <why>" }`, then close.
- **Read-only** toward Claude Code's and Codex's records (plan 1's invariant). The hub writes no Node-panel file (Ruling P5-R1: no writes until plan 8).
- **Idle stays idle.** No timer runs while nothing changes, except these: the existing 30 s reconcile; while a remote is down, a reconnect at most every 30 s; while the Tailscale address is not bound yet, a retry every 30 s.
- **Bench budgets** (`native/bench/budgets.json`, median of 3, Ruling R19) stay green. No budget is loosened. New budgets enter at ≤ their spec ceiling and only ratchet down.
- **Never kill by name pattern.** Only pids your own test started. Never touch the Node panel (4747), `~/Applications/Agent Panel.app` or the Mac's `fleetd` LaunchAgent.
- **redbat:** reach it with `ssh redbat-ts` (as root) and run as the agent user with `sudo -n -u claude -H`. Change nothing there but what Task 3 lists. Leave the Node worker and every other service alone.
- **Public repo:** synthetic fixtures only. No hostnames, tailnet names, IPs, emails or user ids in commits. Deploy scripts take the ssh host as an argument.
- Ruling P5-R1 (main, 2026-11-06): no wire change for panel maps. Until plan 8 moves the shared store to the hub, redbat rows on the hub carry redbat's own titles, labels and archive marks. Merged Meta is built like this:
  - epics and groups come from the remotes, in config order (in practice the Mac's);
  - accounts are unioned by `(engine, name)`, remotes first in config order, then the hub's own;
  - health is the hub's own.
  An unreachable machine's rows are re-sent with `state = Unreachable` and reason `<machine> unreachable since <hh:mm>; was <state word>: <old reason>`. `<hh:mm>` is `fleet_model::decide::hh_mm_utc`, as every other reason. No new field.
- Ruling P5-R2: a loopback TCP peer is allowed without a key only where its uid can be verified. On Linux, `/proc/net/tcp` gives the owning uid of the peer's socket, and it must equal `geteuid()`. On macOS there is no cheap per-socket lookup, so loopback always needs the key. The key preamble is transport, not a `Frame`. It is the 4 bytes `FKEY`, then 32 key bytes. As a little-endian `u32` frame length, `FKEY` would be ~1.26 GB, over `MAX_FRAME`, so it can never be mistaken for one.
- Ruling P5-R3: a Tailscale peer is allowed when all of these hold:
  - `tailscale whois --json <ip>:<port>` answers within 2 s;
  - its `Node.Tags` is empty;
  - its `UserProfile.ID` equals this machine's `Self.UserID` (from `tailscale status --json`, read when the listener starts and re-read once on a mismatch).
  The decision is cached per peer IP for 60 s, allow and deny alike. WhoIs is the CLI, the same on macOS and Linux. With no CLI, every Tailscale peer is refused.
- Ruling P5-R4: the redbat service is a systemd **system** unit `fleetd.service` with `User=claude`. It sets `XDG_RUNTIME_DIR=/run/user/<claude uid>` (claude has linger) so the socket is the canonical `$XDG_RUNTIME_DIR/fleetd.sock`, and `FLEETD_MACHINE=redbat`. This avoids needing the user bus from `sudo -u claude`.

## Review Focus

1. **A tailnet peer that is not Ross's** (a shared-in node, a tagged node, WhoIs timing out or the CLI missing). Expect a refusal with an `Error`, no frame read, and the daemon unaffected. Task 1 tests a stub `tailscale` that answers another user, a tag, a hang (> 2 s) and no binary at all.
2. **The Mac asleep mid-stream** (TCP silently dead, no FIN). The hub must notice within ~30 s by TCP keepalive and mark the Mac's rows Unreachable with their last state, never removing them. On wake it resumes with `run`/`since` (no full snapshot when the run is the same) and restores the true states. Task 4 tests a remote that stops answering without closing (SIGSTOP on its pid), then SIGCONT.
3. **A remote that restarts** (new `run`). Its rows of the old run that are no longer present must go, without ghosts or duplicates, and rows that stay must not flicker through Unreachable for longer than the reconnect. Task 4 tests a restart with one session gone.
4. **A remote that sends rows of another machine** (a hub subscribed to another hub, or a misconfigured name). The hub must never let a remote overwrite or remove its own machine's rows or a third machine's. Task 4 tests a remote whose snapshot carries a row keyed to the hub's own machine.
5. **Tailscale down or not yet up when `fleetd` starts** (redbat boots before tailscaled). Loopback and the Unix socket serve at once; the Tailscale bind is retried every 30 s and succeeds later; `0.0.0.0` is never the fallback. Task 1 tests a stub CLI that fails first, then answers.

---

### Task 1: The network listener and peer auth

Implementer model: **opus** (network + auth). Reviewer: **opus**.

**Files:**
- Create: `native/crates/fleetd/src/net.rs`: config, bind, accept loop, auth.
- Create: `native/crates/fleetd/src/tailscale.rs`: the CLI: `ip -4`, `status --json` self id, `whois --json`, each with a timeout.
- Create: `native/crates/fleetd/src/config.rs`: `fleetd.json`, the key file, the folders.
- Modify: `native/crates/fleetd/src/serve.rs`:
  - make the per-connection `handle` and `terminals` generic over the stream (Unix or TCP);
  - `hung_up` takes a `RawFd`;
  - add `serve_tcp`.
- Modify: `native/crates/fleetd/src/main.rs`: `serve [--listen tailscale|loopback] [--port N] [--config FILE]`.
- Modify: `native/crates/fleetd/src/client.rs`: `Client::connect_tcp(addr, key: Option<[u8;32]>, resume)` and `term_connect_tcp`.
- Modify: `native/crates/fleetd/Cargo.toml`: add `socket2 = "0.5"` (keepalive, `TCP_NODELAY`).
- Modify: `native/PROTOCOL.md`: a new section "Network" only.
- Test: `native/crates/fleetd/tests/net.rs`, `native/crates/fleetd/tests/common/stub_tailscale.rs` (writes a stub `tailscale` script into a temp dir; `FLEETD_TAILSCALE` points at it).

**Interfaces:**
- Produces:
  - `config::Config { listen: Listen, port: u16, hub: Vec<Remote> }`, with `enum Listen { Off, Loopback, Tailscale }` and `Remote { machine: String, addr: String /* host:port */ }`.
  - `config::load(path: Option<&Path>) -> io::Result<Config>`. Default path: macOS `~/Library/Application Support/Fleet/fleetd.json`; Linux `$XDG_CONFIG_HOME/fleet/fleetd.json`, else `~/.config/fleet/fleetd.json`. A missing file means `Listen::Off`, port 4780, no hub. CLI flags override the file.
  - `config::key(dir: &Path) -> io::Result<[u8; 32]>`: `fleetd.key` beside the config, 64 hex chars, created 0600 from `/dev/urandom` on first use. A file with looser mode or bad content is an error, not rewritten.
  - `tailscale::Cli { bin: PathBuf, timeout: Duration }`. `bin` is `FLEETD_TAILSCALE`, else `tailscale` on `PATH`, else `/usr/local/bin/tailscale`, `/usr/bin/tailscale` or `/Applications/Tailscale.app/Contents/MacOS/Tailscale`. Methods: `ipv4() -> io::Result<Ipv4Addr>`, `self_user() -> io::Result<u64>`, `whois(peer: SocketAddr) -> io::Result<Peer { user: u64, tagged: bool }>`.
  - `net::Auth` (the decision, with its 60 s cache), with `net::Auth::check(&self, peer: SocketAddr, local: SocketAddr, stream: &TcpStream) -> Result<(), String>`.
  - `net::listen(cfg: &Config, auth: Arc<Auth>) -> io::Result<Vec<TcpListener>>`.
  - `serve::serve_tcp(l: TcpListener, auth: Arc<Auth>, store, machine, terms)`.
- Consumes: the existing `serve::handle` / `terminals`, `Store`, `Terms`.

- [ ] **Step 1: Write the failing tests** in `tests/net.rs`. Each test starts its own `fleetd serve` (debug binary, temp `FLEETD_HOME`, temp `FLEETD_SOCK`, `--listen loopback --port 0`). `--port 0` is allowed for tests, and the bound port is printed on stderr as `fleetd: listening tcp <addr>`.
  - `off_by_default_binds_no_tcp`: a `fleetd serve` with no flag prints no `listening tcp` line; on macOS `lsof -p <pid> -iTCP` lists nothing (on Linux `/proc/<pid>/net/tcp` has no LISTEN for the pid's inodes).
  - `loopback_with_key_gets_the_board`: `Client::connect_tcp(addr, Some(key), None)` gets a `Snapshot`, then a `Meta`, whose sessions equal the Unix socket's.
  - `loopback_wrong_key_is_refused`: gets `Error` starting `not allowed`, then EOF.
  - `loopback_no_key`: on macOS, refused (`not allowed: loopback needs the key`). On Linux, allowed (same uid, from `/proc/net/tcp`).
  - `refused_peer_is_not_read`: a peer that sends a refused key followed by a valid `Hello` gets exactly one `Error` frame and then EOF.
  - `terminals_over_tcp`: `term_connect_tcp` with the key, then `TermOpen` on the fixture's stand-in `claude attach`, gets `TermOut` (reuse `tests/terms_home`).
  - `never_binds_unspecified`: a stub CLI whose `ip -4` answers `0.0.0.0`, or `8.8.8.8`, or an address on no local interface. The Tailscale bind is refused with a log line, and only loopback is bound.
- [ ] **Step 2:** In `src/net.rs`'s `#[cfg(test)]`, write the failing unit tests for `Auth` with an injected WhoIs (a trait `WhoIs` so the tests need no process):
  - `same_user_untagged_allowed`;
  - `other_user_refused`;
  - `tagged_refused`;
  - `whois_timeout_refused` (the stub sleeps 3 s; the check returns within 2.2 s);
  - `no_cli_refused`;
  - `decision_cached_60s` (the second check within 60 s makes no call; after 60 s, mocked clock, it calls again);
  - `self_user_reread_once_on_mismatch`;
  - `non_tailnet_non_loopback_refused` (a `192.168.x.x` peer).
  Plus a test `tailscale_retry_binds_later` in `tests/net.rs`: the stub CLI fails `ip -4` once, then answers `127.0.0.2` on macOS or `127.0.0.1` on Linux. `FLEETD_TEST_TS_RANGE_ANY=1` (debug builds only) lifts the `100.64/10` check so a test can bind a loopback alias. With `FLEETD_TEST_TS_RETRY_MS=200`, the second bind lands within 2 s.
- [ ] **Step 3: Run** `~/.cargo/bin/cargo test -p fleetd --test net` and `cargo test -p fleetd net::`. Expected: FAIL (modules missing).
- [ ] **Step 4: Implement.** Accept, then `Auth::check` (from the peer's address alone; the loopback-with-key path peeks 4 bytes with a 10 s limit), then hand the stream to the same `handle` the Unix socket uses. Set `TCP_NODELAY` and keepalive (idle 15 s, interval 5 s, 3 probes) on every accepted socket. Allow at most 16 connections in auth at once; past that, refuse at once with `not allowed: busy`. Log every refusal once per peer per minute: `fleetd: tcp refused <ip>: <why>`. Never log key bytes.
- [ ] **Step 5: Run** the tests above, then the full gate on the Mac: `sh native/check.sh`. Expected: all green, bench unchanged (the listener is off in the bench).
- [ ] **Step 6: Live check (read-only, the Mac).** Start your own `fleetd serve --listen tailscale` from `target/release` with `FLEETD_SOCK=$TMPDIR/fleetd-t1.sock`. It must not be the installed service. Your home is fine; it only reads. Then check three things:
  - from redbat, `ssh redbat-ts` then `sudo -n -u claude -H` with a probe sends a proto-4 `Hello` to `<mac tailscale ip>:4780` and gets a `Snapshot` (count the sessions). Build the probe as `fleetd watch --tcp <addr>` (add `--tcp` to `watch`);
  - from the Mac, `fleetd watch --tcp 127.0.0.1:4780` without the key is refused;
  - `lsof -nP -iTCP:4780 -sTCP:LISTEN` shows exactly the two addresses.
  Stop your pid. Record the output (with IPs redacted) in the task report.
- [ ] **Step 7: Write PROTOCOL.md "Network".** It covers:
  - port 4780;
  - the two bind addresses;
  - who is allowed (P5-R2, P5-R3);
  - the `FKEY` preamble and where the key file lives;
  - that after auth the conversation is exactly the Unix socket's (same framing, `Hello` first, both roles);
  - that a refusal is one `Error` then close;
  - keepalive;
  - that a client should treat a dropped TCP connection like a lost socket (reconnect with `run`/`since`).
  Also a "how a client connects" example: phone over Tailscale, and a local tool with the key.
- [ ] **Step 8: Commit** `fleetd: TCP listener on loopback + Tailscale, WhoIs/uid/key auth (Rulings P5-R2, P5-R3)`.

### Task 2: Linux build, tests green on redbat

Implementer model: **sonnet**. Reviewer: **sonnet**.

**Files:**
- Create: `native/deploy/build-linux.sh`. It runs `cargo build --release --target x86_64-unknown-linux-musl -p fleetd` with `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER` set to the toolchain's `rust-lld` (found under `$(rustc --print sysroot)/lib/rustlib/*/bin/rust-lld`), and prints the path and `file` of the result.
- Create: `native/deploy/test-linux.sh <ssh-host>`. It runs the Rust tests on the Linux host in a throwaway place:
  - `rsync` the `native/` tree (no `target/`) to `/tmp/fleetd-ci/src` as user `claude`;
  - install a user-level rustup into `/tmp/fleetd-ci/{rustup,cargo}` (`RUSTUP_HOME`/`CARGO_HOME` there, `--no-modify-path`, profile minimal plus `clippy`, `rustfmt`);
  - run `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` with `HOME=/tmp/fleetd-ci/home`.
  Nothing outside `/tmp/fleetd-ci` is written. Removal is `rm -rf /tmp/fleetd-ci`.
- Modify: `native/crates/fleet-read/tests/codex_tz.rs`: call `libc::tzset()` after setting `TZ` (glibc and musl cache it).
- Modify: `native/crates/fleetd/tests/terms_drop.rs` and `native/crates/fleetd/tests/serve_terms.rs`: `threads()` and `rss()` get Linux bodies from `/proc/self/status` (`Threads:` and `VmRSS:` × 1024). Keep the macOS bodies under `#[cfg(target_os = "macos")]`. Do not skip the tests.
- Modify: any `#[cfg(target_os = "macos")]`-only code whose Linux arm leaves a dead-code warning (e.g. `watch.rs:928` `on_exits`). Under `-D warnings` on Linux it must be warning-free.
- Create (if Step 3 shows it is needed): `native/crates/fleetd/src/pidwatch.rs`, Linux process exits via `pidfd_open` (`libc::SYS_pidfd_open`) polled on one thread. It exposes the same calls `watch.rs` makes on `kq` for pids (`watch_pid(pid)`, the exits callback), so `on_exits` runs on Linux as on macOS. A pidfd that cannot be opened (an old kernel, a pid gone) falls back to the reconcile pass.

**Interfaces:**
- Consumes: Task 1's tests must also pass on Linux. Linux's loopback path is uid-verified (P5-R2).
- Produces: `native/deploy/build-linux.sh` → `native/target/x86_64-unknown-linux-musl/release/fleetd` (static-pie), which Task 3 installs.

- [ ] **Step 1: Run the cross-check:** `~/.cargo/bin/cargo clippy --target x86_64-unknown-linux-musl --all-targets -p fleetd -p fleet-read -p fleet-model -- -D warnings`. Expected: FAIL. Known failures are `proc_pidinfo` in `terms_drop.rs` and `serve_terms.rs`, plus dead code in `watch.rs`.
- [ ] **Step 2: Fix** them as listed, then run Step 1 again. Expected: clean.
- [ ] **Step 3: Write the failing test** `pid_exit_reaches_the_board_on_linux` in `native/crates/fleetd/tests/serve.rs`, `#[cfg(target_os = "linux")]`. A fixture session's pid is a `sleep 30` the test started. The test kills that pid (its own child) and expects a delta that makes the session not live within 1 s, well under the 30 s reconcile. Run it on redbat via `test-linux.sh`. If it already passes (notify/inotify catches it), drop `pidwatch.rs` and note that in the report. Otherwise implement `pidwatch.rs` until it passes.
- [ ] **Step 4: Run** `sh native/deploy/build-linux.sh` (expect `ELF 64-bit LSB pie executable, x86-64 … static-pie linked`), then `sh native/deploy/test-linux.sh redbat-ts`. Expected: fmt, clippy and every test green on redbat. Paste the `test result:` lines into the report.
- [ ] **Step 5: Run** the Mac gate `sh native/check.sh`. Expected: green, bench within budgets.
- [ ] **Step 6: Bench on Linux (report only, not gated).** Copy the bench to `/tmp/fleetd-ci` and run `node bench/run.mjs --quick` there against the Linux build, as claude. It needs `node`, which redbat has. Report hook/transcript/snapshot p95 and idle CPU. A Linux number worse than 2× the Mac's is a finding to report, not to fix here.
- [ ] **Step 7: Commit** `fleetd: Linux (musl static) build, Linux test helpers, tests green on Linux`.

### Task 3: Install `fleetd` on redbat as a service running as `claude`

Implementer model: **sonnet**. Reviewer: **sonnet**. The implementer runs the install. The controller checks the removal steps work by reading them, and does not run them.

**Files:**
- Modify: `native/crates/fleetd/src/service.rs`: add `pub fn systemd_unit(bin: &Path, uid: u32, machine: &str, args: &[&str]) -> String` (pure, unit-tested). The text:
  - `[Unit] Description=fleetd (Fleet session board)`, `After=network-online.target tailscaled.service`, `Wants=network-online.target`;
  - `[Service] User=claude`, `Environment=XDG_RUNTIME_DIR=/run/user/<uid>`, `Environment=FLEETD_MACHINE=redbat` (passed in, not hard-coded: a `machine` parameter), `Environment=PATH=/home/claude/.local/bin:/usr/local/bin:/usr/bin:/bin`, `ExecStart=<bin> serve <args>`, `Restart=always`, `RestartSec=2`, `LimitNOFILE=8192`, `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ReadWritePaths=/run/user/<uid> /home/claude/.local/state/fleet /home/claude/.config/fleet`, `ProtectHome=read-only`;
  - `[Install] WantedBy=multi-user.target`.
  `ProtectHome=read-only` with `ReadWritePaths` keeps the read-only invariant enforced by the OS, not just by the code. Check that `claude agents --json` (the roster CLI) still runs under it. If it needs to write under `~/.claude*`, drop `ProtectHome` and record it as a finding with the reason.
- Modify: `native/crates/fleetd/src/main.rs`: `fleetd systemd-unit --uid N --machine NAME [-- serve args]` prints the unit, so the deploy script never hand-writes it.
- Create: `native/deploy/install-linux.sh <ssh-host> [-- serve args]`. It is idempotent:
  - `scp` the musl binary to the host's `/tmp`;
  - as root, `install -o claude -g claude -m 0755` it into `/home/claude/.local/bin/fleetd` (written to `.fleetd.new`, then `mv`, so a running copy is never overwritten in place);
  - generate the unit by running the new binary's `systemd-unit` on the host, and write `/etc/systemd/system/fleetd.service`;
  - `mkdir -p` the state and config folders as claude;
  - `systemctl daemon-reload && systemctl enable --now fleetd` (or `restart` if already active);
  - print `systemctl is-active fleetd` and the last 5 journal lines.
- Create: `native/deploy/uninstall-linux.sh <ssh-host>`: `systemctl disable --now fleetd`, remove the unit, `daemon-reload`, remove the binary. It keeps `~/.config/fleet` (the key, the hub config) unless `--purge`, which also removes `/home/claude/.config/fleet` and `/home/claude/.local/state/fleet`.
- Create: `native/deploy/README.md`: what each script does, and the exact list of files and units they install or remove.
- Test: `service::tests::systemd_unit_*` (the text, line by line; no `0.0.0.0`; `User=claude`; `ExecStart` carries the args quoted).

**Interfaces:**
- Consumes: Task 2's binary. Task 1's `serve` flags.
- Produces: on redbat, `fleetd.service` active, serving `/run/user/<uid>/fleetd.sock` as `claude`, machine `redbat`, with the listener **off** in this task. Task 4 changes `ExecStart` args by running `install-linux.sh` again with `-- --listen tailscale --hub`.

- [ ] **Step 1: Write the failing** `systemd_unit_*` tests. Run `cargo test -p fleetd service::`. Expected: FAIL.
- [ ] **Step 2: Implement** `systemd_unit` and the subcommand. Run the tests. Expected: PASS.
- [ ] **Step 3: Before installing,** record redbat's state into the report:
  - `systemctl list-units --type=service --state=running --no-pager` (names only);
  - `ps -u claude -o pid,comm` count;
  - whether `/etc/systemd/system/fleetd.service` and `/home/claude/.local/bin/fleetd` exist (both should be absent).
- [ ] **Step 4: Install:** `sh native/deploy/build-linux.sh && sh native/deploy/install-linux.sh redbat-ts`. Expected: `active`.
- [ ] **Step 5: Live check against Node.** On redbat, as claude, run Node's collect and the new board side by side and compare them per `(engine, profile, sessionId)`:
  - Node: `cat fleet/machine.mjs | ssh redbat-ts 'sudo -n -u claude -H env FLEET_DO=collect node --input-type=module -'` (the file from this worktree, piped; nothing is copied onto redbat);
  - board: `ssh redbat-ts 'sudo -n -u claude -H env XDG_RUNTIME_DIR=/run/user/<uid> /home/claude/.local/bin/fleetd snapshot --json'` (one in-process read) **and** one `fleetd watch` first frame from the service's socket. Both must agree with each other.
  Map Node's words as plan 1 did (closed = done|ended, ready = waiting). Report counts per profile (`default`, `gmail`, `rar`), agree, disagree (each one with both sides' state and reason), only-Node, only-fleetd. Any disagreement that is not a ruled divergence (R7, R8, R9) is a finding to fix or explain before the task closes.
- [ ] **Step 6: Idle on redbat.** Sample `ps -o time=,rss= -p <service pid>` 60 s apart with no client connected. Report the CPU ms and RSS. They are held to the Mac budgets (≤ 50 ms/60 s, ≤ 30 MB).
- [ ] **Step 7: After:** repeat Step 3's list. The only differences allowed are `fleetd.service` and the new files. Write the install manifest (each path, owner, mode) and the exact removal commands into `native/deploy/README.md`, using `<host>` and `<uid>` placeholders.
- [ ] **Step 8: Commit** `fleetd: systemd unit + Linux deploy/undeploy scripts; installed on the Linux host as claude (Ruling P5-R4)`.

### Task 4: `fleetd serve --hub`: one merged board, Unreachable machines

Implementer model: **opus** (concurrency + network). Reviewer: **opus**.

**Files:**
- Create: `native/crates/fleetd/src/hub.rs`: one subscriber task per `Remote`, the mirror, the projection, the merged meta.
- Modify: `native/crates/fleetd/src/store.rs`:
  - `apply(&self, upsert: Vec<Session>, remove: Vec<SessionKey>) -> Option<u64>`: one delta, or none when nothing changed;
  - the meta merge: `set_board_meta` sets the **local** part; `set_remote_meta(machine: &str, m: Option<Meta>)` sets a remote's; `with_remote_order(Vec<String>)` fixes the merge order. The published meta is `merge(local, remotes)` by Ruling P5-R1, with health always the local probe's. With no remotes, the published meta equals today's, byte for byte.
- Modify: `native/crates/fleetd/src/main.rs`: `serve --hub`. It takes `Config.hub` (`--hub` alone means "use the config's `hub` list"; an empty list is an error). The hub also implies the listener (`--listen tailscale` unless the config says `loopback`).
- Modify: `native/crates/fleetd/src/client.rs` if Task 1's `connect_tcp` needs a timeout parameter (connect within 5 s).
- Modify: `native/bench/run.mjs`, `native/bench/budgets.json`, `native/bench/README.md`: two hub budgets.
- Modify: `native/PROTOCOL.md` "Network" only: a paragraph "Hub". It covers: what a hub serves; that rows of machine M come only from M's subscription; Unreachable as P5-R1; and that a client of the hub is an ordinary Board client.
- Test: `native/crates/fleetd/tests/hub.rs`.

**Interfaces:**
- Consumes:
  - Task 1: `Client::connect_tcp`, `Config { hub: Vec<Remote> }`, `config::key`. A hub dials a remote's Tailscale address and is authorised there by WhoIs. In tests on loopback it uses the key: the test passes `FLEETD_TEST_REMOTE_KEY_DIR`, debug builds only.
  - `Store::replace`, `Store::upsert`, `fleet_model::decide::hh_mm_utc`.
- Produces:
  - `hub::spawn(store: Arc<Store>, remotes: Vec<Remote>, opts: hub::Options) -> hub::Handle`, where `Options { connect_within: 5s, backoff_min: 1s, backoff_max: 30s, unreachable_after: 3s }`. Debug builds take `FLEETD_TEST_HUB_BACKOFF_MS` and `FLEETD_TEST_HUB_UNREACHABLE_MS`.
  - `hub::unreachable(s: &Session, since_ms: i64) -> Session`: state `Unreachable`, `live = false`, reason per P5-R1 (the state word is `out::state_word`), every other field kept.

**Behaviour the tests pin.** Per remote, the hub keeps a `Mirror { run, seq, sessions: BTreeMap<Key, Session>, meta: Option<Meta> }`, the remote's true board. The `Store` holds a projection of it:
- **connected:** the mirror as is;
- **down for `unreachable_after` or longer:** each mirrored row via `hub::unreachable` (`since` = when it went down);
- a row whose `key.machine` is not the remote's configured `machine` is dropped, and logged once per run.
The store writes for remote M always use `replace(|s| s.key.machine == M, …)` or `apply` with keys of M only. A remote is down when connect fails, a read fails, a seq gap or an `Error` arrives, or TCP keepalive gives up. Then it reconnects with backoff (×2, 1 s → 30 s) and `Hello { since: mirror.seq, run: mirror.run }`:
- a `Delta` (same run) is applied to the mirror;
- a `Snapshot` replaces it (rows gone in the new run are removed from the store);
- then the projection is re-published as connected.
On a remote's `Meta`, `set_remote_meta(M, Some(meta))`. A down remote keeps its last meta (last known).

- [ ] **Step 1: Write the failing tests** in `tests/hub.rs`. Two real `fleetd serve --listen loopback --port 0` remotes over fixture homes, machines `m1` and `m2`, and a hub `fleetd serve --hub` over its own fixture home (machine `h`) with a config listing both. A Unix-socket client reads the hub's board.
  - `merged_board_has_every_machine`: the hub's snapshot holds h's, m1's and m2's sessions, each equal (as bytes) to that machine's own snapshot rows.
  - `remote_change_reaches_the_hub`: a hook write on m1 reaches a hub delta for that row within 500 ms.
  - `meta_merge_p5_r1`:
    - m1's panel dir has `fleet.json` epics `["A","B"]` and labels groups;
    - the hub's `Meta.epics == ["A","B"]` and its groups equal m1's;
    - accounts are m1's, then m2's not already present by `(engine, name)`, then h's;
    - `health` is h's own (`Some` once the hub's own probe samples).
  - `remote_killed_goes_unreachable_not_gone`: SIGKILL m1 (the test's own child). Within `unreachable_after + 1 s` every m1 row is `Unreachable`, with reason matching `^m1 unreachable since \d\d:\d\d; was (working|waiting|needs you|at limit|done|ended): `. The row count is unchanged; m2 and h are untouched.
  - `remote_stopped_without_fin_goes_unreachable`: SIGSTOP m1 (no FIN). With keepalive shortened for the test (`FLEETD_TEST_KEEPALIVE_SECS=1`, debug only), its rows go `Unreachable` within 10 s. SIGCONT: the rows return to their true states with no `Snapshot` sent to the hub (the same run, resumed by `since`, asserted from m1's stderr or a hub debug log line `resumed m1 since <n>`).
  - `remote_restart_new_run_drops_gone_rows`: stop m1, delete one fixture session's files, start m1 again on the same port. The hub's board drops that row, keeps the others, and has no duplicate keys.
  - `remote_cannot_write_other_machines`:
    - a stub remote (a test TCP server speaking the protocol, using `fleet_model::wire`) answers with a `Snapshot` holding rows keyed `h:…`, `m2:…` and `m1:…`;
    - only the `m1:` row lands; h's and m2's rows are byte-for-byte unchanged;
    - a later `Delta` from it that removes `h:` keys removes nothing.
  - `hub_client_resume_works_across_remote_churn`: a hub client resumes with `run`/`since` after m1 went down and came back, and gets a `Delta`, not a `Snapshot`.
  - `idle_hub_is_idle`: with m1 down, no more than one connect attempt per 30 s after the backoff peaks. Use `FLEETD_TEST_HUB_BACKOFF_MS=50`, and assert attempts are spaced ≥ the cap. Count accepted connections on a plain TCP listener the test holds.
  - Unit (in `hub.rs`): `unreachable_keeps_every_field_but_state_live_reason`.
  - Unit (in `store.rs`): `merge_with_no_remotes_is_identity`, `apply_removes_and_upserts_in_one_delta`, `apply_noop_makes_no_delta`.
- [ ] **Step 2: Run** `cargo test -p fleetd --test hub` and the unit tests. Expected: FAIL.
- [ ] **Step 3: Implement** `hub.rs`, the `Store` additions and `serve --hub`. One task per remote, no polling; the only timers are the backoff, the `unreachable_after` grace, and keepalive in the kernel. `Unreachable` is applied by one `replace` per transition, never per row.
- [ ] **Step 4: Run** the tests. Expected: PASS. Run `sh native/check.sh` on the Mac and `sh native/deploy/test-linux.sh redbat-ts`. Expected: both green.
- [ ] **Step 5: Bench, two budgets.** `fleetd bench hub-relay` adds a fixture remote served on loopback TCP and a hub over it, and measures a hook write on the remote to the hub's delta on its Unix socket (200 runs). `hubRelayP95Ms` enters at `min(20, max(measured median p95 × 1.25, 2))`, floor 2. The spec's ceiling is "one round trip + ≤ 20 ms", and this is the "+". `hubIdleCpuMsPer60s` is the hub's CPU over 60 s with one remote connected and nothing changing: budget 50, floor 20, hold 50 (P2-R6). Every existing budget passes unchanged (median of 3).
- [ ] **Step 6: Live (read-only on the Mac; the redbat unit is changed).**
  - Write `/home/claude/.config/fleet/fleetd.json` on redbat as claude: `{"listen":"tailscale","hub":[{"machine":"Mac","addr":"<mac tailscale ip>:4780"}]}`.
  - Re-run `install-linux.sh redbat-ts -- --hub`.
  - On the Mac, start **your own** `fleetd serve --listen tailscale` (release build, `FLEETD_SOCK=$TMPDIR/fleetd-t4.sock`, not the LaunchAgent). Its machine name is Ruling R3's (`Mac`).
  - Check from redbat (`fleetd watch` on the hub socket, as claude): the merged snapshot holds `Mac:` and `redbat:` rows. Counts per machine; the Mac count equals your Mac fleetd's own snapshot.
  - Check from the Mac (`fleetd watch --tcp <redbat tailscale ip>:4780`, allowed by WhoIs): the same merged board, which proves the phone's path.
  - Stop your Mac fleetd (its own pid). Within ~30 s every `Mac:` row on the hub is `Unreachable` with P5-R1's reason, and none is gone.
  - Leave the hub running with the Mac config. It will keep showing the Mac unreachable until the controller turns on the Mac's listener at the switch, which is expected.
  Record every count and the reasons seen (redacted) in the report.
- [ ] **Step 7: Commit** `fleetd: --hub merges each machine's board over the network; Unreachable keeps last state (Ruling P5-R1)`.
