//! Its own test binary: counting this process's open descriptors must not race other tests' ptys.

use std::ffi::OsString;

use fleetd::attach_cmd::Cmd;
use fleetd::pty::Pty;

fn open_fds() -> usize {
    (0..4096)
        .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
        .count()
}

#[test]
fn a_program_that_does_not_exist_is_an_error_and_leaks_no_descriptor() {
    let cmd = Cmd {
        argv: vec![OsString::from("/nonexistent/fleetd-no-such-program")],
        env: vec![],
        cwd: "/tmp".into(),
    };
    let before = open_fds();
    for _ in 0..5 {
        assert!(Pty::spawn(&cmd, 100, 30).is_err());
    }
    assert_eq!(open_fds(), before);
}
