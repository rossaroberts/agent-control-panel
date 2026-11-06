//! The terminals: a port of `fleet/term.mjs` `createTerms` (local branch) with a warm pool on top. A client connection
//! (`ConnId`) opens terminals by numbers of its own (`ref`); each is a pty running what `attach_cmd::command` says.
//!
//! - A released `claude attach` pty is kept warm, keyed by `(profile, job)`, for 10 minutes, at most 6 of them (oldest
//!   evicted first). Opening the same key takes it back: its last 1 MiB of output is replayed, then a size kick makes the
//!   program redraw. Several refs on one key share the pty: output goes to all, input from any goes in.
//! - Codex and trust-folder terminals end when released. One Fleet terminal per Codex thread, and none on a thread a
//!   running Codex holds open. The process scan sees the Codex a Fleet terminal runs too, so once one has gone a scan is
//!   asked for (`Rescan`), and a reopen of its thread meanwhile waits for it (at most `REOPEN_WAIT`) rather than be
//!   refused on the scan from before.
//! - Each pty has one reader thread (into its ring and every attached sink) and one writer thread (input, so a program
//!   that stops reading wedges only itself). A sink holding more than 1 MiB unsent stops the reading of every pty writing
//!   to it until it is down to half, so the programs block on output: nothing is dropped, nothing grows without bound.
//! - Sizes coalesce: the latest pending size per pty, applied at most once per 16 ms. One timer thread serves the size
//!   trailing edges and warm expiry, and exists only while there is something to time.
//!
//! Lock order: `Shared::st`, then a `Term`'s locks, then a sink's. The reader thread takes a `Term`'s lock and lets it go
//! before it takes `Shared::st`. Nothing that blocks (a pty stop, a write) runs under any of them.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

use fleet_model::types::AttachTarget;
use fleet_model::wire::Frame;

use crate::alarm::Alarm;
use crate::attach_cmd::command;
use crate::pty::Pty;

/// A terminal connection, numbered by the socket layer.
pub type ConnId = u64;

/// Terminals open at most, warm ones included (`term.mjs` `MAX`). A cold open at the cap evicts a warm one first.
pub const MAX: usize = 24;
/// Terminals one connection has open at most (`term.mjs` `PER_PAGE`).
pub const PER_PAGE: usize = 8;
/// Released Claude Code terminals kept warm at most.
pub const WARM_MAX: usize = 6;
/// How long a released terminal is kept warm.
pub const WARM_TTL: Duration = Duration::from_secs(600);
/// The recent output each pty keeps for a newcomer.
pub const RING: usize = 1 << 20;
/// Payload bytes a sink may hold unsent before the ptys writing to it stop reading.
pub const SINK_HIGH: usize = 1 << 20;
/// ...and the level they start again at.
pub const SINK_LOW: usize = SINK_HIGH / 2;
/// Sizes are applied at most this often per pty.
pub const RESIZE_EVERY: Duration = Duration::from_millis(16);
/// Input queued for one pty at most. Past it `input` refuses with `InputFull` (nothing of that call is taken) and the
/// socket layer stops reading the connection until `input_pending` drains: keystrokes are never dropped (Ruling P2-R4).
pub const INPUT_MAX: usize = 1 << 20;
/// The most one `TermOut` carries (ring replays are cut to this).
const CHUNK: usize = 64 * 1024;
/// How long a Codex reopen waits for the terminal before it on that thread to be gone and scanned.
pub const REOPEN_WAIT: Duration = Duration::from_millis(500);
/// How long a stopped Codex's thread waits for the process scan it asked for.
const SCAN_WAIT: Duration = Duration::from_secs(2);

// `term.mjs`'s words.
pub const ERR_PICK: &str = "Pick a background session, or a folder that exists.";
pub const ERR_TOO_MANY: &str = "Too many terminals open.";
pub const ERR_PER_PAGE: &str = "Too many terminals open in this tab.";
pub const ERR_CODEX_OPEN: &str = "It is open in another Fleet terminal already.";
/// `fleet.mjs` `codexBusy`'s reason for a thread a running Codex holds.
pub const ERR_CODEX_BUSY: &str =
    "It is open in Codex (the app or a terminal): finish there, or close it there first.";
/// The `TermEnd` of a reopen that finds the program of a warm terminal ended (after its last output).
pub const ERR_ENDED: &str = "The session has ended.";

/// `Terms::input` refused: the terminal holds `INPUT_MAX` of input its program has not read yet. None of the refused
/// call's bytes were taken; send them again once `Terms::input_pending` has gone down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputFull;

