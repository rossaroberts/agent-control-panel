# fleetd bench

`node-before.mjs` measures the current Node panel (`server.mjs`) so the native port has "before" numbers. It starts
its own panel on port 4797 over a made-up home with its own prefs folder; it never touches the live panel on 4747.

    node native/bench/node-before.mjs     # about 4 minutes; writes results/before.json

Measures: hook file written to the session row flipping in `POST /api/fleet` (50 runs, polled every 5 ms), snapshot
latency (50 runs), and, once the panel's warm reads have lapsed, total CPU time over 60 s (`idleCpuMsPer60s`;
`idleCpuPct` is that over 600) and median RSS.

## The gate

    cargo build --release -p fleetd && node native/bench/run.mjs      # about 2.5 minutes; exit 1 if any p95 is over budget
    node native/bench/run.mjs --ratchet                                # then lower budgets.json; commit it

`run.mjs` starts `fleetd serve` on a temp socket over `fixture.mjs`'s home: 400 sessions (40 live, so on the kqueue hot
path), transcripts of 20-110 KB with eight of 1-5 MB, and 600 files in one profile's `fleet-state/` (the real one has
582). The timing is done by the hidden `fleetd bench <hook|transcript|snapshot>` subcommand, which is a socket client:
the clock sits on one side of the socket, with no postcard decoder in Node. It measures hook write to delta (200),
transcript append to delta (200), snapshot on connect (50), and, with one client connected, total server CPU time over
60 s (`idleCpuMsPer60s`, Ruling R5) and median RSS. It prints a before/after table against `results/before.json` and
writes `results/latest.json`.

`--ratchet` sets each budget to max(worst p95 of three load runs x 1.25, floor), never above what it was, and leaves a
budget that is currently over alone. `budgets.json` holds each budget and its floor. A budget only moves down.
Only `results/before.json` is committed.
