//! The `tailscale` CLI, the same on macOS and Linux (Ruling P5-R3): this machine's IPv4, its own user id, and who a peer
//! is. Each call runs the CLI with a timeout and kills it past that.

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Who a tailnet peer is: its user, and whether the node is tagged (a tagged node is owned by its tags, not a person).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub user: u64,
    pub tagged: bool,
}

/// A node `tailscale status --json` lists (this machine's own included): its addresses, its user, whether it is tagged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Known {
    /// `OS` as the CLI says it (`iOS`, `macOS`, `linux`, ...), for the diagnostic listing.
    pub os: String,
    pub ips: Vec<IpAddr>,
    pub user: u64,
    pub tagged: bool,
}

/// What `tailscale status --json` says: this machine's user (`Self.UserID`), and every node it knows (Ruling P5-R7: the
/// listener pre-allows the ones of this user that are untagged).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub self_user: u64,
    pub nodes: Vec<Known>,
}

pub struct Cli {
    pub bin: PathBuf,
    pub timeout: Duration,
}

/// Where the CLI is looked for when it is not on `PATH`.
const PLACES: [&str; 3] = [
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

impl Cli {
    /// `FLEETD_TAILSCALE`, else `tailscale` on `PATH`, else the first of `PLACES` that exists; `None` when there is none.
    /// Each call may take up to 2 s.
    pub fn find() -> Option<Cli> {
        let bin = std::env::var_os("FLEETD_TAILSCALE")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("PATH").and_then(|p| {
                    std::env::split_paths(&p)
                        .map(|d| d.join("tailscale"))
                        .find(|f| f.is_file())
                })
            })
            .or_else(|| PLACES.iter().map(PathBuf::from).find(|f| f.is_file()))?;
        Some(Cli {
            bin,
            timeout: Duration::from_secs(2),
        })
    }

    /// `tailscale ip -4`: this machine's tailnet IPv4.
    pub fn ipv4(&self) -> io::Result<Ipv4Addr> {
        let out = self.run(&["ip", "-4"])?;
        let text = String::from_utf8_lossy(&out);
        let first = text.lines().next().unwrap_or("").trim();
        first
            .parse()
            .map_err(|_| invalid(format!("ip -4 said {first:?}")))
    }

    /// `Self.UserID` of `tailscale status --json`.
    pub fn self_user(&self) -> io::Result<u64> {
        Ok(self.status()?.self_user)
    }

    /// `tailscale status --json`: `Self.UserID`, and each of `Self` and `Peer.*` as a `Known`.
    pub fn status(&self) -> io::Result<Status> {
        parse_status(&self.json(&["status", "--json"])?)
    }

    /// `tailscale whois --json <ip:port>`: `UserProfile.ID`, and whether `Node.Tags` holds any tag.
    pub fn whois(&self, peer: SocketAddr) -> io::Result<Peer> {
        let v = self.json(&["whois", "--json", &peer.to_string()])?;
        let user = v
            .pointer("/UserProfile/ID")
            .and_then(|u| u.as_u64())
            .ok_or_else(|| invalid("whois --json has no UserProfile.ID".into()))?;
        let tagged = v
            .pointer("/Node/Tags")
            .and_then(|t| t.as_array())
            .is_some_and(|t| !t.is_empty());
        Ok(Peer { user, tagged })
    }

    fn json(&self, args: &[&str]) -> io::Result<serde_json::Value> {
        let out = self.run(args)?;
        serde_json::from_slice(&out).map_err(|e| invalid(format!("{}: {e}", args[0])))
    }

    /// Runs the CLI; its stdout. An exit other than 0 is an error carrying its first stderr line; past `timeout` it is
    /// killed and the error is `TimedOut`.
    fn run(&self, args: &[&str]) -> io::Result<Vec<u8>> {
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut o = vec![];
            let mut e = vec![];
            let _ = out.read_to_end(&mut o);
            let _ = err.read_to_end(&mut e);
            let _ = tx.send((o, e));
        });
        let Ok((o, e)) = rx.recv_timeout(self.timeout) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} timed out", args[0]),
            ));
        };
        let status = child.wait()?;
        if !status.success() {
            let e = String::from_utf8_lossy(&e);
            let why = e.lines().next().unwrap_or("").trim();
            return Err(io::Error::other(format!(
                "{} exited {status}: {why}",
                args[0]
            )));
        }
        Ok(o)
    }
}

