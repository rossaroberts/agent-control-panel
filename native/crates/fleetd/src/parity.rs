//! `fleetd roster-parity`: run `CliRoster` and `FileRoster` over the real profiles, log every row where they differ.
//! Read-only. Rows hold real session names and paths, so the log is gitignored and never committed.

use std::io::Write;
use std::time::{Duration, Instant};

use fleet_model::decide::RosterRow;
use fleet_read::home::home_dir;
use fleet_read::profiles::claude_profiles;
use fleet_read::roster::{CliRoster, FileRoster, RosterSource};
use serde_json::{json, Value};

fn key(r: &RosterRow) -> String {
    r.session_id
        .clone()
        .or_else(|| r.id.clone())
        .unwrap_or_default()
}

fn fields(r: &RosterRow) -> Vec<(&'static str, Value)> {
    vec![
        ("id", json!(r.id)),
        ("pid", json!(r.pid)),
        ("pid_alive", json!(r.pid_alive)),
        ("state", json!(r.state)),
        ("status", json!(r.status)),
        ("waiting_for", json!(r.waiting_for)),
        ("active", json!(r.active)),
        ("kind", json!(r.kind)),
        ("name", json!(r.name)),
        ("cwd", json!(r.cwd)),
        ("started_at", json!(r.started_at)),
    ]
}

pub fn run(args: &[String]) {
    let arg = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone());
    let rounds: u32 = arg("--rounds").and_then(|v| v.parse().ok()).unwrap_or(10);
    let interval: u64 = arg("--interval").and_then(|v| v.parse().ok()).unwrap_or(60);
    let out = arg("--out").unwrap_or_else(|| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../bench/results/roster-parity.jsonl"
        )
        .into()
    });
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out)
        .expect("open --out");
    let (cli, files) = (CliRoster::new(), FileRoster);
    let profiles = claude_profiles(&home_dir());
    eprintln!(
        "{} profiles, {rounds} rounds, every {interval}s, log {out}",
        profiles.len()
    );

    for round in 0..rounds {
        let (mut rows, mut diff_rows) = (0, 0);
        let (mut cli_ms, mut file_ms) = (0.0f64, 0.0f64);
        for p in &profiles {
            let t = Instant::now();
            let a = cli.read(p);
            cli_ms += t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            let b = files.read(p);
            file_ms += t.elapsed().as_secs_f64() * 1000.0;
            let (a, b) = match (a, b) {
                (Ok(a), Ok(b)) => (a, b),
                (a, b) => {
                    eprintln!(
                        "round {round} {}: cli {:?} files {:?}",
                        p.id,
                        a.err(),
                        b.err()
                    );
                    continue;
                }
            };
            let mut keys: Vec<String> = a.iter().chain(b.iter()).map(key).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                rows += 1;
                let (ra, rb) = (
                    a.iter().find(|r| key(r) == k),
                    b.iter().find(|r| key(r) == k),
                );
                let differing: Vec<Value> = match (ra, rb) {
                    (Some(x), Some(y)) => fields(x)
                        .into_iter()
                        .zip(fields(y))
                        .filter(|(f, g)| f.1 != g.1)
                        .map(|(f, g)| json!({"field": f.0, "cli": f.1, "files": g.1}))
                        .collect(),
                    (Some(_), None) => {
                        vec![json!({"field": "<row>", "cli": "present", "files": "missing"})]
                    }
                    (None, Some(_)) => {
                        vec![json!({"field": "<row>", "cli": "missing", "files": "present"})]
                    }
                    (None, None) => vec![],
                };
                if !differing.is_empty() {
                    diff_rows += 1;
                    let line =
                        json!({"round": round, "profile": p.id, "key": k, "diffs": differing});
                    writeln!(log, "{line}").expect("write log");
                }
            }
        }
        println!(
            "round {round}: {} profiles, {rows} rows, {diff_rows} differing; cli {cli_ms:.0} ms total, files {file_ms:.2} ms total",
            profiles.len()
        );
        if round + 1 < rounds {
            std::thread::sleep(Duration::from_secs(interval));
        }
    }
}
