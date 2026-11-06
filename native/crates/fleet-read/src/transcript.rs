//! A session's transcript, read incrementally. Transcripts grow to hundreds of megabytes: the first read takes the last
//! 384 KiB (and the first 256 KiB, for the first ask) exactly as `readTranscript` in fleet/machine.mjs does; after that
//! only the bytes past the last offset are read, a line once its newline has arrived.
//!
//! `readTranscript` scans its tail backward from the end and stops at the first assistant line by which it has a
//! context size, something done or said, an ask, 8 steps and 12 tool uses. Read forward, each field keeps its newest
//! value and the line it came from; where that backward scan would have stopped is worked out from those lines, and a
//! value older than it is left out, as Node never sees it. So the summary is Node's for the same lines. One deliberate
//! difference: Node reads a fresh 384 KiB window every time, so once a file outgrows it Node forgets what the window
//! slid past; a `Tail` remembers what it has read since it opened.

pub(crate) mod js;
mod line;
pub(crate) mod reader;

use fleet_model::decide::TailFacts;
use fleet_model::stage::{stage_of, stage_of_tool};
use js::{one_line, slice16, truthy};
use memchr::{memchr_iter, memmem};
use reader::{LineReader, Step};
use serde::Serialize;
use serde_json::Value;
use std::collections::VecDeque;
use std::io;
use std::path::Path;

/// `TAIL` in machine.mjs: how much of the end the first read takes.
pub const TAIL: u64 = 384 * 1024;
/// `headLines`: how much of the start the first ask is looked for in.
pub const HEAD: u64 = 256 * 1024;
const STEPS: usize = 8;
const USES: usize = 12;

/// Its last word a tool call with no result yet: that step is running, since then.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Running {
    pub step: Option<String>,
    pub since: Option<i64>,
    pub tool: Option<String>,
    pub cmd: Option<String>,
}

/// A limit stop: Claude Code's own reply (model `<synthetic>`) saying a limit was hit.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LimitHit {
    pub text: Option<String>,
    pub at: Option<i64>,
}

/// What you typed that still waits for the session to take it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Queued {
    pub text: String,
    pub at: Option<i64>,
}

/// What `readTranscript` returns that a Session or TailFacts uses.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TranscriptSummary {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub context_tokens: Option<u64>,
    pub last_at: Option<i64>,
    pub said: Option<String>,
    pub doing: Option<String>,
    pub last_ask: Option<String>,
    pub first_ask: Option<String>,
    pub turn_over: Option<bool>,
    pub running: Option<Running>,
    pub limit_hit: Option<LimitHit>,
    pub stage: Option<String>,
    pub steps: Vec<String>,
    pub workdir: Option<String>,
    pub queued: Vec<Queued>,
}

/// A value and the line (counted over the lines Node's scan looks at) it came from.
#[derive(Clone, Debug)]
struct At<T> {
    line: u64,
    v: T,
}

#[derive(Clone, Debug)]
struct Ctx {
    tokens: u64,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Clone, Debug)]
struct Ask {
    text: String,
    /// The newest assistant line before it: where Node's scan stops when the ask is the last thing it needed.
    reply_before: Option<u64>,
}

/// The newest of everything, per line in file order.
#[derive(Debug, Default)]
struct Scan {
    lines: u64,
    last_reply: Option<u64>,
    last_at: Option<At<i64>>,
    last_ask: Option<At<Ask>>,
    turn: Option<(bool, Option<Running>)>,
    limit: Option<At<Option<LimitHit>>>,
    ctx: Option<At<Ctx>>,
    doing: Option<At<String>>,
    said: Option<At<String>>,
    workdir: Option<At<String>>,
    steps: VecDeque<At<String>>,
    uses: VecDeque<At<Option<&'static str>>>,
    queue: Vec<Queued>,
    /// While true, complete lines that end inside the first `HEAD` bytes still decide the first ask.
    head_open: bool,
    first_ask: Option<String>,
    first_ask_found: bool,
}

/// A transcript being followed.
#[derive(Debug)]
pub struct Tail {
    reader: LineReader,
    /// `home_dir()`, resolved once per open, for a tool call's `~/` paths.
    home: String,
    scan: Scan,
    summary: TranscriptSummary,
}

impl Tail {
    /// The first read: the last 384 KiB (the line it starts inside dropped, as `tail()` does) and, when that is not the
    /// whole file, the first 256 KiB for the first ask: at most 640 KiB, and never more than the file.
    pub fn open(path: &Path) -> io::Result<Tail> {
        let (reader, first) = LineReader::open(path, TAIL, HEAD)?;
        let mut t = Tail {
            reader,
            home: crate::home::home_dir().to_string_lossy().into_owned(),
            scan: Scan {
                head_open: first.whole,
                ..Scan::default()
            },
            summary: TranscriptSummary::default(),
        };
        t.scan.first_ask_of_head(&first.head);
        t.feed(&first.body);
        t.summary = t.scan.summary();
        Ok(t)
    }

