//! `decide()` against Node's `sessionState` (golden from native/parity/gen-decide-golden.mjs), plus the two deliberate
//! divergences and the reason rules.

use fleet_model::decide::{decide, RosterRow, TailFacts};
use fleet_model::types::{HookEvent, State};
use serde::Deserialize;

#[derive(Deserialize)]
struct Scenario {
    t: TailFacts,
    h: Option<HookEvent>,
    now: i64,
}

#[derive(Deserialize)]
struct Golden {
    rows: Vec<RosterRow>,
    scenarios: Vec<Scenario>,
    /// `node[ri]`: one character per scenario; Node's state as a letter, upper case when live.
    node: Vec<String>,
}

fn golden() -> Golden {
    let text = include_str!("../../../parity/golden/decide.json");
    serde_json::from_str(text).expect("golden/decide.json parses")
}

/// Node's state word and live flag, from a golden code.
fn node_answer(c: char) -> (&'static str, bool) {
    let word = match c.to_ascii_lowercase() {
        'c' => "closed",
        'n' => "needs-you",
        'w' => "working",
        'r' => "ready",
        'p' => "capped",
        other => panic!("unknown golden code {other:?}"),
    };
    (word, c.is_ascii_uppercase())
}

/// Node's state mapped to ours: `closed` is `Done` for a finished job, otherwise `Ended`.
fn map_node(word: &str, r: &RosterRow) -> State {
    match word {
        "needs-you" => State::NeedsYou,
        "working" => State::Working,
        "ready" => State::Waiting,
        "capped" => State::Capped,
        "closed" if r.state.as_deref() == Some("done") => State::Done,
        "closed" => State::Ended,
        other => panic!("unknown node state {other}"),
    }
}

/// The deliberate divergences from Node, each with what Rust says instead. Any other mismatch fails.
#[derive(Debug, PartialEq)]
enum Divergence {
    /// Node: `stopped`/`failed` is closed whatever the pid. Rust: beside a live pid it is a respawned job, open and live
    /// (spec section 7: a live pid is never shown as ended). Its state is what the open branches give, checked against
    /// Node's answer for the same row with `state` null (its twin, also in the golden); its reason is the twin's reason
    /// prefixed `respawned; ` (Ruling R8).
    Respawned,
    /// Node has no pid liveness and calls any pid open. Rust: a pid that is set but dead, on a job that is not `active`,
    /// is not a live session: Ended, or Done when the roster says `done` (Ruling R7).
    DeadPid,
}

fn expected_divergence(r: &RosterRow) -> Option<Divergence> {
    let stopped = matches!(r.state.as_deref(), Some("stopped" | "failed"));
    let has_pid = r.pid.is_some_and(|p| p > 0);
    if stopped && has_pid && r.pid_alive {
        Some(Divergence::Respawned)
    } else if !stopped && has_pid && !r.pid_alive && !r.active {
        Some(Divergence::DeadPid)
    } else {
        None
    }
}

