//! What the Node panel keeps about the board in `~/Library/Application Support/Agent Panel/`, read-only: `fleet.json` (this
//! machine's name, the account display names, the epics), `fleet-titles.json` (the written title, stage and epic of a
//! session), `fleet-labels.json` (your own title and group), `fleet-archived.json` (when you archived a session). Every
//! session is keyed `<machine>:<sessionId>`, as `fleet/fleet.mjs` keys them.
//!
//! A file that is missing, too big, not JSON or not an object counts as empty (`kept` in `fleet.mjs`): a half-written or
//! damaged file never stops the board. Each file is read on its own, so one bad file costs only its own part.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use fleet_model::types::Engine;

use crate::json::read_json;

/// The largest panel file read. The real ones are tens of KiB.
pub const MAX_BYTES: u64 = 8 << 20;

/// `config().epics`: what the work is for.
#[derive(Clone, Debug, PartialEq)]
pub struct Epic {
    pub name: String,
    pub about: String,
    /// Where its TEST site is (an https URL), else `None`.
    pub test: Option<String>,
}

/// A written title (`fleet-titles.json`): `{ title, stage, epic }` among other keys.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Titled {
    pub title: Option<String>,
    pub stage: Option<String>,
    pub epic: Option<String>,
}

/// Your own name and group for a session (`fleet-labels.json`): `{ title, group }`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Label {
    pub title: Option<String>,
    pub group: Option<String>,
}

/// A session's title and epic as the board shows them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Named {
    pub title: Option<String>,
    pub epic: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelMeta {
    /// `fleet.json` `self`: what this machine is called on the board.
    pub self_name: Option<String>,
    /// `fleet.json` `names`: `claude:<email>` / `codex:<email>` to a display name.
    pub names: HashMap<String, String>,
    pub epics: Vec<Epic>,
    /// `fleet.json` `logins`: `<machine>:<profile>` to the account that profile really is.
    pub logins: HashMap<String, String>,
    pub titles: HashMap<String, Titled>,
    pub labels: HashMap<String, Label>,
    /// Key to when it was archived (epoch ms).
    pub archived: HashMap<String, i64>,
}

fn re(cell: &'static OnceLock<Regex>, src: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(src).expect("a fixed pattern"))
}

/// `NAME = /^[\w .-]{1,40}$/` (`fleet.mjs`).
fn is_name(s: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^[\w .-]{1,40}$").is_match(s)
}

/// A kept file: a JSON object, else nothing.
fn kept(dir: &Path, file: &str) -> Map<String, Value> {
    match read_json::<Value>(&dir.join(file), MAX_BYTES) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Everything the panel keeps, from its folder. A missing folder is an empty `PanelMeta`.
pub fn read_panel_meta(dir: &Path) -> PanelMeta {
    let config = kept(dir, "fleet.json");
    let mut m = PanelMeta {
        self_name: config
            .get("self")
            .and_then(Value::as_str)
            .filter(|s| is_name(s))
            .map(str::to_string),
        ..PanelMeta::default()
    };
    if let Some(Value::Object(names)) = config.get("names") {
        m.names = names
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), cut(v.as_str()?, 40))))
            .collect();
    }
    if let Some(Value::Array(epics)) = config.get("epics") {
        static TEST: OnceLock<Regex> = OnceLock::new();
        m.epics = epics
            .iter()
            .filter_map(|e| {
                let name = e.get("name")?.as_str().filter(|n| is_name(n))?;
                // `String(e.about ?? '')`: absent or null is empty.
                let about = match e.get("about") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                };
                let test = e
                    .get("test")
                    .and_then(Value::as_str)
                    .filter(|t| re(&TEST, r"^https://[\w.-]+(/[\w./?=&%-]*)?$").is_match(t))
                    .map(str::to_string);
                Some(Epic {
                    name: name.to_string(),
                    about: cut(&about, 300),
                    test,
                })
            })
            .take(12)
            .collect();
    }
    if let Some(Value::Object(logins)) = config.get("logins") {
        static KEY: OnceLock<Regex> = OnceLock::new();
        static MAIL: OnceLock<Regex> = OnceLock::new();
        m.logins = logins
            .iter()
            .filter_map(|(k, v)| {
                let v = v.as_str()?;
                (re(&KEY, r"^[\w .-]{1,40}:[\w.-]{1,40}$").is_match(k)
                    && re(&MAIL, r"^[^\s@]+@[^\s@]+$").is_match(v))
                .then(|| (k.clone(), v.to_string()))
            })
            .collect();
    }
    m.titles = kept(dir, "fleet-titles.json")
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                Titled {
                    title: text(v.get("title")),
                    stage: text(v.get("stage")),
                    epic: text(v.get("epic")),
                },
            )
        })
        .collect();
    m.labels = kept(dir, "fleet-labels.json")
        .into_iter()
        .map(|(k, v)| {
            (
                k,
                Label {
                    title: text(v.get("title")),
                    group: text(v.get("group")),
                },
            )
        })
        .collect();
    m.archived = kept(dir, "fleet-archived.json")
        .into_iter()
        .filter_map(|(k, v)| {
            // `archived[key]` is truthy: a time (a number, 0 excepted). Anything else is not an archive mark.
            let at = v.as_f64().filter(|n| *n != 0.0 && !n.is_nan())?;
            Some((k, at as i64))
        })
        .collect();
    m
}

