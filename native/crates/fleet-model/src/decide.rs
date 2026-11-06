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
/// Two deliberate divergences from Node: `stopped`/`failed` beside a live pid is a respawned job, open and live (Node:
/// closed), its state decided like any open session's and its reason prefixed `respawned; ` (Ruling R8); a pid that is
/// set but dead is not live (Node treats any pid as alive; Ruling R7).
pub fn decide(
    r: &RosterRow,
    t: &TailFacts,
    h: Option<&HookEvent>,
    now: i64,
) -> (State, bool, String) {
    let state = r.state.as_deref();
    let stopped = matches!(state, Some("stopped" | "failed"));
    let pid_live = r.pid.is_some_and(|p| p > 0) && r.pid_alive;

    // A live pid is never shown as ended: the roster's stopped/failed beside one is a job started again. That decides
    // only that it is open; what it does is decided as for any open session.
    if stopped && pid_live {
        let (s, reason) = open_state(r, t, h, now);
        return (s, true, format!("respawned; {reason}"));
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
    let (s, reason) = open_state(r, t, h, now);
    (s, true, reason)
}

/// How long a hook that needs you may be quiet before a roster that says `busy` makes it working (Node's `quiet > 3000`).
pub const NEEDS_YOU_QUIET_MS: i64 = 3000;
/// How long a working hook may be quiet before a roster that says `idle` makes it waiting (Node's `quiet > 60_000`).
pub const WORKING_QUIET_MS: i64 = 60_000;

/// The first time after `now` (epoch ms) at which `decide` may answer otherwise with nothing read anew: when the quiet
/// of a current hook passes one of its thresholds. `None` when nothing in `decide` waits on the clock. It does not ask
/// whether the answer will change at that time (that also depends on the roster's word then), only that it may.
pub fn due(r: &RosterRow, t: &TailFacts, h: Option<&HookEvent>, now: i64) -> Option<i64> {
    let stopped = matches!(r.state.as_deref(), Some("stopped" | "failed"));
    let pid_live = r.pid.is_some_and(|p| p > 0) && r.pid_alive;
    // Closed (as `decide` decides it) has no clock in it.
    if !(pid_live || (r.active && !stopped)) {
        return None;
    }
    let (h, hs) = current_hook(h, t)?;
    let quiet_from = h.at.max(t.written_at.unwrap_or(0));
    let after = match hs {
        HookSays::NeedsYou => NEEDS_YOU_QUIET_MS,
        HookSays::Working => WORKING_QUIET_MS,
        _ => return None,
    };
    // `quiet > after` first holds at `quiet_from + after + 1`.
    let at = quiet_from + after + 1;
    (at > now).then_some(at)
}

/// The last hook event and what it says, while it is current: it says something other than closed, and nothing was
/// written to the transcript more than 15 s after it.
fn current_hook<'a>(h: Option<&'a HookEvent>, t: &TailFacts) -> Option<(&'a HookEvent, HookSays)> {
    let h = h?;
    let hs = hook_state(h)?;
    (hs != HookSays::Closed && h.at >= t.written_at.unwrap_or(0) - 15_000).then_some((h, hs))
}

/// What an open session is doing, and why.
fn open_state(r: &RosterRow, t: &TailFacts, h: Option<&HookEvent>, now: i64) -> (State, String) {
    let state = r.state.as_deref();
    let status = r.status.as_deref();
    let hook = current_hook(h, t);

    // A background job reads `blocked` both on a question to you and when idle with no prompt yet: only the first needs you.
    let waiting_for = r.waiting_for.as_deref().filter(|w| !w.is_empty());
    if status == Some("waiting")
        || waiting_for.is_some()
        || (state == Some("blocked") && status != Some("idle"))
    {
        // Name what said so: the roster's question, else a current hook that itself says it needs you, else the roster.
        let what = match (waiting_for, hook) {
            (Some(w), _) => w.to_string(),
            (None, Some((h, HookSays::NeedsYou))) => h.e.clone(),
            _ if status == Some("waiting") => "roster waiting".into(),
            _ => "roster blocked".into(),
        };
        return (State::NeedsYou, format!("waiting on you: {what}"));
    }
    // Its last word was a limit stop: capped, whatever its hooks said after.
    if let Some(at) = t.limit_hit_at {
        if at >= t.last_at.unwrap_or(0) - 120_000 {
            return (State::Capped, format!("limit hit at {}", hh_mm_utc(at)));
        }
    }
    // Its hooks, while they are current: what it does, as Claude Code says.
    if let Some((h, hs)) = hook {
        let quiet = now - h.at.max(t.written_at.unwrap_or(0));
        // A permission given goes on with the tool and no event until it ends: the roster says busy, so it works.
        if hs == HookSays::NeedsYou && status == Some("busy") && quiet > NEEDS_YOU_QUIET_MS {
            return (State::Working, "working: roster busy".into());
        }
        // A turn stopped with Esc fires nothing: still "working" a minute on with the roster idle, it is waiting.
        if hs == HookSays::Working && status == Some("idle") && quiet > WORKING_QUIET_MS {
            return (State::Waiting, "idle at prompt".into());
        }
        return match hs {
            HookSays::Ready => (State::Waiting, "idle at prompt".into()),
            HookSays::Working => (State::Working, format!("working: hook {}", h.e)),
            HookSays::NeedsYou => (State::NeedsYou, format!("waiting on you: {}", h.e)),
            HookSays::Capped => (State::Capped, format!("limit hit at {}", hh_mm_utc(h.at))),
            HookSays::Closed => unreachable!("current_hook never returns closed"),
        };
    }
    // Working is what the roster says; with no status, a job's state says it runs.
    if status == Some("busy") {
        return (State::Working, "working: roster busy".into());
    }
    if status.is_none() {
        if let Some(s @ ("working" | "running")) = state {
            return (State::Working, format!("working: roster {s}"));
        }
    }
    (State::Waiting, "idle at prompt".into())
}

/// `hh:mm` in UTC of an epoch-millisecond time.
pub fn hh_mm_utc(ms: i64) -> String {
    let minutes = ms.div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}
