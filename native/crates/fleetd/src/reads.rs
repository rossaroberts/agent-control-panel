//! The reads the board is fed from, all done without the board's lock: the small per-session files (hook, status line),
//! where a transcript is, the jobs' records, and the followed tails behind their own locks, each publishing what it last
//! read as a value (`Seen`) so that nothing waits on a tail's lock to show a session.

use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fleet_model::decide::TailFacts;
use fleet_model::types::HookEvent;
use fleet_read::codex::{RolloutTail, RolloutView};
use fleet_read::hookfile::{parse_hook, valid_id};
use fleet_read::profiles::Profile;
use fleet_read::statusfile::{parse_status, StatusSnap};
use fleet_read::transcript::{Tail, TranscriptSummary};

/// Where a session's transcript is (`transcriptOf`): `projects/<its folder, every character but A-Za-z0-9 a dash>/<id>.jsonl`,
/// else the first project folder that has `<id>.jsonl`. JS replaces per UTF-16 unit, so a character outside the BMP is
/// two dashes.
pub fn transcript_of(p: &Profile, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    let root = p.dir.join("projects");
    let file = format!("{session_id}.jsonl");
    if let Some(cwd) = cwd {
        let mut d = String::with_capacity(cwd.len());
        for c in cwd.chars() {
            if c.is_ascii_alphanumeric() {
                d.push(c);
            } else {
                d.extend(std::iter::repeat_n('-', c.len_utf16()));
            }
        }
        let f = root.join(d).join(&file);
        if f.is_file() {
            return Some(f);
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|d| d.join(&file))
        .find(|f| f.is_file())
}

/// What `jobs/*/state.json` look like (name, modified, length): a change the watcher missed shows as a different one.
pub fn jobs_signature(p: &Profile) -> Vec<(String, i128, u64)> {
    let Ok(rd) = std::fs::read_dir(p.dir.join("jobs")) else {
        return vec![];
    };
    let mut sig: Vec<(String, i128, u64)> = rd
        .flatten()
        .filter_map(|e| {
            let s = sig_of(&e.path().join("state.json"))?;
            Some((
                e.file_name().to_string_lossy().into_owned(),
                s.mtime_ns,
                s.len,
            ))
        })
        .collect();
    sig.sort();
    sig
}

// ---------------------------------------------------------------- small files

/// Which version of a file a read saw: a file replaced by a rename has a new inode; one written in place a new time or
/// length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sig {
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: i128,
}

fn sig_of_meta(m: &std::fs::Metadata) -> Sig {
    Sig {
        ino: m.ino(),
        len: m.len(),
        mtime_ns: m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
    }
}

pub fn sig_of(f: &Path) -> Option<Sig> {
    std::fs::metadata(f).ok().map(|m| sig_of_meta(&m))
}

/// A read of a small file.
#[derive(Clone, Debug)]
pub enum FileRead<T> {
    /// There is no such file (it was removed, or never written): what was held from it goes.
    Gone,
    /// The version seen (stat first, so a file replaced mid-read shows as changed next time) and what it said (`None`:
    /// it does not parse; a file that is there but says nothing usable).
    Read { sig: Sig, v: Option<T> },
    /// The read itself failed (too many open files, permissions, I/O): it proves nothing, and what is held stays.
    Failed,
}

impl<T> FileRead<T> {
    pub fn sig(&self) -> Option<Sig> {
        match self {
            FileRead::Read { sig, .. } => Some(*sig),
            _ => None,
        }
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            FileRead::Read { v, .. } => v.as_ref(),
            _ => None,
        }
    }
}

/// How a file's bytes are read: at most 64 KiB + 1 (the parsers refuse more).
pub type Reader = dyn Fn(&Path) -> io::Result<Vec<u8>>;

