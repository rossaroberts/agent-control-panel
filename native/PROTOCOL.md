# fleetd socket protocol (PROTO 3)

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

`Session`, `SessionKey`, `Engine` and `State` are in `crates/fleet-model/src/types.rs`, fields in declaration order.
Enums are sent by variant index (`State`: 0 NeedsYou, 1 Working, 2 Waiting, 3 Capped, 4 Done, 5 Ended, 6 Unreachable;
`Engine`: 0 Claude, 1 Codex, 2 Muse; `Role`: 0 Board, 1 Terminals). `Session` ends with `job_id: Option<String>`: the
roster's 8-hex `id` of a background job (what `claude attach` takes), `None` for an interactive session.

`AttachTarget` (what `TermOpen` attaches to): 0 `Claude { profile: String, job: String }`, 1
`Codex { profile: String, session_id: String, cwd: String }`, 2 `Trust { profile: String, dir: String }`.

Two things never change, whatever the version, so that any client can be told it is out of date: `Hello` is variant 0
and its first field is `proto`; `Error` is variant 3 with one `message` string.

## The conversation

1. The client connects and sends `Hello { proto: 3, since, run, role }` within 10 s.
2. The server answers with one of:
   - `Error` naming the versions (`protocol 2 not supported; this is proto 3`), then closes, when `proto` is not 3. A
     `Hello` of an older shape is still read as far as its `proto`.
   - `Error` (`malformed Hello`) when the bytes of a `proto` 3 `Hello` do not decode.
   - `Error` (`expected Hello`) when the first frame is not a `Hello`.
   - `Error` (`terminals not available yet`) when `role` is `Terminals`, until the terminal service lands.
   - a `Delta` holding everything changed since `since` (each session as it is now, each one gone), when `since` is
     given **and `run` equals this daemon's run** and the server still holds the deltas back to `since`. When the
     client is already current that delta is empty (`upsert` and `remove` both empty, `seq` = the client's `since`):
     the "caught up" acknowledgement.
   - otherwise a `Snapshot`: the whole board.
3. For `role: Board`, then every change, as a `Delta`, for as long as the client stays connected. The client sends nothing after `Hello`;
   the server treats a closed socket as the client gone.

## Terminals connections (`role: Terminals`)

A separate connection from the board, opened by a client that wants ptys. Frames carry a client-chosen `ref` naming one
terminal; the client sends `TermOpen { ref, target, cols, rows }` and the server answers with `TermOut` frames (the
program's output, in order, never dropped) and, when it is over, one `TermEnd { ref, error }`. `TermIn` (at most 64 KiB
of keystrokes), `TermSize` and `TermClose` go the other way. A `TermOpen` that cannot be served is answered with a
`TermEnd` carrying `error`. Closing a terminal ends only the attach, never the session. The server reads at most 65 KiB (64 KiB of input plus framing)
per client frame on this connection. Until the service lands the server answers this role with an `Error`.

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
- A write the client cannot keep up with for 30 s ends the connection.

## Versioning

Any change to the bytes of a frame (a field added, removed or moved, a variant added anywhere but the end, a type
changed) is a `PROTO` bump, with new golden files (`FLEET_BLESS=1 cargo test -p fleet-model --test golden`, then review
the diff). Version history: 1, the first; 2, `run` in `Hello` and `Snapshot` (Ruling R24); 3, `role` in `Hello`, the
terminal frames, `job_id` in `Session`.