/// `term.mjs` `clamp` for the wire's integers.
fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(20, 400), rows.clamp(5, 200))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------------------------------------- the sink

struct SinkQ {
    frames: VecDeque<Frame>,
    /// Payload bytes of the `TermOut`s queued.
    bytes: usize,
    /// The receiving side is gone: frames are discarded.
    closed: bool,
}

struct SinkCore {
    q: Mutex<SinkQ>,
    /// A frame came (for `recv_timeout`).
    arrived: Condvar,
    /// The queue went down to `SINK_LOW` or closed (for a paused reader).
    drained: Condvar,
    /// A frame came (for `recv`).
    ready: tokio::sync::Notify,
}

/// The sending side of one connection's terminal output: every pty attached to the connection pushes its `TermOut`
/// and `TermEnd` frames here. Bounded by backpressure (see the module doc), never by dropping.
#[derive(Clone)]
pub struct TermSink(Arc<SinkCore>);

/// The receiving side, for the socket layer: frames in the order the ptys pushed them.
pub struct TermSource(Arc<SinkCore>);

/// A sink and its source, for one connection.
pub fn term_sink() -> (TermSink, TermSource) {
    let core = Arc::new(SinkCore {
        q: Mutex::new(SinkQ {
            frames: VecDeque::new(),
            bytes: 0,
            closed: false,
        }),
        arrived: Condvar::new(),
        drained: Condvar::new(),
        ready: tokio::sync::Notify::new(),
    });
    (TermSink(core.clone()), TermSource(core))
}

/// The bytes a frame weighs against `SINK_HIGH`: its output, or the words of an end (so a client that is sent refusals
/// it does not read is held back like one that is sent output).
fn payload(f: &Frame) -> usize {
    match f {
        Frame::TermOut { data, .. } => data.len(),
        Frame::TermEnd { error, .. } => error.as_ref().map_or(0, String::len),
        _ => 0,
    }
}

impl TermSink {
    /// Queues `TermEnd { ref, error }` behind whatever is queued already (the socket layer's answer to an open that
    /// `Terms::open` refused, in order with the output of an earlier terminal of the same `ref`).
    pub fn end(&self, r#ref: u32, error: Option<String>) {
        self.push(Frame::TermEnd { r#ref, error });
    }

    fn push(&self, f: Frame) {
        let mut q = lock(&self.0.q);
        if q.closed {
            return;
        }
        q.bytes += payload(&f);
        q.frames.push_back(f);
        drop(q);
        self.0.arrived.notify_all();
        self.0.ready.notify_one();
    }

    fn bytes(&self) -> usize {
        lock(&self.0.q).bytes
    }

    /// Nothing more goes to this sink, and nothing waits on it.
    fn close(&self) {
        let mut q = lock(&self.0.q);
        q.closed = true;
        drop(q);
        self.0.drained.notify_all();
        self.0.ready.notify_one();
        self.0.arrived.notify_all();
    }

    /// Blocks until this sink is down to `SINK_LOW` (or closed), or `d` passes.
    fn wait_drained(&self, d: Duration) {
        let q = lock(&self.0.q);
        let _ = self
            .0
            .drained
            .wait_timeout_while(q, d, |q| !q.closed && q.bytes > SINK_LOW);
    }
}

impl TermSource {
    fn pop(&self) -> Option<Frame> {
        let mut q = lock(&self.0.q);
        let f = q.frames.pop_front()?;
        q.bytes -= payload(&f);
        if q.bytes <= SINK_LOW {
            self.0.drained.notify_all();
        }
        Some(f)
    }

    /// The next frame, waiting for one.
    pub async fn recv(&self) -> Frame {
        loop {
            let ready = self.0.ready.notified();
            if let Some(f) = self.pop() {
                return f;
            }
            ready.await;
        }
    }

    /// The next frame, waiting at most `d` (blocking).
    pub fn recv_timeout(&self, d: Duration) -> Option<Frame> {
        let end = Instant::now() + d;
        loop {
            if let Some(f) = self.pop() {
                return Some(f);
            }
            let q = lock(&self.0.q);
            let now = Instant::now();
            if now >= end {
                return None;
            }
            let _ = self
                .0
                .arrived
                .wait_timeout_while(q, end - now, |q| q.frames.is_empty());
        }
    }

    /// Payload bytes queued and not yet received.
    pub fn pending_bytes(&self) -> usize {
        lock(&self.0.q).bytes
    }
}

impl Drop for TermSource {
    /// The connection is gone: what is queued goes, and nothing waits on it any more.
    fn drop(&mut self) {
        let mut q = lock(&self.0.q);
        q.closed = true;
        q.frames.clear();
        q.bytes = 0;
        drop(q);
        self.0.drained.notify_all();
    }
}

// ------------------------------------------------------------------------------------------------- one pty

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    /// `claude attach`: shared and kept warm.
    Claude(String, String),
    /// A Codex thread: one terminal at a time.
    Codex(String),
    /// A trust-folder terminal: its own.
    Solo(u64),
}

struct Attached {
    conn: ConnId,
    r#ref: u32,
    sink: TermSink,
}

struct TermSt {
    refs: Vec<Attached>,
    ring: VecDeque<u8>,
    /// The program has gone (its pty hung up).
    ended: bool,
}

struct SizeSt {
    /// The size the pty has.
    cur: (u16, u16),
    /// The latest size asked for and not applied yet.
    pending: Option<(u16, u16)>,
    /// When a size was last applied.
    last: Option<Instant>,
    /// The timer holds a trailing edge for this pty.
    armed: bool,
}

struct InputQ {
    /// Input not yet written, the part being written included (so `input_pending` counts what the program has not taken).
    buf: VecDeque<u8>,
    closed: bool,
}

struct Term {
    id: u64,
    key: Key,
    pty: Pty,
    /// A Codex terminal's going has been announced (`Shared::codex_going`), by its release or by its program's end.
    going: AtomicBool,
    st: Mutex<TermSt>,
    size: Mutex<SizeSt>,
    input: Mutex<InputQ>,
    input_cv: Condvar,
}

impl Term {
    /// Queues `data` whole, or refuses it whole when it would take the queue past `INPUT_MAX` (an empty queue takes any
    /// one call, so an oversized call cannot wait forever). Input for a program that has gone is let go.
    fn push_input(&self, data: &[u8]) -> Result<(), InputFull> {
        let mut q = lock(&self.input);
        if q.closed {
            return Ok(());
        }
        if !q.buf.is_empty() && q.buf.len() + data.len() > INPUT_MAX {
            return Err(InputFull);
        }
        q.buf.extend(data);
        drop(q);
        self.input_cv.notify_one();
        Ok(())
    }

