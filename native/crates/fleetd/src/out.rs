//! JSON for `fleetd snapshot --json` and `fleetd watch` (Ruling R6): the core types keep their Rust names in serde, and
//! here `state` and `engine` take Node's words. Everything else is the type's own serde form.

use fleet_model::types::{Delta, Engine, Session, State};
use serde_json::{json, Value};

pub fn state_word(s: &State) -> &'static str {
    match s {
        State::NeedsYou => "needs-you",
        State::Working => "working",
        State::Waiting => "waiting",
        State::Capped => "capped",
        State::Done => "done",
        State::Ended => "ended",
        State::Unreachable => "unreachable",
    }
}

pub fn engine_word(e: &Engine) -> &'static str {
    match e {
        Engine::Claude => "claude",
        Engine::Codex => "codex",
        Engine::Muse => "muse",
    }
}

pub fn session(s: &Session) -> Value {
    let mut v = serde_json::to_value(s).expect("a Session serializes");
    v["state"] = json!(state_word(&s.state));
    v["engine"] = json!(engine_word(&s.engine));
    v
}

pub fn sessions(list: &[Session]) -> Vec<Value> {
    list.iter().map(session).collect()
}

pub fn snapshot(seq: u64, machine: &str, list: &[Session]) -> Value {
    json!({"type": "snapshot", "seq": seq, "machine": machine, "sessions": sessions(list)})
}

pub fn delta(d: &Delta) -> Value {
    json!({"type": "delta", "seq": d.seq, "upsert": sessions(&d.upsert), "remove": d.remove})
}
