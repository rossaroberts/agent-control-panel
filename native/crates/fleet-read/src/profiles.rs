//! Claude Code profiles, discovered under the home folder (`machine.mjs` `claudeProfiles`), and the account each is logged in to.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::json::read_json;

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// `default` for `~/.claude`, else the part after `.claude-`.
    pub id: String,
    pub dir: PathBuf,
    /// `~/.claude`: Claude Code is run with no `CLAUDE_CONFIG_DIR`.
    pub plain: bool,
    /// Where the profile's `oauthAccount` lives: `~/.claude.json` for the plain profile, else `<dir>/.claude.json`.
    pub config: PathBuf,
}

fn is_profile_name(n: &str) -> bool {
    n == ".claude"
        || n.strip_prefix(".claude-").is_some_and(|rest| {
            (1..=40).contains(&rest.len())
                && rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        })
}

/// Every `~/.claude` and `~/.claude-<name>` folder holding `settings.json`, `.claude.json` or `projects/`, sorted by folder name.
pub fn claude_profiles(home: &Path) -> Vec<Profile> {
    let Ok(rd) = std::fs::read_dir(home) else {
        return vec![];
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| is_profile_name(n))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|n| {
            let dir = home.join(&n);
            if !dir.is_dir() {
                return None;
            }
            if !["settings.json", ".claude.json", "projects"]
                .iter()
                .any(|f| dir.join(f).exists())
            {
                return None;
            }
            let plain = n == ".claude";
            Some(Profile {
                id: if plain {
                    "default".into()
                } else {
                    n[".claude-".len()..].to_string()
                },
                config: if plain {
                    home.join(".claude.json")
                } else {
                    dir.join(".claude.json")
                },
                dir,
                plain,
            })
        })
        .collect()
}

/// The email the profile is logged in as (`oauthAccount.emailAddress` in its config).
pub fn account(p: &Profile) -> Option<String> {
    let v: Value = read_json(&p.config, 8 << 20)?;
    v.get("oauthAccount")?
        .get("emailAddress")?
        .as_str()
        .map(str::to_string)
}