    /// Reads what was appended since the last read, and only that. A file that shrank or was replaced (another inode)
    /// is read again from scratch. Whether the summary or the facts changed.
    pub fn advance(&mut self) -> io::Result<bool> {
        let before = (self.summary.clone(), self.reader.written_at);
        match self.reader.advance()? {
            Step::Reopen => {
                let read = self.reader.bytes_read;
                let path = self.reader.path().to_path_buf();
                *self = Tail::open(&path)?;
                self.reader.bytes_read += read;
            }
            Step::Appended(new) => {
                if self.feed(&new) {
                    self.summary = self.scan.summary();
                }
            }
            Step::Unchanged => {}
        }
        Ok((&self.summary, self.reader.written_at) != (&before.0, before.1))
    }

    /// Takes in bytes read at the offset: every line they complete. Whether any line was taken in.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let Tail {
            reader, scan, home, ..
        } = self;
        reader.feed(bytes, |l, end| scan.line(l, end, home))
    }

    /// The reconcile's read (Ruling R11): first checks that the bytes before the offset are still the ones read there,
    /// and reads the file again from scratch when they are not (rewritten in place), then reads what was appended.
    /// Whether the summary or the facts changed.
    pub fn reconcile(&mut self) -> io::Result<bool> {
        if self.reader.verify()? {
            return self.advance();
        }
        let before = (self.summary.clone(), self.reader.written_at);
        let read = self.reader.bytes_read;
        let path = self.reader.path().to_path_buf();
        *self = Tail::open(&path)?;
        self.reader.bytes_read += read;
        Ok((&self.summary, self.reader.written_at) != (&before.0, before.1))
    }

    /// The file this follows.
    pub fn path(&self) -> &Path {
        self.reader.path()
    }

    pub fn facts(&self) -> TailFacts {
        TailFacts {
            last_at: self.summary.last_at,
            written_at: self.reader.written_at,
            limit_hit_at: self.summary.limit_hit.as_ref().and_then(|l| l.at),
            turn_over: self.summary.turn_over,
        }
    }

    pub fn summary(&self) -> &TranscriptSummary {
        &self.summary
    }

    /// Bytes read from the file since `open`, the first read included.
    pub fn bytes_read(&self) -> u64 {
        self.reader.bytes_read
    }
}

fn str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn date(v: Option<&Value>) -> Option<i64> {
    v.and_then(Value::as_str).and_then(js::date_parse)
}

fn push_ring<T>(ring: &mut VecDeque<T>, cap: usize, v: T) {
    if ring.len() == cap {
        ring.pop_front();
    }
    ring.push_back(v);
}

impl Scan {
    /// The first ask from a separately read head: its newline-ended lines (`headLines` drops the cut last one).
    fn first_ask_of_head(&mut self, head: &[u8]) {
        let mut from = 0;
        for nl in memchr_iter(b'\n', head) {
            let l = &head[from..nl];
            from = nl + 1;
            if !l.is_empty() && !self.first_ask_line(js::parse(l).as_ref()) {
                return;
            }
        }
    }

    /// `out.firstAsk = oneLine(askOf(d), 500)` until one is truthy. Whether to keep looking.
    fn first_ask_line(&mut self, d: Option<&Value>) -> bool {
        self.first_ask = d.and_then(line::ask_of).map(|a| one_line(&a, 500));
        self.first_ask_found = self.first_ask.as_deref().is_some_and(|a| !a.is_empty());
        !self.first_ask_found
    }

