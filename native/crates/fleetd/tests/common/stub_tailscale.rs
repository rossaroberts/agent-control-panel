//! A stand-in `tailscale` CLI for the network tests: a shell script in a temp folder, pointed at by `FLEETD_TAILSCALE`.
//! It answers from files beside it, so a test can change its answers while a daemon runs:
//! - `ip -4`: the contents of `ip`; while `ip-fails` holds a number above 0, it fails instead (and counts it down);
//! - `status --json`: `{"Self": {"UserID": <self-user>}}`;
//! - `whois --json <ip:port>`: `whois.json`, after sleeping `slow` seconds when that file exists.
//!
//! Every call is appended to `calls.log`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SCRIPT: &str = r#"#!/bin/sh
D="$(cd "$(dirname "$0")" && pwd)"
echo "$*" >> "$D/calls.log"
case "$1" in
  ip)
    n=$(cat "$D/ip-fails" 2>/dev/null || echo 0)
    if [ "$n" -gt 0 ]; then
      echo $((n - 1)) > "$D/ip-fails"
      echo "stub: not running" >&2
      exit 1
    fi
    cat "$D/ip" ;;
  status) printf '{"Self": {"ID": "nodeSelf", "UserID": %s}}\n' "$(cat "$D/self-user")" ;;
  whois)
    [ -f "$D/slow" ] && sleep "$(cat "$D/slow")"
    cat "$D/whois.json" ;;
  *) echo "stub: $*" >&2; exit 1 ;;
esac
"#;

pub struct StubTailscale {
    dir: PathBuf,
}

impl StubTailscale {
    /// A stub in `dir/tailscale-stub/` answering `ip` with `ip`, whose own user is 1001 and whose whois answers user 1001,
    /// untagged.
    pub fn new(dir: &Path, ip: &str) -> StubTailscale {
        let dir = dir.join("tailscale-stub");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("tailscale");
        std::fs::write(&bin, SCRIPT).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = StubTailscale { dir };
        s.set_ip(ip);
        s.set_self(1001);
        s.set_whois(1001, &[]);
        s
    }

    /// What `FLEETD_TAILSCALE` names.
    pub fn bin(&self) -> PathBuf {
        self.dir.join("tailscale")
    }

    pub fn set_ip(&self, ip: &str) {
        std::fs::write(self.dir.join("ip"), format!("{ip}\n")).unwrap();
    }

    /// `ip -4` fails the next `n` times.
    pub fn fail_ip(&self, n: u32) {
        std::fs::write(self.dir.join("ip-fails"), format!("{n}\n")).unwrap();
    }

    pub fn set_self(&self, user: u64) {
        std::fs::write(self.dir.join("self-user"), user.to_string()).unwrap();
    }

    pub fn set_whois(&self, user: u64, tags: &[&str]) {
        let tags = serde_json::to_string(tags).unwrap();
        let body = format!(
            r#"{{"Node": {{"ID": 7, "Name": "peer.example.ts.net.", "Tags": {tags}}}, "UserProfile": {{"ID": {user}, "LoginName": "fixture@example.com"}}}}"#
        );
        std::fs::write(self.dir.join("whois.json"), body).unwrap();
    }

    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}
