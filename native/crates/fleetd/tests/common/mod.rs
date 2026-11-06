//! A process under a chosen program name (`codex`, `muse`) for the process-scan tests: this test binary, copied under
//! that name and run as its own `helper_hold` test, which opens the files named in `FLEETD_TEST_HOLD`, writes
//! `FLEETD_TEST_READY` and waits until `FLEETD_TEST_RELEASE` exists. (A copied system binary is not allowed to run; this
//! one is ours.) It is started through a shell that leaves at once, so it is not a child of the test process: the scan
//! skips its own children, as Node's does.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub struct Held {
    pid: u32,
    release: PathBuf,
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

impl Held {
    /// Runs a copy of this binary at `exe`, in `cwd`, holding `files` open, once it says it is ready.
    pub fn start(exe: &Path, files: &[&Path], cwd: &Path) -> Held {
        std::fs::copy(std::env::current_exe().unwrap(), exe).unwrap();
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tag = format!("{}-{n}", std::process::id());
        let ready = std::env::temp_dir().join(format!("fleetd-held-{tag}.ready"));
        let release = std::env::temp_dir().join(format!("fleetd-held-{tag}.release"));
        let _ = std::fs::remove_file(&ready);
        let _ = std::fs::remove_file(&release);
        let hold = files
            .iter()
            .map(|f| f.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        let out = Command::new("/bin/sh")
            .args(["-c", "\"$0\" \"$@\" </dev/null >/dev/null 2>&1 & echo $!"])
            .arg(exe)
            .args(["--exact", "helper_hold", "--nocapture", "--test-threads=1"])
            .env("FLEETD_TEST_HOLD", hold)
            .env("FLEETD_TEST_READY", &ready)
            .env("FLEETD_TEST_RELEASE", &release)
            .current_dir(cwd)
            .output()
            .unwrap();
        let pid: u32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let t0 = Instant::now();
        while !ready.exists() {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "the helper did not start"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&ready);
        Held { pid, release }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Lets it go and waits for it to exit.
    pub fn stop(self) {
        std::fs::write(&self.release, "").unwrap();
        let t0 = Instant::now();
        while alive(self.pid) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&self.release);
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if alive(self.pid) {
            // SAFETY: the helper this test started.
            unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

/// The body of `helper_hold`: nothing unless run by `Held`.
pub fn hold() {
    let Ok(files) = std::env::var("FLEETD_TEST_HOLD") else {
        return;
    };
    let _open: Vec<_> = files
        .lines()
        .filter(|l| !l.is_empty())
        .map(|f| std::fs::File::open(f).unwrap())
        .collect();
    std::fs::write(std::env::var("FLEETD_TEST_READY").unwrap(), "").unwrap();
    let release = PathBuf::from(std::env::var("FLEETD_TEST_RELEASE").unwrap());
    let t0 = Instant::now();
    while !release.exists() && t0.elapsed() < Duration::from_secs(120) {
        std::thread::sleep(Duration::from_millis(10));
    }
}
