//! The one state decision, with its reason (port of `sessionState` in `fleet/machine.mjs`, branch for branch). Pure: no
//! clock, no I/O; `now` and pid liveness come in from the caller.

use serde::{Deserialize, Serialize};

use crate::hook::hook_state;
use crate::types::{HookEvent, HookSays, State};

/// A session's row in Claude Code's roster (`claude agents --json`), plus whether its pid is alive.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RosterRow {
    pub id: Option<String>,
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub pid_alive: bool,
    pub state: Option<String>,
    pub status: Option<String>,
    pub waiting_for: Option<String>,
    pub active: bool,
    pub kind: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub started_at: Option<i64>,
}

/// What the end of a session's transcript says. Times are epoch milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TailFacts {
    pub last_at: Option<i64>,
    pub written_at: Option<i64>,
    pub limit_hit_at: Option<i64>,
    pub turn_over: Option<bool>,
}

/// What a session is doing, whether it is live, and why. A process that is alive is open: working, needing you, stopped
/// on a limit, or waiting at its prompt. One with no live process (and not a job Claude Code still lists as `active`)
/// is closed: `Done` when the roster says `done`, otherwise `Ended`.
///
/// Two deliberate divergences from Node: `stopped`/`failed` beside a live pid is a respawned job, `Waiting` and live
/// (Node: closed); a pid that is set but dead is not live (Node treats any pid as alive).
pub fn decide(
    r: &RosterRow,
    t: &TailFacts,
    h: Option<&HookEvent>,
    now: i64,
) -> (State, bool, String) {
    let state = r.state.as_deref();
    let status = r.status.as_deref();
    let stopped = matches!(state, Some("stopped" | "failed"));
    let pid_live = r.pid.is_some_and(|p| p > 0) && r.pid_alive;
    let open = |s: State, reason: String| (s, true, reason);

    // A live pid is never shown as ended: the roster's stopped/failed beside one is a job started again.
    if stopped && pid_live {
        return open(State::Waiting, "respawned".into());
    }
    // Open means a live process, or a background job Claude Code still lists as active.
    if stopped || !(pid_live || r.active) {
        let (s, reason) = match state {
            Some("done") => (State::Done, "finished"),
            Some("stopped") => (State::Ended, "stopped by Claude Code"),
            Some("failed") => (State::Ended, "failed"),
            _ => (State::Ended, "ended, reason unknown"),
        };
        return (s, false, reason.into());
    }
    // A background job reads `blocked` both on a question to you and when idle with no prompt yet: only the first needs you.
    let waiting_for = r.waiting_for.as_deref().filter(|w| !w.is_empty());
    if status == Some("waiting")
        || waiting_for.is_some()
        || (state == Some("blocked") && status != Some("idle"))
    {
        let what = match (waiting_for, h) {
            (Some(w), _) => w.to_string(),
            (None, Some(h)) => h.e.clone(),
            (None, None) => format!("roster {}", status.or(state).unwrap_or("waiting")),
        };
        return open(State::NeedsYou, format!("waiting on you: {what}"));
    }
    // Its last word was a limit stop: capped, whatever its hooks said after.
    if let Some(at) = t.limit_hit_at {
        if at >= t.last_at.unwrap_or(0) - 120_000 {
            return open(State::Capped, format!("limit hit at {}", hh_mm_utc(at)));
        }
    }
    // Its hooks, while they are current (nothing written since that they didn't see).
    let written_at = t.written_at.unwrap_or(0);
    if let Some((h, hs)) = h.and_then(|h| hook_state(h).map(|hs| (h, hs))) {
        if hs != HookSays::Closed && h.at >= written_at - 15_000 {
            let quiet = now - h.at.max(written_at);
            // A permission given goes on with the tool and no event until it ends: the roster says busy, so it works.
            if hs == HookSays::NeedsYou && status == Some("busy") && quiet > 3000 {
                return open(State::Working, "working: roster busy".into());
            }
            // A turn stopped with Esc fires nothing: still "working" a minute on with the roster idle, it is waiting.
            if hs == HookSays::Working && status == Some("idle") && quiet > 60_000 {
                return open(State::Waiting, "idle at prompt".into());
            }
            return match hs {
                HookSays::Ready => open(State::Waiting, "idle at prompt".into()),
                HookSays::Working => open(State::Working, format!("working: hook {}", h.e)),
                HookSays::NeedsYou => open(State::NeedsYou, format!("waiting on you: {}", h.e)),
                HookSays::Capped => {
                    open(State::Capped, format!("limit hit at {}", hh_mm_utc(h.at)))
                }
                HookSays::Closed => unreachable!("closed hooks are skipped above"),
            };
        }
    }
    // Working is what the roster says; with no status, a job's state says it runs.
    if status == Some("busy") {
        return open(State::Working, "working: roster busy".into());
    }
    if status.is_none() {
        if let Some(s @ ("working" | "running")) = state {
            return open(State::Working, format!("working: roster {s}"));
        }
    }
    open(State::Waiting, "idle at prompt".into())
}

/// `hh:mm` in UTC of an epoch-millisecond time.
fn hh_mm_utc(ms: i64) -> String {
    let minutes = ms.div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}
