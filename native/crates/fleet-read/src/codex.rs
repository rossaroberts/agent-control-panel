//! Codex and Muse sessions (ports of `codexHomes`/`codexAccount`/`codexText`/`recentRollouts`/`yours`/`readRollout` and
//! `museHome` in fleet/machine.mjs).
//!
//! A Codex session is a rollout, `sessions/YYYY/MM/DD/rollout-*.jsonl`, followed through the same byte reader as Claude's
//! transcripts (`transcript::reader::LineReader`: offset, partial line, reopen), so an append costs only its bytes.
//!
//! `readRollout` scans the last 384 KiB backward and stops at the first line by which it has everything it needs. Read
//! forward, each fact keeps its newest occurrence and the line it came from; `Scan::replay` then replays Node's backward
//! scan over those few occurrences (every fact is "the first one found from the end" in Node, so the newest occurrence is
//! the only one that can matter), stopping where Node stops. One deliberate difference, as for `Tail`: a `RolloutTail`
//! remembers what it read since it opened, where Node's window slides.

use crate::json::read_json;
use crate::transcript::js::{self, one_line, truthy};
use crate::transcript::reader::{LineReader, Step};
use crate::transcript::{HEAD, TAIL};
use fleet_model::types::{Engine, Session, SessionKey, State};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const DAY: i64 = 86_400_000;
/// `RECENT_CODEX_MS`: how far back a rollout's last write may be and still be listed.
pub const RECENT_CODEX_MS: i64 = 7 * DAY;
/// `CODEX_LIVE_MS`: a rollout written this recently is open, with or without a process scan.
const CODEX_LIVE_MS: i64 = 10 * 60_000;
/// Rollouts listed at most (`recentRollouts` slices to 200).
const RECENT_MAX: usize = 200;
/// `head(f)`: how much of the first line is read for the thread's meta.
const META_BYTES: u64 = 64 * 1024;

// ---------------------------------------------------------------- the account

/// Who a Codex home is signed in as: the email claim of its id token (`auth.json` `tokens.id_token`, a JWT) and nothing
/// else from that file. `None` for an API key alone, a token that is not a JWT, a payload that is not JSON, or no email.
pub fn codex_account(codex_home: &Path) -> Option<String> {
    let a: Value = read_json(&codex_home.join("auth.json"), 1 << 20)?;
    let token = js::present(a.get("tokens").and_then(|t| t.get("id_token")))
        .map(js::string)
        .unwrap_or_default();
    let payload = base64url(token.split('.').nth(1)?);
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    claims.get("email")?.as_str().map(str::to_string)
}

/// `Buffer.from(s, 'base64url')`: either alphabet, padding ends it, anything else is skipped.
fn base64url(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

// ---------------------------------------------------------------- the rollouts

fn local_ymd(ms: i64) -> (i32, u32, u32) {
    let t = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `tm` is plain data, zeroed before `localtime_r` fills it; both pointers are to live locals.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    (tm.tm_year + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32)
}

fn mtime_ms(f: &Path) -> f64 {
    fs::metadata(f)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_nanos() as f64 / 1e6)
}

/// The first line of a rollout, at most 64 KiB of it (`head`).
fn head_line(f: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = File::open(f).and_then(|f| f.take(META_BYTES).read_to_end(&mut buf));
    buf.truncate(buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len()));
    buf
}

/// A thread you started: Codex also logs the agents it starts and its reviewer's passes, each as a thread of its own.
pub fn is_yours(f: &Path) -> bool {
    let t = js::parse(&head_line(f));
    let src = t
        .as_ref()
        .and_then(|d| d.get("payload"))
        .and_then(|p| p.get("thread_source"));
    match js::present(src) {
        None => true,
        Some(v) => v.as_str() == Some("user"),
    }
}