    fn input_pending(&self) -> usize {
        lock(&self.input).buf.len()
    }

    fn close_input(&self) {
        lock(&self.input).closed = true;
        self.input_cv.notify_one();
    }

    /// Applies `want` if the pty has another size.
    fn apply(&self, s: &mut SizeSt, want: (u16, u16)) {
        if want != s.cur {
            self.pty.resize(want.0, want.1);
            s.cur = want;
            s.last = Some(Instant::now());
        }
    }

    /// A size asked for: applied now if the last was long enough ago, else kept as the pending one. `Some(due)` when the
    /// timer must come back for it.
    fn request_size(&self, want: (u16, u16)) -> Option<Instant> {
        let mut s = lock(&self.size);
        if s.armed {
            s.pending = Some(want);
            return None;
        }
        match s.last {
            Some(t) if t.elapsed() < RESIZE_EVERY => {
                s.pending = Some(want);
                s.armed = true;
                Some(t + RESIZE_EVERY)
            }
            _ => {
                self.apply(&mut s, want);
                None
            }
        }
    }

    /// The timer's trailing edge: the pending size, if any.
    fn apply_pending(&self) {
        let mut s = lock(&self.size);
        s.armed = false;
        if let Some(want) = s.pending.take() {
            self.apply(&mut s, want);
        }
    }

    /// Makes the program redraw for a newcomer: its size, or, when it already has that size (no SIGWINCH would come),
    /// one column less and then its size.
    fn kick(&self, want: (u16, u16)) {
        let mut s = lock(&self.size);
        s.pending = None;
        if want == s.cur {
            self.pty.resize(want.0 - 1, want.1);
        }
        self.pty.resize(want.0, want.1);
        s.cur = want;
        s.last = Some(Instant::now());
    }

