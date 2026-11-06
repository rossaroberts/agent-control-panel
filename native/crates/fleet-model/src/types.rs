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
    /// The roster's 8-hex `id` of a background job: what `claude attach` takes, so a client can build an
    /// `AttachTarget::Claude` from a board row. `None` for an interactive session.
    pub job_id: Option<String>,
    /// What the board calls it: your own title (`fleet-labels.json`), else Claude Code's own name, else the written title
    /// (`fleet-titles.json`), else `name` (`fleet.mjs` `named`).
    pub title: Option<String>,
    /// Its group on the board: yours (`fleet-labels.json` `group`), else the written epic.
    pub epic: Option<String>,
    /// You archived it (`fleet-archived.json`) and it has not been started again or needed you since: not shown.
    pub archived: bool,
    /// The display name of the account it runs under (`fleet.json` `names`, else the email's local part); `None` for
    /// Muse and for an account whose email is not known.
    pub account: Option<String>,
}

/// What a terminal attaches to (the wire form of `fleet/term.mjs`'s `command()` inputs). The strings are validated by
/// the server before they reach a command line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AttachTarget {
    /// `claude attach <job>` under a Claude Code profile.
    Claude { profile: String, job: String },
    /// `codex resume <session_id>` in `cwd`, under a Codex profile.
    Codex {
        profile: String,
        session_id: String,
        cwd: String,
    },
    /// A shell-less `claude` in `dir`, to accept the folder-trust prompt.
    Trust { profile: String, dir: String },
}

/// What changed in the board since the previous delta: the sessions whose serialized form changed, and the sessions that
/// are gone. `seq` numbers deltas one by one from 1; a snapshot carries the `seq` of the last delta it includes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    pub seq: u64,
    pub upsert: Vec<Session>,
    pub remove: Vec<SessionKey>,
}

/// One plan-limit window of an account, as its status line (Claude Code) or its newest `token_count` event (Codex) last
/// reported it. Not corrected for the clock: a window whose `resets_at` has passed reads as 0 used (`fleet.html`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    /// `5h`, `7d`, or `<n>h` for another Codex window.
    pub window: String,
    /// Percent used, 0-100.
    pub used_pct: u8,
    /// When it resets, epoch milliseconds.
    pub resets_at: Option<i64>,
}

/// A signed-in account of this machine, with the limits it last reported.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// The display name: what `Session.account` says.
    pub name: String,
    pub engine: Engine,
    /// The profile it was read from (`default`, a `.claude-<name>` suffix, or `codex`).
    pub profile: String,
    pub limits: Vec<Limit>,
}

/// How loaded this machine is, in whole percent, and the devices' counts (filled by the health probe).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub cpu_pct: u8,
    pub mem_pct: u8,
    pub disk_pct: u8,
    pub dev: u16,
    pub db: u16,
}

/// What the board shows besides the sessions: the groups and the accounts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// The epics of `fleet.json`, in its order.
    pub epics: Vec<String>,
    /// Groups you made (labels' groups that are not an epic), sorted.
    pub groups: Vec<String>,
    pub accounts: Vec<Account>,
    pub health: Option<Health>,
}
