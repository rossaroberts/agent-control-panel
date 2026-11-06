# fleetd bench

`node-before.mjs` measures the current Node panel (`server.mjs`) so the native port has "before" numbers. It starts
its own panel on port 4797 over a made-up home with its own prefs folder; it never touches the live panel on 4747.

    node native/bench/node-before.mjs     # about 4 minutes; writes results/before.json

Measures: hook file written to the session row flipping in `POST /api/fleet` (50 runs, polled every 5 ms), snapshot
latency (50 runs), and, once the panel's warm reads have lapsed, total CPU time over 60 s (`idleCpuMsPer60s`;
`idleCpuPct` is that over 600) and median RSS.

## The gate

    cargo build --release -p fleetd && node native/bench/run.mjs      # about 5 minutes; exit 1 if any budget is over
    node native/bench/run.mjs --ratchet                                # then lower budgets.json; commit it
    node --test native/bench/ratchet.test.mjs                          # the ratchet rules

`run.mjs` starts `fleetd serve` on a temp socket over `fixture.mjs`'s home: 400 sessions (40 live, so on the kqueue hot
path), transcripts of 20-110 KB with eight of 1-5 MB, and 600 files in one profile's `fleet-state/` (the real one has
582). The timing is done by the hidden `fleetd bench <hook|transcript|snapshot>` subcommand, a socket client: the clock
sits on one side of the socket, with no postcard decoder in Node. It refuses to run unless `FLEETD_HOME` is set, and
touches only a profile and a transcript under it. It measures hook write to delta (200), transcript append to delta
(200), snapshot on connect (50), and, with one client connected, total server CPU time over a 60 s window
(`idleCpuMsPer60s`, Ruling R5) and median RSS.

The whole measurement runs three times, each over a fresh server, and each budget is compared with the median of the
three p95s (Ruling R19). It prints a before/after table against `results/before.json` and writes `results/latest.json`
(with the number of rounds and the idle window). `--idle-secs N` under 60 needs `--quick`: a one-round smoke run that
is never a gate and cannot ratchet.

`--ratchet` sets each budget to max(median p95 x 1.25, floor), never above what it was, and leaves a budget that is
currently over alone (`ratchet.mjs`). A budget may also carry `hold`, a minimum the ratchet never goes below (idle CPU holds at 50, Ruling P2-R6). `budgets.json` holds each budget and its floor; the idle floor is 20 ms because
`ps` reports CPU time in 10 ms steps, and the latency floors are 2 ms (hook, transcript, snapshot) and RSS 20 MB so that
a ratchet never lands inside the run-to-run spread (Ruling R23). A budget only moves down. Only `results/before.json` is committed.

## Terminals (plan 2)

`fleetd bench term-echo | term-warm | term-cold` are further modes of the same hidden subcommand, with the same
`FLEETD_HOME` rules (it refuses to run without it, and refuses your real home); they write no files, and open terminals
on the daemon, whose programs are the fixture home's stand-in `claude attach <job>` (`fixture.mjs`: a job id starting
with `e` runs `stty raw -echo; exec cat`, any other prints a banner and then cats). `run.mjs` runs them after the idle window.

- `termEchoPairedAddedP95Ms`: one keystroke to its echo through fleetd, against the same `cat` on a pty of the bench's own,
  sampled in turn (300 pairs); the budget is the p95 of the paired differences (through_i minus direct_i), not the difference of the two p95s. Floor 0.5 ms (Ruling P2-R1).
- `termWarmReopenP95Ms`: a released `claude attach` terminal reopened, to the first replayed byte (50). Floor 8 ms.
- `termColdOpenP95Ms`: a terminal that never ran, to its first byte (30, a new job each time). Floor 50 ms.

Same median-of-3 gate and ratchet as above.

## Not in a budget

A session that is neither live nor recently written is not followed by kqueue. Its first event after going dormant
arrives through FSEvents, about 13 ms (Ruling R21). That path is not in any budget: the fixture's dormant sessions are
there to size the reconcile and the startup read, not to time that event.