/// Rollouts written to in the last `within_ms`: `sessions/YYYY/MM/DD/rollout-*.jsonl`, newest first, at most 200 (of the
/// newest 200 written), of the threads you started. The date folders are the machine's local day, as `new Date()` gives.
/// `now` and the times are epoch milliseconds.
pub fn recent_rollouts(codex_home: &Path, within_ms: i64, now: i64) -> Vec<PathBuf> {
    let since = (now - within_ms) as f64;
    let mut found: Vec<(PathBuf, f64)> = Vec::new();
    for back in 0..=((within_ms + DAY - 1).div_euclid(DAY) + 1) {
        let (y, m, d) = local_ymd(now - back * DAY);
        let dir = codex_home
            .join("sessions")
            .join(y.to_string())
            .join(format!("{m:02}"))
            .join(format!("{d:02}"));
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".jsonl"))
            .collect();
        names.sort();
        for n in names {
            let f = dir.join(n);
            let m = mtime_ms(&f);
            // two days can share a folder when a clock change makes one 25 hours long: listed once
            if m >= since && !found.iter().any(|(p, _)| p == &f) {
                found.push((f, m));
            }
        }
    }
    found.sort_by(|a, b| b.1.total_cmp(&a.1));
    found.truncate(RECENT_MAX);
    found
        .into_iter()
        .map(|(f, _)| f)
        .filter(|f| is_yours(f))
        .collect()
}

// ---------------------------------------------------------------- what a line says

type Obj = Map<String, Value>;

/// `d.payload ?? {}`, read as an object: a payload that is not one has none of the fields looked for.
fn payload_of(d: &Obj) -> Obj {
    match js::present(d.get("payload")) {
        Some(Value::Object(p)) => p.clone(),
        _ => Obj::new(),
    }
}

fn type_is(o: &Obj, t: &str) -> bool {
    o.get("type").and_then(Value::as_str) == Some(t)
}

/// What a Codex log line says, for `role` user or assistant: an event (user_message, agent_message) or a response item (a
/// message with input_text or output_text). `None` for its own instruction blocks (AGENTS.md, page context).
fn codex_text(d: &Obj, p: &Obj, role: &str) -> Option<String> {
    let msg = p.get("message").and_then(Value::as_str);
    let event = match role {
        "user" => "user_message",
        _ => "agent_message",
    };
    let t: Option<String> =
        if type_is(p, event) && msg.is_some() && matches!(role, "user" | "assistant") {
            msg.map(str::to_string)
        } else if type_is(d, "response_item")
            && type_is(p, "message")
            && p.get("role").and_then(Value::as_str) == Some(role)
        {
            p.get("content").and_then(Value::as_array).map(|content| {
                let texts: Vec<Value> = content
                    .iter()
                    .filter(|c| {
                        // /text/.test(c?.type ?? '')
                        js::present(c.get("type")).is_some_and(|t| js::string(t).contains("text"))
                    })
                    .map(|c| c.get("text").cloned().unwrap_or(Value::Null))
                    .collect();
                js::join(&texts, "\n")
            })
        } else {
            None
        };
    let t = js::trim(&t?).to_string();
    if t.is_empty() || t.starts_with('<') || t.starts_with("# AGENTS.md") {
        return None;
    }
    Some(js::slice16(&t, 6000).to_string())
}

/// `oneLine(codexText(d, 'user'), 500)` when it says anything.
fn user_text(d: &Obj, p: &Obj) -> Option<String> {
    codex_text(d, p, "user")
        .map(|t| one_line(&t, 500))
        .filter(|t| !t.is_empty())
}

/// `request_user_input` and `ask_user` calls are questions to you.
fn is_question(name: Option<&Value>) -> bool {
    let n = js::present(name).map(js::string).unwrap_or_default();
    let n = n.to_ascii_lowercase();
    n.contains("request_user_input") || n.contains("ask_user")
}

/// What a function_call is doing, in plain words, asked as a question to you or not.
struct Call {
    asking: String,
    plain: Option<String>,
}