    /// One complete line, `end` the file offset just past its newline.
    fn line(&mut self, l: &[u8], end: u64, home: &str) {
        let d = js::parse(l);
        if self.head_open && !self.first_ask_found {
            if end <= HEAD {
                self.first_ask_line(d.as_ref());
            } else {
                self.head_open = false;
            }
        }
        let Some(d) = d.filter(Value::is_object) else {
            return;
        };
        if memmem::find(l, b"\"queue-operation\"").is_some() {
            self.queue_op(&d);
        }
        if truthy(d.get("isSidechain")) {
            return;
        }
        let n = self.lines;
        self.lines += 1;

        if truthy(d.get("timestamp")) {
            if let Some(t) = date(d.get("timestamp")) {
                self.last_at = Some(At { line: n, v: t });
            }
        }
        if let Some(a) = line::ask_of(&d) {
            self.last_ask = Some(At {
                line: n,
                v: Ask {
                    text: one_line(&a, 500),
                    reply_before: self.last_reply,
                },
            });
        }
        let kind = d.get("type").and_then(Value::as_str);
        let has_msg = truthy(d.get("message"));
        let msg = &d["message"];
        let content = msg.get("content").and_then(Value::as_array);
        if matches!(kind, Some("assistant" | "user")) && has_msg {
            let assistant = kind == Some("assistant");
            let synthetic = msg.get("model").and_then(Value::as_str) == Some("<synthetic>");
            let ended = matches!(
                msg.get("stop_reason").and_then(Value::as_str),
                Some("end_turn" | "stop_sequence")
            );
            let running = if assistant {
                content.and_then(|c| c.iter().rev().find(|c| is_type(c, "tool_use")))
            } else {
                None
            }
            .map(|u| {
                let name = u.get("name").and_then(Value::as_str);
                let input = js::present(u.get("input")).unwrap_or(&Value::Null);
                Running {
                    step: line::step_of(name, input, true),
                    since: date(d.get("timestamp")),
                    tool: name.map(str::to_string),
                    cmd: (name == Some("Bash"))
                        .then(|| input.get("command").and_then(Value::as_str))
                        .flatten()
                        .map(|c| slice16(c, 2000).to_string()),
                }
            });
            self.turn = Some((assistant && (ended || synthetic), running));
        }
        if kind != Some("assistant") || !has_msg {
            return;
        }
        self.last_reply = Some(n);
        let synthetic = msg.get("model").and_then(Value::as_str) == Some("<synthetic>");
        if synthetic {
            let text = content
                .and_then(|c| c.iter().find(|c| is_type(c, "text")))
                .and_then(|c| js::present(c.get("text")));
            let s = text.map(js::string).unwrap_or_default();
            let hit = line::LIMIT_HIT.is_match(&s).then(|| LimitHit {
                text: text.and_then(Value::as_str).map(|t| one_line(t, 200)),
                at: date(d.get("timestamp")),
            });
            self.limit = Some(At { line: n, v: hit });
        }
        if !synthetic && truthy(msg.get("usage")) {
            let u = &msg["usage"];
            let k = |f: &str| u.get(f).and_then(Value::as_f64).unwrap_or(0.0);
            let tokens = (k("input_tokens")
                + k("cache_creation_input_tokens")
                + k("cache_read_input_tokens")) as u64;
            let model = str_of(d.get("requestedModel")).or_else(|| str_of(msg.get("model")));
            self.ctx = Some(At {
                line: n,
                v: Ctx {
                    tokens,
                    model,
                    effort: str_of(d.get("effort")),
                },
            });
        }
        for c in content.into_iter().flatten() {
            if is_type(c, "tool_use") {
                let name = c.get("name").and_then(Value::as_str);
                let input = js::present(c.get("input")).unwrap_or(&Value::Null);
                if let Some(st) = line::step_of(name, input, false).filter(|s| !s.is_empty()) {
                    push_ring(&mut self.steps, STEPS, At { line: n, v: st });
                }
                push_ring(
                    &mut self.uses,
                    USES,
                    At {
                        line: n,
                        v: stage_of_tool(name, input),
                    },
                );
                if let Some(w) = line::where_of(name, input, home) {
                    self.workdir = Some(At { line: n, v: w });
                }
                if let Some(st) = line::step_of(name, input, true).filter(|s| !s.is_empty()) {
                    self.doing = Some(At { line: n, v: st });
                }
            }
            if is_type(c, "text") {
                if let Some(t) = c
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !js::trim(t).is_empty())
                {
                    self.said = Some(At {
                        line: n,
                        v: one_line(t, 400),
                    });
                }
            }
        }
    }

    /// `queuedIn`, one record at a time.
    fn queue_op(&mut self, d: &Value) {
        if d.get("type").and_then(Value::as_str) != Some("queue-operation") {
            return;
        }
        let content = d.get("content").and_then(Value::as_str);
        match d.get("operation").and_then(Value::as_str) {
            Some("enqueue") if content.is_some() => self.queue.push(Queued {
                text: content.unwrap_or_default().to_string(),
                at: date(d.get("timestamp")),
            }),
            Some("dequeue") if !self.queue.is_empty() => {
                self.queue.remove(0);
            }
            Some("remove") => {
                if let Some(i) = self
                    .queue
                    .iter()
                    .position(|q| Some(q.text.as_str()) == content)
                {
                    self.queue.remove(i);
                }
            }
            Some("popAll") => self.queue.clear(),
            _ => {}
        }
    }

    /// The line `readTranscript`'s backward scan stops at, if it stops: the newest assistant line by which every one of
    /// its conditions is met, that is at or before the oldest line each condition needs.
    fn stop_line(&self) -> Option<u64> {
        let ctx = self.ctx.as_ref()?.line;
        let done_or_said = self
            .doing
            .as_ref()
            .map(|a| a.line)
            .max(self.said.as_ref().map(|a| a.line))?;
        let ask = self.last_ask.as_ref().filter(|a| !a.v.text.is_empty())?;
        let steps = (self.steps.len() == STEPS).then(|| self.steps[0].line)?;
        let uses = (self.uses.len() == USES).then(|| self.uses[0].line)?;
        let replies = ctx.min(done_or_said).min(steps).min(uses);
        if ask.line < replies {
            ask.v.reply_before
        } else {
            Some(replies)
        }
    }

    fn summary(&self) -> TranscriptSummary {
        let stop = self.stop_line();
        let seen = |line: u64| stop.is_none_or(|s| line >= s);
        let val = |a: &Option<At<String>>| a.as_ref().filter(|a| seen(a.line)).map(|a| a.v.clone());
        let ctx = self.ctx.as_ref().map(|c| &c.v);
        let (turn_over, running) = match &self.turn {
            Some((over, run)) => (Some(*over), run.clone()),
            None => (None, None),
        };
        let first_ask = self.first_ask.clone();
        TranscriptSummary {
            model: ctx.and_then(|c| c.model.clone()),
            effort: ctx.and_then(|c| c.effort.clone()),
            context_tokens: ctx.map(|c| c.tokens),
            last_at: self.last_at.as_ref().filter(|a| seen(a.line)).map(|a| a.v),
            said: val(&self.said),
            doing: val(&self.doing),
            last_ask: self
                .last_ask
                .as_ref()
                .map(|a| a.v.text.clone())
                .or_else(|| first_ask.clone()),
            first_ask,
            turn_over,
            running,
            limit_hit: self
                .limit
                .as_ref()
                .filter(|a| seen(a.line))
                .and_then(|a| a.v.clone()),
            stage: stage_of(self.uses.iter().rev().map(|u| u.v)).map(str::to_string),
            steps: self.steps.iter().map(|s| s.v.clone()).collect(),
            workdir: val(&self.workdir),
            queued: self
                .queue
                .iter()
                .filter(|q| !q.text.trim_start_matches(js::is_ws).starts_with('<'))
                .cloned()
                .collect(),
        }
    }
}

