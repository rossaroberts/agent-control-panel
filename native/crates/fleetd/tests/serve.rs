//! The socket over a bare `Store` (no watcher): what a client is sent on Hello, and after.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fleet_model::types::{Delta, Engine, Session, SessionKey, State};
use fleet_model::wire::{read_frame, write_frame, Frame, Role, MAX_FRAME, PROTO};
use fleetd::client::Client;
use fleetd::serve::{bind, serve};
use fleetd::store::Store;
use tokio::task::JoinHandle;

fn sess(n: usize, doing: &str) -> Session {
    Session {
        key: SessionKey {
            machine: "fixture-mac".into(),
            profile: "default".into(),
            session_id: format!("sess-{n:04}"),
        },
        engine: Engine::Claude,
        name: Some(format!("fixture {n}")),
        cwd: Some("/tmp/fleetd-fixture".into()),
        state: State::Working,
        reason: "working".into(),
        live: true,
        pid: Some(4242),
        stage: None,
        model: None,
        effort: None,
        context_tokens: None,
        last_at: None,
        said: None,
        doing: Some(doing.into()),
        last_ask: None,
        first_ask: None,
        started_at: None,
        hook_at: None,
        written_at: None,
        job_id: None,
        title: None,
        epic: None,
        archived: false,
        account: None,
    }
}

struct Server {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    task: JoinHandle<()>,
    _guard: fleetd::serve::SocketGuard,
}