fn call_text(p: &Obj) -> Call {
    let x: Obj = match p.get("arguments") {
        Some(Value::String(s)) => match js::parse(s.as_bytes()) {
            Some(Value::Object(o)) => o,
            _ => Obj::new(),
        },
        None | Some(Value::Null) => Obj::new(),
        // JSON.parse of a non-string is JSON.parse of its text: nothing a call carries reads as an object that way.
        Some(_) => Obj::new(),
    };
    let cmd: Option<Value> = match x.get("command") {
        Some(Value::Array(a)) => Some(Value::String(js::join(a, " "))),
        other => js::present(x.get("cmd")).or(other).cloned(),
    };
    let q = js::present(x.get("question"))
        .or_else(|| js::present(x.get("prompt")))
        .or_else(|| js::present(x.get("message")))
        .or_else(|| {
            js::present(
                x.get("questions")
                    .and_then(|qs| qs.get(0))
                    .and_then(|q| q.get("question")),
            )
        })
        .filter(|q| truthy(Some(q)));
    let asking = one_line(
        &format!(
            "Asks you{}",
            q.map(|q| format!(": {}", js::string(q)))
                .unwrap_or_default()
        ),
        200,
    );
    let name = p.get("name").and_then(Value::as_str);
    let plain = if truthy(cmd.as_ref()) {
        let c = js::string(cmd.as_ref().unwrap());
        Some(format!("Run {}", c.split('\n').next().unwrap_or("")))
    } else if name == Some("apply_patch") {
        Some("Edit files".to_string())
    } else {
        name.map(str::to_string)
    }
    .map(|t| one_line(&t, 200))
    .filter(|t| !t.is_empty());
    Call { asking, plain }
}

// ---------------------------------------------------------------- the scan

fn at<T>(a: &Option<At<T>>, pos: u64) -> bool {
    a.as_ref().is_some_and(|a| a.pos == pos)
}

#[derive(Clone, Debug)]
struct At<T> {
    pos: u64,
    v: T,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Done {
    /// task_complete
    Complete,
    /// turn_aborted
    Aborted,
    /// task_started
    Started,
    /// user_message
    UserMessage,
}

#[derive(Clone, Debug)]
struct DoneEv {
    kind: Done,
    /// task_complete's `last_agent_message`, as `oneLine(…, 400)` gives it.
    message: Option<String>,
}

#[derive(Clone, Debug)]
struct Model {
    model: String,
    effort: Option<String>,
}

/// The newest of everything, per line in file order.
#[derive(Debug, Default)]
struct Scan {
    lines: u64,
    last_at: Option<At<i64>>,
    last_ask: Option<At<String>>,
    /// The newest `turn_context` with a model.
    model: Option<At<Model>>,
    /// The oldest `turn_context` read, for the effort of a thread whose contexts all lack a model.
    first_ctx: Option<Option<String>>,
    /// The newest token_count with an info: the context tokens it reports (`None` when the window or the usage is missing).
    tokens: Option<At<Option<u64>>>,
    done: Option<At<DoneEv>>,
    /// The newest function_call or function_call_output: whether it is a question to you, and the call's name.
    last_call: Option<At<(bool, String)>>,
    /// The newest function_call, in the words it has if it is a question to you.
    call_asking: Option<At<String>>,
    /// The newest function_call that has words of its own.
    call_plain: Option<At<String>>,
    /// The newest line with words of the assistant's.
    said: Option<At<String>>,
    head_open: bool,
    first_ask: Option<String>,
    first_ask_found: bool,
}

/// What `readRollout` makes of the scan.
#[derive(Clone, Debug, Default, PartialEq)]
struct Replayed {
    last_at: Option<i64>,
    last_ask: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    context_tokens: Option<u64>,
    done: Option<Done>,
    done_event: Option<&'static str>,
    said: Option<String>,
    doing: Option<String>,
    /// `Some(name)` while its newest call is a question to you that nothing has answered.
    asking: Option<String>,
}

fn effort_of(p: &Obj) -> Option<String> {
    js::present(p.get("effort")).map(js::string)
}

fn number(v: Option<&Value>) -> Option<u64> {
    let f = v?.as_f64()?;
    (f >= 0.0 && f.fract() == 0.0).then_some(f as u64)
}

impl Scan {
    /// The first ask from a separately read head: its newline-ended lines (`headLines` drops the cut last one).
    fn first_ask_of_head(&mut self, head: &[u8]) {
        let mut from = 0;
        for nl in memchr::memchr_iter(b'\n', head) {
            let l = &head[from..nl];
            from = nl + 1;
            if !l.is_empty() && !self.first_ask_line(js::parse(l).as_ref()) {
                return;
            }
        }
    }

