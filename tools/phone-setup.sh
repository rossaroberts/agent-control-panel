#!/bin/sh
# Put the Fleet page on your phone: Tailscale Serve in front of the panel, the public name in fleet.json, a pairing QR.
# Run after Tailscale is installed and signed in on this Mac (App Store or brew cask) and on the phone.
# Usage: sh tools/phone-setup.sh [port]   (default 4747)
set -eu
PORT="${1:-4747}"
TS=""
for c in /Applications/Tailscale.app/Contents/MacOS/Tailscale /opt/homebrew/bin/tailscale /usr/local/bin/tailscale; do
  [ -x "$c" ] && TS="$c" && break
done
[ -n "$TS" ] || { echo "Tailscale is not installed. App Store: open 'macappstore://apps.apple.com/app/id1475387142'  or: brew install --cask tailscale-app"; exit 2; }
if ! "$TS" status >/dev/null 2>&1; then
  echo "Tailscale is installed but not signed in. Open Tailscale from the menu bar and log in, then run this again."; exit 3
fi
NAME="$("$TS" status --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["Self"]["DNSName"].rstrip("."))')"
[ -n "$NAME" ] || { echo "Could not read this Mac's tailnet name (is MagicDNS on in the admin console?)"; exit 4; }
echo "tailnet name: $NAME"
# HTTPS needs certificates enabled once in the admin console (DNS → HTTPS Certificates). serve tells us if not.
if ! "$TS" serve --bg "$PORT" 2>&1 | tee /tmp/ts-serve.out | grep -qi "available\|serving\|https"; then
  if grep -qi "https\|cert" /tmp/ts-serve.out; then echo "Enable HTTPS certificates in the Tailscale admin console (DNS tab), then run again."; cat /tmp/ts-serve.out; exit 5; fi
fi
"$TS" serve status 2>/dev/null | head -5
PREFS="$HOME/Library/Application Support/Agent Panel/fleet.json"
python3 - "$PREFS" "$NAME" <<'PY'
import json, sys, os
p, name = sys.argv[1], sys.argv[2]
d = json.load(open(p)) if os.path.exists(p) else {}
if d.get("public") != name:
    d["public"] = name
    json.dump(d, open(p, "w"), indent=2); print("fleet.json public =", name)
else:
    print("fleet.json already has public =", name)
PY
launchctl kickstart -k "gui/$(id -u)/local.agent-panel" 2>/dev/null && sleep 2 && echo "panel restarted" || echo "restart the panel yourself (node server.mjs)"
code=$(curl -s -o /dev/null -w '%{http_code}' "https://$NAME/manifest.webmanifest" || true)
echo "https://$NAME/manifest.webmanifest -> $code"
echo
echo "Now scan this on the phone (one-time code, 2 minutes):"
cd "$(dirname "$0")/.." && node server.mjs pair
