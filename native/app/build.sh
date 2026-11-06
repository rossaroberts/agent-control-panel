#!/bin/bash
# Builds Fleet.app (default ~/Applications/Fleet.app, bundle id dev.fleet.app, ad-hoc signed) from this Swift package:
# the release FleetApp, its SwiftTerm resources, and the icon rendered from the repo's app/icon.svg the way app/build.sh
# renders Agent Panel's. It touches nothing of Agent Panel.app or the Node panel, and never starts fleetd.
#   FLEET_APP=<path>.app   build somewhere else
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="${FLEET_APP:-$HOME/Applications/Fleet.app}"
[[ "$out" == *.app ]] || { echo "FLEET_APP must end in .app" >&2; exit 1; }

(cd "$here" && swift build -c release --product FleetApp)
bin="$(cd "$here" && swift build -c release --show-bin-path)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
app="$work/Fleet.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$bin/FleetApp" "$app/Contents/MacOS/Fleet"
for b in "$bin"/*.bundle; do [[ -e "$b" ]] && cp -R "$b" "$app/Contents/Resources/"; done
swift "$repo/app/render-icon.swift" "$repo/app/icon.svg" "$work/AppIcon.iconset"
iconutil -c icns -o "$app/Contents/Resources/AppIcon.icns" "$work/AppIcon.iconset"

plist="$app/Contents/Info.plist"
printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict/></plist>\n' > "$plist"
str() { plutil -insert "$1" -string "$2" "$plist"; }
str CFBundleIdentifier dev.fleet.app
str CFBundleName Fleet
str CFBundleDisplayName Fleet
str CFBundleExecutable Fleet
str CFBundleIconFile AppIcon
str CFBundlePackageType APPL
str CFBundleShortVersionString 0.3
str CFBundleVersion "$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo 1)"
str LSMinimumSystemVersion 15.0
str LSApplicationCategoryType public.app-category.developer-tools
plutil -insert NSHighResolutionCapable -bool YES "$plist"
plutil -insert NSSupportsAutomaticTermination -bool NO "$plist"
plutil -lint "$plist" >/dev/null

codesign --force --sign - "$app"

mkdir -p "$(dirname "$out")"
rm -rf "$out"
mv "$app" "$out"
echo "Built $out"
