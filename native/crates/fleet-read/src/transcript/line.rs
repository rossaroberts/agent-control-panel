//! What one transcript line or tool call says, ported from fleet/machine.mjs: `askOf`/`realAsk` (what you typed),
//! `stepOf` (a tool call in plain words), `whereOf`/`branchOf` (the checkout a tool call works in) and `LIMIT_HIT`.

use super::js::{self, basename, dirname, one_line, present, slice16, tmpl, trim, truthy, WS};
use regex::Regex;
use serde_json::Value;
use std::path::Path;
use std::sync::LazyLock;

fn re(s: &str) -> Regex {
    Regex::new(s).expect("transcript regex")
}

/// JS `.`: anything but a line terminator.
const DOT: &str = r"[^\n\r\u{2028}\u{2029}]";

static CONTINUED: LazyLock<Regex> =
    LazyLock::new(|| re("^This session is being continued from a previous conversation"));
static PRIMARY: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"Primary Request and Intent:?\**[{WS}]*((?s:.)*?)(?:\n[{WS}]*(?:[0-9]+\.|#+)[{WS}]*\**[A-Z][^\n]{{2,60}}:|$)"
    ))
});
static CONTINUED_LINE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^This session is being continued[^\n]*\n+"));
static PEER: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"^Another Claude session sent a message:[{WS}]*((?s:.)*?)[{WS}]*This came from another Claude session"
    ))
});
pub static LIMIT_HIT: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"(?i)(hit|reached) your {DOT}{{0,40}}limit")));
static CD: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r#"(?:^|&&|;|\n)[{WS}]*cd[{WS}]+(?:--[{WS}]+)?("[^"]+"|'[^']+'|[^{WS};&|]+)"#
    ))
});
static GIT_C: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r#"(?-u:\b)git[{WS}]+-C[{WS}]+("[^"]+"|'[^']+'|[^{WS}]+)"#
    ))
});
static HEAD_REF: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"(?mR)^ref: refs/heads/({DOT}+)$")));
static GITDIR: LazyLock<Regex> = LazyLock::new(|| re(&format!(r"(?mR)^gitdir:[{WS}]*({DOT}+)$")));

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// `askOf`: what you typed in a user line, or `None` for tool results, commands and what Claude Code sent in your name.
pub fn ask_of(d: &Value) -> Option<String> {
    if str_at(d, "type") != Some("user")
        || truthy(d.get("isMeta"))
        || truthy(d.get("isSidechain"))
        || !truthy(d.get("message"))
    {
        return None;
    }
    let text = match d["message"].get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(xs)) if !xs.iter().any(|x| str_at(x, "type") == Some("tool_result")) => {
            let texts: Vec<Value> = xs
                .iter()
                .filter(|x| str_at(x, "type") == Some("text"))
                .map(|x| x.get("text").cloned().unwrap_or(Value::Null))
                .collect();
            js::join(&texts, "\n")
        }
        _ => String::new(),
    };
    let t = trim(&text);
    if t.is_empty()
        || t.starts_with('<')
        || t.starts_with("[Request interrupted")
        || t.starts_with("Caveat:")
    {
        return None;
    }
    Some(slice16(&real_ask(t), 4000).to_string())
}

/// `realAsk`: a continued session's "Primary Request and Intent", a peer message's own words, else the text.
pub fn real_ask(t: &str) -> String {
    if CONTINUED.is_match(t) {
        let body = match PRIMARY.captures(t) {
            Some(m) => m.get(1).map_or("", |g| g.as_str()).to_string(),
            None => CONTINUED_LINE.replace(t, "").into_owned(),
        };
        return format!("(continued) {}", trim(&body));
    }
    match PEER.captures(t) {
        Some(m) => trim(m.get(1).map_or("", |g| g.as_str())).to_string(),
        None => t.to_string(),
    }
}

/// `stepOf`: a tool call in plain words, its own description when it has one, else what it does to what. `None` for
/// the bookkeeping calls nobody needs to see. `brief` (a row's text): a Bash call with no description is "Run a command".
pub fn step_of(name: Option<&str>, x: &Value, brief: bool) -> Option<String> {
    if let Some(n) = name {
        if n.starts_with("mcp__ccd_turn__") || n == "ToolSearch" {
            return None;
        }
    }
    if let Some(d) = str_at(x, "description") {
        if !trim(d).is_empty() {
            return Some(one_line(d, 200));
        }
    }
    let name = name?;
    if brief && name == "Bash" {
        return Some("Run a command".into());
    }
    let base = |k: &str| str_at(x, k).map(basename).unwrap_or("").to_string();
    let said = match name {
        "Read" => format!("Read {}", base("file_path")),
        "Edit" | "MultiEdit" => format!("Edit {}", base("file_path")),
        "Write" => format!("Write {}", base("file_path")),
        "NotebookEdit" => format!("Edit {}", base("notebook_path")),
        "Grep" => format!("Search for {}", tmpl(x.get("pattern"), "")),
        "Glob" => format!("Find {}", tmpl(x.get("pattern"), "")),
        "WebFetch" => match str_at(x, "url").and_then(js::url_host) {
            Some(h) => format!("Read {h}"),
            None => "Read a web page".into(),
        },
        "WebSearch" => format!("Search the web for {}", tmpl(x.get("query"), "")),
        "Bash" => format!(
            "Run {}",
            tmpl(x.get("command"), "").split('\n').next().unwrap_or("")
        ),
        "AskUserQuestion" => {
            let q = x
                .get("questions")
                .and_then(|q| q.get(0))
                .and_then(|q| q.get("question"));
            format!("Asks: {}", tmpl(q, ""))
        }
        "ExitPlanMode" => "Plan ready for you".into(),
        "TodoWrite" | "TaskUpdate" => "Update its task list".into(),
        "TaskCreate" => format!(
            "Add task: {}",
            tmpl(present(x.get("subject")).or(x.get("title")), "")
        ),
        "Skill" => format!("Use {}", tmpl(x.get("skill"), "a skill")),
        "Agent" | "Task" => format!("Start an agent: {}", tmpl(x.get("prompt"), "")),
        _ => match mcp_parts(name) {
            Some((server, tool)) => {
                let server = server
                    .strip_prefix("plugin_")
                    .or_else(|| server.strip_prefix("claude_ai_"))
                    .unwrap_or(server);
                format!("{} ({})", squash_dashes(tool), server.replace('_', " "))
            }
            None => name.to_string(),
        },
    };
    Some(one_line(&said, 200))
}

