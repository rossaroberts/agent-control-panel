//! Two `fleetd serve` processes started together: exactly one serves, the other exits non-zero, and the socket is
//! never replaced.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn(root: &std::path::Path, sock: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .arg("serve")
        .env("FLEETD_HOME", root)
        .env("HOME", root)
        .env("FLEETD_MACHINE", "fixture-mac")
        .env("FLEETD_SOCK", sock)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn exactly_one_of_two_serves_runs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".claude/jobs")).unwrap();
    fs::write(root.join(".claude/settings.json"), "{}").unwrap();
    let sock = root.join("run/fleetd.sock");
    let mut a = spawn(&root, &sock);
    let mut b = spawn(&root, &sock);
    let t = Instant::now();
    let (loser, mut winner) = loop {
        assert!(t.elapsed() < Duration::from_secs(30), "neither exited");
        if let Some(st) = a.try_wait().unwrap() {
            break ((st, a), b);
        }
        if let Some(st) = b.try_wait().unwrap() {
            break ((st, b), a);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!loser.0.success());
    let msg = {
        use std::io::Read;
        let mut s = String::new();
        loser.1.stderr.unwrap().read_to_string(&mut s).unwrap();
        s
    };
    assert!(msg.contains("another fleetd holds"), "{msg}");
    // The winner is serving, and keeps the same socket.
    let t = Instant::now();
    while !sock.exists() {
        assert!(t.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(20));
    }
    let ino = fs::metadata(&sock).unwrap().ino();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fs::metadata(&sock).unwrap().ino(), ino);
    assert!(winner.try_wait().unwrap().is_none());
    // SAFETY: SIGTERM to our own child.
    unsafe { libc::kill(winner.id() as i32, libc::SIGTERM) };
    assert!(winner.wait().unwrap().success());
    assert!(!sock.exists());
}
