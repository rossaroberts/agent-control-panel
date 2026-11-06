//! Plain data types shared by the fleet crates. Field names of `HookEvent` match what `hook.sh` writes.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Engine {
    Claude,
    Codex,
    Muse,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum State {
    NeedsYou,
    Working,
    Waiting,
    Capped,
    Done,
    Ended,
    Unreachable,
}

/// What a hook event says a session is doing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HookSays {
    Ready,
    Working,
    NeedsYou,
    Capped,
    Closed,
}

/// One hook event, as `hook.sh` writes it: `{ e, t, n, r, i, at }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    /// Event name, e.g. `PreToolUse`.
    pub e: String,
    /// Tool name.
    pub t: Option<String>,
    /// Notification type.
    pub n: Option<String>,
    /// Reason.
    pub r: Option<String>,
    /// Interrupted.
    pub i: Option<bool>,
    /// Epoch milliseconds.
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionKey {
    pub machine: String,
    pub profile: String,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub key: SessionKey,
    pub engine: Engine,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub state: State,
    pub reason: String,
    pub live: bool,
    pub pid: Option<u32>,
    pub stage: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_tokens: Option<u64>,
    pub last_at: Option<i64>,
    pub said: Option<String>,
    pub doing: Option<String>,
    pub last_ask: Option<String>,
    pub first_ask: Option<String>,
    pub started_at: Option<i64>,
    pub hook_at: Option<i64>,
    pub written_at: Option<i64>,
}

/// What changed in the board since the previous delta: the sessions whose serialized form changed, and the sessions that
/// are gone. `seq` numbers deltas one by one from 1; a snapshot carries the `seq` of the last delta it includes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    pub seq: u64,
    pub upsert: Vec<Session>,
    pub remove: Vec<SessionKey>,
}
