//! The terminal manager: cold and warm opens, shared attach, end-while-warm, eviction, Codex's one writer, the limits,
//! resize coalescing and backpressure. The programs are stand-ins in a temp home's `~/.local/bin` (where
//! `attach_cmd::command` looks first): `claude` and `codex` both run `stand-in.py`, which logs each start, prints a
//! banner, answers lines with `GOT <line>`, and prints `REDRAW <cols>x<rows>` on SIGWINCH.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fleet_model::types::AttachTarget;
use fleet_model::wire::Frame;
use fleetd::terms::{self, term_sink, Options, TermSink, TermSource, Terms};

const STAND_IN: &str = r#"#!/usr/bin/python3
import os, sys, signal, time, fcntl, termios, struct
LOG = os.path.join(os.path.dirname(os.path.realpath(__file__)), '..', '..', 'starts.log')
with open(LOG, 'a') as f: f.write('%s %d\n' % (' '.join(sys.argv[1:]), os.getpid()))
def size():
    r, c, _, _ = struct.unpack('HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\0' * 8))
    return c, r
def winch(*_):
    c, r = size()
    with open(LOG + '.winch', 'a') as f: f.write('%.6f %d %d\n' % (time.monotonic(), c, r))
    os.write(1, ('REDRAW %dx%d\r\n' % (c, r)).encode())
signal.signal(signal.SIGWINCH, winch)
os.write(1, ('BANNER %s\r\n' % ' '.join(sys.argv[1:])).encode())
while True:
    line = sys.stdin.buffer.readline()
    if not line: break
    l = line.strip()
    if l == b'flood':
        os.write(1, b'X' * 5000000 + b'\r\nFLOODED\r\n')
    elif l == b'exit-soon':
        time.sleep(0.3); os.write(1, b'LAST WORDS\r\n'); sys.exit(0)
    else:
        os.write(1, b'GOT ' + l + b'\r\n')