    /// `firstAsk = userText(parse(l))` until one is truthy. Whether to keep looking.
    fn first_ask_line(&mut self, d: Option<&Value>) -> bool {
        self.first_ask = d
            .and_then(Value::as_object)
            .and_then(|d| user_text(d, &payload_of(d)));
        self.first_ask_found = self.first_ask.is_some();
        !self.first_ask_found
    }

    /// One complete line, `end` the file offset just past its newline.
    fn line(&mut self, l: &[u8], end: u64) {
        let d = js::parse(l);
        if self.head_open && !self.first_ask_found {
            if end <= HEAD {
                self.first_ask_line(d.as_ref());
            } else {
                self.head_open = false;
            }
        }
        // a line that is not an object has none of the fields looked for
        let Some(Value::Object(d)) = d else { return };
        let pos = self.lines;
        self.lines += 1;
        let p = payload_of(&d);

        if let Some(t) = d
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(js::date_parse)
        {
            self.last_at = Some(At { pos, v: t });
        }
        if let Some(t) = user_text(&d, &p) {
            self.last_ask = Some(At { pos, v: t });
        }
        if type_is(&d, "turn_context") {
            self.first_ctx.get_or_insert_with(|| effort_of(&p));
            if let Some(m) = js::present(p.get("model")).filter(|m| truthy(Some(m))) {
                self.model = Some(At {
                    pos,
                    v: Model {
                        model: js::string(m),
                        effort: effort_of(&p),
                    },
                });
            }
        }
        if type_is(&p, "token_count") {
            if let Some(info) = js::present(p.get("info")).filter(|i| truthy(Some(i))) {
                let size = info.get("model_context_window");
                let used = info
                    .get("last_token_usage")
                    .and_then(|u| u.get("input_tokens"));
                // context = size && used != null ? { tokens: used, … } : null
                let tokens = (truthy(size) && js::present(used).is_some())
                    .then(|| number(used))
                    .flatten();
                self.tokens = Some(At { pos, v: tokens });
            }
        }
        let kind = match p.get("type").and_then(Value::as_str) {
            Some("task_complete") => Some(Done::Complete),
            Some("turn_aborted") => Some(Done::Aborted),
            Some("task_started") => Some(Done::Started),
            Some("user_message") => Some(Done::UserMessage),
            _ => None,
        };
        if let Some(kind) = kind {
            let message = (kind == Done::Complete)
                .then(|| p.get("last_agent_message").and_then(Value::as_str))
                .flatten()
                .map(|m| one_line(m, 400));
            self.done = Some(At {
                pos,
                v: DoneEv { kind, message },
            });
        }
        if type_is(&p, "function_call") || type_is(&p, "function_call_output") {
            let call = type_is(&p, "function_call");
            let name = js::present(p.get("name"));
            let asks = call && is_question(name);
            let name = name.map(js::string).unwrap_or_default();
            self.last_call = Some(At {
                pos,
                v: (asks, name),
            });
            if call {
                let c = call_text(&p);
                self.call_asking = Some(At { pos, v: c.asking });
                if let Some(plain) = c.plain {
                    self.call_plain = Some(At { pos, v: plain });
                }
            }
        }
        if let Some(t) = codex_text(&d, &p, "assistant")
            .map(|t| one_line(&t, 400))
            .filter(|t| !t.is_empty())
        {
            self.said = Some(At { pos, v: t });
        }
    }

