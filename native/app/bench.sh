#!/bin/bash
# The app's launch budgets (spec §9.3), as plan 1's bench gates the daemon's: Fleet.app's own --bench mode, run 1 + 3
# times (the first warms the disk cache and writes the board cache), the median of the 3 against the budget.
#   launch to the last board on screen  ≤ 50 ms   (lastBoardMs, from the board cache)
#   launch to the live board on screen  ≤ 200 ms  (liveMs, against a warm fleetd)
# Run against a running fleetd (FLEETD_SOCK to pick one). Builds the app first unless FLEET_BENCH_NOBUILD=1.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
app="${FLEET_APP:-$HOME/Applications/Fleet.app}"
[[ "${FLEET_BENCH_NOBUILD:-}" == 1 ]] || "$here/build.sh" >/dev/null
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
for i in 0 1 2 3; do
  "$app/Contents/MacOS/Fleet" --bench "$work/run$i.json" >/dev/null 2>&1
done
/usr/bin/python3 - "$work" <<'PY'
import json, statistics, sys, pathlib
runs = [json.loads((pathlib.Path(sys.argv[1]) / f"run{i}.json").read_text()) for i in (1, 2, 3)]
budgets = {"lastBoardMs": 50, "liveMs": 200}
ok = True
for k, b in budgets.items():
    vals = [r.get(k) for r in runs]
    if any(v is None for v in vals) or not all(r.get("fromCache") for r in runs):
        print(f"{k}: not measured {vals} (is fleetd running? was there a board cache?)"); ok = False; continue
    m = statistics.median(vals)
    print(f"{k}: median {m:.1f} ms of {[round(v, 1) for v in vals]} (budget {b}) {'ok' if m <= b else 'OVER'}")
    ok &= m <= b
sys.exit(0 if ok else 1)
PY