    /// Replays the end of the ring to `sink` as `ref`'s output: as much as the sink has room for below `SINK_HIGH`, so a
    /// client that never reads but keeps reopening cannot grow its queue (the size kick redraws what is left out).
    fn replay(st: &TermSt, r#ref: u32, sink: &TermSink) {
        let room = SINK_HIGH.saturating_sub(sink.bytes());
        let skip = st.ring.len().saturating_sub(room);
        let tail: Vec<u8> = st.ring.range(skip..).copied().collect();
        for c in tail.chunks(CHUNK) {
            sink.push(Frame::TermOut {
                r#ref,
                data: c.to_vec(),
            });
        }
    }
}

/// Ends the program off the caller's thread (`Pty::stop` blocks up to ~2 s).
fn stop_detached(t: Arc<Term>) {
    t.close_input();
    let _ = thread::Builder::new()
        .name("term-stop".into())
        .spawn(move || t.pty.stop());
}

// ------------------------------------------------------------------------------------------------- the manager

/// Tunables, for tests.
#[derive(Clone, Debug)]
pub struct Options {
    pub warm_ttl: Duration,
}

impl Default for Options {
    fn default() -> Options {
        Options { warm_ttl: WARM_TTL }
    }
}

type CodexHeld = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// The board's process scan, for after a Codex terminal has gone (`WatchHandle::request_scan` / `wait_scan`).
pub struct Rescan {
    /// Asks for a scan; a ticket for `wait`.
    pub request: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Waits at most the duration for the ticket's scan to be done; whether it was.
    pub wait: Box<dyn Fn(u64, Duration) -> bool + Send + Sync>,
}

struct State {
    terms: HashMap<u64, Arc<Term>>,
    by_key: HashMap<Key, u64>,
    conns: HashMap<(ConnId, u32), u64>,
    /// Released Claude Code terminals (and ones whose program ended while warm), oldest first, with their release time.
    warm: VecDeque<(u64, Instant)>,
    /// Size trailing edges due.
    resizes: Vec<(Instant, Arc<Term>)>,
    timer_on: bool,
    next_id: u64,
    closed: bool,
}

impl State {
    /// Forgets terminal `id` entirely; the term, so the caller can stop it.
    fn remove(&mut self, id: u64) -> Option<Arc<Term>> {
        let t = self.terms.remove(&id)?;
        if self.by_key.get(&t.key) == Some(&id) {
            self.by_key.remove(&t.key);
        }
        self.warm.retain(|(w, _)| *w != id);
        self.conns.retain(|_, v| *v != id);
        Some(t)
    }

    fn live(&self) -> usize {
        self.terms.values().filter(|t| !lock(&t.st).ended).count()
    }

    /// Evicts the oldest warm terminal whose program still runs; false if there is none.
    fn evict_oldest_live(&mut self) -> bool {
        let id = self
            .warm
            .iter()
            .map(|(id, _)| *id)
            .find(|id| self.terms.get(id).is_some_and(|t| !lock(&t.st).ended));
        match id.and_then(|id| self.remove(id)) {
            Some(t) => {
                stop_detached(t);
                true
            }
            None => false,
        }
    }
}

struct Shared {
    home: PathBuf,
    codex_held: CodexHeld,
    rescan: Option<Rescan>,
    opts: Options,
    st: Mutex<State>,
    alarm: Alarm,
    /// Codex threads whose terminal is going: how many, until each is gone and a scan has been done since.
    codex_going: Mutex<HashMap<String, u32>>,
    codex_cv: Condvar,
}

impl Shared {
    /// Codex terminal `t` is going (its release, or its program's end): until `codex_gone`, a reopen of its thread waits.
    /// False when it is not a Codex one, or was announced already.
    fn codex_going(&self, t: &Term) -> bool {
        let Key::Codex(sid) = &t.key else {
            return false;
        };
        if t.going.swap(true, Ordering::SeqCst) {
            return false;
        }
        *lock(&self.codex_going).entry(sid.clone()).or_default() += 1;
        true
    }

    /// Its program is gone (the group is): a scan that sees that, then reopens of the thread go ahead.
    fn codex_gone(&self, sid: &str) {
        if let Some(r) = &self.rescan {
            let ticket = (r.request)();
            (r.wait)(ticket, SCAN_WAIT);
        }
        self.codex_settled(sid);
    }

    fn codex_settled(&self, sid: &str) {
        let mut m = lock(&self.codex_going);
        if let Some(n) = m.get_mut(sid) {
            *n -= 1;
            if *n == 0 {
                m.remove(sid);
            }
        }
        drop(m);
        self.codex_cv.notify_all();
    }

    fn codex_is_going(&self, sid: &str) -> bool {
        lock(&self.codex_going).contains_key(sid)
    }

