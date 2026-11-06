//! The incremental transcript `Tail` against Node's `readTranscript` (golden from native/parity/gen-transcript-golden.mjs):
//! a whole open, and an open plus an `advance` after each appended chunk; a line is not read until its newline; a
//! truncated or replaced file is read again from scratch; an `advance` reads only the new bytes.

use fleet_read::transcript::{Tail, TranscriptSummary};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

/// The checkout the workdir fixtures point into, as the golden names it (gen-transcript-golden.mjs makes it there).
const GOLDEN_WORK: &str = "/tmp/fleet-transcript-fixture";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Golden {
    mtime_s: u64,
    fixtures: Vec<Fixture>,
}

#[derive(Deserialize)]
struct Fixture {
    name: String,
    size: usize,
    cuts: [usize; 2],
    whole: Value,
    chunks: Vec<Value>,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../parity")
}

fn golden() -> Golden {
    let s =
        fs::read_to_string(root().join("golden/transcript.json")).expect("golden/transcript.json");
    serde_json::from_str(&s.replace(GOLDEN_WORK, work())).expect("parse golden")
}

/// A fixture, its checkout path swapped for this run's (the same length, so the golden's cut offsets still hold).
fn fixture(name: &str) -> Vec<u8> {
    let s = fs::read_to_string(root().join("fixtures/transcripts").join(name)).expect("fixture");
    s.replace(GOLDEN_WORK, work()).into_bytes()
}

/// This run's copy of the checkout: one per user (no clash with another user's run), built once per process, each file
/// written whole by a rename (a parallel test or a concurrent run never sees a half-written HEAD). Its path has the
/// golden one's length.
fn work() -> &'static str {
    static WORK: OnceLock<String> = OnceLock::new();
    WORK.get_or_init(|| {
        let uid = fs::metadata(env!("CARGO_MANIFEST_DIR")).unwrap().uid();
        let w = format!("/tmp/fleet-tf-u{uid:014}");
        assert_eq!(w.len(), GOLDEN_WORK.len());
        let put = |f: &str, s: &str| {
            let f = Path::new(&w).join(f);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            let tmp = f.with_extension(format!("tmp{}", std::process::id()));
            fs::write(&tmp, s).unwrap();
            fs::rename(&tmp, &f).unwrap();
        };
        put(".git/HEAD", "ref: refs/heads/fixture-main\n");
        put("wt2/.git", "gitdir: ../gitdirs/wt2\n");
        put("gitdirs/wt2/HEAD", "ref: refs/heads/fixture-wt2\n");
        fs::create_dir_all(Path::new(&w).join(".claude/worktrees/wt1")).unwrap();
        fs::create_dir_all(Path::new(&w).join("src")).unwrap();
        w
    })
}

fn make_work() {
    work();
}

