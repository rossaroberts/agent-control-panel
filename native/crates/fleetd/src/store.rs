//! The board: one truthful `Session` per session, and the numbered deltas that take a reader from any recent `seq` to now.
//! Writers hand in what they re-read; only sessions whose serialized form changed make a delta, and a write that changes
//! nothing makes none. Every call holds one short lock and never waits on I/O, so readers never hold up the watcher.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

pub use fleet_model::types::Delta;
use fleet_model::types::{Session, SessionKey};
use tokio::sync::broadcast;

/// How many deltas `since` can replay; older readers get a snapshot.
pub const RING: usize = 4096;

type Key = (String, String, String);

fn key_of(k: &SessionKey) -> Key {
    (k.machine.clone(), k.profile.clone(), k.session_id.clone())
}

struct Inner {
    seq: u64,
    /// Each session, with the bytes it serializes to (what "changed" is decided on).
    sessions: BTreeMap<Key, (Session, Vec<u8>)>,
    /// The last `RING` deltas, oldest first, `seq`s consecutive.
    ring: VecDeque<Delta>,
}

pub struct Store {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Delta>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

fn bytes(s: &Session) -> Vec<u8> {
    serde_json::to_vec(s).expect("a Session serializes")
}

impl Store {
    pub fn new() -> Store {
        Store {
            inner: Mutex::new(Inner {
                seq: 0,
                sessions: BTreeMap::new(),
                ring: VecDeque::with_capacity(RING),
            }),
            tx: broadcast::channel(RING).0,
        }
    }

    /// Every session, ordered by key, and the `seq` of the last delta they include.
    pub fn snapshot(&self) -> (u64, Vec<Session>) {
        let i = self.inner.lock().unwrap();
        (i.seq, i.sessions.values().map(|(s, _)| s.clone()).collect())
    }

    /// One delta from `seq` to now: each session changed since, as it is now, and each one gone since. `None` when the
    /// ring no longer reaches back to `seq` (or `seq` is ahead of this board, from another run): send a snapshot.
    pub fn since(&self, seq: u64) -> Option<Delta> {
        let i = self.inner.lock().unwrap();
        if seq > i.seq {
            return None;
        }
        let first = i.ring.front().map_or(i.seq + 1, |d| d.seq);
        if seq + 1 < first {
            return None;
        }
        // Newest word per session: an upsert or a removal.
        let mut last: BTreeMap<Key, Option<&Session>> = BTreeMap::new();
        for d in i.ring.iter().filter(|d| d.seq > seq) {
            for s in &d.upsert {
                last.insert(key_of(&s.key), Some(s));
            }
            for k in &d.remove {
                last.insert(key_of(k), None);
            }
        }
        let mut out = Delta {
            seq: i.seq,
            ..Delta::default()
        };
        for (k, s) in last {
            match s {
                Some(s) => out.upsert.push(s.clone()),
                None => out.remove.push(SessionKey {
                    machine: k.0,
                    profile: k.1,
                    session_id: k.2,
                }),
            }
        }
        Some(out)
    }

    /// Every delta from now on, in `seq` order. A reader that falls more than `RING` behind gets `Lagged` and should
    /// resync with `since` or a snapshot.
    pub fn subscribe(&self) -> broadcast::Receiver<Delta> {
        self.tx.subscribe()
    }

    /// The current `seq`.
    pub fn seq(&self) -> u64 {
        self.inner.lock().unwrap().seq
    }

    /// Sessions as re-read: the ones that changed make one delta. Its `seq`, or `None` when nothing changed.
    pub fn upsert(&self, sessions: Vec<Session>) -> Option<u64> {
        self.write(sessions, |_: &Session| false)
    }

    /// Everything within a scope, as re-read: `sessions` is now the whole of it, so a session in scope and not among them
    /// is removed. One delta, or `None` when nothing changed.
    pub fn replace(
        &self,
        in_scope: impl Fn(&Session) -> bool,
        sessions: Vec<Session>,
    ) -> Option<u64> {
        self.write(sessions, in_scope)
    }

    fn write(&self, sessions: Vec<Session>, in_scope: impl Fn(&Session) -> bool) -> Option<u64> {
        let mut i = self.inner.lock().unwrap();
        let mut delta = Delta::default();
        let mut seen = std::collections::BTreeSet::new();
        for s in sessions {
            let k = key_of(&s.key);
            let b = bytes(&s);
            seen.insert(k.clone());
            if i.sessions.get(&k).is_some_and(|(_, old)| *old == b) {
                continue;
            }
            delta.upsert.push(s.clone());
            i.sessions.insert(k, (s, b));
        }
        let gone: Vec<Key> = i
            .sessions
            .iter()
            .filter(|(k, (s, _))| !seen.contains(*k) && in_scope(s))
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            if let Some((s, _)) = i.sessions.remove(&k) {
                delta.remove.push(s.key);
            }
        }
        if delta.upsert.is_empty() && delta.remove.is_empty() {
            return None;
        }
        i.seq += 1;
        delta.seq = i.seq;
        if i.ring.len() == RING {
            i.ring.pop_front();
        }
        i.ring.push_back(delta.clone());
        // No receiver is not an error: the board stays current for the next one.
        let _ = self.tx.send(delta);
        Some(i.seq)
    }
}
