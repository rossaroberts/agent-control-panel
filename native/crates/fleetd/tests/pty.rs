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
    let r = pty.reader();
    pty.write(b"hello\n").unwrap();
    let got = read_until(&r, 5, |a| has(a, b"hello"));
    assert!(has(&got, b"hello"), "{got:?}");
    pty.stop();
}

#[test]
fn resize_reaches_the_program() {
    let pty = Pty::spawn(&sh("echo ready; read x; stty size"), 100, 30).unwrap();
    let r = pty.reader();
    read_until(&r, 5, |a| has(a, b"ready"));
    pty.resize(120, 40);
    pty.write(b"\n").unwrap();
    let got = read_until(&r, 5, |a| has(a, b"40 120"));
    assert!(has(&got, b"40 120"), "{:?}", String::from_utf8_lossy(&got));
}

#[test]
fn the_spawn_size_is_the_initial_size() {
    let pty = Pty::spawn(&sh("stty size"), 77, 22).unwrap();
    let got = read_until(&pty.reader(), 5, |a| has(a, b"22 77"));
    assert!(has(&got, b"22 77"), "{:?}", String::from_utf8_lossy(&got));
}

#[test]
fn stop_kills_a_child_that_ignores_hangup_and_its_grandchild() {
    let pty = Pty::spawn(&sh("trap '' HUP; sleep 300 & echo gc=$!; wait"), 100, 30).unwrap();
    let r = pty.reader();
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
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
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
    let r = pty.reader();
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
    let got = read_until(&pty.reader(), 10, |a| has(a, b"done"));
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
    let got = read_until(&pty.reader(), 5, |a| has(a, b"/dev/"));
    let text = String::from_utf8_lossy(&got).to_string();
    let nums: Vec<&str> = text.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(nums[0], nums[1], "not a group leader: {text}");
    assert!(text.contains("/dev/tty"), "{text}");
}