    /// `readRollout`'s backward scan, over the newest occurrence of each fact: lines visited newest first, each doing what
    /// Node does on it, until Node's break condition holds.
    fn replay(&self) -> Replayed {
        let mut positions: Vec<u64> = [
            self.last_at.as_ref().map(|a| a.pos),
            self.last_ask.as_ref().map(|a| a.pos),
            self.model.as_ref().map(|a| a.pos),
            self.tokens.as_ref().map(|a| a.pos),
            self.done.as_ref().map(|a| a.pos),
            self.last_call.as_ref().map(|a| a.pos),
            self.call_asking.as_ref().map(|a| a.pos),
            self.call_plain.as_ref().map(|a| a.pos),
            self.said.as_ref().map(|a| a.pos),
        ]
        .into_iter()
        .flatten()
        .collect();
        positions.sort_unstable_by(|a, b| b.cmp(a));
        positions.dedup();

        let mut r = Replayed::default();
        let (mut have_tokens, mut done_seen) = (false, false);
        let mut asking: Option<Option<String>> = None; // None: not seen; Some(None): not a question
        let mut said: Option<String> = None;
        for pos in positions {
            if r.last_at.is_none() && at(&self.last_at, pos) {
                r.last_at = self.last_at.as_ref().map(|a| a.v);
            }
            if r.last_ask.is_none() && at(&self.last_ask, pos) {
                r.last_ask = self.last_ask.as_ref().map(|a| a.v.clone());
            }
            if r.model.is_none() && at(&self.model, pos) {
                let m = &self.model.as_ref().unwrap().v;
                r.model = Some(m.model.clone());
                r.effort = m.effort.clone();
            }
            if !have_tokens && at(&self.tokens, pos) {
                have_tokens = true;
                r.context_tokens = self.tokens.as_ref().unwrap().v;
            }
            if !done_seen && at(&self.done, pos) {
                done_seen = true;
                let ev = &self.done.as_ref().unwrap().v;
                r.done = Some(ev.kind);
                r.done_event = Some(match ev.kind {
                    Done::Complete => "task_complete",
                    Done::Aborted => "turn_aborted",
                    Done::Started => "task_started",
                    Done::UserMessage => "user_message",
                });
                match ev.kind {
                    // said ??= oneLine(last_agent_message, 400)
                    Done::Complete if said.is_none() => said = ev.message.clone(),
                    // interrupted: idle, and no question stands
                    Done::Aborted if asking.is_none() => asking = Some(None),
                    _ => {}
                }
            }
            if asking.is_none() && at(&self.last_call, pos) {
                let (asks, name) = &self.last_call.as_ref().unwrap().v;
                asking = Some(asks.then(|| name.clone()));
            }
            if r.doing.is_none() {
                let found = if matches!(asking, Some(Some(_))) {
                    self.call_asking.as_ref().filter(|a| a.pos == pos)
                } else {
                    self.call_plain.as_ref().filter(|a| a.pos == pos)
                };
                if let Some(a) = found {
                    r.doing = Some(a.v.clone());
                }
            }
            if said.as_deref().is_none_or(str::is_empty) && at(&self.said, pos) {
                said = self.said.as_ref().map(|a| a.v.clone());
            }
            if have_tokens
                && done_seen
                && (r.doing.as_deref().is_some_and(|d| !d.is_empty())
                    || said.as_deref().is_some_and(|s| !s.is_empty()))
                && r.model.is_some()
                && r.last_ask.is_some()
            {
                break;
            }
        }
        if r.model.is_none() {
            // every turn_context lacked a model: the last one scanned, the oldest, leaves its effort
            r.effort = self.first_ctx.clone().flatten();
        }
        r.said = said.filter(|s| !s.is_empty());
        r.asking = asking.flatten();
        r
    }
}

// ---------------------------------------------------------------- the thread

/// The first line of a rollout, `session_meta`: what the thread says about itself.
#[derive(Clone, Debug, Default)]
struct Meta {
    id: Option<String>,
    originator: Option<String>,
    cwd: Option<String>,
    source: Option<String>,
    started_at: Option<i64>,
}

fn read_meta(f: &Path) -> (Meta, bool) {
    let line = head_line(f);
    let payload = match js::parse(&line) {
        Some(Value::Object(d)) => payload_of(&d),
        _ => return (Meta::default(), false),
    };
    let s = |k: &str| payload.get(k).and_then(Value::as_str).map(str::to_string);
    (
        Meta {
            id: js::present(payload.get("id")).map(js::string),
            originator: s("originator"),
            cwd: s("cwd"),
            source: s("source"),
            started_at: payload
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(js::date_parse),
        },
        true,
    )
}

/// A Codex rollout being followed.
#[derive(Debug)]
pub struct RolloutTail {
    path: PathBuf,
    reader: LineReader,
    scan: Scan,
    meta: Meta,
    /// The first line has been read whole and parsed: until then it is looked at again as the file grows.
    meta_read: bool,
}

impl RolloutTail {
    /// The first read: the last 384 KiB (the line it starts inside dropped) and, when that is not the whole file, the
    /// first 256 KiB for the first ask. Never more than the file.
    pub fn open(path: &Path) -> io::Result<RolloutTail> {
        let (reader, first) = LineReader::open(path, TAIL, HEAD)?;
        let (meta, meta_read) = read_meta(path);
        let mut t = RolloutTail {
            path: path.to_path_buf(),
            reader,
            scan: Scan {
                head_open: first.whole,
                ..Scan::default()
            },
            meta,
            meta_read,
        };
        t.scan.first_ask_of_head(&first.head);
        t.feed(&first.body);
        Ok(t)
    }

