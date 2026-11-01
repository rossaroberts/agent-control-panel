#!/bin/sh
# Fleet's Artifact guard: a PreToolUse hook (matcher "Artifact") that stops a session publishing claude.ai Artifacts nobody
# asked for, and says where the file goes instead. Reading, listing, opening and managing existing Artifacts pass; a publish
# (the default, when `action` is absent) is denied unless ACP_ALLOW_ARTIFACT=1 is set or its `url` is a line of
# ~/.fleet-panel/artifact-allow.txt (the standing pages you keep updating go there).
#
# Like hook.sh it must never get in a session's way: it exits 0 always, one jq, and fails open (prints nothing) when it
# cannot read the event. A deny is the JSON on stdout that Claude Code reads (permissionDecision "deny", the reason shown
# to the session). Installed by fleet/install-hooks.mjs.
[ "$ACP_ALLOW_ARTIFACT" = 1 ] && exit 0
J=$(command -v jq 2>/dev/null || { [ -x /opt/homebrew/bin/jq ] && echo /opt/homebrew/bin/jq; } || { [ -x /usr/local/bin/jq ] && echo /usr/local/bin/jq; })
[ -n "$J" ] || exit 0
line=$("$J" -r '((.tool_input | objects) // {}) as $i | [($i.action | strings // ""), ($i.url | strings // "")] | @tsv' 2>/dev/null) || exit 0
[ -n "$line" ] || exit 0 # nothing to read (empty input)
action=${line%%"	"*}
url=${line#*"	"}
case "$action" in read|list|open|quickstart|pin|unpin|delete) exit 0 ;; esac
[ -n "$url" ] && [ -f "$HOME/.fleet-panel/artifact-allow.txt" ] && grep -Fxq -- "$url" "$HOME/.fleet-panel/artifact-allow.txt" 2>/dev/null && exit 0
printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Fleet: Artifacts are off here. Write the document as a file in the repo (research and designs → docs/superpowers/specs/, outbound drafts → pm/outbox/, reports where the skill says) and give the user its path; the Fleet Documents pane renders .md and .html a session writes. To publish anyway, the user adds the URL to ~/.fleet-panel/artifact-allow.txt or sets ACP_ALLOW_ARTIFACT=1."}}'
exit 0
