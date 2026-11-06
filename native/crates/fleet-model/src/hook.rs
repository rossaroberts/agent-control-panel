//! Maps a hook event to what it says the session is doing (port of `hookState` in `fleet/machine.mjs`).

use crate::types::{HookEvent, HookSays};

/// What the last hook event says a session is doing, or `None` when it says nothing.
pub fn hook_state(h: &HookEvent) -> Option<HookSays> {
    let t = h.t.as_deref().unwrap_or("");
    let n = h.n.as_deref().unwrap_or("");
    let r = h.r.as_deref();
    match h.e.as_str() {
        // A compaction mid-turn: the turn goes on, the roster says so.
        "SessionStart" if r == Some("compact") => None,
        "SessionStart" => Some(HookSays::Ready),
        "PreToolUse" => Some(match t {
            "AskUserQuestion" | "ExitPlanMode" => HookSays::NeedsYou,
            _ => HookSays::Working,
        }),
        // Stopped with Esc.
        "PostToolUseFailure" if h.i == Some(true) => Some(HookSays::Ready),
        "PostToolUseFailure" => Some(HookSays::Working),
        "Notification" => match n {
            "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" => {
                Some(HookSays::NeedsYou)
            }
            "idle_prompt" => Some(HookSays::Ready),
            _ => None,
        },
        "StopFailure" if r == Some("rate_limit") => Some(HookSays::Capped),
        "StopFailure" => Some(HookSays::Ready),
        "PostCompact" if r == Some("manual") => Some(HookSays::Ready),
        "PostCompact" => Some(HookSays::Working),
        "UserPromptSubmit" | "PostToolUse" | "PermissionDenied" | "ElicitationResult"
        | "PreCompact" => Some(HookSays::Working),
        "PermissionRequest" | "Elicitation" => Some(HookSays::NeedsYou),
        "Stop" => Some(HookSays::Ready),
        "SessionEnd" => Some(HookSays::Closed),
        _ => None,
    }
}
