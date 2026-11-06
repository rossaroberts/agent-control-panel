# Native port: roadmap

**Spec:** `docs/superpowers/specs/2026-11-06-fleet-native-port-design.md` (approved by Ross 2026-11-06: "make it
even faster … let's build it and go").

The spec covers eight subsystems that each ship working, testable software on their own. One plan each, in this
order; each plan is written when the one before it lands, so it argues from real interfaces, not guessed ones.

| # | Plan | Ships | Depends on |
|---|---|---|---|
| 1 | `2026-11-06-native-port-01-fleetd-core.md` | Bench harness with the Node "before" numbers; `fleetd` on the Mac: profiles, Claude Code + Codex sessions, the one state decision with reasons, file-event watching, incremental transcript reads, snapshot + deltas on a local socket; parity with `machine.mjs` collect; launchd service | — |
| 2 | terminals | `fleetd` owns ptys for `claude attach` / `codex resume` / tmux; warm pool; scrollback; stream on the socket; predictive-echo protocol fields | 1 |
| 3 | Mac app: board (`2026-11-06-native-port-03-mac-app.md`) | SwiftUI/AppKit `Fleet.app`: sidebar board (Needs you + epics), meters, machine health, launcher presets, keyboard nav (↑/↓, ⌘1–9, ⌘K, ⌘[ ⌘]), native GPU terminal, last board from disk at launch | 1, 2 |
| 4 | actions + permission card + Documents pane | launch / send / interject / close / archive (free, bulk) / adopt / resume / revive / fork / decide; speech hold-to-talk | 3 |
| 5 | hub on redbat | `fleetd --hub`: Linux build, systemd as `claude`, watcher on redbat, merge of machines, shared state store (renames, labels, archive, seen, epics, names), Tailscale-only listen + WhoIs allow-list, Mac fast path merge, hub-unreachable mode, import of Node panel state | 1–4 |
| 6 | Context, editable | documents, structured edit of start scripts and session defaults, hash-checked saves with history, live watch | 5 |
| 7 | Satellite | GraphQL + ETag reads on the hub, read-only guard, table in the app | 5 |
| 8 | the switch | budgets all green against the bench; the session titler (today's `claude -p --model sonnet` titles + epics in `fleet.mjs`) ported into `fleetd`, since plan 3 only reads `fleet-titles.json`; `fleetd` becomes the writer of titles/labels/archive; hook.sh pointed at `fleetd`; Node panel stopped; rollback documented | 1–7 |
| 9 | iPhone | SwiftUI iOS app on the hub protocol, APNs from the hub (needs an Apple Developer account) | 5, 8 |

Every plan inherits plan 1's bench: a PR that breaks a budget fails, and a budget only moves down (the ratchet).
