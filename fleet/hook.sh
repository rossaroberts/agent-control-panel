#!/bin/sh
# Fleet's hook: what a Claude Code session is doing, the moment it changes. Each event of the main conversation (a
# subagent's carry an agent_id and are left out, as are notifications that say nothing about its state) leaves one small
# file, <profile>/fleet-state/<session>.json, replaced whole; Fleet reads it (and watches the folder).
#
# It must never get in a session's way: it prints nothing (a hook's output can reach Claude), always exits 0, and takes a
# few milliseconds (one jq). Installed by fleet/install-hooks.mjs for the events in its list, with a 5 s timeout.
#
# Beside the state file it keeps the session's documents: a PostToolUse of Write/Edit/MultiEdit/NotebookEdit whose path ends
# in .md .markdown .html .htm .svg .txt .csv .json appends {"p":path,"t":tool,"at":ms} to <session>.docs.jsonl (trimmed to
# its last 200 lines past 64 KB). Fleet's Documents pane lists and renders them.
J=$(command -v jq 2>/dev/null || { [ -x /opt/homebrew/bin/jq ] && echo /opt/homebrew/bin/jq; } || { [ -x /usr/local/bin/jq ] && echo /usr/local/bin/jq; })
[ -n "$J" ] || exit 0
line=$("$J" -r 'select((.agent_id // "") == "")
  | select(.hook_event_name != "Notification" or ((.notification_type // "") | test("^(permission_prompt|idle_prompt|elicitation_dialog|elicitation_url_dialog)$")))
  | "\(.session_id // "")\t\({e: .hook_event_name, t: (.tool_name // null), n: (.notification_type // null),
       r: (.reason // .source // .trigger // null), i: (.is_interrupt // null), at: (now * 1000 | floor)} | tojson)"
     + "\t" + (if .hook_event_name == "PostToolUse" and ((.tool_name // "") | test("^(Write|Edit|MultiEdit|NotebookEdit)$"))
         then (((.tool_input | objects | (.file_path // .notebook_path)) | strings) // "") as $f
           | if ($f | test("\\.(md|markdown|html|htm|svg|txt|csv|json)$"; "i")) then ({p: $f, t: .tool_name, at: (now * 1000 | floor)} | tojson) else "" end
         else "" end)' 2>/dev/null) || exit 0
s=${line%%"	"*}
j=${line#*"	"}
doc=${j#*"	"}
j=${j%%"	"*}
case "$s" in ''|*[!0-9A-Za-z_-]*) exit 0 ;; esac
d="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/fleet-state"
{ mkdir -p "$d" && printf '%s\n' "$j" > "$d/.$s.$$" && mv -f "$d/.$s.$$" "$d/$s.json"; } 2>/dev/null || rm -f "$d/.$s.$$" 2>/dev/null
if [ -n "$doc" ]; then
  f="$d/$s.docs.jsonl"
  { printf '%s\n' "$doc" >> "$f"; n=$(wc -c < "$f"); n=$((n + 0)); [ "$n" -le 65536 ] || { tail -n 200 "$f" > "$d/.$s.docs.$$" && mv -f "$d/.$s.docs.$$" "$f"; }; } 2>/dev/null || rm -f "$d/.$s.docs.$$" 2>/dev/null
fi
exit 0
