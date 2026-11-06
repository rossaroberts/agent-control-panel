# Native port: parallel lanes (from 2026-11-06)

Ross asked to fan out and approved it: "do it … lets go phone". Four lanes run at once, each in its own git worktree
and branch, off `docs/native-port` @ da3d5fa. The controller (the main session) merges each lane back into
`docs/native-port` when it is done and green.

| Lane | Worktree · branch | Owns | Plan(s) |
|---|---|---|---|
| A · Mac app | `~/projects/acp-wt/native-port` · `docs/native-port` | `native/app/**`; the wire contract: `fleet-model/src/wire.rs`, `types.rs`, goldens, `native/PROTOCOL.md` | 03 (finish: terminal pane), 04 (actions) |
| B · hub | `~/projects/acp-wt/native-hub` · `native/hub` | `fleetd` network listener + Tailscale auth, Linux build, redbat install, `--hub` merge, shared state store | 05 |
| C · Context + Satellite | `~/projects/acp-wt/native-context-satellite` · `native/context-satellite` | `fleetd` Context (read + edit) and Satellite (GitHub reads), their own frames in their own file | 06, 07 |
| E · actions + switch (daemon) | `~/projects/acp-wt/native-actions` · `native/actions` | `fleetd` actions on an `Actions` role (own frames in `wire_actions.rs`), permission decide via hook.sh, the session titler, fleetd as writer of the panel files, the switch procedure (written, not run) | 04 (daemon), 08 (daemon) |
| D · phone | `~/projects/acp-wt/native-ios` · `native/ios` | `native/ios/**` (iOS app); may add `.iOS(...)` to `native/app/Package.swift` platforms and nothing else there | 09 (pulled forward) |

## Rules every lane follows

1. **Spec is binding:** `docs/superpowers/specs/2026-11-06-fleet-native-port-design.md`. Wire contract: `native/PROTOCOL.md`.
   All earlier rulings (plans 1–3, in each plan's `.superpowers/sdd/<plan>/global-constraints.md` on `docs/native-port`)
   still bind.
2. **The wire is lane A's.** Lane B needs no new frames: the hub subscribes to each machine's `fleetd` as an ordinary
   Board client and serves the same protocol. Lane C may add exactly two `Role` variants (`Context`, `Satellite`) at the
   END of `enum Role`, plus its own frame enums in a NEW file (`fleet-model/src/wire_cs.rs`) spoken only on those roles,
   with its own goldens, and bump `PROTO`. The `PROTO` number and the goldens are reconciled by the controller at merge.
   No lane edits `Session`, `Frame`, or `Meta` except lane A. If a lane needs a wire change beyond this, it sends a
   message to the controller (`SendMessage` to `main`) and waits.
3. **Process:** each lane writes its plan with the superpowers:writing-plans skill (save it under
   `docs/superpowers/plans/`), then runs it with superpowers:subagent-driven-development: a fresh implementer per task,
   a fresh reviewer per task, fix loops, a final whole-branch review on the most capable model. Ledger in the lane's own
   `.superpowers/sdd/<plan>/`. Delegation depth: the lane controller spawns implementers and reviewers; they spawn
   nothing.
4. **Never kill processes by name pattern** (`pkill -f`, `killall`): only pids your own test started. Never touch the
   installed `fleetd` LaunchAgent on the Mac except through `fleetd install-service`, and only lane A does that. Never
   touch the Node panel (port 4747) or `~/Applications/Agent Panel.app`.
5. **Real data is read-only:** nothing writes to Claude Code's or Codex's records. Lane C's Context edits are the one
   sanctioned write, to the documents Context lists, hash-checked, with history (spec §8), and only in tests against
   fixtures until its live check, which is read-only.
6. **Public repo:** synthetic fixtures only; no emails, hostnames, tokens, real session names or paths in commits.
7. **Commit on your own branch; do not push; do not merge.** Report back to the controller with: the plan path, the
   commits, the final review verdict, every `Ruling:` line from the ledger, and what Ross can try.
