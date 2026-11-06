use fleet_model::hook::hook_state;
use fleet_model::types::{HookEvent, HookSays};

fn ev(e: &str, t: Option<&str>, n: Option<&str>, r: Option<&str>, i: Option<bool>) -> HookEvent {
    HookEvent {
        e: e.to_string(),
        t: t.map(str::to_string),
        n: n.map(str::to_string),
        r: r.map(str::to_string),
        i,
        at: 1_793_932_024_711,
    }
}

#[test]
fn session_start_is_ready_unless_compacting() {
    assert_eq!(
        hook_state(&ev("SessionStart", None, None, None, None)),
        Some(HookSays::Ready)
    );
    assert_eq!(
        hook_state(&ev("SessionStart", None, None, Some("compact"), None)),
        None
    );
}

#[test]
fn pre_tool_use_asks_or_works() {
    assert_eq!(
        hook_state(&ev("PreToolUse", Some("AskUserQuestion"), None, None, None)),
        Some(HookSays::NeedsYou)
    );
    assert_eq!(
        hook_state(&ev("PreToolUse", Some("ExitPlanMode"), None, None, None)),
        Some(HookSays::NeedsYou)
    );
    assert_eq!(
        hook_state(&ev("PreToolUse", Some("Bash"), None, None, None)),
        Some(HookSays::Working)
    );
}

#[test]
fn post_tool_use_failure_with_interrupt_is_ready() {
    assert_eq!(
        hook_state(&ev("PostToolUseFailure", None, None, None, Some(true))),
        Some(HookSays::Ready)
    );
    assert_eq!(
        hook_state(&ev("PostToolUseFailure", None, None, None, Some(false))),
        Some(HookSays::Working)
    );
    assert_eq!(
        hook_state(&ev("PostToolUseFailure", None, None, None, None)),
        Some(HookSays::Working)
    );
}

#[test]
fn notification_by_type() {
    for n in [
        "permission_prompt",
        "elicitation_dialog",
        "elicitation_url_dialog",
    ] {
        assert_eq!(
            hook_state(&ev("Notification", None, Some(n), None, None)),
            Some(HookSays::NeedsYou),
            "{n}"
        );
    }
    assert_eq!(
        hook_state(&ev("Notification", None, Some("idle_prompt"), None, None)),
        Some(HookSays::Ready)
    );
    assert_eq!(
        hook_state(&ev("Notification", None, Some("auth_success"), None, None)),
        None
    );
    assert_eq!(
        hook_state(&ev("Notification", None, None, None, None)),
        None
    );
}

#[test]
fn stop_failure_rate_limit_is_capped() {
    assert_eq!(
        hook_state(&ev("StopFailure", None, None, Some("rate_limit"), None)),
        Some(HookSays::Capped)
    );
    assert_eq!(
        hook_state(&ev("StopFailure", None, None, Some("other"), None)),
        Some(HookSays::Ready)
    );
    assert_eq!(
        hook_state(&ev("StopFailure", None, None, None, None)),
        Some(HookSays::Ready)
    );
}

#[test]
fn post_compact_manual_is_ready() {
    assert_eq!(
        hook_state(&ev("PostCompact", None, None, Some("manual"), None)),
        Some(HookSays::Ready)
    );
    assert_eq!(
        hook_state(&ev("PostCompact", None, None, Some("auto"), None)),
        Some(HookSays::Working)
    );
}

#[test]
fn working_events() {
    for e in [
        "UserPromptSubmit",
        "PostToolUse",
        "PermissionDenied",
        "ElicitationResult",
        "PreCompact",
    ] {
        assert_eq!(
            hook_state(&ev(e, None, None, None, None)),
            Some(HookSays::Working),
            "{e}"
        );
    }
}

#[test]
fn needs_you_ready_closed_and_unknown() {
    assert_eq!(
        hook_state(&ev("PermissionRequest", None, None, None, None)),
        Some(HookSays::NeedsYou)
    );
    assert_eq!(
        hook_state(&ev("Elicitation", None, None, None, None)),
        Some(HookSays::NeedsYou)
    );
    assert_eq!(
        hook_state(&ev("Stop", None, None, None, None)),
        Some(HookSays::Ready)
    );
    assert_eq!(
        hook_state(&ev("SessionEnd", None, None, None, None)),
        Some(HookSays::Closed)
    );
    assert_eq!(hook_state(&ev("NoSuchEvent", None, None, None, None)), None);
    assert_eq!(hook_state(&ev("", None, None, None, None)), None);
}

#[test]
fn deserializes_the_line_hook_sh_writes() {
    let line = r#"{"e":"PreToolUse","t":"Bash","n":null,"r":null,"i":null,"at":1793932024711}"#;
    let h: HookEvent = serde_json::from_str(line).expect("hook.sh line parses");
    assert_eq!(h, ev("PreToolUse", Some("Bash"), None, None, None));
    assert_eq!(hook_state(&h), Some(HookSays::Working));
}

#[test]
fn hook_file_missing_optional_keys_still_parses() {
    let h: HookEvent = serde_json::from_str(r#"{"e":"Stop","at":1793932024711}"#).expect("parses");
    assert_eq!(h, ev("Stop", None, None, None, None));
    assert_eq!(hook_state(&h), Some(HookSays::Ready));
}
