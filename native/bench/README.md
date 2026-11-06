# fleetd bench

`node-before.mjs` measures the current Node panel (`server.mjs`) so the native port has "before" numbers. It starts
its own panel on port 4797 over a made-up home with its own prefs folder; it never touches the live panel on 4747.

    node native/bench/node-before.mjs     # about 4 minutes; writes results/before.json

Measures: hook file written to the session row flipping in `POST /api/fleet` (50 runs, polled every 5 ms), snapshot
latency (50 runs), and, once the panel's warm reads have lapsed, total CPU time over 60 s (`idleCpuMsPer60s`;
`idleCpuPct` is that over 600) and median RSS.

`budgets.json` holds each budget and its floor. A budget only moves down (the ratchet). Only `results/before.json` is committed.
