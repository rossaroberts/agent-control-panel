#!/bin/bash
# The app's launch budgets (spec §9.3, Rulings P3-R6 and P3-R7), as plan 1's bench gates the daemon's. Fleet.app's own
# --floor mode (one empty AppKit window: what any app pays on this machine right now) and its --bench mode (the board)
# run interleaved, one warm-up pair then 3 pairs, and the medians of the 3 are gated relative to the floor:
#   lastBoardMs ≤ floorMs + 60    launch to the last board on screen (from the board cache)
#   liveMs      ≤ floorMs + 100   launch to the live board on screen (against a warm fleetd)
# The absolute 120 / 200 ms are reported, not gated. The medians go to native/bench/results/app-launch.json
# (FLEET_BENCH_OUT to put them elsewhere). Run against a running fleetd (FLEETD_SOCK to pick one); builds the app first
# unless FLEET_BENCH_NOBUILD=1.
# Then the interactions (Ruling ALL-R1): its --ui-bench mode (a synthetic board, an in-process echo pty; no fleetd)
# runs 3 times, and every action's median of the 3 runs' medians is gated at 16 ms (one frame): a warm switch (⌘[ ⌘],
# ↑/↓, ⌘1), a cold open's "connecting…", Return and ⌃`, a key's echo, ⌘K and each keystroke, ⌘⇧F, a delta. Return
# typed in a terminal must reach its program. Results to native/bench/results/app-ui.json (FLEET_BENCH_UI_OUT).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
app="${FLEET_APP:-$HOME/Applications/Fleet.app}"
out="${FLEET_BENCH_OUT:-$here/../bench/results/app-launch.json}"
[[ "${FLEET_BENCH_NOBUILD:-}" == 1 ]] || "$here/build.sh" >/dev/null
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
bin="$app/Contents/MacOS/Fleet"
for i in 0 1 2 3; do
  "$bin" --floor "$work/floor$i.json" >/dev/null 2>&1
  "$bin" --bench "$work/run$i.json" >/dev/null 2>&1
done
launch=0 ui=0
/usr/bin/python3 - "$work" "$out" <<'PY' || launch=1
import json, statistics, sys, pathlib
work, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
runs = [json.loads((work / f"run{i}.json").read_text()) for i in (1, 2, 3)]
floors = [json.loads((work / f"floor{i}.json").read_text())["floorMs"] for i in (1, 2, 3)]
floor = statistics.median(floors)
print(f"floorMs: median {floor:.1f} ms of {[round(v, 1) for v in floors]}")
result = {"floorMs": floor, "floors": floors}
ok = True
for k, over, absolute in (("lastBoardMs", 60, 120), ("liveMs", 100, 200)):
    vals = [r.get(k) for r in runs]
    if any(v is None for v in vals) or not all(r.get("fromCache") for r in runs):
        print(f"{k}: not measured {vals} (is fleetd running? was there a board cache?)"); ok = False; continue
    m = statistics.median(vals)
    gate = floor + over
    result[k] = m
    result[k.replace("Ms", "Runs")] = vals
    print(f"{k}: median {m:.1f} ms of {[round(v, 1) for v in vals]}; gate floor+{over} = {gate:.1f} "
          f"{'ok' if m <= gate else 'OVER'}; absolute {absolute} {'met' if m <= absolute else 'not met'} (reported only)")
    ok &= m <= gate
result["phases"] = runs[1].get("phases", {})
result["ok"] = ok
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
sys.exit(0 if ok else 1)
PY
for i in 1 2 3; do "$bin" --ui-bench "$work/ui$i.json" >/dev/null 2>&1; done
/usr/bin/python3 - "$work" "${FLEET_BENCH_UI_OUT:-$here/../bench/results/app-ui.json}" <<'PY' || ui=1
import json, statistics, sys, pathlib
work, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
runs = [json.loads((work / f"ui{i}.json").read_text()) for i in (1, 2, 3)]
budget, ok, result = 16.0, True, {}
for k in sorted(k for k in runs[0] if isinstance(runs[0][k], (int, float)) and not isinstance(runs[0][k], bool)):
    vals = [r.get(k) for r in runs]
    if any(v is None for v in vals):
        print(f"{k}: not measured {vals}"); ok = False; continue
    m = statistics.median(vals)
    result[k] = m
    print(f"{k:16} median {m:6.2f} ms of {[round(v, 2) for v in vals]} {'ok' if m <= budget else 'OVER 16'}")
    ok &= m <= budget
for r in runs:
    if r.get("misses"): print(f"missed: {r['misses']}"); ok = False
reaches = all(r.get("returnReachesTerminal") for r in runs)
print(f"Return in a terminal reaches its program: {reaches}")
ok &= reaches
result["returnReachesTerminal"] = reaches
result["ok"] = ok
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
sys.exit(0 if ok else 1)
PY
exit $(( launch || ui ))
