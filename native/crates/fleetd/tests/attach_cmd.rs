//! `attach_cmd::command` against the oracle: `tests/golden/attach_cmd.json` is generated from `fleet/term.mjs` by
//! `tests/golden/gen-attach-cmd.mjs`. The home here is a synthetic temp folder with stand-in claude/codex binaries.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use fleet_model::types::AttachTarget;
use fleetd::attach_cmd::command;

fn home() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir_all(d.path().join(".local/bin")).unwrap();
    for b in ["claude", "codex"] {
        fs::write(d.path().join(".local/bin").join(b), "#!/bin/sh\n").unwrap();
    }
    fs::create_dir_all(d.path().join("work/x")).unwrap();
    d
}

fn claude(profile: &str, job: &str) -> AttachTarget {
    AttachTarget::Claude {
        profile: profile.into(),
        job: job.into(),
    }
}
fn codex(profile: &str, id: &str, cwd: &str) -> AttachTarget {
    AttachTarget::Codex {
        profile: profile.into(),
        session_id: id.into(),
        cwd: cwd.into(),
    }
}
fn trust(profile: &str, dir: &str) -> AttachTarget {
    AttachTarget::Trust {
        profile: profile.into(),
        dir: dir.into(),
    }
}

const U: &str = "0123abcd-4567-89ab-cdef-0123456789ab";

#[test]
fn every_target_matches_term_mjs() {
    let h = home();
    let hs = h.path().to_str().unwrap();
    let golden = include_str!("golden/attach_cmd.json").replace("{HOME}", hs);
    let cases: Vec<serde_json::Value> = serde_json::from_str(&golden).unwrap();
    assert!(cases.len() >= 8);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let target: AttachTarget = serde_json::from_value(c["target"].clone()).unwrap();
        let cmd = command(&target, h.path()).unwrap_or_else(|| panic!("{name}: None"));
        let argv: Vec<&str> = cmd.argv.iter().map(|a| a.to_str().unwrap()).collect();
        let want: Vec<&str> = c["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(argv, want, "{name}: argv");
        assert_eq!(
            cmd.cwd,
            Path::new(c["cwd"].as_str().unwrap()),
            "{name}: cwd"
        );
        // The env: the keys term.mjs sets, plus the two config homes; the rest is inherited.
        let keys = [
            "TERM",
            "COLORTERM",
            "COLORFGBG",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ];
        let got: BTreeMap<&str, &str> = cmd
            .env
            .iter()
            .filter_map(|(k, v)| {
                keys.iter()
                    .find(|n| **n == k.to_str().unwrap())
                    .map(|n| (*n, v.to_str().unwrap()))
            })
            .collect();
        let want: BTreeMap<&str, &str> = c["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().unwrap()))
            .collect();
        assert_eq!(got, want, "{name}: env");
    }
}

#[test]
fn env_strips_what_must_not_leak_and_defaults_lang() {
    let h = home();
    let cmd = command(&claude("default", "abcd1234"), h.path()).unwrap();
    for (k, _) in &cmd.env {
        let k = k.to_str().unwrap();
        assert!(
            !(k.starts_with("CLAUDE")
                || k.starts_with("FLEET_")
                || k == "TMUX"
                || k == "TMUX_PANE"),
            "leaked {k}"
        );
    }
    let lang = cmd
        .env
        .iter()
        .find(|(k, _)| k == "LANG")
        .map(|(_, v)| v.to_str().unwrap().to_string());
    let want = std::env::var("LANG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "en_US.UTF-8".into());
    assert_eq!(lang, Some(want));
    assert_eq!(cmd.env.iter().filter(|(k, _)| k == "TERM").count(), 1);
}

#[test]
fn binary_lookup_prefers_local_bin_and_falls_back_to_the_bare_name_or_a_system_path() {
    let h = home();
    let cmd = command(&claude("default", "abcd1234"), h.path()).unwrap();
    assert_eq!(
        cmd.argv[0],
        h.path().join(".local/bin/claude").into_os_string()
    );
    // No stand-in in this home: the first system path that exists, else the bare name.
    let bare = tempfile::tempdir().unwrap();
    let a = command(&claude("default", "abcd1234"), bare.path())
        .unwrap()
        .argv
        .remove(0);
    let system = ["/opt/homebrew/bin/claude", "/usr/local/bin/claude"]
        .into_iter()
        .find(|p| Path::new(p).exists());
    assert_eq!(a.to_str().unwrap(), system.unwrap_or("claude"));
}

#[test]
fn bad_values_give_none() {
    let h = home();
    let p = h.path();
    let long = format!("/{}", "a".repeat(401));
    let bad = [
        claude("bad profile", "abcd1234"),
        claude("", "abcd1234"),
        claude(&"p".repeat(41), "abcd1234"),
        claude("default", "ABCD1234"),
        claude("default", "abcd123"),
        claude("default", "abcd12345"),
        claude("default", "abcd123g"),
        codex("codex", "0123ABCD-4567-89ab-cdef-0123456789ab", "~"),
        codex("codex", "0123abcd-4567-89ab-cdef-0123456789a", "~"),
        codex("codex", U, "relative/dir"),
        codex("co dex", U, "~"),
        trust("default", "relative"),
        trust("default", "~user"),
        trust("default", "~/a\nb"),
        trust("default", "/a\0b"),
        trust("default", "/a\rb"),
        trust("default", &long),
        trust("bad/profile", "~"),
    ];
    for t in bad {
        assert!(command(&t, p).is_none(), "{t:?}");
    }
}

#[test]
fn a_missing_folder_gives_none() {
    let h = home();
    assert!(command(&trust("default", "~/nowhere"), h.path()).is_none());
    assert!(command(&codex("codex", U, "~/nowhere"), h.path()).is_none());
    assert!(command(&trust("default", "/nonexistent-fleetd-dir"), h.path()).is_none());
    assert!(command(&trust("default", "~/work"), h.path()).is_some());
}
