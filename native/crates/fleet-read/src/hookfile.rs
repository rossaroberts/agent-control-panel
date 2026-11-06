//! A session's hook file, `<profile>/fleet-state/<session>.json` (one small file replaced at each event).

use std::sync::OnceLock;

use fleet_model::types::HookEvent;
use regex::Regex;

use crate::json::read_json;
use crate::profiles::Profile;

const MAX_BYTES: u64 = 64 * 1024;

/// The last hook event of a session, or `None` when there is no usable signal: a session id that is not `^[A-Za-z0-9_-]{8,80}$`,
/// no file, one over 64 KiB, or one that does not parse as an event (bad JSON, no `e`/`at`, wrong types).
pub fn read_hook(p: &Profile, session_id: &str) -> Option<HookEvent> {
    static ID: OnceLock<Regex> = OnceLock::new();
    let ok = ID.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{8,80}$").unwrap());
    if !ok.is_match(session_id) {
        return None;
    }
    read_json(
        &p.dir.join("fleet-state").join(format!("{session_id}.json")),
        MAX_BYTES,
    )
}