fn set_mtime(f: &Path, secs: u64) {
    let t = UNIX_EPOCH + Duration::from_secs(secs);
    File::options()
        .write(true)
        .open(f)
        .unwrap()
        .set_times(FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

fn append(f: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(f)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

/// The summary and facts in the shape gen-transcript-golden.mjs writes Node's answer in.
fn as_node(t: &Tail) -> Value {
    let s: &TranscriptSummary = t.summary();
    let f = t.facts();
    assert_eq!(f.last_at, s.last_at);
    assert_eq!(f.turn_over, s.turn_over);
    assert_eq!(f.limit_hit_at, s.limit_hit.as_ref().and_then(|l| l.at));
    json!({
        "model": s.model, "effort": s.effort, "contextTokens": s.context_tokens, "lastAt": s.last_at,
        "said": s.said, "doing": s.doing, "lastAsk": s.last_ask, "firstAsk": s.first_ask, "turnOver": s.turn_over,
        "running": s.running.as_ref().map(|r| json!({ "step": r.step, "since": r.since, "tool": r.tool, "cmd": r.cmd })),
        "limitHit": s.limit_hit.as_ref().map(|l| json!({ "text": l.text, "at": l.at })),
        "stage": s.stage, "steps": s.steps, "workdir": s.workdir,
        "queued": s.queued.iter().map(|q| json!({ "text": q.text, "at": q.at })).collect::<Vec<_>>(),
        "writtenAt": f.written_at,
    })
}

fn assert_node(t: &Tail, want: &Value, what: &str) {
    let got = as_node(t);
    if &got != want {
        let (g, w) = (got.as_object().unwrap(), want.as_object().unwrap());
        let diff: Vec<_> = w
            .iter()
            .filter(|(k, v)| g.get(*k) != Some(v))
            .map(|(k, v)| format!("{k}: rust {} node {v}", g[k]))
            .collect();
        panic!("{what}:\n  {}", diff.join("\n  "));
    }
}

#[test]
fn whole_and_chunked_reads_match_node() {
    make_work();
    let g = golden();
    assert!(g.fixtures.len() >= 10);
    assert!(
        g.fixtures.iter().any(|f| f.size > 384 * 1024),
        "one fixture bigger than the tail"
    );
    let dir = tempfile::tempdir().unwrap();
    for fx in &g.fixtures {
        let buf = fixture(&fx.name);
        assert_eq!(buf.len(), fx.size, "{} changed since the golden", fx.name);
        let file = dir.path().join(&fx.name);

        fs::write(&file, &buf).unwrap();
        set_mtime(&file, g.mtime_s);
        assert_node(
            &Tail::open(&file).unwrap(),
            &fx.whole,
            &format!("{} whole", fx.name),
        );

        let bounds = [0, fx.cuts[0], fx.cuts[1], buf.len()];
        fs::write(&file, b"").unwrap();
        let mut tail: Option<Tail> = None;
        for i in 0..3 {
            append(&file, &buf[bounds[i]..bounds[i + 1]]);
            set_mtime(&file, g.mtime_s + 1 + i as u64);
            match tail.as_mut() {
                None => tail = Some(Tail::open(&file).unwrap()),
                Some(t) => {
                    t.advance().unwrap();
                }
            }
            assert_node(
                tail.as_ref().unwrap(),
                &fx.chunks[i],
                &format!("{} after chunk {}", fx.name, i + 1),
            );
        }
    }
}

/// Review Focus 3: a line is read when its newline arrives, never before, even when what came so far is whole JSON.
#[test]
fn a_partial_line_waits_for_its_newline() {
    make_work();
    let g = golden();
    let fx = g
        .fixtures
        .iter()
        .find(|f| f.name == "running.jsonl")
        .unwrap();
    let buf = fixture(&fx.name);
    let body = &buf[..buf.len() - 1]; // without the last newline
    let last = body.iter().rposition(|&b| b == b'\n').unwrap() + 1; // where the last line starts
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("partial.jsonl");
    let before = dir.path().join("before.jsonl");
    fs::write(&before, &buf[..last]).unwrap();
    set_mtime(&before, g.mtime_s);
    let without_last = as_node(&Tail::open(&before).unwrap());
    assert_ne!(
        without_last["running"], fx.whole["running"],
        "the last line is what makes it run"
    );

    for (cut, what) in [
        (last + 40, "mid-line"),
        (body.len(), "whole JSON, no newline"),
    ] {
        fs::write(&file, &buf[..cut]).unwrap();
        set_mtime(&file, g.mtime_s);
        let mut t = Tail::open(&file).unwrap();
        assert_eq!(as_node(&t), without_last, "open, {what}");
        assert!(!t.advance().unwrap(), "nothing new");
        assert_eq!(
            as_node(&t),
            without_last,
            "advance with nothing new, {what}"
        );
        if cut < body.len() {
            append(&file, &buf[cut..body.len()]);
            set_mtime(&file, g.mtime_s);
            t.advance().unwrap();
            assert_eq!(
                as_node(&t),
                without_last,
                "advance, rest of the line but no newline"
            );
        }
        append(&file, b"\n");
        set_mtime(&file, g.mtime_s);
        assert!(t.advance().unwrap(), "the newline completes the line");
        assert_node(&t, &fx.whole, &format!("after the newline, {what}"));
    }
}

/// Review Focus 2: a file truncated and written again, or replaced by another (a new inode), is read from scratch.
#[test]
fn truncate_and_replace_reopen() {
    make_work();
    let g = golden();
    let by = |n: &str| g.fixtures.iter().find(|f| f.name == n).unwrap();
    let (a, b) = (by("turn-over.jsonl"), by("limit.jsonl"));
    assert!(b.size < a.size);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("t.jsonl");
    let fresh = |f: &Path| as_node(&Tail::open(f).unwrap());

    // Truncated to nothing, seen empty, then written again.
    fs::write(&file, fixture(&a.name)).unwrap();
    set_mtime(&file, g.mtime_s);
    let mut t = Tail::open(&file).unwrap();
    assert_node(&t, &a.whole, "first");
    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(0)
        .unwrap();
    set_mtime(&file, g.mtime_s + 1);
    assert!(t.advance().unwrap());
    assert_eq!(as_node(&t), fresh(&file), "empty");
    append(&file, &fixture(&b.name));
    set_mtime(&file, g.mtime_s);
    assert!(t.advance().unwrap());
    assert_node(&t, &b.whole, "rewritten after an empty read");

    // Truncated and rewritten shorter between two reads.
    fs::write(&file, fixture(&a.name)).unwrap();
    set_mtime(&file, g.mtime_s);
    let mut t = Tail::open(&file).unwrap();
    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(0)
        .unwrap();
    append(&file, &fixture(&b.name));
    set_mtime(&file, g.mtime_s);
    assert!(t.advance().unwrap());
    assert_node(&t, &b.whole, "truncated and rewritten");
    assert_eq!(as_node(&t), fresh(&file));

    // Replaced by a rename: a new inode, even one that is longer than what was read.
    let mut t = Tail::open(&file).unwrap();
    let other = dir.path().join("other.jsonl");
    fs::write(&other, fixture(&a.name)).unwrap();
    set_mtime(&other, g.mtime_s);
    fs::rename(&other, &file).unwrap();
    assert!(t.advance().unwrap());
    assert_node(&t, &a.whole, "replaced");
    assert_eq!(as_node(&t), fresh(&file));
}

/// After the first read, only new bytes are read: one appended line costs at most that line's bytes.
#[test]
fn advance_reads_only_new_bytes() {
    make_work();
    let g = golden();
    let fx = g.fixtures.iter().find(|f| f.name == "big.jsonl").unwrap();
    let buf = fixture(&fx.name);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.jsonl");
    fs::write(&file, &buf).unwrap();
    let mut t = Tail::open(&file).unwrap();
    let first = t.bytes_read();
    assert!(buf.len() as u64 > 384 * 1024);
    assert!(
        first <= (buf.len() as u64).min((384 + 256) * 1024),
        "the first read is the tail and the head: {first}"
    );

    assert!(!t.advance().unwrap());
    assert_eq!(t.bytes_read(), first, "nothing new, nothing read");

    let line = br#"{"type":"assistant","timestamp":"2026-11-06T12:00:00.000Z","message":{"model":"claude-fixture-2","stop_reason":"end_turn","content":[{"type":"text","text":"All deployed."}],"usage":{"input_tokens":7}}}"#;
    let mut line = line.to_vec();
    line.push(b'\n');
    append(&file, &line);
    assert!(t.advance().unwrap());
    assert!(
        t.bytes_read() - first <= line.len() as u64,
        "read {} for a {}-byte line",
        t.bytes_read() - first,
        line.len()
    );
    assert_eq!(t.summary().said.as_deref(), Some("All deployed."));
    assert_eq!(t.summary().model.as_deref(), Some("claude-fixture-2"));
    assert_eq!(t.summary().turn_over, Some(true));
    assert_eq!(
        t.summary().first_ask.as_deref(),
        Some("Rebuild the fixture index from scratch")
    );
}
