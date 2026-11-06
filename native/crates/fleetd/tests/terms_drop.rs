//! Dropping `Terms` while a reader is parked on a full sink: the reader leaves and every fd it held closes. Its own test
//! binary, so no other test's ptys move the process's fd and thread counts.

mod terms_home;

use std::time::{Duration, Instant};

use terms_home::*;

fn fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

fn threads() -> i32 {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    assert_eq!(n, size);
    info.pti_threadnum
}

#[test]
fn dropping_terms_frees_a_reader_parked_on_a_full_sink() {
    let h = Home::new();
    let mut c = Conn::new(1);
    let (fd0, th0) = (fds(), threads());
    let t = h.terms();
    c.open(&t, 1, claude("000000a1")).unwrap();
    assert!(c.until_has(1, "BANNER", BOOT));
    t.input(1, 1, b"flood\n").unwrap();
    // Not read from here on: the sink fills and the reader parks.
    assert!(wait_for(Duration::from_secs(10), || c.src.pending_bytes()
        > fleetd::terms::SINK_HIGH));
    std::thread::sleep(Duration::from_millis(200));
    assert!(fds() > fd0 && threads() > th0);
    let s = Instant::now();
    drop(t);
    let back = wait_for(Duration::from_secs(1), || fds() == fd0 && threads() == th0);
    assert!(
        back,
        "after {:?}: fds {} (baseline {fd0}), threads {} (baseline {th0})",
        s.elapsed(),
        fds(),
        threads()
    );
    drop(c); // the source stayed alive throughout
}
