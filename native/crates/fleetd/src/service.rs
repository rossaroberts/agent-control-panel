//! The launchd user agent: `~/Library/LaunchAgents/dev.fleet.fleetd.plist`, running `~/.local/bin/fleetd serve`.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "dev.fleet.fleetd";

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The agent's plist. `bin` is the absolute path of the installed binary, `log` the file both streams go to.
pub fn plist(bin: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>SoftResourceLimits</key>
  <dict>
    <key>NumberOfFiles</key>
    <integer>8192</integer>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        bin = esc(&bin.to_string_lossy()),
        log = esc(&log.to_string_lossy())
    )
}

fn home() -> PathBuf {
    fleet_read::home::home_dir()
}

/// `FLEETD_LAUNCHAGENTS_DIR` (tests), else `~/Library/LaunchAgents`.
fn agents_dir() -> PathBuf {
    std::env::var_os("FLEETD_LAUNCHAGENTS_DIR")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Library/LaunchAgents"))
}

pub fn plist_path() -> PathBuf {
    agents_dir().join(format!("{LABEL}.plist"))
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> io::Result<bool> {
    let out = Command::new("launchctl").args(args).output()?;
    if !out.status.success() {
        eprint!(
            "launchctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out.status.success())
}

/// Copies the running binary to `~/.local/bin/fleetd` (by rename, so a running copy is never overwritten in place),
/// writes the plist and loads it. Safe to run again: it boots out the loaded copy first.
pub fn install() -> io::Result<()> {
    let bin_dir = home().join(".local/bin");
    std::fs::create_dir_all(&bin_dir)?;
    let bin = bin_dir.join("fleetd");
    let me = std::env::current_exe()?;
    if me.canonicalize()? != bin.canonicalize().unwrap_or_default() {
        let tmp = bin_dir.join(".fleetd.new");
        std::fs::copy(&me, &tmp)?;
        std::fs::rename(&tmp, &bin)?;
    }
    let log = home().join("Library/Logs/Fleet/fleetd.log");
    std::fs::create_dir_all(log.parent().unwrap())?;
    let path = plist_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("plist.new");
    std::fs::write(&tmp, plist(&bin, &log))?;
    std::fs::rename(&tmp, &path)?;
    let (domain, target) = (domain(), format!("{}/{LABEL}", domain()));
    let _ = launchctl(&["bootout", &target]); // not loaded is fine
                                              // Right after a bootout, launchd can still be tearing the old job down and refuse (error 5): try again.
    let mut loaded = false;
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if launchctl(&["bootstrap", &domain, &path.to_string_lossy()])? {
            loaded = true;
            break;
        }
    }
    if !loaded {
        return Err(io::Error::other("launchctl bootstrap failed"));
    }
    println!("installed {} and loaded {LABEL}", path.display());
    Ok(())
}

/// Unloads the agent and removes its plist. The binary and the log stay.
pub fn uninstall() -> io::Result<()> {
    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    match std::fs::remove_file(plist_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    println!("unloaded {LABEL}");
    Ok(())
}