"#;

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("stand-in.py");
        std::fs::write(&script, STAND_IN).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["claude", "codex"] {
            std::os::unix::fs::symlink(&script, bin.join(name)).unwrap();
        }
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        Home { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn starts(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("starts.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Starts whose argv mentions `word`.
    fn starts_of(&self, word: &str) -> Vec<i32> {
        self.starts()
            .iter()
            .filter(|l| l.split(' ').any(|w| w == word))
            .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
            .collect()
    }

    /// (seconds, cols, rows) of each SIGWINCH the stand-ins saw.
    fn winches(&self) -> Vec<(f64, u16, u16)> {
        std::fs::read_to_string(self.dir.path().join("starts.log.winch"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let v: Vec<&str> = l.split(' ').collect();
                (
                    v[0].parse().unwrap(),
                    v[1].parse().unwrap(),
                    v[2].parse().unwrap(),
                )
            })
            .collect()
    }

    fn terms(&self) -> Terms {
        Terms::new(self.path(), |_| false).unwrap()
    }
}

fn claude(job: &str) -> AttachTarget {
    AttachTarget::Claude {
        profile: "default".into(),
        job: job.into(),
    }
}

fn codex(sid: &str) -> AttachTarget {
    AttachTarget::Codex {
        profile: "codex".into(),
        session_id: sid.into(),
        cwd: "~/work".into(),
    }
}

fn job(n: usize) -> String {
    format!("{n:08x}")
}

/// Seconds a cold stand-in gets to print its banner (python starts slowly when the suite runs 25 at once).
const BOOT: f64 = 20.0;

const SID: &str = "0199aaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

/// One connection's view: its sink for opens, and what has come out of its source, per ref.
struct Conn {
    id: u64,
    sink: TermSink,
    src: TermSource,
    /// Each frame received, in order: (ref, Some(payload length)) for output, (ref, None) for an end.
    got: Vec<(u32, Option<usize>)>,
    outs: HashMap<u32, Vec<u8>>,
}

impl Conn {
    fn new(id: u64) -> Conn {
        let (sink, src) = term_sink();
        Conn {
            id,
            sink,
            src,
            got: vec![],
            outs: HashMap::new(),
        }
    }

    fn open(&self, t: &Terms, r: u32, target: AttachTarget) -> Result<(), String> {
        t.open(self.id, r, target, 100, 30, self.sink.clone())
    }

    fn output(&self, r: u32) -> &[u8] {
        self.outs.get(&r).map_or(&[], |v| v.as_slice())
    }

    fn ended(&self, r: u32) -> bool {
        self.got.iter().any(|(x, d)| *x == r && d.is_none())
    }

    /// Receives one frame, waiting until `end` at most; false if none came.
    fn take(&mut self, end: Instant) -> bool {
        let now = Instant::now();
        if now >= end {
            return false;
        }
        match self.src.recv_timeout(end - now) {
            Some(Frame::TermOut { r#ref, data }) => {
                self.got.push((r#ref, Some(data.len())));
                self.outs.entry(r#ref).or_default().extend(data);
            }
            Some(Frame::TermEnd { r#ref, .. }) => self.got.push((r#ref, None)),
            Some(f) => panic!("unexpected {f:?}"),
            None => {}
        }
        true
    }

    /// Receives until `done(self)` or `secs` pass; whether it was done.
    fn until(&mut self, secs: f64, done: impl Fn(&Conn) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while !done(self) {
            if !self.take(end) {
                return false;
            }
        }
        true
    }

    /// Receives until `ref`'s output holds `needle` (looking only at what is new each time).
    fn until_has(&mut self, r: u32, needle: &str, secs: f64) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        let mut from = 0;
        loop {
            let out = self.output(r);
            if has(&out[from..], needle) {
                return true;
            }
            from = out.len().saturating_sub(needle.len());
            if !self.take(end) {
                return false;
            }
        }
    }
}

fn has(all: &[u8], needle: &str) -> bool {
    all.windows(needle.len()).any(|w| w == needle.as_bytes())
}

fn pos(all: &[u8], needle: &str) -> Option<usize> {
    all.windows(needle.len())
        .position(|w| w == needle.as_bytes())
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn gone_within(pid: i32, d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

fn wait_for(d: Duration, f: impl Fn() -> bool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

// (a)
#[test]
fn a_cold_terminal_shows_the_banner_and_echoes() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("0000000a")).unwrap();
    assert!(
        c.until_has(1, "BANNER attach 0000000a", BOOT),
        "{:?}",
        String::from_utf8_lossy(c.output(1))
    );
    t.input(1, 1, b"hello\n");
    assert!(c.until_has(1, "GOT hello", 5.0));
    assert_eq!(h.starts_of("0000000a").len(), 1);
}

// (b)
#[test]
fn a_warm_reopen_replays_the_ring_then_redraws_without_a_new_process() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("0000000b")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT));
    t.input(1, 1, b"before\n");
    assert!(c.until_has(1, "GOT before", 5.0));
    t.close(1, 1);
    assert!(c.until(2.0, |c| c.ended(1)), "a close gets its TermEnd");
    assert_eq!(t.warm_count(), 1);
    assert!(
        t.timer_armed(),
        "the expiry is timed while something is warm"
    );

    // Same size as before: the size kick goes through cols-1.
    c.open(&t, 2, claude("0000000b")).unwrap();
    assert!(
        c.until_has(2, "REDRAW 100x30", 5.0),
        "{:?}",
        String::from_utf8_lossy(c.output(2))
    );
    let out = c.output(2).to_vec();
    let (banner, got, redraw) = (
        pos(&out, "BANNER").unwrap(),
        pos(&out, "GOT before").unwrap(),
        pos(&out, "REDRAW 100x30").unwrap(),
    );
    assert!(banner < got && got < redraw, "ring first, then the redraw");
    assert_eq!(h.starts_of("0000000b").len(), 1, "the stand-in ran once");
    assert_eq!(t.warm_count(), 0);

    // A new size is applied as it is.
    t.close(1, 2);
    c.open_sized(&t, 3, claude("0000000b"), 120, 40);
    assert!(c.until_has(3, "REDRAW 120x40", 5.0));
    assert_eq!(h.starts_of("0000000b").len(), 1);
}

impl Conn {
    fn open_sized(&self, t: &Terms, r: u32, target: AttachTarget, cols: u16, rows: u16) {
        t.open(self.id, r, target, cols, rows, self.sink.clone())
            .unwrap();
    }
}

// (c)
#[test]
fn two_connections_on_one_job_share_one_process() {
    let h = Home::new();
    let t = h.terms();
    let (mut a, mut b) = (Conn::new(1), Conn::new(2));
    a.open(&t, 7, claude("0000000c")).unwrap();
    assert!(a.until_has(7, "BANNER", BOOT));
    b.open(&t, 9, claude("0000000c")).unwrap();
    assert!(b.until_has(9, "BANNER", BOOT), "the newcomer gets the ring");
    assert_eq!(h.starts_of("0000000c").len(), 1, "one process");
    assert_eq!(t.live_count(), 1);
    t.input(1, 7, b"from-a\n");
    t.input(2, 9, b"from-b\n");
    assert!(a.until_has(7, "GOT from-a", 5.0) && a.until_has(7, "GOT from-b", 5.0));
    assert!(b.until_has(9, "GOT from-a", 5.0) && b.until_has(9, "GOT from-b", 5.0));
    // One letting go leaves the other attached, not warm.
    t.close(1, 7);
    assert_eq!(t.warm_count(), 0);
    t.input(2, 9, b"still\n");
    assert!(b.until_has(9, "GOT still", 5.0));
    t.drop_conn(2);
    assert_eq!(
        t.warm_count(),
        1,
        "the last one released sends it to the pool"
    );
}

// (d)
#[test]
fn a_program_that_exits_while_warm_tells_the_next_opener_then_starts_fresh() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("0000000d")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT));
    t.input(1, 1, b"exit-soon\n");
    assert!(
        c.until_has(1, "exit-soon", 5.0),
        "echoed before release: {:?}",
        String::from_utf8_lossy(c.output(1))
    );
    t.close(1, 1);
    let pid = h.starts_of("0000000d")[0];
    assert!(
        gone_within(pid, Duration::from_secs(5)),
        "the stand-in exits by itself"
    );
    assert!(wait_for(Duration::from_secs(2), || t.live_count() == 0));

    c.open(&t, 2, claude("0000000d")).unwrap();
    assert!(c.until(5.0, |c| c.ended(2)));
    let out = c.output(2).to_vec();
    assert!(
        has(&out, "LAST WORDS"),
        "{:?}",
        String::from_utf8_lossy(&out)
    );
    let end_at = c
        .got
        .iter()
        .position(|(r, d)| *r == 2 && d.is_none())
        .unwrap();
    assert!(
        c.got[end_at + 1..].iter().all(|(r, _)| *r != 2),
        "TermEnd is last"
    );
    assert_eq!(h.starts_of("0000000d").len(), 1, "not re-spawned silently");
    assert_eq!(t.warm_count(), 0);

    c.open(&t, 3, claude("0000000d")).unwrap();
    assert!(c.until_has(3, "BANNER", BOOT));
    assert_eq!(
        h.starts_of("0000000d").len(),
        2,
        "the open after that starts fresh"
    );
}

// (e)
#[test]
fn a_seventh_released_terminal_evicts_and_stops_the_oldest() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    for i in 1..=7 {
        c.open(&t, i as u32, claude(&job(0x100 + i))).unwrap();
        assert!(c.until_has(i as u32, "BANNER", BOOT));
        t.close(1, i as u32);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(t.warm_count(), terms::WARM_MAX);
    let first = h.starts_of(&job(0x101))[0];
    assert!(
        gone_within(first, Duration::from_secs(5)),
        "the oldest is stopped"
    );
    for i in 2..=7 {
        assert!(
            alive(h.starts_of(&job(0x100 + i))[0]),
            "warm {i} still runs"
        );
    }
    c.open(&t, 20, claude(&job(0x101))).unwrap();
    assert!(c.until_has(20, "BANNER", BOOT));
    assert_eq!(h.starts_of(&job(0x101)).len(), 2, "evicted: a fresh start");
}

#[test]
fn a_warm_terminal_expires_and_the_timer_then_leaves() {
    let h = Home::new();
    let t = Terms::with_options(
        h.path(),
        |_| false,
        Options {
            warm_ttl: Duration::from_millis(300),
        },
    )
    .unwrap();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("0000000e")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT));
    assert!(!t.timer_armed(), "nothing to time while attached");
    t.close(1, 1);
    assert!(t.timer_armed());
    let pid = h.starts_of("0000000e")[0];
    assert!(
        gone_within(pid, Duration::from_secs(5)),
        "stopped at expiry"
    );
    assert_eq!(t.warm_count(), 0);
    assert!(
        wait_for(Duration::from_secs(1), || !t.timer_armed()),
        "idle stays idle"
    );
}

