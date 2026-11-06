//! The pty: a child in its own session on a fresh terminal, driven through the master.

use std::ffi::OsString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use fleetd::attach_cmd::Cmd;
use fleetd::pty::Pty;

fn cmd(argv: &[&str]) -> Cmd {
    Cmd {
        argv: argv.iter().map(OsString::from).collect(),
        env: vec![
            ("TERM".into(), "xterm-256color".into()),
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
        ],
        cwd: "/tmp".into(),
    }
}

fn sh(script: &str) -> Cmd {
    cmd(&["/bin/sh", "-c", script])
}

/// Read from the master until `done(&all)` holds or `secs` pass; returns everything read.
fn read_until(fd: &OwnedFd, secs: u64, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut all = Vec::new();
    while Instant::now() < end && !done(&all) {
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, 100) } <= 0 {
            continue;
        }
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        all.extend_from_slice(&buf[..n as usize]);
    }
    all
}

fn has(all: &[u8], needle: &[u8]) -> bool {
    all.windows(needle.len()).any(|w| w == needle)
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn cat_echoes_what_is_written() {
    let pty = Pty::spawn(&cmd(&["/bin/cat"]), 100, 30).unwrap();
    let r = pty.reader().unwrap();
    pty.write(b"hello\n").unwrap();
    let got = read_until(&r, 5, |a| has(a, b"hello"));
    assert!(has(&got, b"hello"), "{got:?}");
    pty.stop();
}

#[test]
fn resize_reaches_the_program() {
    let pty = Pty::spawn(&sh("echo ready; read x; stty size"), 100, 30).unwrap();
    let r = pty.reader().unwrap();
    read_until(&r, 5, |a| has(a, b"ready"));
    pty.resize(120, 40);
    pty.write(b"\n").unwrap();
    let got = read_until(&r, 5, |a| has(a, b"40 120"));
    assert!(has(&got, b"40 120"), "{:?}", String::from_utf8_lossy(&got));
}

#[test]
fn the_spawn_size_is_the_initial_size() {
    let pty = Pty::spawn(&sh("stty size"), 77, 22).unwrap();
    let got = read_until(&pty.reader().unwrap(), 5, |a| has(a, b"22 77"));
    assert!(has(&got, b"22 77"), "{:?}", String::from_utf8_lossy(&got));
}

#[test]
fn stop_kills_a_child_that_ignores_hangup_and_its_grandchild() {
    let pty = Pty::spawn(&sh("trap '' HUP; sleep 300 & echo gc=$!; wait"), 100, 30).unwrap();
    let r = pty.reader().unwrap();
    let got = read_until(&r, 5, |a| a.contains(&b'\n') && has(a, b"gc="));
    let text = String::from_utf8_lossy(&got).to_string();
    let gc: i32 = text
        .split("gc=")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(gc));
    let t = Instant::now();
    pty.stop();
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(1400) && took < Duration::from_secs(3),
        "{took:?}"
    );
    let end = Instant::now() + Duration::from_millis(500);
    while alive(gc) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(gc), "grandchild survived");
    assert!(pty.wait().is_some_and(|s| !s.success()));
}

