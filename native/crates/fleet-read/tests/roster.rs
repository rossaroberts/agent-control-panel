use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fleet_read::profiles::Profile;
use fleet_read::roster::{child_env, pid_alive, CliRoster, FileRoster, RosterError, RosterSource};

fn script(dir: &Path, body: &str) -> PathBuf {
    let f = dir.join("claude");
    fs::write(&f, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
    f
}

fn profile(dir: &Path, plain: bool) -> Profile {
    Profile {
        id: "t".into(),
        dir: dir.to_path_buf(),
        plain,
        config: dir.join(".claude.json"),
    }
}

#[test]
fn cli_roster_parses_rows_and_marks_running_ones_active() {
    let bin = tempfile::tempdir().unwrap();
    let me = std::process::id();
    let all = format!(
        r#"[{{"id":"aaaaaaaa","sessionId":"s-run","pid":{me},"state":"working","status":"busy","kind":"background","name":"one","cwd":"/w/one","startedAt":1700000000000,"waitingFor":"permission prompt"}},
            {{"id":"bbbbbbbb","sessionId":"s-dead","pid":999999,"state":"stopped","kind":"background"}},
            {{"id":"cccccccc","sessionId":"s-done","state":"done","kind":"background"}},
            {{"id":null,"sessionId":"s-tty","pid":{me},"status":"idle","kind":"interactive"}},
            {{"id":"dddddddd","name":"no session id"}}]"#
    );
    let running = r#"[{"id":"aaaaaaaa","sessionId":"s-run"},{"id":null,"sessionId":"s-tty"}]"#;
    let c = script(
        bin.path(),
        &format!(
            "case \"$*\" in *--all*) cat <<'EOF'\n{all}\nEOF\n;; *) cat <<'EOF'\n{running}\nEOF\n;; esac"
        ),
    );
    let dir = tempfile::tempdir().unwrap();
    let rows = CliRoster::with_claude(c)
        .read(&profile(dir.path(), false))
        .unwrap();
    assert_eq!(rows.len(), 4, "a row with no sessionId is not a session");

    let run = &rows[0];
    assert_eq!(run.id.as_deref(), Some("aaaaaaaa"));
    assert_eq!(run.session_id.as_deref(), Some("s-run"));
    assert_eq!(run.pid, Some(me));
    assert!(run.pid_alive && run.active);
    assert_eq!(run.state.as_deref(), Some("working"));
    assert_eq!(run.status.as_deref(), Some("busy"));
    assert_eq!(run.waiting_for.as_deref(), Some("permission prompt"));
    assert_eq!(run.kind.as_deref(), Some("background"));
    assert_eq!(run.name.as_deref(), Some("one"));
    assert_eq!(run.cwd.as_deref(), Some("/w/one"));
    assert_eq!(run.started_at, Some(1_700_000_000_000));

    let dead = &rows[1];
    assert_eq!(dead.pid, Some(999_999));
    assert!(!dead.pid_alive && !dead.active);
    assert!(!rows[2].active && rows[2].pid.is_none() && !rows[2].pid_alive);
    assert!(rows[3].active && rows[3].id.is_none());
}

#[test]
fn a_pidless_running_job_with_a_stale_record_is_not_active() {
    let bin = tempfile::tempdir().unwrap();
    let row = r#"[{"id":"eeeeeeee","sessionId":"s-old","state":"blocked","kind":"background"}]"#;
    let c = script(bin.path(), &format!("cat <<'EOF'\n{row}\nEOF"));
    let dir = tempfile::tempdir().unwrap();
    let job = dir.path().join("jobs/eeeeeeee");
    fs::create_dir_all(&job).unwrap();
    fs::write(
        job.join("state.json"),
        r#"{"state":"blocked","updatedAt":"2020-01-01T00:00:00.000Z"}"#,
    )
    .unwrap();
    let rows = CliRoster::with_claude(c)
        .read(&profile(dir.path(), false))
        .unwrap();
    assert!(!rows[0].active);
}

