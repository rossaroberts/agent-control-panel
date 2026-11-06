use std::fs;
use std::path::Path;

use fleet_read::hookfile::read_hook;
use fleet_read::profiles::{account, claude_profiles, Profile};

fn touch(p: &Path, body: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn discovers_profiles_by_marker_files() {
    let h = tempfile::tempdir().unwrap();
    touch(&h.path().join(".claude/settings.json"), "{}");
    touch(&h.path().join(".claude-aol/.claude.json"), "{}");
    fs::create_dir_all(h.path().join(".claude-enterprise/projects")).unwrap();
    fs::create_dir_all(h.path().join(".claude-x")).unwrap();
    touch(&h.path().join(".claude-x/notes.txt"), "no marker");
    touch(&h.path().join(".claude-file"), "a file, not a folder");
    fs::create_dir_all(h.path().join(".claudefoo/projects")).unwrap();

    let ps = claude_profiles(h.path());
    let ids: Vec<&str> = ps.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["default", "aol", "enterprise"]);
    assert!(ps[0].plain && !ps[1].plain);
    assert_eq!(ps[0].config, h.path().join(".claude.json"));
    assert_eq!(ps[1].config, h.path().join(".claude-aol/.claude.json"));
}

fn profile(dir: &Path) -> Profile {
    Profile {
        id: "t".into(),
        dir: dir.to_path_buf(),
        plain: false,
        config: dir.join(".claude.json"),
    }
}

#[test]
fn account_reads_the_email() {
    let h = tempfile::tempdir().unwrap();
    let p = profile(h.path());
    assert_eq!(account(&p), None);
    touch(
        &p.config,
        r#"{"oauthAccount":{"emailAddress":"dev@example.test"}}"#,
    );
    assert_eq!(account(&p).as_deref(), Some("dev@example.test"));
    touch(&p.config, "not json");
    assert_eq!(account(&p), None);
}

#[test]
fn hook_reads_a_good_file_and_refuses_the_rest() {
    let h = tempfile::tempdir().unwrap();
    let p = profile(h.path());
    let id = "0123abcd-4567";
    let f = |n: &str| h.path().join("fleet-state").join(format!("{n}.json"));

    touch(
        &f(id),
        r#"{"e":"PreToolUse","t":"Bash","at":1700000000000}"#,
    );
    let ev = read_hook(&p, id).unwrap();
    assert_eq!(
        (ev.e.as_str(), ev.t.as_deref(), ev.at),
        ("PreToolUse", Some("Bash"), 1_700_000_000_000)
    );

    // A session id that could leave the folder, or is too short.
    touch(&h.path().join("x.json"), r#"{"e":"Stop","at":1}"#);
    assert!(read_hook(&p, "../x").is_none());
    assert!(read_hook(&p, "../../x0000000").is_none());
    assert!(read_hook(&p, "short").is_none());

    // 64 KiB is read, one byte more is refused.
    let pad = |n: usize| {
        let head = r#"{"e":"Stop","at":1,"pad":""#;
        let tail = r#""}"#;
        format!("{head}{}{tail}", "a".repeat(n - head.len() - tail.len()))
    };
    touch(&f("exactly64k0"), &pad(65_536));
    assert!(read_hook(&p, "exactly64k0").is_some());
    touch(&f("toolarge00"), &pad(65_537));
    assert_eq!(fs::metadata(f("toolarge00")).unwrap().len(), 65_537);
    assert!(read_hook(&p, "toolarge00").is_none());
}

#[test]
fn a_hook_file_that_does_not_parse_is_no_signal() {
    let h = tempfile::tempdir().unwrap();
    let p = profile(h.path());
    for (i, body) in [
        "{not json",
        "",
        r#"{"e":"Stop"}"#,
        r#"{"e":"Stop","at":"yesterday"}"#,
        r#"{"e":7,"at":1}"#,
        r#"{"at":1}"#,
        "[]",
        "null",
    ]
    .iter()
    .enumerate()
    {
        let id = format!("badhook{i:02}");
        touch(
            &h.path().join("fleet-state").join(format!("{id}.json")),
            body,
        );
        assert!(read_hook(&p, &id).is_none(), "{body:?}");
    }
    assert!(read_hook(&p, "nosuchfile1").is_none());
}