#[test]
fn stop_is_prompt_when_the_child_obeys_hangup() {
    let pty = Pty::spawn(&cmd(&["/bin/cat"]), 100, 30).unwrap();
    let t = Instant::now();
    pty.stop();
    assert!(
        t.elapsed() < Duration::from_millis(1000),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn bytes_survive_unchanged_even_split_mid_sequence() {
    let pty = Pty::spawn(&sh("stty raw -echo; echo ready; exec cat"), 100, 30).unwrap();
    let r = pty.reader().unwrap();
    read_until(&r, 5, |a| has(a, b"ready"));
    pty.write(b"\xff\xfe\x1b[3").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    pty.write(b"1m").unwrap();
    let want = b"\xff\xfe\x1b[31m";
    let got = read_until(&r, 5, |a| has(a, want));
    assert!(has(&got, want), "{got:?}");
    pty.stop();
}

#[test]
fn the_child_sees_only_fds_0_1_2() {
    // A descriptor the daemon holds that is NOT close-on-exec must still not reach the child.
    let leak = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert!(leak > 2);
    let pty = Pty::spawn(&sh("for i in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40; do if [ -e /dev/fd/$i ]; then echo open=$i; fi; done; echo done"), 100, 30).unwrap();
    let got = read_until(&pty.reader().unwrap(), 10, |a| has(a, b"done"));
    unsafe { libc::close(leak) };
    let text = String::from_utf8_lossy(&got).to_string();
    let open: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("open="))
        .collect();
    assert_eq!(open, ["0", "1", "2"], "{text}");
}

#[test]
fn the_child_leads_its_own_group_and_has_the_pty_as_its_terminal() {
    let pty = Pty::spawn(&sh("ps -o pid=,pgid= -p $$; tty"), 100, 30).unwrap();
    let got = read_until(&pty.reader().unwrap(), 5, |a| has(a, b"/dev/"));
    let text = String::from_utf8_lossy(&got).to_string();
    let nums: Vec<&str> = text.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(nums[0], nums[1], "not a group leader: {text}");
    assert!(text.contains("/dev/tty"), "{text}");
}

/// A shell that leaves a HUP-proof `sleep` behind in its group and exits; returns the pty and the sleep's pid.
fn orphan_maker() -> (Pty, i32) {
    let pty = Pty::spawn(&sh("trap '' HUP; sleep 300 & echo gc=$!; exit 0"), 100, 30).unwrap();
    let got = read_until(&pty.reader().unwrap(), 5, |a| {
        has(a, b"gc=") && a.ends_with(b"\n")
    });
    let text = String::from_utf8_lossy(&got).to_string();
    let gc = text
        .split("gc=")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    (pty, gc)
}

#[test]
fn stop_after_the_child_exited_on_its_own_returns_at_once_and_leaves_others_alone() {
    // A sibling in a group of its own must stay alive: nothing may signal a group that is not this child's.
    let sibling = Pty::spawn(&cmd(&["/bin/cat"]), 100, 30).unwrap();
    let pty = Pty::spawn(&sh("exit 3"), 100, 30).unwrap();
    read_until(&pty.reader().unwrap(), 2, |_| false);
    let t = Instant::now();
    pty.stop();
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(pty.wait().and_then(|s| s.code()), Some(3));
    sibling.write(b"still here\n").unwrap();
    let got = read_until(&sibling.reader().unwrap(), 5, |a| has(a, b"still here"));
    assert!(has(&got, b"still here"), "sibling was disturbed");
    sibling.stop();
}

#[test]
fn stop_twice_and_stop_after_wait_do_nothing_the_second_time() {
    let pty = Pty::spawn(&cmd(&["/bin/cat"]), 100, 30).unwrap();
    pty.stop();
    let t = Instant::now();
    pty.stop();
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
    assert!(pty.wait().is_some());
    pty.stop();

    let other = Pty::spawn(&sh("exit 0"), 100, 30).unwrap();
    assert!(other.wait().is_some_and(|s| s.success()));
    let t = Instant::now();
    other.stop();
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn stop_still_kills_an_orphan_grandchild_after_the_leader_exited() {
    let (pty, gc) = orphan_maker();
    assert!(pty.wait().is_some_and(|s| s.success()));
    assert!(alive(gc));
    pty.stop();
    let end = Instant::now() + Duration::from_millis(500);
    while alive(gc) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!alive(gc), "orphan survived stop");
}

#[test]
fn wait_in_one_thread_and_stop_in_another_both_see_the_status() {
    let pty = std::sync::Arc::new(Pty::spawn(&cmd(&["/bin/cat"]), 100, 30).unwrap());
    let waiter = {
        let p = std::sync::Arc::clone(&pty);
        std::thread::spawn(move || p.wait())
    };
    std::thread::sleep(Duration::from_millis(100));
    pty.stop();
    assert!(waiter.join().unwrap().is_some());
}

#[test]
fn dropping_a_pty_hangs_up_the_program_without_blocking() {
    let pty = Pty::spawn(&sh("trap '' HUP; sleep 300 & echo gc=$!; wait"), 100, 30).unwrap();
    let got = read_until(&pty.reader().unwrap(), 5, |a| {
        has(a, b"gc=") && a.ends_with(b"\n")
    });
    let text = String::from_utf8_lossy(&got).to_string();
    let gc: i32 = text
        .split("gc=")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let t = Instant::now();
    drop(pty);
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    let end = Instant::now() + Duration::from_secs(4);
    while alive(gc) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(gc), "drop left the group running");
}

/// Cold spawn time with the daemon's raised descriptor limit (it asks for 8192): `cargo test -- --ignored --nocapture`.
#[test]
#[ignore]
fn cold_spawn_time_with_a_big_fd_limit() {
    let lim = libc::rlimit {
        rlim_cur: 8192,
        rlim_max: 8192,
    };
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
    let mut times = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        let pty = Pty::spawn(&cmd(&["/usr/bin/true"]), 100, 30).unwrap();
        times.push(t.elapsed());
        pty.wait();
    }
    times.sort();
    eprintln!("spawn p50 {:?} max {:?}", times[15], times[29]);
}

#[test]
fn wait_after_reap_returns_the_cached_status_and_never_waits_on_a_recycled_pid() {
    let a = Pty::spawn(&sh("exit 3"), 80, 24).unwrap();
    assert_eq!(a.wait().unwrap().code(), Some(3));
    // Another Pty's child, and a handle on `a` whose pid has been recycled to it (as the kernel may do once `a` is reaped).
    let b = Pty::spawn(&sh("sleep 1; exit 7"), 80, 24).unwrap();
    let recycled = a.__reaped_with_pid_of(&b).unwrap();
    let t = Instant::now();
    assert_eq!(
        recycled.wait().unwrap().code(),
        Some(3),
        "the cached status"
    );
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "waited {:?} on someone else's child",
        t.elapsed()
    );
    drop(recycled);
    assert_eq!(
        b.wait().unwrap().code(),
        Some(7),
        "b's status is b's to reap"
    );
}
