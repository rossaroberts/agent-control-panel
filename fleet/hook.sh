#!/bin/sh
# Fleet's hook: what a Claude Code session is doing, the moment it changes. Each event of the main conversation (a
# subagent's carry an agent_id and are left out, as are notifications that say nothing about its state) leaves one small
# file, <profile>/fleet-state/<session>.json, replaced whole; Fleet reads it (and watches the folder).
#
# It must never get in a session's way: it prints nothing (a hook's output can reach Claude), always exits 0, and takes a
# few milliseconds (one jq). Installed by fleet/install-hooks.mjs for the events in its list, with a 5 s timeout.
J=$(command -v jq 2>/dev/null || { [ -x /opt/homebrew/bin/jq ] && echo /opt/homebrew/bin/jq; } || { [ -x /usr/local/bin/jq ] && echo /usr/local/bin/jq; })
[ -n "$J" ] || exit 0
line=$("$J" -r 'select((.agent_id // "") == "")
  | select(.hook_event_name != "Notification" or ((.notification_type // "") | test("^(permission_prompt|idle_prompt|elicitation_dialog|elicitation_url_dialog)$")))
  | "\(.session_id // "")\t\({e: .hook_event_name, t: (.tool_name // null), n: (.notification_type // null),
       r: (.reason // .source // .trigger // null), i: (.is_interrupt // null), at: (now * 1000 | floor)} | tojson)"' 2>/dev/null) || exit 0
s=${line%%"	"*}
j=${line#*"	"}
case "$s" in ''|*[!0-9A-Za-z_-]*) exit 0 ;; esac
d="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/fleet-state"
{ mkdir -p "$d" && printf '%s\n' "$j" > "$d/.$s.$$" && mv -f "$d/.$s.$$" "$d/$s.json"; } 2>/dev/null || rm -f "$d/.$s.$$" 2>/dev/null
exit 0
