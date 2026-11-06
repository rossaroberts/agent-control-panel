# fleetd socket protocol (PROTO 4)

The contract between `fleetd` and its clients (the Swift app, `fleetd watch`, the bench). The Rust definition is
`crates/fleet-model/src/wire.rs`; the bytes are pinned by the golden frames in `crates/fleet-model/tests/golden/`.

## Transport

- A Unix socket, mode 0600: `~/Library/Application Support/Fleet/fleetd.sock` on macOS,
  `$XDG_RUNTIME_DIR/fleetd.sock` (else `~/.local/state/fleet/fleetd.sock`) on Linux, or `FLEETD_SOCK`.
- Each frame is a `u32` little-endian body length, then the body: one [postcard](https://postcard.jamesmunns.com/)
  encoded `Frame`. Postcard is positional (no field names), integers are varints, `Option` is a `0`/`1` tag then the
  value, strings and lists are a varint length then their items, an enum is a varint variant index then its fields.
- A body over 64 MiB is refused. The server reads at most 4 KiB for a `Hello`.

## Frames

Variant indexes are part of the format:

| # | Frame | Direction | Fields, in order |
|---|---|---|---|
| 0 | `Hello` | client to server, first and only | `proto: u16`, `since: Option<u64>`, `run: Option<u64>`, `role: Role` |
| 1 | `Snapshot` | server to client | `run: u64`, `seq: u64`, `machine: String`, `sessions: Vec<Session>` |
| 2 | `Delta` | server to client | `seq: u64`, `upsert: Vec<Session>`, `remove: Vec<SessionKey>` |
| 3 | `Error` | server to client, then close | `message: String` |
| 4 | `TermOpen` | client to server (Terminals) | `ref: u32`, `target: AttachTarget`, `cols: u16`, `rows: u16` |
| 5 | `TermIn` | client to server (Terminals) | `ref: u32`, `data: Vec<u8>` (at most 64 KiB) |
| 6 | `TermSize` | client to server (Terminals) | `ref: u32`, `cols: u16`, `rows: u16` |
| 7 | `TermClose` | client to server (Terminals) | `ref: u32` |
| 8 | `TermOut` | server to client (Terminals) | `ref: u32`, `data: Vec<u8>` |
| 9 | `TermEnd` | server to client (Terminals) | `ref: u32`, `error: Option<String>` |
| 10 | `Meta` | server to client (Board) | `epics: Vec<String>`, `groups: Vec<String>`, `accounts: Vec<Account>`, `health: Option<Health>` |

`Session`, `SessionKey`, `Engine` and `State` are in `crates/fleet-model/src/types.rs`, fields in declaration order.
Enums are sent by variant index (`State`: 0 NeedsYou, 1 Working, 2 Waiting, 3 Capped, 4 Done, 5 Ended, 6 Unreachable;
`Engine`: 0 Claude, 1 Codex, 2 Muse; `Role`: 0 Board, 1 Terminals). `Session` ends with `job_id: Option<String>`: the
roster's 8-hex `id` of a background job (what `claude attach` takes), `None` for an interactive session; then, new in
proto 4, `title: Option<String>`, `epic: Option<String>`, `archived: bool`, `account: Option<String>` (below).

`Account` is `name: String`, `engine: Engine`, `profile: String`, `limits: Vec<Limit>`; `Limit` is `window: String`
(`5h`, `7d`, or `<n>h`), `used_pct: u8` (0-100), `resets_at: Option<i64>` (epoch milliseconds); `Health` is `cpu_pct: u8`,
`mem_pct: u8`, `disk_pct: u8`, `dev: u16`, `db: u16`.

`AttachTarget` (what `TermOpen` attaches to): 0 `Claude { profile: String, job: String }`, 1
`Codex { profile: String, session_id: String, cwd: String }`, 2 `Trust { profile: String, dir: String }`.

Two things never change, whatever the version, so that any client can be told it is out of date: `Hello` is variant 0
and its first field is `proto`; `Error` is variant 3 with one `message` string.

## The conversation

1. The client connects and sends `Hello { proto: 4, since, run, role }` within 10 s.
2. The server answers with one of:
   - `Error` naming the versions (`protocol 3 not supported; this is proto 4`), then closes, when `proto` is not 4. A
     `Hello` of an older shape is still read as far as its `proto`.
   - `Error` (`malformed Hello`) when the bytes of a `proto` 4 `Hello` do not decode.
   - `Error` (`expected Hello`) when the first frame is not a `Hello`.
   - nothing, when `role` is `Terminals`: the connection is a Terminals connection from here on (below).
   - a `Delta` holding everything changed since `since` (each session as it is now, each one gone), when `since` is
     given **and `run` equals this daemon's run** and the server still holds the deltas back to `since`. When the
     client is already current that delta is empty (`upsert` and `remove` both empty, `seq` = the client's `since`):
     the "caught up" acknowledgement.
   - otherwise a `Snapshot`: the whole board.
3. For `role: Board`, then a `Meta` (below), then every change, as a `Delta` or a `Meta`, for as long as the client stays
   connected. The client sends nothing after `Hello`; the server treats a closed socket as the client gone.

## Board metadata: `Session` fields and `Meta`

`fleetd` reads, and never writes, the Node panel's files in `~/Library/Application Support/Agent Panel/` (or
`FLEETD_PANEL_DIR`): `fleet.json`, `fleet-titles.json`, `fleet-labels.json`, `fleet-archived.json`. Each is optional, and a
missing, truncated or non-JSON one counts as empty. A session is keyed `<machine>:<session_id>` in them.

- **`title`** is `fleet.mjs`'s: your label's `title`, else (a Claude session only) Claude Code's own `name` unless it is a
  made-up placeholder (`<folder>-<two hex digits>`), else the written title, else `name`.
- **`epic`** is your label's `group`, else the written epic.
- **`archived`** is true when you archived the session and it is not live-and-started-since, nor needs you; the board does
  not show an archived row.
- **`account`** is the display name of the account the session runs under: `fleet.json` `names["claude:<email>"]` (or
  `codex:<email>`), else the email's local part; `None` for Muse and where no email is known.
- A change to any of the four files re-sends the sessions it changes, as a `Delta`, within milliseconds.
- **`Meta`** is sent right after a board connection's first frame (the `Snapshot`, or the caught-up `Delta`), again after
  every `Snapshot` sent later, and whenever it changes. `epics` are `fleet.json`'s, in its order; `groups` are the groups
  you made (labels' groups that are not an epic), sorted; `accounts` hold one per engine and email with the newest plan
  limits its status lines (Claude Code) or logs (Codex) reported. A `Limit` is as last reported: a client shows a window
  whose `resets_at` has passed as 0 used. `health` is `None` until the machine's health is read.
  The daemon samples it only while at least one board connection is open: the first sample at once when a client
  connects, then every 5 s. `cpu_pct` is the one-minute load over the cores (held to 100), `mem_pct` active + wired +
  compressor memory over the installed memory (Linux: total less available), `disk_pct` how full the home folder's
  volume is, `dev` the ports listened on by `node`/`next*`/`vite`/`bun`/`deno` processes and `db` the running
  `supabase_db_*` containers, both counted as `fleet/machine.mjs` counts them and asked at most every 30 s. A Meta is sent
  when any of these moves. A client that connects after a quiet spell gets the last health held, then the fresh one.

## Terminals connections (`role: Terminals`)

A separate connection from the board, opened by a client that wants ptys; board and terminal connections are fully
independent (a terminal flood never delays a board's deltas). The server sends nothing after the `Hello` until the client
asks. Frames carry a client-chosen `ref` naming one terminal of this connection: a `ref` is greater than 0, and a
frame with `ref` 0 is ignored (no answer, as `term.mjs`).

- **`TermOpen { ref, target, cols, rows }`** opens a terminal: the server answers with `TermOut` frames (the program's
  output, in order, never dropped) and, when it is over, one `TermEnd { ref, error: None }`. An open that cannot be
  served is answered with `TermEnd { ref, error }`, `error` being `term.mjs`'s words (`Pick a background session, or a
  folder that exists.`, `Too many terminals open.`, `Too many terminals open in this tab.`, `It is open in another
  Fleet terminal already.`, or the busy reason). A reopen of a kept-warm terminal whose program ended while it was warm
  is sent what that program last printed and then `TermEnd { ref, error: "The session has ended." }`; the open after
  that starts it afresh. A reopen of a Codex thread right after its terminal was closed waits up to 500 ms for that
  program to be gone, rather than be refused as open in Codex. That `TermEnd` comes in order with the connection's output: after
  everything already sent for an earlier terminal of the same `ref` (its output, then its own `TermEnd`). A `ref` already open on this connection is left as it is (no answer).
  The server handles the open before it reads the next frame, so a `TermIn` sent right behind it finds it open. At most
  8 terminals per connection and 24 in all; cols are clamped to 20-400 and rows to 5-200.
- **`TermIn { ref, data }`** is keystrokes, **at most 64 KiB** (65,536 bytes) of `data`. A `TermIn` of more is a client
  bug: the server answers `Error` (`TermIn of <n> bytes; at most 65536`) and closes the connection (its terminals are
  released, as at any hang-up), and none of that frame's bytes reach the program. Send a paste in pieces.
- **`TermSize { ref, cols, rows }`**: the window changed; sizes are coalesced to at most one per 16 ms (the latest).
- **`TermClose { ref }`** releases the terminal: the server answers `TermEnd { ref, error: None }`. Closing ends only the
  attach, never the session: a `claude attach` terminal is kept warm for a reopen, Codex and trust-folder ones end.
- A `TermIn`, `TermSize` or `TermClose` for a `ref` that is not open (never opened, closed, or ended) is ignored: no
  answer, no error.
- Hanging up releases every terminal of the connection, as `TermClose` would, without the `TermEnd`s.
- Any other frame, a body that does not decode, or a body over 65 KiB (64 KiB of input plus framing) is answered with an
  `Error` and the connection closes.

**Backpressure, both ways; nothing is dropped.**

- Output: when more than 1 MiB of a connection's output is waiting to be written to it, the server stops reading every
  pty of that connection until it is down to 512 KiB, so the programs block on their output (refusals' words count
  toward the 1 MiB too, and past it the server stops reading the client). A slow reader is kept and never costs
  unbounded memory. A client whose output is over 1 MiB and that has taken nothing at all for 30 s is closed (Ruling
  P2-R5, as the board's 30 s write limit); its terminals are released as at a hang-up.
- Input: when a terminal holds 1 MiB of input its program has not read, the server keeps the `TermIn` that did not fit
  and **stops reading this connection's frames** until it fits (offered again every 5 ms while held, and only then): a
  `TermSize`, `TermClose` or another terminal's `TermIn` behind it waits too, and the client's own writes block once the
  socket's buffer is full. If that terminal ends while its input is held, the held frame is let go and reading resumes.
  If the client hangs up while its input is held, the server notices (even with frames of its still unread) and
  releases its terminals.

## Rules a client relies on

- **`run`** is random at each daemon start. A `seq` counts within one run only: after the daemon restarts, `seq` begins
  again at 0. A client resumes with the `run` of the last `Snapshot` it was sent and the `seq` of the last frame it
  applied. From another run it is sent a `Snapshot`, never a delta over a board of the old run.
- **`seq`**: a `Snapshot` carries the `seq` of the last delta it includes. After the first frame, deltas arrive with
  consecutive `seq`s (`seq` = previous + 1). A gap means the client lost track: it should reconnect with its
  `run`/`seq` (or with neither) and take what it is sent.
- **A `Snapshot` may arrive at any time**, not only first: a client that falls more than 4096 deltas behind (it reads
  too slowly) is sent a fresh `Snapshot`, which replaces its board whole. Deltas then continue from that snapshot's
  `seq`.
- **Applying a delta**: replace or add each session in `upsert` by its key (`machine`, `profile`, `session_id`), and
  drop each key in `remove`.
- **`Unreachable` is never produced by `fleetd` for its own machine.** It is the state a client gives a session of a
  machine it cannot reach (another Mac's daemon, a lost connection); a local board never holds it.
- A write the client takes nothing of for 30 s ends the connection (on a Terminals connection, only while its output
  is over 1 MiB).
- **On a Terminals connection, read and write on independent paths** (two tasks, two threads, or one non-blocking
  loop). A write may block for as long as a program does not read its input (backpressure, above), and output keeps
  coming meanwhile: a client that writes and reads on one serial path deadlocks, and once its unread output is over
  1 MiB for 30 s it is closed.
- **A `TermIn` of more than 64 KiB closes the connection and every terminal on it.** Split input (a paste) into pieces
  of at most 65,536 bytes.
- When an `Error` closes a Terminals connection, output still queued for it and not yet written (`TermOut`,
  `TermEnd`) is discarded: the `Error` goes out ahead of it, as the last frame.

## Network

The same conversation over TCP, for a phone on the tailnet, another machine's hub, or a local tool. **Off by default**:
`fleetd serve` listens on TCP only with `--listen loopback|tailscale` or the config's `"listen"`.

- **Config**: `fleetd.json` (`~/Library/Application Support/Fleet/` on macOS; `$XDG_CONFIG_HOME/fleet/`, else
  `~/.config/fleet/`, on Linux; or `--config FILE`): `{"listen": "off"|"loopback"|"tailscale", "port": 47470, "hub": [...]}`.
  A missing file is `off`, port 47470; a malformed one stops the daemon. `--listen` and `--port` override the file.
- **Port 47470**, the same on every address bound.
- **Two addresses at most**, IPv4 only: `127.0.0.1:47470` (`loopback` and `tailscale`), and this machine's Tailscale IPv4
  from `tailscale ip -4` (`tailscale`), bound only when it lies in `100.64.0.0/10` and is assigned to a local interface.
  Never `0.0.0.0`, never another interface, never IPv6. While the Tailscale address cannot be had (Tailscale down, not
  logged in), it is tried again every 30 s; loopback serves meanwhile.
- **Who is allowed**, decided before a byte of the peer is read as a frame:
  - **local**: a peer whose source address is this machine's own (loopback, an interface's, the Tailscale address, or
    `Self.TailscaleIPs`), on either listener: a process here that dials the Tailscale address is local too (Ruling
    P5-R9). It sends the key preamble with the right key. On Linux, also a peer whose socket belongs to the daemon's own
    uid (read from `/proc/net/tcp`), with no key. On macOS there is no cheap per-socket uid, so a local peer always needs
    the key (Ruling P5-R2).
  - **Tailscale**: a peer whose node is untagged and whose user is this machine's own Tailscale user (`Self.UserID`),
    asked of `tailscale whois --json <ip>:<port>` with a 2 s limit (Ruling P5-R3). The nodes `tailscale status --json`
    lists when the listener starts are allowed at once by the same rule, with no call (Ruling P5-R7); this machine's own
    node never is. A decision is kept 60 s per peer IP, allow and deny alike; a kept allow older than that is used at
    once and asked again in the background, and if the answer is now a deny every connection from that IP is closed. No
    CLI: every Tailscale peer is refused. A Tailscale peer sends no key.
  - at most 16 connections are being decided or refused at once (a refusal holds its place until its socket is gone);
    past that a connection is refused (`busy`) at once.
- **The key preamble** (transport, not a `Frame`): the 4 bytes `FKEY`, then the 32 key bytes, before the `Hello`. Read as a
  frame length, `FKEY` is ~1.26 GB, over the 64 MiB limit, so it is never mistaken for a frame. The key is
  `fleetd.key` beside `fleetd.json`: 64 hex characters, made 0600 on first use; a key file of looser mode or other content
  is an error, never rewritten. A local tool reads it from there.
- **After auth**, the conversation is exactly the Unix socket's: the same framing, `Hello` first within 10 s, both roles
  (`Board` and `Terminals`), every rule above.
- **A refusal** is one `Error { message: "not allowed: <why>" }` (`loopback needs the key`, `wrong key`, `a tagged node`,
  `another tailnet user`, `whois timed out`, `not a tailnet address`, `no tailscale CLI`, `busy`, ...), then the server
  closes. Nothing the peer sent is read as a frame. The daemon logs `fleetd: tcp refused <ip>: <why>` at most once per
  peer per minute, never a key.
- **Keepalive and latency**: both ends set `TCP_NODELAY` and TCP keepalive (idle 15 s, then a probe every 5 s, 3 probes):
  a peer gone without a FIN is noticed in about 30 s.
- **A dropped TCP connection is a lost socket**: the client reconnects with `run`/`since` (Rules a client relies on) and
  takes what it is sent.

How a client connects:

- **The phone, over Tailscale**: dial `<the Mac's Tailscale IPv4>:47470`, send `Hello { proto: 4, since, run, role: Board }`
  (no preamble), read the `Snapshot` (or the caught-up `Delta`), the `Meta`, then deltas. A second connection with
  `role: Terminals` drives terminals. On a drop, reconnect with the last `run` and `seq`.
- **A local tool with the key**: dial `127.0.0.1:47470`, write `FKEY`, the 32 bytes of `fleetd.key` (decoded from hex), then
  the `Hello`, in one write; then read as over the Unix socket. `fleetd watch --tcp <host:port>` connects without a key
  (a Tailscale peer).

## Versioning

Any change to the bytes of a frame (a field added, removed or moved, a variant added anywhere but the end, a type
changed) is a `PROTO` bump, with new golden files (`FLEET_BLESS=1 cargo test -p fleet-model --test golden`, then review
the diff). Version history: 1, the first; 2, `run` in `Hello` and `Snapshot` (Ruling R24); 3, `role` in `Hello`, the
terminal frames, `job_id` in `Session`; 4, `title`, `epic`, `archived`, `account` in `Session`, and the `Meta` frame.
