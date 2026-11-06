//! `PanelMeta` over the synthetic panel folder `tests/fixtures/panel/`: the naming, archive and group rules against Node's
//! (`native/parity/golden/panelmeta.json`, from `gen-panelmeta-golden.mjs`), the files' own filters, and a damaged folder.

use std::fs;
use std::path::PathBuf;

use fleet_model::types::Engine;
use fleet_read::panelmeta::{read_panel_meta, PanelMeta};
use serde::Deserialize;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/panel")
}

#[derive(Deserialize)]
struct Naming {
    key: String,
    engine: String,
    name: Option<String>,
    title: Option<String>,
    epic: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hiding {
    key: String,
    live: bool,
    started_at: Option<i64>,
    needs_you: bool,
    archived: bool,
}

#[derive(Deserialize)]
struct Golden {
    naming: Vec<Naming>,
    hiding: Vec<Hiding>,
    groups: Vec<String>,
}

fn golden() -> Golden {
    serde_json::from_str(include_str!("../../../parity/golden/panelmeta.json"))
        .expect("golden/panelmeta.json parses")
}

fn engine(w: &str) -> Engine {
    match w {
        "claude" => Engine::Claude,
        "codex" => Engine::Codex,
        _ => panic!("engine {w}"),
    }
}

#[test]
fn title_and_epic_follow_the_node_rule_for_label_own_titled_and_name() {
    let m = read_panel_meta(&fixture());
    let g = golden();
    assert!(g.naming.len() > 100, "the grid is there");
    for c in &g.naming {
        let got = m.named(&c.key, &engine(&c.engine), c.name.as_deref());
        assert_eq!(
            (got.title, got.epic),
            (c.title.clone(), c.epic.clone()),
            "{} {} {:?}",
            c.key,
            c.engine,
            c.name
        );
    }
}

#[test]
fn archived_keys_are_flagged_by_the_showable_rule() {
    let m = read_panel_meta(&fixture());
    let g = golden();
    for c in &g.hiding {
        assert_eq!(
            m.is_archived(&c.key, c.live, c.started_at, c.needs_you),
            c.archived,
            "{} live {} started {:?} needs you {}",
            c.key,
            c.live,
            c.started_at,
            c.needs_you
        );
    }
    // A closed session you archived is flagged; one that was never archived, or whose mark is 0, is not.
    assert!(m.is_archived("fixture-mac:sess-0010-fixture", false, None, false));
    assert!(!m.is_archived("fixture-mac:sess-0013-fixture", false, None, false));
    assert!(!m.is_archived("fixture-mac:sess-0020-fixture", false, None, false));
}

#[test]
fn the_groups_you_made_are_node_s_and_leave_out_the_epics() {
    let m = read_panel_meta(&fixture());
    assert_eq!(m.groups(), golden().groups);
}

#[test]
fn the_config_is_filtered_as_node_filters_it() {
    let m = read_panel_meta(&fixture());
    assert_eq!(m.self_name.as_deref(), Some("fixture-mac"));
    let names: Vec<&str> = m.epics.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["Fixtures", "Other work", "Third"],
        "order kept, a bad name dropped"
    );
    assert_eq!(
        m.epics[0].test.as_deref(),
        Some("https://test.example.com/app")
    );
    assert_eq!(m.epics[1].about, "", "a null about is empty");
    assert_eq!(m.epics[2].about.chars().count(), 300, "about is cut at 300");
    assert_eq!(m.epics[2].test, None, "a TEST site must be https");
    assert_eq!(m.logins.len(), 1, "only a good key with an email stays");
    assert_eq!(
        m.logins.get("fixture-mac:alt").map(String::as_str),
        Some("real@example.com")
    );
}

#[test]
fn an_account_is_named_by_fleet_json_else_by_its_email_s_local_part() {
    let m = read_panel_meta(&fixture());
    assert_eq!(m.account_name(&Engine::Claude, "ada@example.com"), "work");
    assert_eq!(
        m.account_name(&Engine::Codex, "ada@example.com"),
        "codex-work"
    );
    // The same email under another engine has no name of its own.
    assert_eq!(m.account_name(&Engine::Muse, "ada@example.com"), "ada");
    // A name that is not a string is not a name; one over 40 characters is cut to 40.
    assert_eq!(m.account_name(&Engine::Claude, "bob@example.com"), "bob");
    assert_eq!(
        m.account_name(&Engine::Claude, "long@example.com").len(),
        40
    );
    assert_eq!(m.account_name(&Engine::Codex, "eve@example.com"), "eve");
    assert_eq!(m.account_name(&Engine::Claude, "no-at-sign"), "no-at-sign");
}

/// Review Focus 5: a damaged or absent folder is an empty `PanelMeta`, and never a panic.
#[test]
fn a_missing_truncated_or_non_json_file_counts_as_empty() {
    let d = tempfile::tempdir().unwrap();
    // No folder at all.
    assert_eq!(
        read_panel_meta(&d.path().join("absent")),
        PanelMeta::default()
    );
    // An empty folder.
    assert_eq!(read_panel_meta(d.path()), PanelMeta::default());

    // A truncated titles file costs only the titles.
    for f in ["fleet.json", "fleet-labels.json", "fleet-archived.json"] {
        fs::copy(fixture().join(f), d.path().join(f)).unwrap();
    }
    let titles = fs::read_to_string(fixture().join("fleet-titles.json")).unwrap();
    fs::write(
        d.path().join("fleet-titles.json"),
        &titles[..titles.len() / 2],
    )
    .unwrap();
    let m = read_panel_meta(d.path());
    assert!(m.titles.is_empty());
    assert!(!m.labels.is_empty() && !m.epics.is_empty() && !m.archived.is_empty());

    // Non-JSON fleet.json costs only the config.
    fs::write(d.path().join("fleet.json"), "this is {not json").unwrap();
    let m = read_panel_meta(d.path());
    assert!(
        m.self_name.is_none() && m.epics.is_empty() && m.names.is_empty() && m.logins.is_empty()
    );
    assert!(!m.labels.is_empty());

    // JSON that is not an object (`null`, a list, a string) is nothing, as `kept` has it.
    for body in ["null", "[1,2]", "\"text\"", "7", ""] {
        for f in [
            "fleet.json",
            "fleet-titles.json",
            "fleet-labels.json",
            "fleet-archived.json",
        ] {
            fs::write(d.path().join(f), body).unwrap();
        }
        assert_eq!(read_panel_meta(d.path()), PanelMeta::default(), "{body:?}");
    }
    // A directory where a file should be, and a file over the size cap, are nothing too.
    fs::remove_file(d.path().join("fleet-labels.json")).unwrap();
    fs::create_dir(d.path().join("fleet-labels.json")).unwrap();
    let big = format!("{{\"k\": \"{}\"}}", "x".repeat(9 << 20));
    fs::write(d.path().join("fleet-titles.json"), big).unwrap();
    assert_eq!(read_panel_meta(d.path()), PanelMeta::default());
}
