//! `stage_of_tool` / `stage_of` against Node's `stageOfTool` / `stageOf` (golden from native/parity/gen-transcript-golden.mjs).

use fleet_model::stage::{stage_of, stage_of_tool};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Tool {
    name: Value,
    input: Value,
    stage: Option<String>,
}

#[derive(Deserialize)]
struct Seq {
    uses: Vec<(Value, Value)>,
    stage: Option<String>,
}

#[derive(Deserialize)]
struct Golden {
    tools: Vec<Tool>,
    seqs: Vec<Seq>,
}

fn golden() -> Golden {
    let f = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../parity/golden/stage.json"
    );
    serde_json::from_str(&std::fs::read_to_string(f).expect("golden/stage.json"))
        .expect("parse golden")
}

#[test]
fn stage_of_tool_matches_node() {
    let g = golden();
    assert!(g.tools.len() > 50);
    for t in &g.tools {
        let got = stage_of_tool(t.name.as_str(), &t.input);
        assert_eq!(got, t.stage.as_deref(), "{} {}", t.name, t.input);
    }
}

#[test]
fn stage_of_matches_node() {
    for s in &golden().seqs {
        let got = stage_of(s.uses.iter().map(|(n, i)| stage_of_tool(n.as_str(), i)));
        assert_eq!(got, s.stage.as_deref(), "{:?}", s.uses);
    }
}
