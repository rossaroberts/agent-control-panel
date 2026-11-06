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

fn opts(args: &[String]) -> Result<Opts, String> {
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
    Ok(Opts {
        n: num("--n", 50)?,
        gap: Duration::from_millis(num("--gap-ms", 10)? as u64),
        profile: get("--profile").unwrap_or_default().into(),
        sid: get("--sid").unwrap_or_default(),
        transcript: get("--transcript").unwrap_or_default().into(),
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
    let o = opts(args).map_err(io::Error::other)?;
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