impl PanelMeta {
    /// A session's title and epic (`fleet.mjs` `named`): `label.title ?? own ?? titled.title ?? name`, and
    /// `label.group ?? titled.epic`. `own` is the name Claude Code gave a Claude session, where it is not a made-up
    /// placeholder (`<folder>-<two hex digits>`). `name` is the session's `name` as the board holds it.
    pub fn named(&self, key: &str, engine: &Engine, name: Option<&str>) -> Named {
        static PLACEHOLDER: OnceLock<Regex> = OnceLock::new();
        let label = self.labels.get(key);
        let titled = self.titles.get(key);
        let own = name
            .map(js_trim)
            .filter(|n| {
                *engine == Engine::Claude
                    && !n.is_empty()
                    && !re(&PLACEHOLDER, r"^[\w.-]+-[0-9a-f]{2}$").is_match(n)
            })
            .map(str::to_string);
        Named {
            title: label
                .and_then(|l| l.title.clone())
                .or(own)
                .or_else(|| titled.and_then(|t| t.title.clone()))
                .or_else(|| name.map(str::to_string)),
            epic: label
                .and_then(|l| l.group.clone())
                .or_else(|| titled.and_then(|t| t.epic.clone())),
        }
    }

    /// Whether a session is archived (the negation of `fleet.mjs` `showable`): it was hidden, and is not live and started
    /// since, nor waiting on you. `started_at` is epoch ms.
    pub fn is_archived(
        &self,
        key: &str,
        live: bool,
        started_at: Option<i64>,
        needs_you: bool,
    ) -> bool {
        let Some(hid) = self.archived.get(key) else {
            return false;
        };
        !(live && (started_at.unwrap_or(0) > *hid || needs_you))
    }

    /// Groups you made, after the epics: the labels' groups that are not an epic's name, sorted, each once.
    pub fn groups(&self) -> Vec<String> {
        let epics: HashSet<&str> = self.epics.iter().map(|e| e.name.as_str()).collect();
        let mut g: Vec<String> = self
            .labels
            .values()
            .filter_map(|l| l.group.clone())
            .filter(|g| !g.is_empty() && !epics.contains(g.as_str()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        g.sort();
        g
    }

    /// The display name of an account: `names["<engine>:<email>"]`, else the email's local part.
    pub fn account_name(&self, engine: &Engine, email: &str) -> String {
        let word = match engine {
            Engine::Claude => "claude",
            Engine::Codex => "codex",
            Engine::Muse => "muse",
        };
        self.names
            .get(&format!("{word}:{email}"))
            .filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| email.split('@').next().unwrap_or(email).to_string())
    }
}

/// JavaScript's `trim()`: Unicode white space and line terminators, both ends.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}
