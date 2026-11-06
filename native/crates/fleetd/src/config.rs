//! `fleetd.json` (whether and where the TCP listener listens, and the hub's remotes), the key file beside it, and where
//! both live. Only read, except the key, which is made once (0600) when a listener first needs it.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The TCP port (Global Constraints), unless `--port` or the config's `port` says otherwise.
pub const DEFAULT_PORT: u16 = 47470;

/// The key file's name, beside the config.
pub const KEY_FILE: &str = "fleetd.key";

/// What the TCP listener binds: nothing, `127.0.0.1` only, or `127.0.0.1` and this machine's Tailscale IPv4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Listen {
    #[default]
    Off,
    Loopback,
    Tailscale,
}

impl std::str::FromStr for Listen {
    type Err = String;
    fn from_str(s: &str) -> Result<Listen, String> {
        match s {
            "off" => Ok(Listen::Off),
            "loopback" => Ok(Listen::Loopback),
            "tailscale" => Ok(Listen::Tailscale),
            _ => Err(format!("--listen {s}: one of off, loopback, tailscale")),
        }
    }
}

/// A machine whose daemon a hub follows: its board name and `host:port`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Remote {
    pub machine: String,
    pub addr: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: Listen,
    pub port: u16,
    pub hub: Vec<Remote>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            listen: Listen::Off,
            port: DEFAULT_PORT,
            hub: vec![],
        }
    }
}

/// Where `fleetd.json` is: macOS `~/Library/Application Support/Fleet/`; Linux `$XDG_CONFIG_HOME/fleet/`, else
/// `~/.config/fleet/`.
pub fn default_path() -> PathBuf {
    let home = fleet_read::home::home_dir();
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support/Fleet/fleetd.json");
    }
    match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        Some(d) => PathBuf::from(d).join("fleet/fleetd.json"),
        None => home.join(".config/fleet/fleetd.json"),
    }
}

/// The config at `path` (else `default_path()`). A missing file is the default: no listener, port 47470, no hub. A file
/// that is there but is not this shape is an error, never a silent default.
pub fn load(path: Option<&Path>) -> io::Result<Config> {
    let path = path.map(Path::to_path_buf).unwrap_or_else(default_path);
    match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    }
}

/// The folder the key lives in for a config at `path` (else the default one).
pub fn dir_of(path: Option<&Path>) -> PathBuf {
    let path = path.map(Path::to_path_buf).unwrap_or_else(default_path);
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// The loopback key: `dir/fleetd.key`, 64 hex characters (a trailing newline is allowed), made 0600 from `/dev/urandom`
/// the first time. A key file that is not a regular file of this user's, is readable by anyone else, or does not hold a
/// key is an error: it is never rewritten.
pub fn key(dir: &Path) -> io::Result<[u8; 32]> {
    let path = dir.join(KEY_FILE);
    match read_key(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        r => return r,
    }
    std::fs::create_dir_all(dir)?;
    let mut raw = [0u8; 32];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut raw)?;
    }
    // Written whole under a temp name, then linked into place: a reader never sees half a key, and a key another process
    // made meanwhile wins (link refuses to replace it).
    let tmp = dir.join(format!(".{KEY_FILE}.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let made: io::Result<()> = (|| {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(format!("{}\n", hex(&raw)).as_bytes())?;
        f.sync_all()?;
        match std::fs::hard_link(&tmp, &path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            r => r,
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    made?;
    read_key(&path)
}

fn read_key(path: &Path) -> io::Result<[u8; 32]> {
    use std::os::unix::fs::MetadataExt;
    let bad = |why: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {why}", path.display()),
        )
    };
    let m = std::fs::symlink_metadata(path)?;
    if !m.file_type().is_file() {
        return Err(bad("not a regular file".into()));
    }
    // SAFETY: geteuid has no preconditions.
    if m.uid() != unsafe { libc::geteuid() } {
        return Err(bad("not owned by this user".into()));
    }
    if m.mode() & 0o077 != 0 {
        return Err(bad(format!(
            "mode {:o} is looser than 0600; left alone",
            m.mode() & 0o777
        )));
    }
    let text = std::fs::read(path)?;
    let text = text.strip_suffix(b"\n").unwrap_or(&text);
    unhex(text).ok_or_else(|| bad("not 64 hex characters; left alone".into()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(t: &[u8]) -> Option<[u8; 32]> {
    if t.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in t.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_missing_file_is_off_on_47470() {
        let d = tempfile::tempdir().unwrap();
        let c = load(Some(&d.path().join("fleetd.json"))).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!((c.listen, c.port), (Listen::Off, 47470));
    }

    #[test]
    fn a_file_is_read_and_a_bad_one_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("fleetd.json");
        std::fs::write(
            &p,
            r#"{"listen": "tailscale", "hub": [{"machine": "m1", "addr": "100.64.0.9:47470"}]}"#,
        )
        .unwrap();
        let c = load(Some(&p)).unwrap();
        assert_eq!(c.listen, Listen::Tailscale);
        assert_eq!(c.port, 47470);
        assert_eq!(
            c.hub,
            vec![Remote {
                machine: "m1".into(),
                addr: "100.64.0.9:47470".into()
            }]
        );
        std::fs::write(&p, r#"{"listen": "everywhere"}"#).unwrap();
        assert_eq!(
            load(Some(&p)).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn the_key_is_made_once_0600_and_kept() {
        let d = tempfile::tempdir().unwrap();
        let k = key(d.path()).unwrap();
        let p = d.path().join(KEY_FILE);
        let m = std::fs::metadata(&p).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap().trim().len(), 64);
        assert_eq!(key(d.path()).unwrap(), k);
        assert_ne!(k, [0u8; 32]);
    }

    #[test]
    fn a_loose_or_bad_key_is_an_error_and_not_rewritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(KEY_FILE);
        std::fs::write(&p, "ab".repeat(32)).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = key(d.path()).unwrap_err();
        assert!(e.to_string().contains("looser than 0600"), "{e}");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(key(d.path()).unwrap(), [0xab; 32]);
        std::fs::write(&p, "not a key").unwrap();
        assert!(key(d.path()).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "not a key");
    }
}