fn parse_status(v: &serde_json::Value) -> io::Result<Status> {
    let self_user = v
        .pointer("/Self/UserID")
        .and_then(|u| u.as_u64())
        .ok_or_else(|| invalid("status --json has no Self.UserID".into()))?;
    let node = |n: &serde_json::Value| -> Option<Known> {
        Some(Known {
            os: n
                .get("OS")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string(),
            ips: n
                .get("TailscaleIPs")?
                .as_array()?
                .iter()
                .filter_map(|i| i.as_str()?.parse().ok())
                .collect(),
            user: n.get("UserID")?.as_u64()?,
            tagged: n
                .get("Tags")
                .and_then(|t| t.as_array())
                .is_some_and(|t| !t.is_empty()),
        })
    };
    let mut nodes: Vec<Known> = v.get("Self").and_then(node).into_iter().collect();
    if let Some(peers) = v.get("Peer").and_then(|p| p.as_object()) {
        nodes.extend(peers.values().filter_map(node));
    }
    Ok(Status { self_user, nodes })
}

fn invalid(m: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn cli(dir: &std::path::Path, body: &str) -> Cli {
        let bin = dir.join("tailscale");
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Cli {
            bin,
            timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn reads_ip_self_and_whois() {
        let d = tempfile::tempdir().unwrap();
        let c = cli(
            d.path(),
            r#"case "$1" in
  ip) echo 100.64.0.7 ;;
  status) echo '{"Self": {"UserID": 12345}}' ;;
  whois) echo '{"Node": {"Tags": ["tag:server"]}, "UserProfile": {"ID": 777}}' ;;
esac"#,
        );
        assert_eq!(c.ipv4().unwrap(), Ipv4Addr::new(100, 64, 0, 7));
        assert_eq!(c.self_user().unwrap(), 12345);
        let p = c.whois("100.64.0.8:5000".parse().unwrap()).unwrap();
        assert_eq!(
            p,
            Peer {
                user: 777,
                tagged: true
            }
        );
    }

    #[test]
    fn status_lists_self_and_peers() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"Self": {"UserID": 5, "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]},
                "Peer": {"nodekey:a": {"UserID": 5, "TailscaleIPs": ["100.64.0.2"]},
                         "nodekey:b": {"UserID": 9, "Tags": ["tag:ci"], "TailscaleIPs": ["100.64.0.3"]}}}"#,
        )
        .unwrap();
        let s = parse_status(&v).unwrap();
        assert_eq!(s.self_user, 5);
        assert_eq!(s.nodes.len(), 3);
        assert_eq!(s.nodes[0].ips.len(), 2);
        assert!(s.nodes.iter().any(|n| n.user == 9 && n.tagged));
        assert!(s.nodes.iter().any(|n| n.user == 5
            && !n.tagged
            && n.ips == vec!["100.64.0.2".parse::<IpAddr>().unwrap()]));
    }

    #[test]
    fn untagged_when_tags_are_absent_or_null() {
        let d = tempfile::tempdir().unwrap();
        let c = cli(
            d.path(),
            r#"echo '{"Node": {"Tags": null}, "UserProfile": {"ID": 1}}'"#,
        );
        assert!(!c.whois("100.64.0.8:5000".parse().unwrap()).unwrap().tagged);
    }

    #[test]
    fn a_failing_or_slow_cli_is_an_error_in_time() {
        let d = tempfile::tempdir().unwrap();
        let c = cli(d.path(), "echo 'not logged in' >&2; exit 1");
        let e = c.ipv4().unwrap_err();
        assert!(e.to_string().contains("not logged in"), "{e}");
        let mut c = cli(d.path(), "exec sleep 5");
        c.timeout = Duration::from_millis(300);
        let t0 = std::time::Instant::now();
        assert_eq!(c.self_user().unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
}
