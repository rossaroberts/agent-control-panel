//! The stand-in home and a connection's view, for the terminal tests (`terms.rs`, `terms_drop.rs`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fleet_model::types::AttachTarget;
use fleet_model::wire::Frame;
use fleetd::terms::{term_sink, TermSink, TermSource, Terms};

pub const STAND_IN: &str = r#"#!/usr/bin/python3
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
    elif l == b'flood50':
        for _ in range(50): os.write(1, b'X' * 1000000)
        os.write(1, b'\r\nFLOODED\r\n')
    elif l == b'exit-soon':
        time.sleep(0.3); os.write(1, b'LAST WORDS\r\n'); sys.exit(0)
    else:
        os.write(1, b'GOT ' + l + b'\r\n')
"#;

pub struct Home {
    pub dir: tempfile::TempDir,
}

impl Home {
    pub fn new() -> Home {
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

    pub fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    pub fn starts(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("starts.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Starts whose argv mentions `word`.
    pub fn starts_of(&self, word: &str) -> Vec<i32> {
        self.starts()
            .iter()
            .filter(|l| l.split(' ').any(|w| w == word))
            .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
            .collect()
    }

    /// (seconds, cols, rows) of each SIGWINCH the stand-ins saw.
    pub fn winches(&self) -> Vec<(f64, u16, u16)> {
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

    pub fn terms(&self) -> Terms {
        Terms::new(self.path(), |_| false).unwrap()
    }
}

pub fn claude(job: &str) -> AttachTarget {
    AttachTarget::Claude {
        profile: "default".into(),
        job: job.into(),
    }
}

pub fn codex(sid: &str) -> AttachTarget {
    AttachTarget::Codex {
        profile: "codex".into(),
        session_id: sid.into(),
        cwd: "~/work".into(),
    }
}

pub fn job(n: usize) -> String {
    format!("{n:08x}")
}

/// Seconds a cold stand-in gets to print its banner (python starts slowly when the suite runs 25 at once).
pub const BOOT: f64 = 20.0;

pub const SID: &str = "0199aaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

/// One connection's view: its sink for opens, and what has come out of its source, per ref.
pub struct Conn {
    pub id: u64,
    pub sink: TermSink,
    pub src: TermSource,
    /// Each frame received, in order: (ref, Some(payload length)) for output, (ref, None) for an end.
    pub got: Vec<(u32, Option<usize>)>,
    pub outs: HashMap<u32, Vec<u8>>,
    /// Each `TermEnd`'s error, per ref, in order.
    pub ends: HashMap<u32, Vec<Option<String>>>,
}

impl Conn {
    pub fn new(id: u64) -> Conn {
        let (sink, src) = term_sink();
        Conn {
            id,
            sink,
            src,
            got: vec![],
            outs: HashMap::new(),
            ends: HashMap::new(),
        }
    }

    pub fn open(&self, t: &Terms, r: u32, target: AttachTarget) -> Result<(), String> {
        t.open(self.id, r, target, 100, 30, self.sink.clone())
    }

    pub fn output(&self, r: u32) -> &[u8] {
        self.outs.get(&r).map_or(&[], |v| v.as_slice())
    }

    pub fn ended(&self, r: u32) -> bool {
        self.got.iter().any(|(x, d)| *x == r && d.is_none())
    }

    /// Receives one frame, waiting until `end` at most; false if none came.
    pub fn take(&mut self, end: Instant) -> bool {
        let now = Instant::now();
        if now >= end {
            return false;
        }
        match self.src.recv_timeout(end - now) {
            Some(Frame::TermOut { r#ref, data }) => {
                self.got.push((r#ref, Some(data.len())));
                self.outs.entry(r#ref).or_default().extend(data);
            }
            Some(Frame::TermEnd { r#ref, error }) => {
                self.got.push((r#ref, None));
                self.ends.entry(r#ref).or_default().push(error);
            }
            Some(f) => panic!("unexpected {f:?}"),
            None => {}
        }
        true
    }

    /// Receives until `done(self)` or `secs` pass; whether it was done.
    pub fn until(&mut self, secs: f64, done: impl Fn(&Conn) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs_f64(secs);
        while !done(self) {
            if !self.take(end) {
                return false;
            }
        }
        true
    }

    /// Receives until `ref`'s output holds `needle` (looking only at what is new each time).
    pub fn until_has(&mut self, r: u32, needle: &str, secs: f64) -> bool {
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

pub fn has(all: &[u8], needle: &str) -> bool {
    all.windows(needle.len()).any(|w| w == needle.as_bytes())
}

pub fn pos(all: &[u8], needle: &str) -> Option<usize> {
    all.windows(needle.len())
        .position(|w| w == needle.as_bytes())
}

pub fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

pub fn gone_within(pid: i32, d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

pub fn wait_for(d: Duration, f: impl Fn() -> bool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

impl Conn {
    pub fn open_sized(&self, t: &Terms, r: u32, target: AttachTarget, cols: u16, rows: u16) {
        t.open(self.id, r, target, cols, rows, self.sink.clone())
            .unwrap();
    }
}
