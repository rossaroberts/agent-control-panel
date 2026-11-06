//! The program a terminal runs: a port of `fleet/term.mjs` `command()`, local branch. `claude attach <job>`, `codex resume`
//! in a folder, or a bare `claude` in a folder (for its trust prompt). `None` for a bad request or a missing folder.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use fleet_model::types::AttachTarget;

/// What to exec: the full argv, the full environment (inherited, minus what must not leak, plus the terminal's own), and
/// the working folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

/// `^[\w.-]{1,40}$`
fn profile_ok(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `^[0-9a-f]{8}$`
fn job_ok(s: &str) -> bool {
    s.len() == 8
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`
fn uuid_ok(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == 5
        && parts.iter().zip(lens).all(|(p, n)| {
            p.len() == n
                && p.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// `^(~|~\/[^\0\n\r]{0,400}|\/[^\0\n\r]{0,400})$`: absolute or under `~`, one line (length counted in UTF-16 units, as JS does).
fn dir_ok(s: &str) -> bool {
    let rest = if s == "~" {
        return true;
    } else if let Some(r) = s.strip_prefix("~/") {
        r
    } else if let Some(r) = s.strip_prefix('/') {
        r
    } else {
        return false;
    };
    !rest.contains(['\0', '\n', '\r']) && rest.encode_utf16().count() <= 400
}

/// The first of ~/.local/bin/<name>, /opt/homebrew/bin/<name>, /usr/local/bin/<name> that exists, else the bare name.
fn bin(home: &Path, name: &str) -> OsString {
    [
        home.join(".local/bin").join(name),
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
    ]
    .into_iter()
    .find(|p| p.exists())
    .map(PathBuf::into_os_string)
    .unwrap_or_else(|| name.into())
}

/// Node's `path.join(home, rel)`: joined, then `.` and `..` and doubled slashes folded away.
fn join_norm(home: &Path, rel: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in home.join(rel).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// `~` is home, `~/x` is under it, anything else is taken as it is.
fn expand(dir: &str, home: &Path) -> PathBuf {
    match dir.strip_prefix("~/") {
        Some(rest) => join_norm(home, rest),
        None if dir == "~" => home.to_path_buf(),
        None => PathBuf::from(dir),
    }
}

/// The environment a terminal's program gets: ours without `CLAUDE*`, `FLEET_*`, `TMUX`, `TMUX_PANE`, plus the terminal's own.
fn base_env() -> Vec<(OsString, OsString)> {
    let lang = std::env::var_os("LANG")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "en_US.UTF-8".into());
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| {
            let k = k.to_string_lossy();
            !(k.starts_with("CLAUDE") || k.starts_with("FLEET_") || k == "TMUX" || k == "TMUX_PANE")
        })
        .filter(|(k, _)| {
            !matches!(
                k.to_str(),
                Some("TERM" | "COLORTERM" | "COLORFGBG" | "LANG")
            )
        })
        .collect();
    for (k, v) in [
        ("TERM", "xterm-256color".into()),
        ("COLORTERM", "truecolor".into()),
        ("COLORFGBG", "15;0".into()),
        ("LANG", lang),
    ] {
        env.push((k.into(), v));
    }
    env
}

fn set(env: &mut Vec<(OsString, OsString)>, key: &str, val: PathBuf) {
    env.retain(|(k, _)| k != key);
    env.push((key.into(), val.into_os_string()));
}

pub fn command(target: &AttachTarget, home: &Path) -> Option<Cmd> {
    match target {
        AttachTarget::Codex {
            profile,
            session_id,
            cwd,
        } => {
            if !(uuid_ok(session_id) && dir_ok(cwd) && profile_ok(profile)) {
                return None;
            }
            let mut env = base_env();
            if profile != "codex" {
                set(&mut env, "CODEX_HOME", home.join(format!(".{profile}")));
            }
            let cwd = expand(cwd, home);
            // --no-daemon: without it Codex starts a shared background service that keeps the thread (and blocks the Codex
            // app, "open in another app") after this terminal closes.
            cwd.exists().then(|| Cmd {
                argv: vec![
                    bin(home, "codex"),
                    "resume".into(),
                    "--no-daemon".into(),
                    session_id.into(),
                    "--dangerously-bypass-approvals-and-sandbox".into(),
                ],
                env,
                cwd,
            })
        }
        AttachTarget::Claude { profile, job } => {
            if !(profile_ok(profile) && job_ok(job)) {
                return None;
            }
            let mut env = base_env();
            if profile != "default" {
                set(
                    &mut env,
                    "CLAUDE_CONFIG_DIR",
                    home.join(format!(".claude-{profile}")),
                );
            }
            Some(Cmd {
                argv: vec![bin(home, "claude"), "attach".into(), job.into()],
                env,
                cwd: home.to_path_buf(),
            })
        }
        AttachTarget::Trust { profile, dir } => {
            if !(profile_ok(profile) && dir_ok(dir)) {
                return None;
            }
            let mut env = base_env();
            if profile != "default" {
                set(
                    &mut env,
                    "CLAUDE_CONFIG_DIR",
                    home.join(format!(".claude-{profile}")),
                );
            }
            let cwd = expand(dir, home);
            cwd.exists().then(|| Cmd {
                argv: vec![bin(home, "claude")],
                env,
                cwd,
            })
        }
    }
}