impl Server {
    fn start(n: usize) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleetd.sock");
        let store = Arc::new(Store::new());
        if n > 0 {
            store.upsert((0..n).map(|i| sess(i, "start")).collect());
        }
        let (listener, _guard) = bind(&path).unwrap();
        let terms =
            Arc::new(fleetd::terms::Terms::new(dir.path().to_path_buf(), |_| false).unwrap());
        let task = tokio::spawn(serve(listener, store.clone(), "fixture-mac".into(), terms));
        Server {
            _dir: dir,
            path,
            store,
            task,
            _guard,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The next frame that is not a `Meta` (those have their own tests below: every connection is sent one after its first
/// frame).
async fn next(c: &mut Client) -> Frame {
    loop {
        match next_any(c).await {
            Frame::Meta(_) => continue,
            f => return f,
        }
    }
}

async fn next_any(c: &mut Client) -> Frame {
    tokio::time::timeout(Duration::from_secs(5), c.next())
        .await
        .expect("a frame in time")
        .unwrap()
}

async fn nothing(c: &mut Client) {
    let quiet = async {
        loop {
            if !matches!(c.next().await, Ok(Frame::Meta(_))) {
                return;
            }
        }
    };
    assert!(tokio::time::timeout(Duration::from_millis(200), quiet)
        .await
        .is_err());
}

#[tokio::test]
async fn hello_without_since_gets_a_snapshot() {
    let s = Server::start(3);
    let mut c = Client::connect(&s.path, None).await.unwrap();
    match next(&mut c).await {
        Frame::Snapshot {
            run,
            seq,
            machine,
            sessions,
        } => {
            assert_eq!(run, s.store.run());
            assert_eq!(seq, 1);
            assert_eq!(machine, "fixture-mac");
            assert_eq!(sessions.len(), 3);
        }
        f => panic!("{f:?}"),
    }
}

#[tokio::test]
async fn hello_with_since_gets_only_what_came_after() {
    let s = Server::start(2);
    s.store.upsert(vec![sess(0, "second")]); // seq 2
    s.store.upsert(vec![sess(1, "third")]); // seq 3
    let run = s.store.run();
    let mut c = Client::connect(&s.path, Some((run, 2))).await.unwrap();
    match next(&mut c).await {
        Frame::Delta(Delta {
            seq,
            upsert,
            remove,
        }) => {
            assert_eq!(seq, 3);
            assert_eq!(upsert.len(), 1);
            assert_eq!(upsert[0].key.session_id, "sess-0001");
            assert!(remove.is_empty());
        }
        f => panic!("{f:?}"),
    }
    // Fully current: an empty delta at once as the acknowledgement (R18), then the next change when it comes.
    let mut cur = Client::connect(&s.path, Some((run, 3))).await.unwrap();
    assert_eq!(
        next(&mut cur).await,
        Frame::Delta(Delta {
            seq: 3,
            upsert: vec![],
            remove: vec![]
        })
    );
    nothing(&mut cur).await;
    s.store.upsert(vec![sess(0, "fourth")]);
    match next(&mut cur).await {
        Frame::Delta(d) => assert_eq!(d.seq, 4),
        f => panic!("{f:?}"),
    }
    // A since this board never reached (another run) gets a snapshot.
    let mut ahead = Client::connect(&s.path, Some((run, 99))).await.unwrap();
    assert!(matches!(
        next(&mut ahead).await,
        Frame::Snapshot { seq: 4, .. }
    ));
}

/// Ruling R24: a `seq` counts within one daemon run. A Hello from another run (a restarted daemon starts at 0 again),
/// or with no run, gets the whole board, never a delta applied over a board of the old run, nor a "caught up".
#[tokio::test]
async fn since_from_another_run_gets_a_snapshot() {
    let s = Server::start(2);
    s.store.upsert(vec![sess(0, "second")]); // seq 2
    let other = s.store.run() ^ 1;
    for resume in [Some((other, 1)), Some((other, 2))] {
        let mut c = Client::connect(&s.path, resume).await.unwrap();
        match next(&mut c).await {
            Frame::Snapshot { run, seq, .. } => assert_eq!((run, seq), (s.store.run(), 2)),
            f => panic!("{resume:?}: {f:?}"),
        }
    }
    // A Hello with a since and no run: also the board.
    let mut st = tokio::net::UnixStream::connect(&s.path).await.unwrap();
    let hello = Frame::Hello {
        proto: PROTO,
        since: Some(2),
        run: None,
        role: Role::Board,
    };
    write_frame(&mut st, &hello).await.unwrap();
    assert!(matches!(
        read_frame(&mut st, MAX_FRAME).await.unwrap(),
        Frame::Snapshot { seq: 2, .. }
    ));
}

/// A client of proto 1 (whose Hello has no `run`, so does not decode as proto 2's) is told the version, by name.
#[tokio::test]
async fn a_proto_1_hello_gets_an_error_naming_proto_4() {
    use tokio::io::AsyncWriteExt;
    let s = Server::start(1);
    let mut st = tokio::net::UnixStream::connect(&s.path).await.unwrap();
    // Proto 1's Hello { proto: 1, since: Some(5) }, length-prefixed.
    st.write_all(&[4, 0, 0, 0, 0x00, 0x01, 0x01, 0x05])
        .await
        .unwrap();
    match read_frame(&mut st, MAX_FRAME).await.unwrap() {
        Frame::Error { message } => {
            assert_eq!(message, "protocol 1 not supported; this is proto 4")
        }
        f => panic!("{f:?}"),
    }
}

#[tokio::test]
async fn wrong_proto_gets_error_and_a_closed_connection() {
    let s = Server::start(1);
    let mut c = Client::connect_proto(&s.path, PROTO + 1, None)
        .await
        .unwrap();
    assert!(matches!(next(&mut c).await, Frame::Error { .. }));
    let e = tokio::time::timeout(Duration::from_secs(5), c.next())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn a_first_frame_that_is_not_hello_is_refused() {
    let s = Server::start(1);
    let mut st = tokio::net::UnixStream::connect(&s.path).await.unwrap();
    write_frame(
        &mut st,
        &Frame::Error {
            message: "x".into(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_frame(&mut st, MAX_FRAME).await.unwrap(),
        Frame::Error { .. }
    ));
}

#[tokio::test]
async fn socket_mode_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let s = Server::start(0);
    let mode = std::fs::metadata(&s.path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn a_live_socket_is_not_taken_over_but_a_dead_one_is() {
    let s = Server::start(0);
    assert_eq!(
        bind(&s.path).err().unwrap().kind(),
        std::io::ErrorKind::AddrInUse
    );
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("old.sock");
    drop(std::os::unix::net::UnixListener::bind(&p).unwrap()); // leaves the file, nobody listening
    assert!(p.exists());
    let (_l, _g) = bind(&p).unwrap();
}

#[tokio::test]
async fn a_regular_file_at_the_path_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("fleetd.sock");
    std::fs::write(&p, "precious").unwrap();
    let e = bind(&p).err().unwrap();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "precious");
}

#[tokio::test]
async fn the_lock_stops_a_second_bind_and_dropping_the_guard_removes_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("fleetd.sock");
    let (_l, g) = bind(&p).unwrap();
    assert_eq!(
        bind(&p).err().unwrap().kind(),
        std::io::ErrorKind::AddrInUse
    );
    assert!(p.exists());
    drop(g);
    assert!(!p.exists());
}

#[tokio::test]
async fn an_oversized_length_is_refused_before_it_is_allocated() {
    let mut huge: &[u8] = &[0xff, 0xff, 0xff, 0x7f];
    let e = read_frame(&mut huge, MAX_FRAME).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    // Within the global cap but over this reader's: also refused (the server's Hello limit).
    let mut big: &[u8] = &[0x00, 0x20, 0x00, 0x00];
    let e = read_frame(&mut big, 4096).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn a_truncated_frame_is_an_eof_not_a_hang_or_a_big_allocation() {
    // Claims 60 MiB, delivers 3 bytes, then the peer closes.
    let mut cut: &[u8] = &[0x00, 0x00, 0x00, 0x03, 1, 2, 3];
    let e = read_frame(&mut cut, MAX_FRAME).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    let mut cut: &[u8] = &[0x00, 0x00, 0x00, 0x04, 1, 2, 3];
    let e = read_frame(&mut cut, MAX_FRAME).await.unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
}

#[tokio::test]
async fn an_oversized_hello_is_dropped_by_the_server() {
    let s = Server::start(0);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut st = tokio::net::UnixStream::connect(&s.path).await.unwrap();
    st.write_all(&(1_000_000u32).to_le_bytes()).await.unwrap();
    let mut b = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(5), st.read(&mut b))
        .await
        .unwrap();
    assert!(matches!(n, Ok(0) | Err(_)));
}

#[tokio::test]
async fn two_clients_get_the_same_delta() {
    let s = Server::start(1);
    let mut a = Client::connect(&s.path, None).await.unwrap();
    let mut b = Client::connect(&s.path, None).await.unwrap();
    assert!(matches!(next(&mut a).await, Frame::Snapshot { .. }));
    assert!(matches!(next(&mut b).await, Frame::Snapshot { .. }));
    s.store.upsert(vec![sess(0, "changed"), sess(5, "new")]);
    let (da, db) = (next(&mut a).await, next(&mut b).await);
    assert_eq!(da, db);
    match da {
        Frame::Delta(d) => {
            assert_eq!(d.seq, 2);
            assert_eq!(d.upsert.len(), 2);
        }
        f => panic!("{f:?}"),
    }
}

#[tokio::test]
async fn a_client_that_falls_behind_is_sent_a_fresh_snapshot() {
    let s = Server::start(1);
    // Connects, then reads nothing while the store runs far past the ring and the socket buffers.
    let mut slow = Client::connect(&s.path, None).await.unwrap();
    let mut fast = Client::connect(&s.path, None).await.unwrap();
    assert!(matches!(next(&mut fast).await, Frame::Snapshot { .. }));
    let big = "x".repeat(4000);
    let writes = fleetd::store::RING + 200;
    let t = std::time::Instant::now();
    let reader = tokio::spawn(async move {
        // The fast client keeps up the whole time.
        let mut seq = 1;
        while seq < (writes as u64 + 1) {
            match fast.next().await.unwrap() {
                Frame::Delta(d) => seq = d.seq,
                Frame::Snapshot { seq: s, .. } => seq = s,
                Frame::Meta(_) => {}
                f => panic!("{f:?}"),
            }
        }
    });
    for i in 0..writes {
        s.store.upsert(vec![sess(0, &format!("{i}{big}"))]);
        if i % 64 == 0 {
            tokio::task::yield_now().await;
        }
    }
    tokio::time::timeout(Duration::from_secs(20), reader)
        .await
        .expect("the fast client is not held up by the slow one")
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(20));
    // The slow one now reads: whatever it is sent ends at the board's seq, and a snapshot is among it or it caught up.
    let mut last = 0;
    let mut saw_snapshot = false;
    while last < s.store.seq() {
        match next(&mut slow).await {
            Frame::Snapshot { seq, sessions, .. } => {
                saw_snapshot = true;
                last = seq;
                assert_eq!(sessions.len(), 1);
            }
            Frame::Delta(d) => {
                assert!(d.seq > last);
                last = d.seq;
            }
            f => panic!("{f:?}"),
        }
    }
    assert!(
        saw_snapshot,
        "a client {writes} deltas behind gets a snapshot"
    );
}

/// A proto-2 Hello (no `role`) is told, by name, which protocol this daemon speaks.
#[tokio::test]
async fn a_proto_2_hello_gets_an_error_naming_proto_4() {
    use tokio::io::AsyncWriteExt;
    let s = Server::start(1);
    let mut st = tokio::net::UnixStream::connect(&s.path).await.unwrap();
    // Proto 2's Hello { proto: 2, since: Some(5), run: None }, length-prefixed.
    st.write_all(&[5, 0, 0, 0, 0x00, 0x02, 0x01, 0x05, 0x00])
        .await
        .unwrap();
    match read_frame(&mut st, MAX_FRAME).await.unwrap() {
        Frame::Error { message } => assert!(message.contains("proto 4"), "{message}"),
        f => panic!("{f:?}"),
    }
}

/// A Hello of this version whose bytes are wrong is a malformed Hello, not an unsupported version.
#[tokio::test]
async fn a_malformed_current_proto_hello_says_so() {
    use tokio::io::AsyncWriteExt;
    let s = Server::start(1);
    let mut st = tokio::net::UnixStream::connect(&s.path).await.unwrap();
    // variant 0, proto 4, then nothing
    st.write_all(&[2, 0, 0, 0, 0x00, 0x04]).await.unwrap();
    match read_frame(&mut st, MAX_FRAME).await.unwrap() {
        Frame::Error { message } => assert_eq!(message, "malformed Hello"),
        f => panic!("{f:?}"),
    }
}

/// A proto-3 Hello (before `title`, `epic`, `archived`, `account` and `Meta`) is told, by name, which protocol this is.
#[tokio::test]
async fn a_proto_3_hello_gets_an_error_naming_proto_4() {
    let s = Server::start(1);
    let mut st = tokio::net::UnixStream::connect(&s.path).await.unwrap();
    // Proto 3's Hello { proto: 3, since: None, run: None, role: Board }, length-prefixed.
    tokio::io::AsyncWriteExt::write_all(&mut st, &[5, 0, 0, 0, 0x00, 0x03, 0x00, 0x00, 0x00])
        .await
        .unwrap();
    match read_frame(&mut st, MAX_FRAME).await.unwrap() {
        Frame::Error { message } => {
            assert_eq!(message, "protocol 3 not supported; this is proto 4")
        }
        f => panic!("{f:?}"),
    }
}

/// A board connection is sent the groups and accounts right after its first frame (a snapshot, or the caught-up
/// acknowledgement), the current ones, health kept; and again whenever they change, and not when they do not.
#[tokio::test]
async fn a_board_connection_is_sent_the_meta_after_its_first_frame_and_after_each_change() {
    use fleet_model::types::{Account, Health, Limit, Meta};
    let s = Server::start(2);
    let account = Account {
        name: "work".into(),
        engine: Engine::Claude,
        profile: "default".into(),
        limits: vec![Limit {
            window: "5h".into(),
            used_pct: 40,
            resets_at: Some(1_793_970_000_000),
        }],
    };
    let meta = Meta {
        epics: vec!["Fixtures".into()],
        groups: vec!["Experiments".into()],
        accounts: vec![account],
        health: None,
    };
    assert!(s.store.set_board_meta(meta.clone()));
    assert!(!s.store.set_board_meta(meta.clone()), "the same: no change");

    let mut c = Client::connect(&s.path, None).await.unwrap();
    assert!(matches!(next_any(&mut c).await, Frame::Snapshot { .. }));
    assert_eq!(next_any(&mut c).await, Frame::Meta(meta.clone()));
    nothing(&mut c).await;

    // A change is sent at once; the health the probe set is kept when the board's part is set again.
    let health = Health {
        cpu_pct: 5,
        mem_pct: 50,
        disk_pct: 70,
        dev: 2,
        db: 1,
    };
    assert!(s.store.modify_meta(|m| {
        m.health = Some(health);
        true
    }));
    let want = Meta {
        health: Some(health),
        ..meta.clone()
    };
    assert_eq!(next_any(&mut c).await, Frame::Meta(want.clone()));
    let next_meta = Meta {
        groups: vec![],
        ..meta
    };
    assert!(s.store.set_board_meta(next_meta.clone()));
    assert_eq!(
        next_any(&mut c).await,
        Frame::Meta(Meta {
            health: Some(health),
            ..next_meta
        })
    );

    // A client resuming with a current since is sent the caught-up acknowledgement, then the Meta.
    let run = s.store.run();
    let mut cur = Client::connect(&s.path, Some((run, 1))).await.unwrap();
    assert!(matches!(next_any(&mut cur).await, Frame::Delta(_)));
    assert!(matches!(next_any(&mut cur).await, Frame::Meta(_)));
}