#[test]
fn config_dir_is_set_unless_plain_and_markers_are_stripped() {
    let bin = tempfile::tempdir().unwrap();
    // Echo CLAUDE_CONFIG_DIR and whether any FLEET_/CLAUDE marker survived, through `name`.
    let c = script(
        bin.path(),
        r#"case "$*" in *--all*) printf '[{"id":"aaaaaaaa","sessionId":"s1","name":"cfg=%s leaked=%s"}]' "${CLAUDE_CONFIG_DIR-unset}" "$(env | grep -c '^FLEET_')";; *) echo '[]';; esac"#,
    );
    let dir = tempfile::tempdir().unwrap();
    let r = CliRoster::with_claude(c);
    let named = r.read(&profile(dir.path(), false)).unwrap()[0]
        .name
        .clone()
        .unwrap();
    assert_eq!(named, format!("cfg={} leaked=0", dir.path().display()));
    let plain = r.read(&profile(dir.path(), true)).unwrap()[0]
        .name
        .clone()
        .unwrap();
    assert_eq!(plain, "cfg=unset leaked=0");

    let vars = ["CLAUDE_X", "CLAUDECODE", "FLEET_A", "HOME", "PATH", "LANG"]
        .map(|k| (k.to_string(), format!("v-{k}")));
    let env = child_env(
        vars.into_iter(),
        Path::new("/h"),
        &profile(Path::new("/h/.claude-a"), false),
    );
    let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["HOME", "LANG", "PATH", "CLAUDE_CONFIG_DIR"]);
    let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
    assert!(path.starts_with("/h/.local/bin:/opt/homebrew/bin:") && path.ends_with(":v-PATH"));
}

#[test]
fn a_slow_claude_times_out_within_the_budget() {
    let bin = tempfile::tempdir().unwrap();
    let c = script(bin.path(), "sleep 9\necho '[]'");
    let dir = tempfile::tempdir().unwrap();
    let t = Instant::now();
    let r = CliRoster::with_claude(c).read(&profile(dir.path(), false));
    assert_eq!(r.unwrap_err(), RosterError::Timeout);
    assert!(
        t.elapsed() < Duration::from_millis(8500),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn failing_and_garbled_claude_are_errors_not_empty_rosters() {
    let dir = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let c = script(bin.path(), "echo boom >&2; exit 1");
    assert_eq!(
        CliRoster::with_claude(c)
            .read(&profile(dir.path(), false))
            .unwrap_err(),
        RosterError::Failed("boom".into())
    );
    let c = script(bin.path(), "echo not-json");
    assert!(matches!(
        CliRoster::with_claude(c).read(&profile(dir.path(), false)),
        Err(RosterError::Parse(_))
    ));
    let r = CliRoster::with_claude(bin.path().join("missing"));
    assert!(matches!(
        r.read(&profile(dir.path(), false)),
        Err(RosterError::Spawn(_))
    ));
}

#[test]
fn pid_liveness() {
    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(999_999));
    assert!(!pid_alive(0));
}

#[test]
fn file_roster_joins_jobs_with_their_process_records() {
    let dir = tempfile::tempdir().unwrap();
    let me = std::process::id();
    let w = |rel: &str, body: String| {
        let f = dir.path().join(rel);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, body).unwrap();
    };
    w("jobs/aaaaaaaa/state.json", r#"{"state":"working","name":"j","sessionId":"s-job","cwd":"/w","createdAt":"2026-11-06T00:00:00.000Z","updatedAt":"2026-11-06T00:00:01.000Z"}"#.into());
    w(
        &format!("sessions/{me}.json"),
        format!(
            r#"{{"pid":{me},"sessionId":"s-job","jobId":"aaaaaaaa","status":"busy","startedAt":1700000000000,"kind":"bg"}}"#
        ),
    );
    w(
        "jobs/bbbbbbbb/state.json",
        r#"{"state":"done","sessionId":"s-done","createdAt":"2026-11-06T00:00:00.000Z"}"#.into(),
    );
    w(
        "sessions/999999.json",
        r#"{"pid":999999,"sessionId":"s-gone"}"#.into(),
    );
    w("jobs/not-a-job/state.json", "{}".into());

    let mut rows = FileRoster.read(&profile(dir.path(), false)).unwrap();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(
        rows.len(),
        2,
        "dead interactive record and non-job folder are not rows"
    );
    let j = &rows[0];
    assert_eq!(
        (j.id.as_deref(), j.pid, j.pid_alive, j.status.as_deref()),
        (Some("aaaaaaaa"), Some(me), true, Some("busy"))
    );
    assert!(j.active);
    assert_eq!(j.started_at, Some(1_700_000_000_000));
    let d = &rows[1];
    assert!(!d.active && d.pid.is_none());
    assert_eq!(d.started_at, Some(1_793_923_200_000)); // 2026-11-06T00:00:00Z
}