    /// Reads what was appended since the last read, and only that. A file that shrank or was replaced is read again from
    /// scratch. Whether anything changed.
    pub fn advance(&mut self) -> io::Result<bool> {
        let before = self.reader.written_at;
        let took = match self.reader.advance()? {
            Step::Reopen => {
                let read = self.reader.bytes_read;
                *self = RolloutTail::open(&self.path)?;
                self.reader.bytes_read += read;
                return Ok(true);
            }
            Step::Appended(new) => self.feed(&new),
            Step::Unchanged => false,
        };
        if !self.meta_read {
            (self.meta, self.meta_read) = read_meta(&self.path);
        }
        Ok(took || self.reader.written_at != before)
    }

    /// The reconcile's read (Ruling R11): reads the file again from scratch when the bytes before the offset are no
    /// longer the ones read there, else what was appended. Whether anything changed.
    pub fn reconcile(&mut self) -> io::Result<bool> {
        if self.reader.verify()? {
            return self.advance();
        }
        let read = self.reader.bytes_read;
        *self = RolloutTail::open(&self.path)?;
        self.reader.bytes_read += read;
        Ok(true)
    }

    /// The file this follows.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn feed(&mut self, bytes: &[u8]) -> bool {
        let RolloutTail { reader, scan, .. } = self;
        reader.feed(bytes, |l, end| scan.line(l, end))
    }

    /// Bytes read from the file since `open`, the first read included.
    pub fn bytes_read(&self) -> u64 {
        self.reader.bytes_read
    }

    /// What the scan says now, as a value: `view().session(open, now)` is `session(open, now)`, without the tail.
    pub fn view(&self) -> RolloutView {
        RolloutView {
            path: self.path.clone(),
            r: self.scan.replay(),
            meta: self.meta.clone(),
            written: self.reader.written_at,
            first_ask: self.scan.first_ask.clone(),
        }
    }

