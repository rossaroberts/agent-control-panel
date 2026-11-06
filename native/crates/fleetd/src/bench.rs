//! The bench's client side (`fleetd bench <hook|transcript|snapshot> ...`, hidden from the usage line). It connects to a
//! running `fleetd serve`, makes the change the mode names, and times, in this process, how long the matching frame takes
//! to arrive. `native/bench/run.mjs` starts the daemon over a fixture home and reads the JSON this prints:
//! `{"ms":[...]}`, one number per sample, in milliseconds. Reading postcard from Node would add a decoder to the
//! measurement; this keeps the clock on one side of the socket.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fleet_model::wire::Frame;
use serde_json::json;
use tokio::time::timeout;

use crate::client::Client;
use crate::serve::socket_path;

const WAIT: Duration = Duration::from_secs(3);

struct Opts {
    mode: String,
    n: usize,
    gap: Duration,
    profile: PathBuf,
    sid: String,
    transcript: PathBuf,
}

/// The bench writes hook files and appends to transcripts, so it is for fixtures only: it runs only with `FLEETD_HOME`
/// set, and only on a profile and a transcript that lie under it (Global Constraints: fleetd never writes to Claude
/// Code's or Codex's records). `home` is the value of `FLEETD_HOME`, if any.
fn opts(args: &[String], home: Option<PathBuf>) -> Result<Opts, String> {
    let home = home
        .filter(|h| !h.as_os_str().is_empty())
        .ok_or("bench: refusing to run without FLEETD_HOME (it is for fixture homes only)")?;
    let home = home
        .canonicalize()
        .map_err(|e| format!("bench: FLEETD_HOME: {e}"))?;
    let mode = args.first().cloned().ok_or("bench: a mode is needed")?;
    let get = |k: &str| {
        args.windows(2)
            .find(|w| w[0] == k)
            .map(|w| w[1].clone())
            .ok_or(format!("bench: {k} is needed"))
    };
    let num = |k: &str, d: usize| {
        get(k).ok().map_or(Ok(d), |v| {
            v.parse().map_err(|_| format!("bench: {k} is a number"))
        })
    };
    let under_home = |k: &str| -> Result<PathBuf, String> {
        let p = PathBuf::from(get(k)?)
            .canonicalize()
            .map_err(|e| format!("bench: {k}: {e}"))?;
        if p.starts_with(&home) {
            Ok(p)
        } else {
            Err(format!("bench: {k} is not under FLEETD_HOME"))
        }
    };
    let (mut profile, mut sid, mut transcript) = (PathBuf::new(), String::new(), PathBuf::new());
    match mode.as_str() {
        "hook" => profile = under_home("--profile")?,
        "transcript" => transcript = under_home("--transcript")?,
        "snapshot" => {}
        m => return Err(format!("bench: unknown mode {m}")),
    }
    if mode != "snapshot" {
        sid = get("--sid")?;
        if sid.len() != 36
            || !sid
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || b == b'-')
        {
            return Err("bench: --sid must match ^[0-9a-f-]{36}$".into());
        }
    }
    Ok(Opts {
        n: num("--n", 50)?,
        gap: Duration::from_millis(num("--gap-ms", 10)? as u64),
        profile,
        sid,
        transcript,
        mode,
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A hook event as hook.sh writes it: a temp file renamed over `<session>.json`. Returns its `at`.
fn hook(o: &Opts, e: &str) -> io::Result<i64> {
    let at = now_ms();
    let d = o.profile.join("fleet-state");
    let tmp = d.join(format!(".{}.{}", o.sid, std::process::id()));
    let ev = json!({"e": e, "t": null, "n": null, "r": null, "i": null, "at": at});
    fs::write(&tmp, format!("{ev}\n"))?;
    fs::rename(&tmp, d.join(format!("{}.json", o.sid)))?;
    Ok(at)
}

fn append(o: &Opts, text: &str) -> io::Result<()> {
    let line = json!({"type":"assistant","timestamp":"2026-11-06T12:00:00.000Z",
        "message":{"model":"claude-fixture-2","stop_reason":"end_turn",
                   "content":[{"type":"text","text":text}],"usage":{"input_tokens":7}}});
    let mut f = fs::OpenOptions::new().append(true).open(&o.transcript)?;
    writeln!(f, "{line}")
}

/// Reads frames until a delta upserts this session with `want`, and returns the time since `t0`.
async fn until(
    c: &mut Client,
    sid: &str,
    t0: Instant,
    want: impl Fn(&fleet_model::types::Session) -> bool,
) -> io::Result<f64> {
    loop {
        let f = timeout(WAIT, c.next())
            .await
            .map_err(|_| io::Error::other("bench: no delta in 3 s"))??;
        if let Frame::Delta(d) = f {
            if d.upsert.iter().any(|s| s.key.session_id == sid && want(s)) {
                return Ok(t0.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
}

/// Connects and reads the opening snapshot, so what follows is deltas only.
async fn connected() -> io::Result<Client> {
    let mut c = Client::connect(&socket_path(), None).await?;
    loop {
        if matches!(timeout(WAIT, c.next()).await??, Frame::Snapshot { .. }) {
            // A caught-up client may also be sent an empty delta; reading on skips it.
            return Ok(c);
        }
    }
}

pub async fn run(args: &[String]) -> io::Result<()> {
    let home = std::env::var_os("FLEETD_HOME").map(PathBuf::from);
    let o = opts(args, home).map_err(io::Error::other)?;
    let mut ms = Vec::with_capacity(o.n);
    match o.mode.as_str() {
        "hook" => {
            let mut c = connected().await?;
            for i in 0..o.n {
                let e = if i % 2 == 0 {
                    "UserPromptSubmit"
                } else {
                    "Stop"
                };
                let t0 = Instant::now();
                let at = hook(&o, e)?;
                ms.push(until(&mut c, &o.sid, t0, |s| s.hook_at == Some(at)).await?);
                tokio::time::sleep(o.gap).await;
            }
        }
        "transcript" => {
            let mut c = connected().await?;
            for i in 0..o.n {
                let text = format!("bench reply {i} {}", now_ms());
                let t0 = Instant::now();
                append(&o, &text)?;
                ms.push(
                    until(&mut c, &o.sid, t0, |s| {
                        s.said.as_deref() == Some(text.as_str())
                    })
                    .await?,
                );
                tokio::time::sleep(o.gap).await;
            }
        }
        "snapshot" => {
            for _ in 0..o.n {
                let t0 = Instant::now();
                connected().await?;
                ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                tokio::time::sleep(o.gap).await;
            }
        }
        m => return Err(io::Error::other(format!("bench: unknown mode {m}"))),
    }
    println!("{}", json!({ "ms": ms }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SID: &str = "bbbbbbbb-0000-4000-8000-000000000001";

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A home with a profile and a transcript, and a second folder outside it.
    fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude-work/fleet-state")).unwrap();
        fs::write(home.path().join("t.jsonl"), "").unwrap();
        (home, tempfile::tempdir().unwrap())
    }

    fn err(a: &[&str], home: Option<PathBuf>) -> String {
        opts(&args(a), home).err().expect("a refusal")
    }

    #[test]
    fn a_fixture_home_is_accepted() {
        let (h, _) = fixture();
        let p = h.path().join(".claude-work");
        let t = h.path().join("t.jsonl");
        let o = opts(
            &args(&["hook", "--profile", p.to_str().unwrap(), "--sid", SID]),
            Some(h.path().into()),
        )
        .unwrap();
        assert_eq!(o.profile, p.canonicalize().unwrap());
        let o = opts(
            &args(&[
                "transcript",
                "--transcript",
                t.to_str().unwrap(),
                "--sid",
                SID,
            ]),
            Some(h.path().into()),
        )
        .unwrap();
        assert_eq!(o.transcript, t.canonicalize().unwrap());
        assert!(opts(&args(&["snapshot"]), Some(h.path().into())).is_ok());
    }

    #[test]
    fn it_refuses_without_fleetd_home() {
        let (h, _) = fixture();
        let p = h.path().join(".claude-work");
        let a = ["hook", "--profile", p.to_str().unwrap(), "--sid", SID];
        assert!(err(&a, None).contains("FLEETD_HOME"));
        assert!(err(&a, Some(PathBuf::new())).contains("FLEETD_HOME"));
    }

    #[test]
    fn it_refuses_a_profile_or_transcript_outside_the_home() {
        let (h, other) = fixture();
        let out = other.path().to_str().unwrap();
        assert!(err(
            &["hook", "--profile", out, "--sid", SID],
            Some(h.path().into())
        )
        .contains("not under FLEETD_HOME"));
        let f = other.path().join("t.jsonl");
        fs::write(&f, "").unwrap();
        assert!(err(
            &[
                "transcript",
                "--transcript",
                f.to_str().unwrap(),
                "--sid",
                SID
            ],
            Some(h.path().into())
        )
        .contains("not under FLEETD_HOME"));
    }

    #[test]
    fn it_refuses_dot_dot_traversal_out_of_the_home() {
        let (h, other) = fixture();
        let sneaky = format!(
            "{}/.claude-work/../../{}",
            h.path().display(),
            other.path().strip_prefix("/").unwrap().display()
        );
        let e = err(
            &["hook", "--profile", &sneaky, "--sid", SID],
            Some(h.path().into()),
        );
        assert!(
            e.contains("not under FLEETD_HOME") || e.contains("--profile"),
            "{e}"
        );
    }

    #[test]
    fn it_refuses_a_symlink_that_leaves_the_home() {
        let (h, other) = fixture();
        let link = h.path().join("link");
        std::os::unix::fs::symlink(other.path(), &link).unwrap();
        assert!(err(
            &["hook", "--profile", link.to_str().unwrap(), "--sid", SID],
            Some(h.path().into())
        )
        .contains("not under FLEETD_HOME"));
    }

    #[test]
    fn hook_needs_a_profile_and_transcript_needs_a_transcript() {
        let (h, _) = fixture();
        assert!(err(&["hook", "--sid", SID], Some(h.path().into())).contains("--profile is needed"));
        assert!(err(&["transcript", "--sid", SID], Some(h.path().into()))
            .contains("--transcript is needed"));
    }

    #[test]
    fn it_refuses_a_session_id_that_is_not_36_of_hex_and_dash() {
        let (h, _) = fixture();
        let p = h.path().join(".claude-work");
        let p = p.to_str().unwrap();
        for bad in [
            "",
            "../../../etc/passwd",
            "BBBBBBBB-0000-4000-8000-000000000001",
            "bbbbbbbb-0000-4000-8000-00000000000g",
            "bbbbbbbb-0000-4000-8000-00000000000",
            "bbbbbbbb-0000-4000-8000-0000000000011",
        ] {
            let e = err(
                &["hook", "--profile", p, "--sid", bad],
                Some(h.path().into()),
            );
            assert!(e.contains("--sid"), "{bad}: {e}");
        }
        assert!(err(&["hook", "--profile", p], Some(h.path().into())).contains("--sid is needed"));
    }

    #[test]
    fn an_unknown_mode_is_refused() {
        let (h, _) = fixture();
        assert!(err(&["frob"], Some(h.path().into())).contains("unknown mode"));
    }
}
