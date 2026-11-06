//! What the board shows besides the sessions' own state: each session's title, epic, archive mark and account, the groups,
//! and the accounts with their plan limits. Titles, epics and archive marks come from the Node panel's files
//! (`fleet_read::panelmeta`, read-only); the account of a session from the profile's login; the limits from the
//! status-line snapshots (Claude Code) and the Codex logs' `token_count` events.
//!
//! `BoardMeta` holds only values: its methods do no file I/O, so they run under the watcher's board lock. The reads that
//! fill it (`read_accounts`, `read_claude_limits`, `fleet_read::panelmeta::read_panel_meta`) are made before it.

use std::collections::HashMap;
use std::path::Path;

use fleet_model::types::{Account, Engine, Limit, Meta, Session, State};
use fleet_read::codex::{codex_account, codex_limits_fallback};
use fleet_read::panelmeta::PanelMeta;
use fleet_read::profiles::{account, Profile};
use fleet_read::statusfile::read_status;

/// The newest plan-limit reading of an account: when it was written (epoch ms), and its windows.
pub type Reading = (i64, Vec<Limit>);

/// The profile id Codex's one home is known by (`~/.codex`).
pub const CODEX_PROFILE: &str = "codex";

#[derive(Clone, Debug, Default)]
pub struct BoardMeta {
    pub machine: String,
    pub panel: PanelMeta,
    /// Claude Code profile id to the email it is logged in as.
    pub emails: HashMap<String, String>,
    pub codex_email: Option<String>,
    /// Claude Code profile id to its newest status-line limits.
    pub claude_limits: HashMap<String, Reading>,
    /// Codex's limits from older logs, for a day when no recent log reports any.
    pub codex_fallback: Option<Reading>,
}

/// Who each profile (and Codex) is logged in as, and the newest limits Codex's older logs report.
pub fn read_accounts(profiles: &[Profile], codex_home: &Path, now: i64) -> Accounts {
    Accounts {
        emails: profiles
            .iter()
            .filter_map(|p| Some((p.id.clone(), account(p)?)))
            .collect(),
        codex_email: codex_account(codex_home),
        codex_fallback: codex_limits_fallback(codex_home, now),
    }
}

#[derive(Debug, Default)]
pub struct Accounts {
    pub emails: HashMap<String, String>,
    pub codex_email: Option<String>,
    pub codex_fallback: Option<Reading>,
}

/// A profile's newest status-line limits among all its `fleet-status/*.json` (`statusSnapshots`).
pub fn read_claude_limits(p: &Profile) -> Option<Reading> {
    let mut newest: Option<Reading> = None;
    for e in std::fs::read_dir(p.dir.join("fleet-status"))
        .ok()?
        .flatten()
    {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let Some(sid) = name.strip_suffix(".json").filter(|s| !s.starts_with('.')) else {
            continue;
        };
        let Some(snap) = read_status(p, sid) else {
            continue;
        };
        if let (Some(at), Some(l)) = (snap.at, snap.limits) {
            if newest.as_ref().is_none_or(|(n, _)| at > *n) {
                newest = Some((at, l));
            }
        }
    }
    newest
}

impl BoardMeta {
    pub fn new(machine: String) -> BoardMeta {
        BoardMeta {
            machine,
            ..BoardMeta::default()
        }
    }

    pub fn set_accounts(&mut self, a: Accounts) {
        self.emails = a.emails;
        self.codex_email = a.codex_email;
        self.codex_fallback = a.codex_fallback;
    }

    /// The email an account is, by engine and profile: Claude Code's `logins` entry of `fleet.json` where it has one
    /// (the account a profile really is, where its config file says another), else its config's.
    fn email_of(&self, engine: &Engine, profile: &str) -> Option<String> {
        match engine {
            Engine::Claude => self
                .panel
                .logins
                .get(&format!("{}:{profile}", self.machine))
                .or_else(|| self.emails.get(profile))
                .cloned(),
            Engine::Codex => self.codex_email.clone(),
            Engine::Muse => None,
        }
    }

    /// The display name of a session's account.
    pub fn account_of(&self, engine: &Engine, profile: &str) -> Option<String> {
        let email = self.email_of(engine, profile)?;
        Some(self.panel.account_name(engine, &email))
    }

    /// Fills the four board fields of a session built from its own files.
    pub fn overlay(&self, s: &mut Session) {
        let key = format!("{}:{}", self.machine, s.key.session_id);
        let named = self.panel.named(&key, &s.engine, s.name.as_deref());
        s.title = named.title;
        s.epic = named.epic;
        s.archived = self
            .panel
            .is_archived(&key, s.live, s.started_at, s.state == State::NeedsYou);
        s.account = self.account_of(&s.engine, &s.key.profile);
    }

    /// The groups and accounts. `profiles` are the Claude Code profile ids in order, `codex` Codex's newest limits
    /// among its logs. One account per engine and email (the same login in two profiles is one budget, with the newest
    /// reading), in discovery order: Claude Code's profiles, then Codex.
    pub fn meta(&self, profiles: &[String], codex: Option<&Reading>) -> Meta {
        let mut accounts: Vec<(Engine, String, Account, i64)> = vec![];
        let mut add = |engine: Engine, profile: &str, reading: Option<&Reading>| {
            let Some(email) = self.email_of(&engine, profile) else {
                return;
            };
            let (at, limits) = reading.map_or((i64::MIN, vec![]), |(a, l)| (*a, l.clone()));
            match accounts
                .iter_mut()
                .find(|(e, m, _, _)| *e == engine && *m == email)
            {
                Some((_, _, a, was)) => {
                    if at > *was {
                        a.limits = limits;
                        *was = at;
                    }
                }
                None => accounts.push((
                    engine.clone(),
                    email.clone(),
                    Account {
                        name: self.panel.account_name(&engine, &email),
                        engine,
                        profile: profile.to_string(),
                        limits,
                    },
                    at,
                )),
            }
        };
        for p in profiles {
            add(Engine::Claude, p, self.claude_limits.get(p));
        }
        add(
            Engine::Codex,
            CODEX_PROFILE,
            codex.or(self.codex_fallback.as_ref()),
        );
        Meta {
            epics: self.panel.epics.iter().map(|e| e.name.clone()).collect(),
            groups: self.panel.groups(),
            accounts: accounts.into_iter().map(|(_, _, a, _)| a).collect(),
            health: None,
        }
    }
}