    /// Waits at most `d` for no terminal of thread `sid` to be going.
    fn wait_codex(&self, sid: &str, d: Duration) {
        let m = lock(&self.codex_going);
        let _ = self
            .codex_cv
            .wait_timeout_while(m, d, |m| m.contains_key(sid));
    }
}

/// Stops Codex terminal `t` off the caller's thread, as `stop_detached`, then lets its thread's reopens go (after a scan).
fn stop_codex(sh: &Arc<Shared>, t: Arc<Term>) {
    t.close_input();
    let Key::Codex(sid) = t.key.clone() else {
        return stop_detached(t);
    };
    let (weak, s) = (Arc::downgrade(sh), sid.clone());
    let spawned = thread::Builder::new()
        .name("term-stop".into())
        .spawn(move || {
            t.pty.stop();
            if let Some(sh) = weak.upgrade() {
                sh.codex_gone(&s);
            }
        });
    if spawned.is_err() {
        // Nothing will announce it gone: reopens fall back to the board's word.
        sh.codex_settled(&sid);
    }
}

/// The terminals of this daemon.
pub struct Terms {
    sh: Arc<Shared>,
}

impl Terms {
    /// `codex_held(session_id)` says a running Codex holds that thread open (the board's open set:
    /// `WatchHandle::codex_held`).
    pub fn new(
        home: PathBuf,
        codex_held: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> io::Result<Terms> {
        Terms::with_options(home, codex_held, Options::default())
    }

    pub fn with_options(
        home: PathBuf,
        codex_held: impl Fn(&str) -> bool + Send + Sync + 'static,
        opts: Options,
    ) -> io::Result<Terms> {
        Terms::with_rescan(home, codex_held, opts, None)
    }

    /// As `with_options`, with the board's process scan to ask for once a Codex terminal has gone (without one, a reopen
    /// waits only for the program to be gone).
    pub fn with_rescan(
        home: PathBuf,
        codex_held: impl Fn(&str) -> bool + Send + Sync + 'static,
        opts: Options,
        rescan: Option<Rescan>,
    ) -> io::Result<Terms> {
        Ok(Terms {
            sh: Arc::new(Shared {
                home,
                codex_held: Box::new(codex_held),
                rescan,
                codex_going: Mutex::new(HashMap::new()),
                codex_cv: Condvar::new(),
                opts,
                st: Mutex::new(State {
                    terms: HashMap::new(),
                    by_key: HashMap::new(),
                    conns: HashMap::new(),
                    warm: VecDeque::new(),
                    resizes: vec![],
                    timer_on: false,
                    next_id: 1,
                    closed: false,
                }),
                alarm: Alarm::new()?,
            }),
        })
    }

    /// Opens terminal `ref` of connection `conn` on `target`, its output to `out`. An error is `term.mjs`'s words for
    /// it, for a `TermEnd`. A `ref` the connection already has open is left as it is (as `term.mjs` does).
    pub fn open(
        &self,
        conn: ConnId,
        r#ref: u32,
        target: AttachTarget,
        cols: u16,
        rows: u16,
        out: TermSink,
    ) -> Result<(), String> {
        if r#ref == 0 {
            return Ok(()); // `term.mjs` takes refs > 0 only
        }
        let size = clamp_size(cols, rows);
        // Its filesystem checks, before the lock (used only past the checks that come first under it).
        let cmd = command(&target, &self.sh.home);
        let mut waited = false;
        let mut st = loop {
            let st = lock(&self.sh.st);
            if let AttachTarget::Codex { session_id, .. } = &target {
                if !waited && self.sh.codex_is_going(session_id) {
                    drop(st);
                    self.sh.wait_codex(session_id, REOPEN_WAIT);
                    waited = true;
                    continue;
                }
            }
            break st;
        };
        if st.conns.contains_key(&(conn, r#ref)) {
            return Ok(());
        }
        if st.conns.keys().filter(|(c, _)| *c == conn).count() >= PER_PAGE {
            return Err(ERR_PER_PAGE.into());
        }
        let cmd = cmd.ok_or(ERR_PICK)?;
        let key = match &target {
            AttachTarget::Claude { profile, job } => Key::Claude(profile.clone(), job.clone()),
            AttachTarget::Codex { session_id, .. } => Key::Codex(session_id.clone()),
            AttachTarget::Trust { .. } => Key::Solo(st.next_id),
        };
        if let Key::Claude(..) = key {
            if let Some(t) = st.by_key.get(&key).and_then(|id| st.terms.get(id)).cloned() {
                self.join(&mut st, t, conn, r#ref, out, size);
                return Ok(());
            }
        }
        let live = st.live();
        let can_evict = st
            .warm
            .iter()
            .any(|(id, _)| st.terms.get(id).is_some_and(|t| !lock(&t.st).ended));
        if live >= MAX && !can_evict {
            return Err(ERR_TOO_MANY.into());
        }
        if let Key::Codex(sid) = &key {
            if st.by_key.contains_key(&key) {
                return Err(ERR_CODEX_OPEN.into());
            }
            if (self.sh.codex_held)(sid) {
                return Err(ERR_CODEX_BUSY.into());
            }
        }
        let pty = Pty::spawn(&cmd, size.0, size.1).map_err(|e| e.to_string())?;
        let reader = pty.reader().map_err(|e| e.to_string())?;
        // Only now that the new one runs: a failed spawn evicts nothing (MAX + 1 live for a moment).
        if live >= MAX {
            st.evict_oldest_live();
        }
        let id = st.next_id;
        st.next_id += 1;
        let t = Arc::new(Term {
            id,
            key: key.clone(),
            pty,
            going: AtomicBool::new(false),
            st: Mutex::new(TermSt {
                refs: vec![Attached {
                    conn,
                    r#ref,
                    sink: out,
                }],
                ring: VecDeque::new(),
                ended: false,
            }),
            size: Mutex::new(SizeSt {
                cur: size,
                pending: None,
                last: None,
                armed: false,
            }),
            input: Mutex::new(InputQ {
                buf: VecDeque::new(),
                closed: false,
            }),
            input_cv: Condvar::new(),
        });
        let (rt, wt, sh) = (t.clone(), t.clone(), Arc::downgrade(&self.sh));
        let spawned = thread::Builder::new()
            .name(format!("term-{id}-out"))
            .spawn(move || read_loop(rt, File::from(reader), sh))
            .and_then(|_| {
                thread::Builder::new()
                    .name(format!("term-{id}-in"))
                    .spawn(move || write_loop(wt))
            });
        if let Err(e) = spawned {
            stop_detached(t);
            return Err(e.to_string());
        }
        st.terms.insert(id, t);
        st.by_key.insert(key, id);
        st.conns.insert((conn, r#ref), id);
        Ok(())
    }

    /// A newcomer on an existing pty (warm or in use): the ring, then a redraw at its size. One whose program has ended
    /// gets the ring and then `TermEnd`, and the pty is gone (the next open starts a fresh one).
    fn join(
        &self,
        st: &mut State,
        t: Arc<Term>,
        conn: ConnId,
        r#ref: u32,
        out: TermSink,
        size: (u16, u16),
    ) {
        st.warm.retain(|(w, _)| *w != t.id);
        let mut ts = lock(&t.st);
        Term::replay(&ts, r#ref, &out);
        if ts.ended {
            out.push(Frame::TermEnd {
                r#ref,
                error: Some(ERR_ENDED.into()),
            });
            drop(ts);
            st.remove(t.id);
            return;
        }
        ts.refs.push(Attached {
            conn,
            r#ref,
            sink: out,
        });
        drop(ts);
        st.conns.insert((conn, r#ref), t.id);
        t.kick(size);
    }

    fn term(&self, conn: ConnId, r#ref: u32) -> Option<Arc<Term>> {
        let st = lock(&self.sh.st);
        st.conns
            .get(&(conn, r#ref))
            .and_then(|id| st.terms.get(id))
            .cloned()
    }

    /// Keystrokes for terminal `ref` (ignored if it is not open). Never blocks: a writer thread per pty feeds it.
    /// `Err(InputFull)`: refused whole, the terminal already holds `INPUT_MAX` its program has not read; stop reading
    /// this connection and send them again once `input_pending` has gone down.
    pub fn input(&self, conn: ConnId, r#ref: u32, data: &[u8]) -> Result<(), InputFull> {
        match self.term(conn, r#ref) {
            Some(t) => t.push_input(data),
            None => Ok(()),
        }
    }

    /// Input queued and not yet taken by the programs, over the terminals connection `conn` has open (the most of any
    /// one of them: that is the one `InputFull` is about).
    pub fn input_pending(&self, conn: ConnId) -> usize {
        let terms: Vec<Arc<Term>> = {
            let st = lock(&self.sh.st);
            st.conns
                .iter()
                .filter(|((c, _), _)| *c == conn)
                .filter_map(|(_, id)| st.terms.get(id).cloned())
                .collect()
        };
        terms.iter().map(|t| t.input_pending()).max().unwrap_or(0)
    }

    /// Terminal `ref`'s window is now `cols` x `rows` (coalesced: at most one size per 16 ms reaches the program, the
    /// latest).
    pub fn size(&self, conn: ConnId, r#ref: u32, cols: u16, rows: u16) {
        let Some(t) = self.term(conn, r#ref) else {
            return;
        };
        if let Some(due) = t.request_size(clamp_size(cols, rows)) {
            let mut st = lock(&self.sh.st);
            st.resizes.push((due, t));
            self.arm(&mut st);
        }
    }

    /// Releases terminal `ref` (it gets its `TermEnd`). The attach ends; the session never does. The last ref of a
    /// `claude attach` pty sends it to the warm pool; any other pty is stopped.
    pub fn close(&self, conn: ConnId, r#ref: u32) {
        let mut st = lock(&self.sh.st);
        self.release(&mut st, conn, r#ref, true);
    }

    /// Connection `conn` has gone: all its terminals are released.
    pub fn drop_conn(&self, conn: ConnId) {
        let mut st = lock(&self.sh.st);
        let refs: Vec<u32> = st
            .conns
            .keys()
            .filter(|(c, _)| *c == conn)
            .map(|(_, r)| *r)
            .collect();
        for r in refs {
            self.release(&mut st, conn, r, false);
        }
    }

    fn release(&self, st: &mut State, conn: ConnId, r#ref: u32, tell: bool) {
        let Some(id) = st.conns.remove(&(conn, r#ref)) else {
            return;
        };
        let Some(t) = st.terms.get(&id).cloned() else {
            return;
        };
        let (empty, ended) = {
            let mut ts = lock(&t.st);
            if let Some(i) = ts
                .refs
                .iter()
                .position(|a| a.conn == conn && a.r#ref == r#ref)
            {
                let a = ts.refs.remove(i);
                if tell {
                    a.sink.push(Frame::TermEnd { r#ref, error: None });
                }
            }
            (ts.refs.is_empty(), ts.ended)
        };
        if !empty {
            return;
        }
        if ended {
            st.remove(id);
            return;
        }
        match t.key {
            Key::Claude(..) => {
                st.warm.push_back((id, Instant::now()));
                while st.warm.len() > WARM_MAX {
                    let Some((old, _)) = st.warm.pop_front() else {
                        break;
                    };
                    if let Some(o) = st.remove(old) {
                        stop_detached(o);
                    }
                }
                self.arm(st);
            }
            _ => {
                if let Some(t) = st.remove(id) {
                    if self.sh.codex_going(&t) {
                        stop_codex(&self.sh, t);
                    } else {
                        stop_detached(t);
                    }
                }
            }
        }
    }

    /// Makes sure the timer runs (it exits by itself when there is nothing to time).
    fn arm(&self, st: &mut State) {
        if st.timer_on {
            self.sh.alarm.poke();
            return;
        }
        st.timer_on = true;
        let sh = Arc::downgrade(&self.sh);
        if thread::Builder::new()
            .name("terms-timer".into())
            .spawn(move || timer(sh))
            .is_err()
        {
            st.timer_on = false;
        }
    }

    /// Ptys whose program runs (attached or warm).
    pub fn live_count(&self) -> usize {
        lock(&self.sh.st).live()
    }

    /// Terminals in the warm pool (an ended one waiting to tell its next opener included).
    pub fn warm_count(&self) -> usize {
        lock(&self.sh.st).warm.len()
    }

    /// The timer thread exists (only while something is warm or a size is pending).
    pub fn timer_armed(&self) -> bool {
        lock(&self.sh.st).timer_on
    }
}

impl Drop for Terms {
    /// Every pty is stopped (off this thread), and the timer leaves.
    fn drop(&mut self) {
        let mut st = lock(&self.sh.st);
        st.closed = true;
        let ids: Vec<u64> = st.terms.keys().copied().collect();
        for id in ids {
            if let Some(t) = st.remove(id) {
                // A reader parked on a full sink wakes, finds nothing attached, and reads on to the hang-up.
                for a in std::mem::take(&mut lock(&t.st).refs) {
                    a.sink.close();
                }
                stop_detached(t);
            }
        }
        st.resizes.clear();
        drop(st);
        self.sh.alarm.poke();
    }
}

/// Warm expiry and size trailing edges, until there is neither.
fn timer(sh: Weak<Shared>) {
    loop {
        let Some(sh) = sh.upgrade() else { return };
        let now = Instant::now();
        let (expired, due, next) = {
            let mut st = lock(&sh.st);
            if st.closed {
                st.timer_on = false;
                return;
            }
            let mut expired = vec![];
            while let Some(&(id, at)) = st.warm.front() {
                if at + sh.opts.warm_ttl > now {
                    break;
                }
                st.warm.pop_front();
                if let Some(t) = st.remove(id) {
                    expired.push(t);
                }
            }
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut st.resizes)
                .into_iter()
                .partition(|(at, _)| *at <= now);
            st.resizes = later;
            let next = st
                .warm
                .front()
                .map(|(_, at)| *at + sh.opts.warm_ttl)
                .into_iter()
                .chain(st.resizes.iter().map(|(at, _)| *at))
                .min();
            if next.is_none() {
                st.timer_on = false;
            }
            (expired, due, next)
        };
        for t in expired {
            if !lock(&t.st).ended {
                stop_detached(t);
            }
        }
        for (_, t) in due {
            t.apply_pending();
        }
        let Some(next) = next else { return };
        sh.alarm
            .wait(Some(next.saturating_duration_since(Instant::now())));
    }
}

/// The pty's output, into its ring and every attached sink, until the program has gone. Pauses while a sink is full.
fn read_loop(t: Arc<Term>, mut f: File, sh: Weak<Shared>) {
    let mut buf = vec![0u8; CHUNK];
    let mut paused = false;
    loop {
        // Backpressure: stop reading while any sink is over the high mark, until all are down to the low one.
        loop {
            let sinks: Vec<TermSink> = lock(&t.st).refs.iter().map(|a| a.sink.clone()).collect();
            if !paused {
                paused = sinks.iter().any(|s| s.bytes() > SINK_HIGH);
            }
            if !paused {
                break;
            }
            match sinks.iter().find(|s| s.bytes() > SINK_LOW) {
                // Re-looked at every 100 ms: the sink may have been detached meanwhile.
                Some(s) => s.wait_drained(Duration::from_millis(100)),
                None => {
                    paused = false;
                    break;
                }
            }
        }
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO: the program's side has closed
        };
        let data = &buf[..n];
        let mut ts = lock(&t.st);
        ts.ring.extend(data);
        let over = ts.ring.len().saturating_sub(RING);
        ts.ring.drain(..over);
        for a in &ts.refs {
            a.sink.push(Frame::TermOut {
                r#ref: a.r#ref,
                data: data.to_vec(),
            });
        }
    }
    // A Codex that left by itself: the scan must see it gone too (unless its release already said so). Marked before
    // the ends go out, so a reopen they prompt waits for that scan.
    let rescan = sh
        .upgrade()
        .filter(|sh| !lock(&sh.st).closed && sh.codex_going(&t));
    // The program has gone: every attached ref has had its last bytes, now its end.
    let had_refs = {
        let mut ts = lock(&t.st);
        ts.ended = true;
        for a in &ts.refs {
            a.sink.push(Frame::TermEnd {
                r#ref: a.r#ref,
                error: None,
            });
        }
        !std::mem::take(&mut ts.refs).is_empty()
    };
    t.close_input();
    if let Some(sh) = sh.upgrade() {
        let mut st = lock(&sh.st);
        let warm = st.warm.iter().any(|(id, _)| *id == t.id);
        let current = st.terms.get(&t.id).is_some_and(|x| Arc::ptr_eq(x, &t));
        // A warm one stays, ended, to tell its next opener; any other goes now.
        if current && (had_refs || !warm) {
            st.remove(t.id);
        }
    }
    t.pty.stop(); // reaps it, and hangs up on whatever of its group is left
    if let (Some(sh), Key::Codex(sid)) = (rescan, &t.key) {
        sh.codex_gone(sid);
    }
}

/// Input to the program, from its queue. A program that does not read blocks only this thread.
fn write_loop(t: Arc<Term>) {
    loop {
        // A chunk is copied out and stays queued until it is written, so `input_pending` never undercounts.
        let data: Vec<u8> = {
            let q = lock(&t.input);
            let q = t
                .input_cv
                .wait_while(q, |q| q.buf.is_empty() && !q.closed)
                .unwrap_or_else(|e| e.into_inner());
            if q.closed {
                return;
            }
            q.buf.range(..q.buf.len().min(CHUNK)).copied().collect()
        };
        if t.pty.write(&data).is_err() {
            return;
        }
        lock(&t.input).buf.drain(..data.len());
    }
}