// (f)
#[test]
fn codex_has_one_writer_and_is_never_pooled() {
    let h = Home::new();
    let busy: Arc<Mutex<HashSet<String>>> = Arc::default();
    let b2 = busy.clone();
    let t = Terms::new(h.path(), move |sid| b2.lock().unwrap().contains(sid)).unwrap();
    let (mut a, b) = (Conn::new(1), Conn::new(2));
    a.open(&t, 1, codex(SID)).unwrap();
    assert!(a.until_has(1, &format!("BANNER resume --no-daemon {SID}"), BOOT));
    assert_eq!(
        b.open(&t, 1, codex(SID)),
        Err("It is open in another Fleet terminal already.".into())
    );
    let other = "0199aaaa-bbbb-4ccc-8ddd-ffffffffffff";
    busy.lock().unwrap().insert(other.into());
    assert_eq!(
        b.open(&t, 2, codex(other)),
        Err(
            "It is open in Codex (the app or a terminal): finish there, or close it there first."
                .into()
        )
    );
    assert_eq!(h.starts_of(other).len(), 0);

    t.close(1, 1);
    let pid = h.starts_of(SID)[0];
    assert!(
        gone_within(pid, Duration::from_secs(5)),
        "released Codex is stopped"
    );
    assert_eq!(t.warm_count(), 0);
    a.open(&t, 3, codex(SID)).unwrap();
    assert!(a.until_has(3, "BANNER", BOOT));
    assert_eq!(h.starts_of(SID).len(), 2);
}