    /// The session as of `now` (epoch milliseconds). `open`: a running Codex holds this rollout open.
    ///
    /// Open while a running Codex holds its log, else while it was written in the last ten minutes; a question to you stays
    /// up until answered, up to three days. A thread the panel started (`codex exec`, one turn) is over once its process
    /// has let go of its log. Closed is `Done · finished` when its last record is a finished turn, else
    /// `Ended · ended, reason unknown`: a reason is never guessed.
    ///
    /// Node, with no process scan at all (`open` null), counts an exec thread as still open; here `open == false` is "the
    /// scan ran and found it free", the only case the process scan of this port produces.
    pub fn session(&self, open: bool, now: i64) -> Session {
        self.view().session(open, now)
    }
}

/// A rollout's facts as of its last read, held apart from the tail (so a reader need not wait on the tail's lock).
#[derive(Clone, Debug)]
pub struct RolloutView {
    path: PathBuf,
    r: Replayed,
    meta: Meta,
    written: Option<i64>,
    first_ask: Option<String>,
}

impl RolloutView {
    /// When the log was last written (epoch ms).
    pub fn written_at(&self) -> Option<i64> {
        self.written
    }

    /// The first time after `now` at which `session(open, _)` may answer otherwise with nothing read anew: when its age
    /// passes ten minutes, or three days with a question up. `None` when its liveness does not wait on the clock.
    pub fn due(&self, open: bool, now: i64) -> Option<i64> {
        let exited = self.meta.source.as_deref() == Some("exec") && !open;
        if exited || open {
            return None;
        }
        // `age < n` first fails at `written + n`; a question up keeps it live the longer of the two.
        let n = if self.r.asking.is_some() {
            3 * DAY
        } else {
            CODEX_LIVE_MS
        };
        let at = self.written.unwrap_or(0) + n;
        (at > now).then_some(at)
    }

    /// The session as of `now` (epoch milliseconds). `open`: a running Codex holds this rollout open.
    ///
    /// Open while a running Codex holds its log, else while it was written in the last ten minutes; a question to you stays
    /// up until answered, up to three days. A thread the panel started (`codex exec`, one turn) is over once its process
    /// has let go of its log. Closed is `Done · finished` when its last record is a finished turn, else
    /// `Ended · ended, reason unknown`: a reason is never guessed.
    ///
    /// Node, with no process scan at all (`open` null), counts an exec thread as still open; here `open == false` is "the
    /// scan ran and found it free", the only case the process scan of this port produces.
    pub fn session(&self, open: bool, now: i64) -> Session {
        let r = self.r.clone();
        let m = &self.meta;
        let written = self.written;
        let age = now - written.unwrap_or(0);
        let exited = m.source.as_deref() == Some("exec") && !open;
        let live =
            !exited && (open || age < CODEX_LIVE_MS || (r.asking.is_some() && age < 3 * DAY));
        let (state, reason) = if !live {
            if r.done == Some(Done::Complete) {
                (State::Done, "finished".to_string())
            } else {
                (State::Ended, "ended, reason unknown".to_string())
            }
        } else if let Some(name) = &r.asking {
            (State::NeedsYou, format!("waiting on you: {name}"))
        } else if matches!(r.done, Some(Done::Started | Done::UserMessage)) {
            (
                State::Working,
                format!("working: {}", r.done_event.unwrap_or("")),
            )
        } else {
            (State::Waiting, "idle at prompt".to_string())
        };
        let id = m.id.clone().unwrap_or_else(|| {
            self.path
                .file_name()
                .map(|n| {
                    js::basename(&n.to_string_lossy())
                        .trim_end_matches(".jsonl")
                        .to_string()
                })
                .unwrap_or_default()
        });
        let nonempty =
            |s: &Option<String>, n| s.as_ref().map(|s| one_line(s, n)).filter(|s| !s.is_empty());
        Session {
            key: SessionKey {
                machine: String::new(), // the store fills it in
                profile: "codex".into(),
                session_id: id,
            },
            engine: Engine::Codex,
            name: nonempty(&m.originator, 60),
            cwd: m.cwd.as_deref().map(tilde),
            state,
            reason,
            live,
            pid: None,
            stage: None,
            model: r.model.filter(|m| !m.is_empty()),
            effort: r.effort,
            context_tokens: r.context_tokens,
            last_at: r.last_at.or(written),
            said: r.said,
            doing: r.doing,
            last_ask: r.last_ask.or_else(|| self.first_ask.clone()),
            first_ask: self.first_ask.clone(),
            started_at: m.started_at,
            hook_at: None,
            written_at: written,
            job_id: None,
        }
    }
}