#[test]
fn decide_matches_node_golden_except_the_listed_divergences() {
    let g = golden();
    assert_eq!(g.node.len(), g.rows.len());
    let (mut same, mut respawned, mut dead_pid) = (0, 0, 0);
    let mut failures = Vec::new();
    let mut mismatches = 0;
    for (ri, (r, codes)) in g.rows.iter().zip(&g.node).enumerate() {
        assert_eq!(codes.chars().count(), g.scenarios.len(), "row {ri}");
        // A respawned row's twin: the same row with `state` null, which Node decides through its open branches.
        let twin = (expected_divergence(r) == Some(Divergence::Respawned)).then(|| {
            let want = RosterRow {
                state: None,
                ..r.clone()
            };
            let ti = g
                .rows
                .iter()
                .position(|x| *x == want)
                .expect("twin row in golden");
            (g.rows[ti].clone(), g.node[ti].chars().collect::<Vec<_>>())
        });
        for (si, (s, c)) in g.scenarios.iter().zip(codes.chars()).enumerate() {
            let (word, node_live) = node_answer(c);
            let got = decide(r, &s.t, s.h.as_ref(), s.now);
            let ok = match expected_divergence(r) {
                Some(Divergence::Respawned) => {
                    respawned += 1;
                    let (twin_row, twin_codes) = twin.as_ref().unwrap();
                    let (twin_word, twin_live) = node_answer(twin_codes[si]);
                    let twin_got = decide(twin_row, &s.t, s.h.as_ref(), s.now);
                    word == "closed"
                        && twin_live
                        && got.0 == map_node(twin_word, twin_row)
                        && got.1
                        && got.2 == format!("respawned; {}", twin_got.2)
                }
                Some(Divergence::DeadPid) => {
                    dead_pid += 1;
                    let want = if r.state.as_deref() == Some("done") {
                        State::Done
                    } else {
                        State::Ended
                    };
                    node_live && word != "closed" && got.0 == want && !got.1
                }
                None => {
                    same += 1;
                    got.0 == map_node(word, r) && got.1 == node_live
                }
            };
            if !ok {
                mismatches += 1;
            }
            if !ok && failures.len() < 20 {
                failures.push(format!(
                    "row {ri} scenario {si}: node {word}/{node_live}, rust {got:?}\n  r={r:?}\n  t={:?} h={:?} now={}",
                    s.t, s.h, s.now
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{mismatches} mismatches (first {} shown):\n{}",
        failures.len(),
        failures.join("\n")
    );
    // Every family is exercised.
    assert!(
        same > 0 && respawned > 0 && dead_pid > 0,
        "{same} {respawned} {dead_pid}"
    );
}

#[test]
fn reasons_are_never_empty() {
    let g = golden();
    for r in &g.rows {
        for s in &g.scenarios {
            let (_, _, reason) = decide(r, &s.t, s.h.as_ref(), s.now);
            assert!(!reason.trim().is_empty(), "empty reason for {r:?}");
            assert!(
                !reason.ends_with(':') && !reason.ends_with(": "),
                "dangling reason {reason:?}"
            );
        }
    }
}

#[test]
fn no_record_is_ended_reason_unknown() {
    let got = decide(
        &RosterRow::default(),
        &TailFacts::default(),
        None,
        1_793_932_000_000,
    );
    assert_eq!(
        got,
        (State::Ended, false, "ended, reason unknown".to_string())
    );
}

#[test]
fn stopped_beside_a_live_pid_with_session_start_is_waiting_respawned() {
    // Review Focus 1, as Ruling R8 has it: open and live, state from the open branches, reason prefixed.
    let r = RosterRow {
        pid: Some(4242),
        pid_alive: true,
        state: Some("stopped".into()),
        ..RosterRow::default()
    };
    let t = TailFacts {
        last_at: Some(1_793_932_000_000),
        written_at: Some(1_793_932_000_000),
        ..TailFacts::default()
    };
    let h = HookEvent {
        e: "SessionStart".into(),
        t: None,
        n: None,
        r: None,
        i: None,
        at: 1_793_932_000_000,
    };
    let (state, live, reason) = decide(&r, &t, Some(&h), 1_793_932_005_000);
    assert_eq!((state, live), (State::Waiting, true));
    assert!(reason.starts_with("respawned; "), "{reason}");
    assert_eq!(reason, "respawned; idle at prompt");
}

#[test]
fn respawned_at_a_permission_prompt_needs_you() {
    let r = RosterRow {
        pid: Some(4242),
        pid_alive: true,
        state: Some("failed".into()),
        waiting_for: Some("permission".into()),
        ..RosterRow::default()
    };
    assert_eq!(
        decide(&r, &TailFacts::default(), None, 1_793_932_000_000),
        (
            State::NeedsYou,
            true,
            "respawned; waiting on you: permission".to_string()
        )
    );
}

#[test]
fn roster_needs_you_names_the_roster_unless_a_current_hook_asks() {
    let at = 1_793_932_000_000;
    let t = TailFacts {
        last_at: Some(at),
        written_at: Some(at),
        ..TailFacts::default()
    };
    let hook = |e: &str, tool: Option<&str>, n: Option<&str>, off: i64| HookEvent {
        e: e.into(),
        t: tool.map(Into::into),
        n: n.map(Into::into),
        r: None,
        i: None,
        at: at + off,
    };
    // Hooks that do not say "needs you" (and a stale one that does): never named.
    let others = [
        Some(hook("PreToolUse", Some("Bash"), None, 0)),
        Some(hook("SessionStart", None, None, 0)),
        Some(hook("SessionEnd", None, None, 0)),
        Some(hook("Stop", None, None, 0)),
        Some(hook("StopFailure", None, None, 0)),
        Some(hook(
            "Notification",
            None,
            Some("permission_prompt"),
            -20_000,
        )),
        None,
    ];
    let triggers = [
        (None, Some("waiting"), "waiting on you: roster waiting"),
        (
            Some("blocked"),
            Some("busy"),
            "waiting on you: roster blocked",
        ),
    ];
    for (state, status, want) in triggers {
        let r = RosterRow {
            pid: Some(4242),
            pid_alive: true,
            state: state.map(Into::into),
            status: status.map(Into::into),
            ..RosterRow::default()
        };
        for h in &others {
            assert_eq!(
                decide(&r, &t, h.as_ref(), at + 1000),
                (State::NeedsYou, true, want.to_string()),
                "{h:?}"
            );
        }
        // A current hook that itself asks is named.
        let asks = hook("Notification", None, Some("permission_prompt"), 0);
        assert_eq!(
            decide(&r, &t, Some(&asks), at + 1000).2,
            "waiting on you: Notification"
        );
    }
}

#[test]
fn reasons_name_what_said_so() {
    let live = |state: Option<&str>, status: Option<&str>, waiting_for: Option<&str>| RosterRow {
        pid: Some(4242),
        pid_alive: true,
        state: state.map(Into::into),
        status: status.map(Into::into),
        waiting_for: waiting_for.map(Into::into),
        ..RosterRow::default()
    };
    let at = 1_793_932_000_000; // 2026-11-06 02:26:40 UTC
    let t = TailFacts {
        last_at: Some(at),
        written_at: Some(at),
        ..TailFacts::default()
    };
    let hook = |e: &str, tool: Option<&str>| HookEvent {
        e: e.into(),
        t: tool.map(Into::into),
        n: None,
        r: None,
        i: None,
        at,
    };
    let reason = |r: &RosterRow, t: &TailFacts, h: Option<&HookEvent>| decide(r, t, h, at + 1000).2;

    assert_eq!(
        reason(&live(None, None, Some("permission")), &t, None),
        "waiting on you: permission"
    );
    assert_eq!(
        reason(
            &live(None, None, None),
            &t,
            Some(&hook("PreToolUse", Some("AskUserQuestion")))
        ),
        "waiting on you: PreToolUse"
    );
    let capped = TailFacts {
        limit_hit_at: Some(at - 60_000),
        ..t.clone()
    };
    assert_eq!(
        reason(&live(None, None, None), &capped, None),
        "limit hit at 02:25"
    );
    assert_eq!(
        reason(
            &live(None, None, None),
            &t,
            Some(&hook("PreToolUse", Some("Bash")))
        ),
        "working: hook PreToolUse"
    );
    assert_eq!(
        reason(&live(None, Some("busy"), None), &t, None),
        "working: roster busy"
    );
    assert_eq!(
        reason(&live(None, Some("idle"), None), &t, None),
        "idle at prompt"
    );
    let dead = |state: &str| RosterRow {
        state: Some(state.into()),
        ..RosterRow::default()
    };
    assert_eq!(reason(&dead("done"), &t, None), "finished");
    assert_eq!(reason(&dead("stopped"), &t, None), "stopped by Claude Code");
    assert_eq!(reason(&dead("failed"), &t, None), "failed");
}
