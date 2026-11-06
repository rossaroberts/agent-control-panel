//! The process scan: a process named `codex` holding a `.jsonl` open is found, and one named `muse` is listed, without
//! running `ps` or `lsof`.

mod common;

use std::time::{Duration, Instant};

use common::Held;
use fleetd::procscan::scan;

#[test]
fn helper_hold() {
    common::hold();
}

#[test]
fn finds_codex_logs_and_muse_processes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let log = root.join("rollout-fixture.jsonl");
    std::fs::write(&log, "{}\n").unwrap();
    let other = root.join("not-a-log.txt");
    std::fs::write(&other, "").unwrap();
    let codex = Held::start(&root.join("codex"), &[&log, &other], &root);
    let muse = Held::start(&root.join("muse"), &[], &root);

    let t0 = Instant::now();
    let s = scan().expect("the scan works");
    let took = t0.elapsed();
    eprintln!("process scan: {:.2} ms", took.as_secs_f64() * 1000.0);
    assert!(s.codex_open.contains(&log), "{:?}", s.codex_open);
    assert!(!s.codex_open.contains(&other));
    let m = s
        .muse
        .iter()
        .find(|m| m.pid == muse.pid())
        .expect("the muse process");
    assert_eq!(m.cwd.as_deref(), Some(root.to_str().unwrap()));
    assert_eq!(m.args, "--exact helper_hold --nocapture --test-threads=1");
    assert!(m.since > 0);
    assert!(took < Duration::from_millis(200), "{took:?}");

    codex.stop();
    muse.stop();
    let s = scan().unwrap();
    assert!(
        !s.codex_open.contains(&log),
        "let go when the process exits"
    );
    assert!(
        s.muse.is_empty()
            || s.muse
                .iter()
                .all(|m| m.cwd.as_deref() != Some(root.to_str().unwrap()))
    );
}
