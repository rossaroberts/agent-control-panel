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
    /// Node: `stopped`/`failed` is closed whatever the pid. Rust: beside a live pid it is a respawned job that has not
    /// spoken yet, so Waiting, live, reason `respawned` (spec section 7: a live pid is never shown as ended).
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
        for (si, (s, c)) in g.scenarios.iter().zip(codes.chars()).enumerate() {
            let (word, node_live) = node_answer(c);
            let got = decide(r, &s.t, s.h.as_ref(), s.now);
            let ok = match expected_divergence(r) {
                Some(Divergence::Respawned) => {
                    respawned += 1;
                    word == "closed" && got == (State::Waiting, true, "respawned".to_string())
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
    assert_eq!(
        decide(&r, &t, Some(&h), 1_793_932_005_000),
        (State::Waiting, true, "respawned".to_string())
    );
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