#[test]
fn a_trust_terminal_is_stopped_when_released_and_bad_targets_are_refused() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(
        &t,
        1,
        AttachTarget::Trust {
            profile: "default".into(),
            dir: "~/work".into(),
        },
    )
    .unwrap();
    assert!(c.until_has(1, "BANNER", BOOT));
    let pid = *h
        .starts()
        .iter()
        .map(|l| l.rsplit(' ').next().unwrap().parse::<i32>().unwrap())
        .collect::<Vec<_>>()
        .last()
        .unwrap();
    t.close(1, 1);
    assert!(gone_within(pid, Duration::from_secs(5)));
    assert_eq!(t.warm_count(), 0);
    let pick = "Pick a background session, or a folder that exists.";
    assert_eq!(c.open(&t, 2, claude("NOTAJOB!")), Err(pick.into()));
    assert_eq!(
        c.open(
            &t,
            3,
            AttachTarget::Trust {
                profile: "default".into(),
                dir: "~/missing".into()
            }
        ),
        Err(pick.into())
    );
}

// (g)
#[test]
fn the_limits_use_term_mjs_words_and_a_full_house_evicts_warm_first() {
    let h = Home::new();
    let t = h.terms();
    let mut conns: Vec<Conn> = (1..=4).map(Conn::new).collect();
    let mut n = 0;
    for c in conns.iter_mut().take(3) {
        for r in 1..=8u32 {
            n += 1;
            c.open(&t, r, claude(&job(0x200 + n))).unwrap();
        }
    }
    assert_eq!(
        conns[0].open(&t, 9, claude(&job(0x300))),
        Err("Too many terminals open in this tab.".into())
    );
    assert_eq!(t.live_count(), terms::MAX);
    assert_eq!(
        conns[3].open(&t, 1, claude(&job(0x301))),
        Err("Too many terminals open.".into())
    );
    // A shared open needs no slot.
    conns[3].open(&t, 2, claude(&job(0x201))).unwrap();
    // With one released (warm), a cold open evicts it rather than refusing.
    assert!(conns[2].until_has(8, "BANNER", BOOT));
    t.close(conns[2].id, 8);
    assert_eq!(t.warm_count(), 1);
    conns[3].open(&t, 3, claude(&job(0x302))).unwrap();
    assert_eq!(t.warm_count(), 0);
    assert_eq!(t.live_count(), terms::MAX);
    let evicted = h.starts_of(&job(0x200 + 24));
    assert_eq!(evicted.len(), 1);
    assert!(
        gone_within(evicted[0], Duration::from_secs(5)),
        "the warm one was stopped"
    );
}

