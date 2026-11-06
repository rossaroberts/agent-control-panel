//! The TCP listener of a real `fleetd serve` (Rulings P5-R2, P5-R3): off unless asked, loopback needs the key on macOS
//! (and is uid-checked on Linux), a refused peer gets one `Error` and is never read, and the Tailscale address is bound
//! only when it is a tailnet address on a local interface. Each test starts its own daemon, over its own temp home and
//! socket, and stops it by its own pid.

mod common;
mod terms_home;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::stub_tailscale::StubTailscale;
use fleet_model::types::{AttachTarget, Session};
use fleet_model::wire::{decode, encode, Frame, Role, PROTO};
use fleetd::client::{term_connect_tcp, Client};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_fleetd");

/// A `fleetd serve` of this test's, with its stderr collected line by line.
struct Daemon {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    sock: PathBuf,
    cfg_dir: PathBuf,
    _sock_dir: tempfile::TempDir,
}

impl Daemon {
    /// Starts `fleetd serve <args>` over `home`, with the config (absent unless the test writes one) in `home/cfg`, and
    /// waits until it says it is serving.
    fn start(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Daemon {
        let sock_dir = tempfile::tempdir().unwrap();
        let sock = sock_dir.path().join("fleetd.sock");
        let cfg_dir = home.join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        let mut c = Command::new(BIN);
        c.arg("serve")
            .args(args)
            .arg("--config")
            .arg(cfg_dir.join("fleetd.json"))
            .env("FLEETD_HOME", home)
            .env("HOME", home)
            .env("FLEETD_MACHINE", "fixture-mac")
            .env("FLEETD_SOCK", &sock)
            .env("FLEETD_PANEL_DIR", home.join("panel"))
            .env("FLEETD_ROSTER", "cli")
            .env(
                "PATH",
                format!("{}:{path}", home.join(".local/bin").display()),
            )
            // Never the real CLI: a test that wants one passes its stub.
            .env("FLEETD_TAILSCALE", home.join("no-tailscale-here"))
            .env_remove("FLEETD_TEST_TS_RANGE_ANY")
            .env_remove("FLEETD_TEST_TS_RETRY_MS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let err = child.stderr.take().unwrap();
        let l2 = lines.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                l2.lock().unwrap().push(l);
            }
        });
        let d = Daemon {
            child,
            lines,
            sock,
            cfg_dir,
            _sock_dir: sock_dir,
        };
        d.wait_line(Duration::from_secs(30), |l| {
            l.starts_with("fleetd: serving")
        })
        .unwrap_or_else(|| panic!("the daemon did not start: {:?}", d.lines()));
        d
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// The first line (since the start) that `f` takes, waiting up to `within`.
    fn wait_line(&self, within: Duration, f: impl Fn(&str) -> bool) -> Option<String> {
        let end = Instant::now() + within;
        loop {
            if let Some(l) = self.lines().into_iter().find(|l| f(l)) {
                return Some(l);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The addresses it said it listens on (`fleetd: listening tcp <addr>`), in order.
    fn listening(&self) -> Vec<String> {
        self.lines()
            .iter()
            .filter_map(|l| l.strip_prefix("fleetd: listening tcp "))
            .map(|a| a.split(' ').next().unwrap().to_string())
            .collect()
    }

    /// The loopback listener's address, once it is bound.
    fn loopback(&self) -> String {
        self.wait_line(Duration::from_secs(10), |l| {
            l.starts_with("fleetd: listening tcp 127.0.0.1:")
        })
        .unwrap_or_else(|| panic!("no loopback listener: {:?}", self.lines()));
        self.listening()
            .into_iter()
            .find(|a| a.starts_with("127.0.0.1:"))
            .unwrap()
    }

    /// The key beside the config (the daemon makes it before it listens).
    fn key(&self) -> [u8; 32] {
        fleetd::config::key(&self.cfg_dir).unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: SIGTERM to this test's own child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The TCP addresses process `pid` listens on.
#[cfg(target_os = "macos")]
fn tcp_listens(pid: u32) -> Vec<String> {
    let o = Command::new("lsof")
        .args([
            "-a",
            "-nP",
            "-p",
            &pid.to_string(),
            "-iTCP",
            "-sTCP:LISTEN",
            "-Fn",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(String::from)
        .collect()
}

/// The TCP addresses process `pid` listens on: the LISTEN rows of `/proc/net/tcp{,6}` whose inode is one of its fds.
#[cfg(target_os = "linux")]
fn tcp_listens(pid: u32) -> Vec<String> {
    let inodes: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter_map(|l| {
            let s = l.to_string_lossy().to_string();
            Some(s.strip_prefix("socket:[")?.strip_suffix(']')?.to_string())
        })
        .collect();
    let mut out = vec![];
    for table in ["tcp", "tcp6"] {
        let text = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")).unwrap_or_default();
        for l in text.lines().skip(1) {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() > 9 && f[3] == "0A" && inodes.iter().any(|i| i == f[9]) {
                out.push(format!("{table} {}", f[1]));
            }
        }
    }
    out
}

/// How many TCP sockets process `pid` holds (listeners included).
#[cfg(target_os = "macos")]
fn tcp_sockets(pid: u32) -> usize {
    let o = Command::new("lsof")
        .args(["-a", "-nP", "-p", &pid.to_string(), "-iTCP", "-Fn"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| l.starts_with('n'))
        .count()
}

/// How many TCP sockets process `pid` holds (listeners included): its socket fds found in `/proc/net/tcp{,6}`.
#[cfg(target_os = "linux")]
fn tcp_sockets(pid: u32) -> usize {
    let inodes: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|d| {
            d.filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
                .filter_map(|l| {
                    let s = l.to_string_lossy().to_string();
                    Some(s.strip_prefix("socket:[")?.strip_suffix(']')?.to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    let mut n = 0;
    for table in ["tcp", "tcp6"] {
        let text = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")).unwrap_or_default();
        n += text
            .lines()
            .skip(1)
            .filter(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() > 9 && inodes.iter().any(|i| i == f[9])
            })
            .count();
    }
    n
}

/// A home whose stand-in roster lists `n` live interactive sessions (run by this test's pid), so the board is not empty.
fn board_home(n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let p = root.join(".claude");
    for d in ["fleet-state", "fleet-status", "jobs", "projects"] {
        std::fs::create_dir_all(p.join(d)).unwrap();
    }
    for d in [".local/bin", "stand-in", "panel"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(p.join("settings.json"), "{}").unwrap();
    let rows: Vec<_> = (0..n)
        .map(|i| {
            json!({"sessionId": format!("sess-{i:04}-fixture"), "pid": std::process::id(), "kind": "interactive",
                   "status": "idle", "name": format!("fixture {i}"), "cwd": "/tmp/fleetd-fixture",
                   "startedAt": 1_793_900_000_000i64})
        })
        .collect();
    let running: Vec<_> = (0..n)
        .map(|i| json!({"sessionId": format!("sess-{i:04}-fixture")}))
        .collect();
    let s = root.join("stand-in");
    std::fs::write(s.join("all.json"), serde_json::to_string(&rows).unwrap()).unwrap();
    std::fs::write(
        s.join("running.json"),
        serde_json::to_string(&running).unwrap(),
    )
    .unwrap();
    let script = format!(
        "#!/bin/sh\nS='{}'\ncase \"$*\" in\n  'agents --json --all') cat \"$S/all.json\" ;;\n  'agents --json') cat \"$S/running.json\" ;;\n  *) echo \"stand-in: $*\" >&2; exit 1 ;;\nesac\n",
        s.display()
    );
    let bin = root.join(".local/bin/claude");
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// The first non-`Meta` frame, within 10 s.
async fn first(c: &mut Client) -> Frame {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), c.next())
            .await
            .expect("a frame in time")
            .unwrap()
        {
            Frame::Meta(_) => continue,
            f => return f,
        }
    }
}

/// The board the Unix socket serves: (seq, sessions).
async fn unix_board(sock: &Path) -> (u64, Vec<Session>) {
    let mut c = Client::connect(sock, None).await.unwrap();
    match first(&mut c).await {
        Frame::Snapshot { seq, sessions, .. } => (seq, sessions),
        f => panic!("expected a Snapshot, got {f:?}"),
    }
}

/// Waits until the board holds `n` sessions (the startup read and its roster run are done).
async fn board_of(sock: &Path, n: usize) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if unix_board(sock).await.1.len() == n {
            return;
        }
        assert!(Instant::now() < end, "the board never held {n} sessions");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Reads frames from a raw stream until EOF; each frame, and whether the end was a clean EOF.
fn frames_until_eof(s: &mut std::net::TcpStream) -> (Vec<Frame>, bool) {
    // macOS refuses the option (EINVAL) on a socket the server has already finished with: it is at EOF or reset then,
    // and the read below says which.
    let _ = s.set_read_timeout(Some(Duration::from_secs(15)));
    let mut all = vec![];
    let clean = loop {
        let mut buf = [0u8; 4096];
        match s.read(&mut buf) {
            Ok(0) => break true,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) => {
                eprintln!("read: {e}");
                break false;
            }
        }
    };
    let mut frames = vec![];
    let mut at = 0;
    while at + 4 <= all.len() {
        let len = u32::from_le_bytes(all[at..at + 4].try_into().unwrap()) as usize;
        assert!(at + 4 + len <= all.len(), "a cut frame");
        frames.push(decode(&all[at + 4..at + 4 + len]).unwrap());
        at += 4 + len;
    }
    assert_eq!(at, all.len(), "bytes past the last frame");
    (frames, clean)
}

fn hello(role: Role) -> Vec<u8> {
    encode(&Frame::Hello {
        proto: PROTO,
        since: None,
        run: None,
        role,
    })
    .unwrap()
}

// ------------------------------------------------------------------------------------------------- tests

#[test]
fn off_by_default_binds_no_tcp() {
    let home = board_home(0);
    let d = Daemon::start(home.path(), &[], &[]);
    std::thread::sleep(Duration::from_millis(500));
    assert!(d.listening().is_empty(), "{:?}", d.lines());
    assert_eq!(tcp_listens(d.pid()), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_with_key_gets_the_board() {
    let home = board_home(2);
    let d = Daemon::start(home.path(), &["--listen", "loopback", "--port", "0"], &[]);
    let addr = d.loopback();
    assert_eq!(tcp_listens(d.pid()).len(), 1, "{:?}", tcp_listens(d.pid()));
    board_of(&d.sock, 2).await;
    let (useq, usessions) = unix_board(&d.sock).await;
    let mut c = Client::connect_tcp(&addr, Some(d.key()), None)
        .await
        .unwrap();
    let f = tokio::time::timeout(Duration::from_secs(10), c.next())
        .await
        .unwrap()
        .unwrap();
    let (seq, sessions) = match f {
        Frame::Snapshot {
            seq,
            sessions,
            machine,
            ..
        } => {
            assert_eq!(machine, "fixture-mac");
            (seq, sessions)
        }
        f => panic!("expected a Snapshot, got {f:?}"),
    };
    let m = tokio::time::timeout(Duration::from_secs(10), c.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(m, Frame::Meta(_)), "then a Meta: {m:?}");
    assert_eq!(sessions.len(), 2);
    let keys = |v: &[Session]| v.iter().map(|s| s.key.clone()).collect::<Vec<_>>();
    assert_eq!(keys(&sessions), keys(&usessions));
    if seq == useq {
        assert_eq!(sessions, usessions);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_wrong_key_is_refused() {
    let home = board_home(0);
    let d = Daemon::start(home.path(), &["--listen", "loopback", "--port", "0"], &[]);
    let addr = d.loopback();
    let mut wrong = d.key();
    wrong[0] ^= 1;
    let mut c = Client::connect_tcp(&addr, Some(wrong), None).await.unwrap();
    match tokio::time::timeout(Duration::from_secs(10), c.next())
        .await
        .unwrap()
    {
        Ok(Frame::Error { message }) => {
            assert!(message.starts_with("not allowed"), "{message}")
        }
        f => panic!("expected an Error, got {f:?}"),
    }
    let e = tokio::time::timeout(Duration::from_secs(10), c.next())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof, "{e}");
    let refused = d
        .wait_line(Duration::from_secs(5), |l| {
            l.starts_with("fleetd: tcp refused 127.0.0.1: ")
        })
        .unwrap_or_else(|| panic!("no refusal logged: {:?}", d.lines()));
    // The key never reaches the log.
    let hex: String = wrong.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        !d.lines().iter().any(|l| l.contains(&hex[..16])),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_no_key() {
    let home = board_home(0);
    let d = Daemon::start(home.path(), &["--listen", "loopback", "--port", "0"], &[]);
    let addr = d.loopback();
    let mut c = Client::connect_tcp(&addr, None, None).await.unwrap();
    let f = tokio::time::timeout(Duration::from_secs(15), c.next())
        .await
        .unwrap();
    if cfg!(target_os = "macos") {
        match f {
            Ok(Frame::Error { message }) => {
                assert_eq!(message, "not allowed: loopback needs the key")
            }
            f => panic!("expected an Error, got {f:?}"),
        }
        let e = tokio::time::timeout(Duration::from_secs(10), c.next())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    } else {
        // Linux: the peer's socket is this uid's (`/proc/net/tcp`), so no key is needed.
        assert!(matches!(f, Ok(Frame::Snapshot { .. })), "{f:?}");
    }
}

#[test]
fn refused_peer_is_not_read() {
    let home = board_home(0);
    let d = Daemon::start(home.path(), &["--listen", "loopback", "--port", "0"], &[]);
    let addr = d.loopback();
    let mut wrong = d.key();
    wrong[31] ^= 0x80;
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    let mut out = b"FKEY".to_vec();
    out.extend_from_slice(&wrong);
    out.extend_from_slice(&hello(Role::Board));
    s.write_all(&out).unwrap();
    let (frames, clean) = frames_until_eof(&mut s);
    assert_eq!(frames.len(), 1, "{frames:?}");
    match &frames[0] {
        Frame::Error { message } => assert!(message.starts_with("not allowed: "), "{message}"),
        f => panic!("expected an Error, got {f:?}"),
    }
    assert!(clean, "a FIN after the Error, not a reset");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminals_over_tcp() {
    let home = terms_home::Home::new();
    std::fs::create_dir_all(home.path().join("panel")).unwrap();
    let d = Daemon::start(&home.path(), &["--listen", "loopback", "--port", "0"], &[]);
    let addr = d.loopback();
    let (mut tx, mut rx) = term_connect_tcp(&addr, Some(d.key())).await.unwrap();
    tx.send(&Frame::TermOpen {
        r#ref: 1,
        target: AttachTarget::Claude {
            profile: "default".into(),
            job: "000000a1".into(),
        },
        cols: 100,
        rows: 30,
    })
    .await
    .unwrap();
    let mut out = vec![];
    let end = Instant::now() + Duration::from_secs_f64(terms_home::BOOT);
    while !terms_home::has(&out, "BANNER attach 000000a1") {
        let left = end.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.next()).await {
            Ok(Ok(Frame::TermOut { r#ref: 1, data })) => out.extend(data),
            Ok(f) => panic!("unexpected {f:?}"),
            Err(_) => panic!("no banner: {:?}", String::from_utf8_lossy(&out)),
        }
    }
    tx.send(&Frame::TermIn {
        r#ref: 1,
        data: b"over tcp\n".to_vec(),
    })
    .await
    .unwrap();
    while !terms_home::has(&out, "GOT over tcp") {
        match tokio::time::timeout(Duration::from_secs(5), rx.next()).await {
            Ok(Ok(Frame::TermOut { r#ref: 1, data })) => out.extend(data),
            f => panic!("unexpected {f:?}"),
        }
    }
    tx.send(&Frame::TermClose { r#ref: 1 }).await.unwrap();
}

#[test]
fn never_binds_unspecified() {
    // An address on no local interface, inside the tailnet range.
    let cases = [
        ("0.0.0.0", false),
        ("0.0.0.0", true),
        ("8.8.8.8", true),
        ("100.127.255.254", false),
        ("100.127.255.254", true),
    ];
    for (ip, any) in cases {
        let home = board_home(0);
        let stub = StubTailscale::new(home.path(), ip);
        let bin = stub.bin();
        let mut env = vec![("FLEETD_TAILSCALE", bin.to_str().unwrap())];
        if any {
            env.push(("FLEETD_TEST_TS_RANGE_ANY", "1"));
        }
        let d = Daemon::start(home.path(), &["--listen", "tailscale", "--port", "0"], &env);
        d.loopback();
        d.wait_line(Duration::from_secs(5), |l| {
            l.starts_with(&format!("fleetd: tailscale: not binding {ip}: "))
        })
        .unwrap_or_else(|| panic!("{ip} (any {any}): no refusal: {:?}", d.lines()));
        let listening = d.listening();
        assert_eq!(listening.len(), 1, "{ip}: {listening:?}");
        assert!(listening[0].starts_with("127.0.0.1:"), "{listening:?}");
        let os = tcp_listens(d.pid());
        assert_eq!(os.len(), 1, "{ip}: {os:?}");
        #[cfg(target_os = "macos")]
        assert!(os[0].starts_with("127.0.0.1:"), "{os:?}");
        #[cfg(target_os = "linux")]
        assert!(os[0].starts_with("tcp 0100007F:"), "{os:?}");
    }
}

#[test]
fn tailscale_retry_binds_later() {
    // The Tailscale address the stub answers once it is up. Linux: 127.0.0.2, a real second bind (all of 127/8 is on
    // lo). macOS: 127.0.0.1, unavoidably, since lo0 has no other loopback address unless aliased; there the stand-in
    // shares the loopback listener.
    let ts = if cfg!(target_os = "linux") {
        "127.0.0.2"
    } else {
        "127.0.0.1"
    };
    let home = board_home(0);
    let stub = StubTailscale::new(home.path(), ts);
    stub.fail_ip(1);
    let bin = stub.bin();
    let env = [
        ("FLEETD_TAILSCALE", bin.to_str().unwrap()),
        ("FLEETD_TEST_TS_RANGE_ANY", "1"),
        ("FLEETD_TEST_TS_RETRY_MS", "200"),
    ];
    let d = Daemon::start(home.path(), &["--listen", "tailscale", "--port", "0"], &env);
    let addr = d.loopback();
    let failed = Instant::now();
    d.wait_line(Duration::from_secs(5), |l| {
        l.starts_with("fleetd: tailscale: ip -4: ") && l.ends_with("retrying in 0.2s")
    })
    .unwrap_or_else(|| panic!("no failure logged: {:?}", d.lines()));
    let port = addr.rsplit(':').next().unwrap();
    d.wait_line(Duration::from_secs(2), |l| {
        l == format!("fleetd: listening tcp {ts}:{port} (tailscale)")
    })
    .unwrap_or_else(|| panic!("no second bind within 2 s: {:?}", d.lines()));
    assert!(failed.elapsed() < Duration::from_secs(2));
    assert_eq!(
        stub.calls()
            .iter()
            .filter(|c| c.starts_with("ip -4"))
            .count(),
        2
    );
    // Bound: no more retries.
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        stub.calls()
            .iter()
            .filter(|c| c.starts_with("ip -4"))
            .count(),
        2
    );
}

/// Ruling P5-R9: a process on this machine that dials its own Tailscale address is local, not a tailnet peer: it needs
/// the key (or, on Linux, the uid), though Self is this machine's own user, untagged. WhoIs is never asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_tailscale_ip_is_local_needs_key() {
    // The stand-in Tailscale address: on Linux a real second bind (all of 127/8 is on lo); macOS has only 127.0.0.1 on
    // lo0, so there it is the loopback listener's address.
    let ts = if cfg!(target_os = "linux") {
        "127.0.0.2"
    } else {
        "127.0.0.1"
    };
    let home = board_home(0);
    let stub = StubTailscale::new(home.path(), ts);
    let bin = stub.bin();
    let env = [
        ("FLEETD_TAILSCALE", bin.to_str().unwrap()),
        ("FLEETD_TEST_TS_RANGE_ANY", "1"),
    ];
    let d = Daemon::start(home.path(), &["--listen", "tailscale", "--port", "0"], &env);
    let port = d.loopback().rsplit(':').next().unwrap().to_string();
    let addr = format!("{ts}:{port}");
    d.wait_line(Duration::from_secs(5), |l| {
        l == format!("fleetd: listening tcp {addr} (tailscale)")
    })
    .unwrap_or_else(|| panic!("no tailscale listener: {:?}", d.lines()));
    let mut c = Client::connect_tcp(&addr, None, None).await.unwrap();
    let f = tokio::time::timeout(Duration::from_secs(10), c.next())
        .await
        .unwrap();
    if cfg!(target_os = "macos") {
        match f {
            Ok(Frame::Error { message }) => {
                assert_eq!(message, "not allowed: loopback needs the key")
            }
            f => panic!("expected a refusal, got {f:?}"),
        }
    } else {
        assert!(
            matches!(f, Ok(Frame::Snapshot { .. })),
            "the uid path: {f:?}"
        );
    }
    let mut c = Client::connect_tcp(&addr, Some(d.key()), None)
        .await
        .unwrap();
    let f = tokio::time::timeout(Duration::from_secs(10), c.next())
        .await
        .unwrap();
    assert!(
        matches!(f, Ok(Frame::Snapshot { .. })),
        "with the key: {f:?}"
    );
    assert!(
        !stub.calls().iter().any(|c| c.starts_with("whois")),
        "{:?}",
        stub.calls()
    );
}

/// A burst of refused peers that never hang up holds at most the auth cap (16) of the daemon's TCP sockets at any
/// moment: a refusal keeps its place in auth until its socket is gone, and past the cap a peer is refused at once.
#[test]
fn a_burst_of_refusals_holds_at_most_the_cap() {
    let home = board_home(0);
    let d = Daemon::start(home.path(), &["--listen", "loopback", "--port", "0"], &[]);
    let addr = d.loopback();
    let mut wrong = d.key();
    wrong[5] ^= 1;
    let mut preamble = b"FKEY".to_vec();
    preamble.extend_from_slice(&wrong);
    let base = tcp_sockets(d.pid());
    let mut held = vec![];
    for _ in 0..64 {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(&preamble).unwrap();
        held.push(s);
    }
    let mut most = 0;
    let end = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < end {
        most = most.max(tcp_sockets(d.pid()) - base);
        std::thread::sleep(Duration::from_millis(20));
    }
    // A busy refusal's socket can be caught for a moment between its accept and its close.
    assert!(most <= 16 + 2, "{most} TCP sockets held at once");
    // Every peer was told, or closed on.
    for s in &mut held {
        // A socket the server has already reset (a busy refusal) refuses the option on macOS: that is a close too.
        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
        let mut b = [0u8; 256];
        let n = s.read(&mut b).unwrap_or(0);
        if n > 0 {
            let len = u32::from_le_bytes(b[..4].try_into().unwrap()) as usize;
            assert!(matches!(
                decode(&b[4..4 + len]).unwrap(),
                Frame::Error { .. }
            ));
        }
    }
}

/// A peer that sends part of the preamble and then half-closes is refused at once, not spun on until the 10 s limit:
/// 16 of them at once (the whole auth cap) are all answered well inside the time one spin would take.
#[test]
fn half_closed_preamble_is_refused_fast() {
    let home = board_home(0);
    let d = Daemon::start(home.path(), &["--listen", "loopback", "--port", "0"], &[]);
    let addr = d.loopback();
    for (n, part) in [(1usize, &b"FKE"[..]), (16, &b"FKEYabc"[..])] {
        let t0 = Instant::now();
        let mut peers: Vec<std::net::TcpStream> = (0..n)
            .map(|_| {
                let mut s = std::net::TcpStream::connect(&addr).unwrap();
                s.write_all(part).unwrap();
                s.shutdown(std::net::Shutdown::Write).unwrap();
                s
            })
            .collect();
        for s in &mut peers {
            let (frames, _) = frames_until_eof(s);
            assert!(
                frames.len() <= 1
                    && frames
                        .iter()
                        .all(|f| matches!(f, Frame::Error { message } if message.starts_with("not allowed"))),
                "{frames:?}"
            );
        }
        let took = t0.elapsed();
        eprintln!("{n} half-closed peer(s) answered in {took:?}");
        let budget = Duration::from_millis(if n == 1 { 100 } else { 500 });
        assert!(
            took < budget,
            "{n} half-closed peer(s) answered in {took:?}"
        );
    }
}