fn read_capped(f: &Path) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::fs::File::open(f)?
        .take(fleet_read::hookfile::MAX_BYTES + 1)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_small<T>(f: &Path, read: &Reader, parse: impl FnOnce(&[u8]) -> Option<T>) -> FileRead<T> {
    let sig = match std::fs::metadata(f) {
        Ok(m) => sig_of_meta(&m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return FileRead::Gone,
        Err(_) => return FileRead::Failed,
    };
    match read(f) {
        Ok(b) => FileRead::Read { sig, v: parse(&b) },
        Err(e) if e.kind() == io::ErrorKind::NotFound => FileRead::Gone,
        Err(_) => FileRead::Failed,
    }
}

pub fn hook_path(p: &Profile, sid: &str) -> PathBuf {
    p.dir.join("fleet-state").join(format!("{sid}.json"))
}

pub fn status_path(p: &Profile, sid: &str) -> PathBuf {
    p.dir.join("fleet-status").join(format!("{sid}.json"))
}

/// `read_hook_file` reading the file's bytes with `read` (the seam tests use to make reads fail).
pub fn read_hook_file_with(p: &Profile, sid: &str, read: &Reader) -> FileRead<HookEvent> {
    if !valid_id(sid) {
        return FileRead::Gone;
    }
    read_small(&hook_path(p, sid), read, parse_hook)
}

pub fn read_hook_file(p: &Profile, sid: &str) -> FileRead<HookEvent> {
    read_hook_file_with(p, sid, &read_capped)
}

pub fn read_status_file(p: &Profile, sid: &str) -> FileRead<StatusSnap> {
    read_small(&status_path(p, sid), &read_capped, |b| parse_status(sid, b))
}

/// Takes a read into what is held (`held` the value, `sig` the version it came from): a failed read changes nothing; a
/// file gone clears it; a read of another version, not older than the one held, replaces it (so of two racing reads
/// the slower never overwrites the newer). Whether anything changed.
pub fn take<T>(held: &mut Option<T>, sig: &mut Option<Sig>, r: FileRead<T>) -> bool {
    match r {
        FileRead::Failed => false,
        FileRead::Gone => {
            let had = sig.is_some() || held.is_some();
            *held = None;
            *sig = None;
            had
        }
        FileRead::Read { sig: n, v } => {
            if sig.is_some_and(|o| o == n || n.mtime_ns < o.mtime_ns) {
                return false;
            }
            *sig = Some(n);
            *held = v;
            true
        }
    }
}

// ---------------------------------------------------------------- tails

/// What a tail last read, as a value.
pub trait Snap {
    type View: Clone;
    fn snap(&self) -> Self::View;
}

#[derive(Clone, Debug, Default)]
pub struct TailView {
    pub facts: TailFacts,
    pub summary: TranscriptSummary,
}

impl Snap for Tail {
    type View = TailView;
    fn snap(&self) -> TailView {
        TailView {
            facts: self.facts(),
            summary: self.summary().clone(),
        }
    }
}

impl Snap for RolloutTail {
    type View = RolloutView;
    fn snap(&self) -> RolloutView {
        self.view()
    }
}

/// A view and the number of the read it came from: a newer number replaces an older one, never the reverse.
#[derive(Clone, Debug)]
pub struct Seen<V> {
    pub gen: u64,
    pub v: V,
}

/// A followed file's tail behind its own lock, with its path readable without it. Every read through `run` numbers the
/// view it publishes.
pub struct TailCell<T> {
    path: PathBuf,
    cell: Mutex<(T, u64)>,
}

pub type SharedTail = Arc<TailCell<Tail>>;
pub type SharedRollout = Arc<TailCell<RolloutTail>>;

impl<T: Snap> TailCell<T> {
    /// A cell for a tail just opened, and its first view.
    pub fn new(path: PathBuf, t: T) -> (Arc<TailCell<T>>, Seen<T::View>) {
        let v = t.snap();
        (
            Arc::new(TailCell {
                path,
                cell: Mutex::new((t, 0)),
            }),
            Seen { gen: 0, v },
        )
    }

    /// Runs a read (`advance`, `reconcile`) under the tail's own lock: its result and the view after it.
    pub fn run<R>(&self, f: impl FnOnce(&mut T) -> R) -> (R, Seen<T::View>) {
        let mut g = self.cell.lock().unwrap();
        let r = f(&mut g.0);
        g.1 += 1;
        let seen = Seen {
            gen: g.1,
            v: g.0.snap(),
        };
        (r, seen)
    }
}

impl<T> TailCell<T> {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Test seam: holds this tail's lock for `d`, as a slow read would.
    #[doc(hidden)]
    pub fn hold(&self, d: std::time::Duration) {
        let _g = self.cell.lock().unwrap();
        std::thread::sleep(d);
    }
}

pub fn new_tail(t: Tail) -> (SharedTail, Seen<TailView>) {
    let p = t.path().to_path_buf();
    TailCell::new(p, t)
}

pub fn new_rollout(t: RolloutTail) -> (SharedRollout, Seen<RolloutView>) {
    let p = t.path().to_path_buf();
    TailCell::new(p, t)
}

/// A read error that means the file is gone (the tail is let go); any other error leaves it followed.
pub fn gone(r: &io::Result<bool>) -> bool {
    matches!(r, Err(e) if e.kind() == io::ErrorKind::NotFound)
}