/// `~` for the home folder, as `tilde` does.
fn tilde(p: &str) -> String {
    let home = crate::home::home_dir().to_string_lossy().into_owned();
    match p.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => p.to_string(),
    }
}

/// A rollout's session, read whole as of `now`.
pub fn codex_session_at(f: &Path, open: bool, now: i64) -> Session {
    match RolloutTail::open(f) {
        Ok(t) => t.session(open, now),
        Err(_) => unreadable(f),
    }
}

/// A rollout's session, read whole as of now. `open`: a running Codex holds this rollout open (what Node derives from the
/// process scan: the `.jsonl` files each running `codex` has open, via `/proc/<pid>/fd` or `lsof`).
pub fn codex_session(f: &Path, open: bool) -> Session {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    codex_session_at(f, open, now)
}

/// A rollout that cannot be read (gone between listing and reading): closed, with no record.
fn unreadable(f: &Path) -> Session {
    let id = f
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Session {
        key: SessionKey {
            machine: String::new(),
            profile: "codex".into(),
            session_id: id,
        },
        engine: Engine::Codex,
        name: None,
        cwd: None,
        state: State::Ended,
        reason: "ended, reason unknown".into(),
        live: false,
        pid: None,
        stage: None,
        model: None,
        effort: None,
        context_tokens: None,
        last_at: None,
        said: None,
        doing: None,
        last_ask: None,
        first_ask: None,
        started_at: None,
        hook_at: None,
        written_at: None,
        job_id: None,
    }
}

// ---------------------------------------------------------------- Muse

/// Muse where its CLI is installed and signed in (`museHome`): an executable `muse` in the usual places and
/// `~/.config/muse/auth.json`.
pub fn muse_installed(home: &Path) -> bool {
    let dirs = [
        home.join(".local").join("bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ];
    muse_installed_with(home, &dirs)
}

/// `muse_installed` over the given program folders.
pub fn muse_installed_with(home: &Path, bin_dirs: &[PathBuf]) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let found = bin_dirs.iter().any(|d| {
        fs::metadata(d.join("muse"))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    });
    found && home.join(".config").join("muse").join("auth.json").exists()
}

/// A running Muse is a session of the Muse account (`collect`): its pid is its id, and it is working while it runs. Muse
/// writes no log fleet reads, so nothing else is known. `since` is when the process started (epoch milliseconds).
pub fn muse_session(pid: u32, cwd: Option<&str>, since: i64, args: &str) -> Session {
    Session {
        key: SessionKey {
            machine: String::new(), // the store fills it in
            profile: "muse".into(),
            session_id: pid.to_string(),
        },
        engine: Engine::Muse,
        name: Some("muse".into()),
        cwd: cwd.map(tilde),
        state: State::Working,
        reason: "working: running process".into(),
        live: true,
        pid: Some(pid),
        stage: None,
        model: None,
        effort: None,
        context_tokens: None,
        last_at: Some(since),
        said: None,
        doing: Some(args.to_string()),
        last_ask: None,
        first_ask: None,
        started_at: Some(since),
        hook_at: None,
        written_at: None,
        job_id: None,
    }
}
