//! Where a session is on the way to production, from what its tools did last: the first one (newest first) that ships,
//! promotes, tests or builds says so; reading and looking around alone is planning. No model's guess.
//! A port of `stageOfTool` / `stageOf` in fleet/machine.mjs (golden: native/parity/golden/stage.json).

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub const PLANNING: &str = "planning";

/// `STAGE_OF_RUN`: a command's stage, the first pattern that matches. JS's `\b` and `\w` are ASCII, as here.
static STAGE_OF_RUN: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let b = r"(?-u:\b)";
    let dot = r"[^\n\r\u{2028}\u{2029}]"; // JS `.`
    let w = r"[A-Za-z0-9_]";
    [
        ("deploying", format!(r"{b}(gh workflow run{b}|deploy\.sh|deploy/\S*\.sh{b}|release\.yml|ship\.yml|kubectl|docker compose {dot}*{b}up{b})")),
        ("promoting", format!(r"{b}(gh pr (create|merge|ready|edit|update-branch|checks)|git push|gh run (watch|view)|merge{dot}queue)")),
        ("testing", format!(r"{b}(npm (run )?(test|t){b}|test:[\w:-]+|vitest|jest|playwright|pytest|smoke(:[\w-]+)?|typecheck|tsc{b}|lint{b}|knip|verify-gate|check:[\w-]+|docs:check|node --test)").replace(r"\w", w)),
        ("building", format!(r"{b}(npm (i|install|ci){b}|db:(reset|rebuild)|new:migration|npm run build|git (commit|add|rebase|merge){b})")),
    ]
    .into_iter()
    .map(|(s, re)| (s, Regex::new(&format!("(?i){re}")).expect("STAGE_OF_RUN")))
    .collect()
});

static REVIEW_SKILL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("verify-gate|code-review|review").expect("skill"));

/// JS `String(v)` for the values a tool input holds; `null`/absent are handled by the caller's `??`.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(xs) => xs
            .iter()
            .map(|x| {
                if x.is_null() {
                    String::new()
                } else {
                    js_string(x)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `x ?? y` over JSON: absent and `null` fall through.
fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// The stage one tool use says, or `None` for agents, tasks and questions, which say nothing about where the work is.
/// `name` is `None` when the transcript's name is not a string.
pub fn stage_of_tool(name: Option<&str>, input: &Value) -> Option<&'static str> {
    let name = name?;
    match name {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "apply_patch" => return Some("building"),
        "Read" | "Grep" | "Glob" | "WebFetch" | "WebSearch" | "ExitPlanMode" | "LSP" => {
            return Some(PLANNING)
        }
        _ if name.starts_with("mcp__") => return Some(PLANNING),
        _ => {}
    }
    if name == "Skill" {
        let skill = present(input.get("skill"))
            .map(js_string)
            .unwrap_or_default();
        return REVIEW_SKILL.is_match(&skill).then_some("testing");
    }
    if matches!(name, "Bash" | "shell" | "exec_command") {
        let cmd = match input.get("command") {
            Some(Value::Array(xs)) => xs
                .iter()
                .map(|x| {
                    if x.is_null() {
                        String::new()
                    } else {
                        js_string(x)
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            c => present(c)
                .or_else(|| present(input.get("cmd")))
                .map(js_string)
                .unwrap_or_default(),
        };
        return Some(
            STAGE_OF_RUN
                .iter()
                .find(|(_, re)| re.is_match(&cmd))
                .map_or(PLANNING, |(s, _)| s),
        );
    }
    None
}

/// The stage from per-use stages, newest first: the first that is not planning wins, else planning if any use looked.
pub fn stage_of<'a>(uses: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    let mut looked = None;
    for st in uses.into_iter().flatten() {
        if st != PLANNING {
            return Some(st);
        }
        looked.get_or_insert(st);
    }
    looked
}
