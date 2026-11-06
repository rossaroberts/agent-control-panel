//! A watcher thread that panics ends the daemon with a non-zero status (main.rs's panic hook; release builds abort), so
//! launchd starts a fresh one, instead of a live process serving a board no thread updates any more.

use std::fs;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fleet_model::wire::Frame;
use fleetd::client::Client;

#[tokio::test]
async fn a_panicking_watcher_thread_ends_the_process_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".claude/jobs")).unwrap();
    fs::write(root.join(".claude/settings.json"), "{}").unwrap();
    let sock = root.join("run/fleetd.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .arg("serve")
        .env("FLEETD_HOME", &root)
        .env("HOME", &root)
        .env("FLEETD_MACHINE", "fixture-mac")
        .env("FLEETD_SOCK", &sock)
        .env("PATH", "/usr/bin:/bin")
        .env("FLEETD_TEST_PANIC", "1500")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // It serves first: the panic comes to a daemon that is up.
    let t = Instant::now();
    while !sock.exists() {
        assert!(t.elapsed() < Duration::from_secs(10), "never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut c = Client::connect(&sock, None).await.unwrap();
    let f = tokio::time::timeout(Duration::from_secs(5), c.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(f, Frame::Snapshot { .. }), "{f:?}");
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if t.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("still running 10 s on: a panicked watcher left a live daemon");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(!status.success(), "{status:?}");
    let mut err = String::new();
    use std::io::Read;
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(
        err.contains("fleetd: panic on thread fleetd-reconcile"),
        "{err}"
    );
    // The client sees the daemon go.
    let e = tokio::time::timeout(Duration::from_secs(5), c.next())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
}