// (h)
#[test]
fn a_resize_storm_coalesces_and_ends_at_the_last_size() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("000000f0")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT));
    // 600 sizes in 10 s, in bursts of 30 (a window drag), every one different.
    let mut last = (0, 0);
    for burst in 0..20u16 {
        for i in 0..30u16 {
            last = (40 + burst * 5 + i % 5, 10 + i);
            t.size(1, 1, last.0, last.1);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let want = format!("REDRAW {}x{}", last.0, last.1);
    assert!(c.until(5.0, |c| {
        let s = String::from_utf8_lossy(c.output(1)).into_owned();
        s.lines()
            .rfind(|l| l.contains("REDRAW"))
            .is_some_and(|l| l.contains(&want))
    }));
    let w = h.winches();
    assert_eq!(
        (w.last().unwrap().1, w.last().unwrap().2),
        last,
        "the pty ends at the last size"
    );
    assert!(w.len() <= 2 * 20, "{} redraws for 20 bursts", w.len());
    let min_gap = w
        .windows(2)
        .map(|p| p[1].0 - p[0].0)
        .fold(f64::MAX, f64::min);
    // Applied 16 ms apart; the stand-in's handler may run a few ms late, never 16 ms early.
    assert!(
        min_gap >= 0.012,
        "two redraws {:.1} ms apart",
        min_gap * 1000.0
    );
    assert!(
        wait_for(Duration::from_secs(1), || !t.timer_armed()),
        "no timer once the sizes are applied"
    );
}

#[test]
fn a_full_sink_stops_the_reading_and_loses_nothing() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("000000f1")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT));
    t.input(1, 1, b"flood\n");
    std::thread::sleep(Duration::from_millis(1500));
    let held = c.src.pending_bytes();
    assert!(
        held > terms::SINK_LOW,
        "the flood reached the sink ({held})"
    );
    assert!(
        held <= terms::SINK_HIGH + 64 * 1024,
        "bounded: {held} queued"
    );
    // Another terminal of another connection is unaffected meanwhile.
    let mut d = Conn::new(2);
    d.open(&t, 1, claude("000000f2")).unwrap();
    assert!(d.until_has(1, "BANNER", BOOT));
    assert!(c.until_has(1, "FLOODED", 20.0));
    let xs = c.output(1).iter().filter(|b| **b == b'X').count();
    assert_eq!(xs, 5_000_000, "every byte arrived");
}

#[test]
fn input_to_a_program_that_does_not_read_wedges_only_that_terminal() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("000000f3")).unwrap();
    c.open(&t, 2, claude("000000f4")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT) && c.until_has(2, "BANNER", BOOT));
    let pid = h.starts_of("000000f3")[0];
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let big = vec![b'k'; 60 * 1024];
    let s = Instant::now();
    for _ in 0..40 {
        t.input(1, 1, &big); // far more than the tty takes while nobody reads
    }
    assert!(
        s.elapsed() < Duration::from_millis(500),
        "input never blocks the caller"
    );
    t.input(1, 2, b"other\n");
    assert!(
        c.until_has(2, "GOT other", 5.0),
        "the other terminal still gets its keys"
    );
    unsafe { libc::kill(pid, libc::SIGCONT) };
}

#[test]
fn dropping_terms_stops_every_program() {
    let h = Home::new();
    let t = h.terms();
    let mut c = Conn::new(1);
    c.open(&t, 1, claude("000000f5")).unwrap();
    c.open(&t, 2, claude("000000f6")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT) && c.until_has(2, "BANNER", BOOT));
    t.close(1, 2);
    drop(t);
    for j in ["000000f5", "000000f6"] {
        assert!(
            gone_within(h.starts_of(j)[0], Duration::from_secs(5)),
            "{j}"
        );
    }
}
