//! The launchd agent: the plist as a pure function, install and uninstall against a temp LaunchAgents folder with
//! `launchctl` stubbed on PATH. Nothing here touches the real LaunchAgents or loads anything.

use std::fs;
use std::path::Path;
use std::process::Command;

use fleetd::service::{plist, LABEL};

#[test]
fn plist_has_what_launchd_needs() {
    let p = plist(
        Path::new("/Users/x/.local/bin/fleetd"),
        Path::new("/Users/x/Library/Logs/Fleet/fleetd.log"),
    );
    for want in [
        "<string>dev.fleet.fleetd</string>",
        "<string>/Users/x/.local/bin/fleetd</string>\n    <string>serve</string>",
        "<key>RunAtLoad</key>\n  <true/>",
        "<key>KeepAlive</key>\n  <true/>",
        "<key>NumberOfFiles</key>\n    <integer>8192</integer>",
        "<key>StandardOutPath</key>\n  <string>/Users/x/Library/Logs/Fleet/fleetd.log</string>",
        "<key>StandardErrorPath</key>\n  <string>/Users/x/Library/Logs/Fleet/fleetd.log</string>",
    ] {
        assert!(p.contains(want), "missing {want:?} in\n{p}");
    }
    assert_eq!(LABEL, "dev.fleet.fleetd");
    assert!(plist(Path::new("/a&b/<c>"), Path::new("/l")).contains("/a&amp;b/&lt;c&gt;"));
}

#[test]
fn install_is_idempotent_and_uninstall_removes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let stub = root.join("stub");
    fs::create_dir_all(&stub).unwrap();
    let calls = root.join("calls.log");
    let sh = stub.join("launchctl");
    fs::write(
        &sh,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 0\n", calls.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&sh, fs::Permissions::from_mode(0o755)).unwrap();
    let agents = root.join("LaunchAgents");
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let run = |cmd: &str| {
        let o = Command::new(env!("CARGO_BIN_EXE_fleetd"))
            .arg(cmd)
            .env("FLEETD_HOME", &root)
            .env("HOME", &root)
            .env("FLEETD_LAUNCHAGENTS_DIR", &agents)
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    run("install-service");
    run("install-service");
    let plist_file = agents.join("dev.fleet.fleetd.plist");
    let text = fs::read_to_string(&plist_file).unwrap();
    let bin = root.join(".local/bin/fleetd");
    assert!(bin.exists());
    assert!(text.contains(&format!(
        "<string>{}</string>\n    <string>serve</string>",
        bin.display()
    )));
    assert!(text.contains(&format!("{}/Library/Logs/Fleet/fleetd.log", root.display())));
    let uid = unsafe { libc::getuid() };
    let log = fs::read_to_string(&calls).unwrap();
    let want = format!(
        "bootout gui/{uid}/dev.fleet.fleetd\nbootstrap gui/{uid} {}\n",
        plist_file.display()
    );
    assert_eq!(
        log,
        want.repeat(2),
        "each install boots out, then bootstraps"
    );
    run("uninstall-service");
    assert!(!plist_file.exists());
    assert!(fs::read_to_string(&calls)
        .unwrap()
        .ends_with(&format!("bootout gui/{uid}/dev.fleet.fleetd\n")));
}