/// `/^mcp__([^_]+(?:_[^_]+)*)__(.+)$/`: the server (no `__` in it, no `_` at either end) and the tool (no line break).
fn mcp_parts(name: &str) -> Option<(&str, &str)> {
    let s = name.strip_prefix("mcp__")?;
    if s.starts_with('_') {
        return None;
    }
    let p = s.find("__")?;
    let tool = &s[p + 2..];
    (p > 0 && !tool.is_empty() && !tool.contains(['\n', '\r', '\u{2028}', '\u{2029}']))
        .then(|| (&s[..p], tool))
}

/// `.replace(/[_-]+/g, ' ')`.
fn squash_dashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = false;
    for c in s.chars() {
        let dash = c == '_' || c == '-';
        if !dash {
            out.push(c);
        } else if !run {
            out.push(' ');
        }
        run = dash;
    }
    out
}

/// `whereOf`: the folder a tool call works in, when it says and it is in a git checkout that is not Claude Code's own.
pub fn where_of(name: Option<&str>, x: &Value, home: &str) -> Option<String> {
    let abs = |v: Option<&Value>| -> Option<String> {
        let v = v?.as_str()?;
        if v.starts_with('/') {
            Some(v.to_string())
        } else {
            v.strip_prefix('~')
                .filter(|r| r.starts_with('/'))
                .map(|r| format!("{home}{r}"))
        }
    };
    let f = abs(present(x.get("file_path")).or(x.get("notebook_path")));
    if let Some(f) = f {
        return work(dirname(&f), home);
    }
    if let (Some("Bash"), Some(cmd)) = (name, str_at(x, "command")) {
        let m = CD.captures(cmd).or_else(|| GIT_C.captures(cmd))?;
        let g = m.get(1)?.as_str();
        let g = g.strip_prefix(['"', '\'']).unwrap_or(g);
        let g = g.strip_suffix(['"', '\'']).unwrap_or(g);
        return work(&abs(Some(&Value::String(g.to_string())))?, home);
    }
    work(&abs(x.get("path"))?, home)
}

fn work(d: &str, home: &str) -> Option<String> {
    (!d.is_empty() && !in_claude_dir(d) && branch_of(d, home).is_some()).then(|| d.to_string())
}

/// `/\/\.claude(-[\w.-]+)?(\/(?!worktrees\/)|$)/`: a Claude Code folder (but its worktrees are checkouts).
pub(super) fn in_claude_dir(d: &str) -> bool {
    let word = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-');
    let b = d.as_bytes();
    let ends_ok =
        |i: usize| i == b.len() || (b[i] == b'/' && !b[i + 1..].starts_with(b"worktrees/"));
    d.match_indices("/.claude").any(|(at, m)| {
        let i = at + m.len();
        if ends_ok(i) {
            return true;
        }
        // with the `-name` part: every length the regex could back off to
        if b.get(i) != Some(&b'-') {
            return false;
        }
        let run = b[i + 1..].iter().take_while(|&&c| word(c)).count();
        (1..=run).any(|k| ends_ok(i + 1 + k))
    })
}

/// `branchOf`: the branch a folder's checkout is on (a worktree's `.git` is a file naming its git dir), from the
/// files, six folders up at most; `None` for a detached head or no checkout.
pub fn branch_of(cwd: &str, home: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let dir = if cwd == "~" {
        home.to_string()
    } else if let Some(r) = cwd.strip_prefix("~/") {
        format!("{home}/{r}")
    } else {
        cwd.to_string()
    };
    let mut d = dir.as_str();
    for _ in 0..6 {
        if d.is_empty() || d == dirname(d) {
            break;
        }
        let g = Path::new(d).join(".git");
        let head = match std::fs::metadata(&g) {
            Ok(m) if m.is_dir() => std::fs::read_to_string(g.join("HEAD")).ok(),
            Ok(_) => match std::fs::read_to_string(&g) {
                Ok(s) => match GITDIR
                    .captures(&s)
                    .map(|c| trim(&c[1]).to_string())
                    .filter(|t| !t.is_empty())
                {
                    Some(to) => std::fs::read_to_string(Path::new(d).join(to).join("HEAD")).ok(),
                    None => Some(String::new()),
                },
                Err(_) => None,
            },
            Err(_) => None,
        };
        match head {
            None => d = dirname(d),
            Some(h) => return HEAD_REF.captures(&h).map(|c| trim(&c[1]).to_string()),
        }
    }
    None
}
