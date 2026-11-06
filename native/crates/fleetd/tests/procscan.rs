//! The Codex open-file scan: a process named `codex` holding a `.jsonl` open is found, without running `lsof`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fleetd::procscan::codex_open;

#[test]
fn finds_the_logs_a_codex_process_holds_open() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let log = root.join("rollout-fixture.jsonl");
    std::fs::write(&log, "{}\n").unwrap();
    let other = root.join("not-a-log.txt");
    std::fs::write(&other, "").unwrap();
    // A shell under the name `codex` (a link: a copied system binary is not allowed to run): it opens the log on fd 3
    // and the text file on fd 4, then waits on its stdin.
    let codex = root.join("codex");
    std::os::unix::fs::symlink("/bin/bash", &codex).unwrap();
    let mut child = Command::new(&codex)
        .args(["-c", "exec 3<\"$1\" 4<\"$2\"; echo ready; read x", "sh"])
        .arg(&log)
        .arg(&other)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut ready = [0u8; 6];
    std::io::Read::read_exact(&mut out, &mut ready).unwrap();

    let t0 = Instant::now();
    let open = codex_open().expect("the scan works");
    let took = t0.elapsed();
    eprintln!("codex open scan: {:.2} ms", took.as_secs_f64() * 1000.0);
    assert!(open.contains(&log), "{open:?}");
    assert!(!open.contains(&other));
    assert!(took < Duration::from_millis(200), "{took:?}");

    child.stdin.take().unwrap().write_all(b"\n").unwrap();
    child.wait().unwrap();
    let open = codex_open().unwrap();
    assert!(!open.contains(&log), "let go when the process exits");
}
