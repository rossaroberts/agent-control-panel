//! A session's hook file, `<profile>/fleet-state/<session>.json` (one small file replaced at each event).

use std::sync::OnceLock;

use fleet_model::types::HookEvent;
use regex::Regex;

use crate::json::read_json;
use crate::profiles::Profile;

pub const MAX_BYTES: u64 = 64 * 1024;

/// A session id a hook file may be named by: `^[A-Za-z0-9_-]{8,80}$`.
pub fn valid_id(session_id: &str) -> bool {
    static ID: OnceLock<Regex> = OnceLock::new();
    ID.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{8,80}$").unwrap())
        .is_match(session_id)
}

/// A hook file's bytes as an event, or `None`: over 64 KiB, or not an event (bad JSON, no `e`/`at`, wrong types).
pub fn parse_hook(bytes: &[u8]) -> Option<HookEvent> {
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

/// The last hook event of a session, or `None` when there is no usable signal: a session id that is not `^[A-Za-z0-9_-]{8,80}$`,
/// no file, one over 64 KiB, or one that does not parse as an event (bad JSON, no `e`/`at`, wrong types).
pub fn read_hook(p: &Profile, session_id: &str) -> Option<HookEvent> {
    if !valid_id(session_id) {
        return None;
    }
    read_json(
        &p.dir.join("fleet-state").join(format!("{session_id}.json")),
        MAX_BYTES,
    )
}
