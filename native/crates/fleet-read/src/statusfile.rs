//! A session's status-line snapshot, `<profile>/fleet-status/<session>.json` (written whole, by a rename, by
//! fleet/statusline.mjs at each status-line refresh). Only what the session board uses is kept: when it was written, the
//! model, and the context window's size and use. Read-only.

use serde_json::Value;

use crate::json::read_json;
use crate::profiles::Profile;

pub const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusSnap {
    /// Epoch ms it was written; `None` when the file says nothing usable (then it is never fresh, as Node's
    /// `undefined >= x` is false).
    pub at: Option<i64>,
    pub model: Option<String>,
    /// `context_window.context_window_size`.
    pub window: Option<u64>,
    /// `context_window.current_usage`: input + cache creation + cache read tokens.
    pub used_tokens: Option<u64>,
    /// `context_window.used_percentage`.
    pub used_pct: Option<f64>,
}

/// The snapshot of a session, or `None`: a bad id, no file, one over 64 KiB, not JSON, or naming another session.
pub fn read_status(p: &Profile, session_id: &str) -> Option<StatusSnap> {
    if !valid_id(session_id) {
        return None;
    }
    let d: Value = read_json(
        &p.dir
            .join("fleet-status")
            .join(format!("{session_id}.json")),
        MAX_BYTES,
    )?;
    snap_of(session_id, &d)
}

/// A status file's bytes as a snapshot of `session_id`, or `None`: over 64 KiB, not JSON, or naming another session.
pub fn parse_status(session_id: &str, bytes: &[u8]) -> Option<StatusSnap> {
    if bytes.len() as u64 > MAX_BYTES || !valid_id(session_id) {
        return None;
    }
    snap_of(session_id, &serde_json::from_slice(bytes).ok()?)
}

/// statusline.mjs's own test: `^[\w-]{8,80}$`.
fn valid_id(session_id: &str) -> bool {
    (8..=80).contains(&session_id.len())
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn snap_of(session_id: &str, d: &Value) -> Option<StatusSnap> {
    if d.get("session_id")?.as_str()? != session_id {
        return None;
    }
    let cw = d.get("context_window").filter(|v| v.is_object());
    let num = |v: Option<&Value>| v.and_then(Value::as_f64).unwrap_or(0.0);
    let used_tokens = cw
        .and_then(|c| c.get("current_usage"))
        .filter(|u| u.is_object())
        .map(|u| {
            (num(u.get("input_tokens"))
                + num(u.get("cache_creation_input_tokens"))
                + num(u.get("cache_read_input_tokens")))
            .max(0.0) as u64
        });
    Some(StatusSnap {
        at: d.get("at").and_then(Value::as_f64).map(|n| n as i64),
        model: d
            .get("model")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        window: cw
            .and_then(|c| c.get("context_window_size"))
            .and_then(Value::as_f64)
            .filter(|n| *n > 0.0)
            .map(|n| n as u64),
        used_tokens,
        used_pct: cw
            .and_then(|c| c.get("used_percentage"))
            .and_then(Value::as_f64),
    })
}

/// `contextOf`'s token count: the status line's when it is fresh (written no more than a minute before the transcript's
/// last line, and not a 0 right after a restart when the transcript knows better), else the transcript's. `model` and
/// `transcript_tokens` are the transcript's. The window for a percentage without a size is `windowOf` with nothing
/// learned from other sessions: 1M for a `[1m]` model or past 200k tokens, else 200k.
pub fn context_tokens(
    sl: Option<&StatusSnap>,
    transcript_last_at: Option<i64>,
    transcript_tokens: Option<u64>,
    model: Option<&str>,
) -> Option<u64> {
    let sl_tokens = sl.and_then(|s| {
        s.used_tokens.or_else(|| {
            let pct = s.used_pct?;
            let window = s.window.unwrap_or_else(|| {
                let big = model.is_some_and(|m| m.ends_with("[1m]"))
                    || transcript_tokens.unwrap_or(0) > 200_000;
                if big {
                    1_000_000
                } else {
                    200_000
                }
            });
            Some((pct * window as f64 / 100.0).round().max(0.0) as u64)
        })
    });
    let fresh = match (sl, sl_tokens) {
        (Some(s), Some(n)) => {
            s.at.is_some_and(|at| at >= transcript_last_at.unwrap_or(0) - 60_000)
                && (n > 0 || transcript_tokens.unwrap_or(0) == 0)
        }
        _ => false,
    };
    if fresh {
        sl_tokens
    } else {
        transcript_tokens
    }
}