fn is_type(c: &Value, t: &str) -> bool {
    c.get("type").and_then(Value::as_str) == Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_surrogates_are_read_as_js_reads_them() {
        let v = js::parse(br#"{"t":"cut \ud83d","ok":"\ud83d\ude00","esc":"\\ud83d"}"#)
            .expect("parsed");
        assert_eq!(v["t"], "cut \u{fffd}");
        assert_eq!(v["ok"], "\u{1f600}");
        assert_eq!(v["esc"], "\\ud83d");
    }

    #[test]
    fn js_helpers() {
        assert_eq!(js::one_line("  a \u{feff}\n\t b  ", 10), "a b");
        assert_eq!(js::slice16("a\u{1f600}b", 2), "a");
        assert_eq!(js::slice16("看板x", 2), "看板");
        assert_eq!(
            js::date_parse("2026-11-06T09:00:00.000Z"),
            Some(1_793_955_600_000)
        );
        assert_eq!(
            js::date_parse("2026-11-06T09:00:00.1234Z"),
            Some(1_793_955_600_123)
        );
        assert_eq!(
            js::date_parse("2026-11-06T09:00:00+02:00"),
            Some(1_793_948_400_000)
        );
        assert_eq!(js::date_parse("2026-11-06"), Some(1_793_923_200_000));
        assert_eq!(
            js::date_parse("2026-02-30T00:00:00Z"),
            Some(1_772_409_600_000)
        );
        assert_eq!(js::date_parse("2026-11-06T09:00:00"), None);
        assert_eq!(js::date_parse("1970-01-01T00:00:00Z"), None);
        for (p, d, b) in [
            ("/a//b", "/a/", "b"),
            ("/a/b/", "/a", "b"),
            ("a", ".", "a"),
            ("/", "/", ""),
            ("//a", "//", "a"),
            ("a/b//", "a", "b"),
        ] {
            assert_eq!((js::dirname(p), js::basename(p)), (d, b), "{p}");
        }
        assert_eq!(
            js::url_host("https://A.Example.TEST:443/x").as_deref(),
            Some("a.example.test")
        );
        assert_eq!(
            js::url_host("http://u:p@h.test:80/").as_deref(),
            Some("h.test")
        );
        assert_eq!(js::url_host("file:///x").as_deref(), Some(""));
        assert_eq!(js::url_host("not a url"), None);
        assert_eq!(js::url_host("https://"), None);
    }

    #[test]
    fn claude_folders() {
        for (d, yes) in [
            ("/r/.claude", true),
            ("/r/.claude/x", true),
            ("/r/.claude-work/x", true),
            ("/r/.claude/worktrees/wt", false),
            ("/r/.claude-work/worktrees/wt", false),
            ("/r/.clauder", false),
            ("/r/x.claude", false),
        ] {
            assert_eq!(line::in_claude_dir(d), yes, "{d}");
        }
    }
}
