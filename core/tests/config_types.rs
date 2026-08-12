use std::path::PathBuf;

use coretempo_core::types::config::{EdgeKind, WorkflowFile};
use coretempo_core::types::id::AgentId;

const SPEC_TOML: &str = r#"
[workflow]
name = "core-tempo-dev"
db = "./tempo.db"
port = 4820
ask_timeout_minutes = 30

[agents.planner]
dir = "~/projects/CoreTempo"
prompt = "You are the planning agent…"
model = "opus"
auto_clear = true

[agents.builder]
dir = "~/projects/CoreTempo"
prompt = "You implement tasks sent to you…"
permission_mode = "acceptEdits"
"#;

#[test]
fn parses_spec_example() {
    let wf: WorkflowFile = toml::from_str(SPEC_TOML).unwrap();
    assert_eq!(wf.workflow.name, "core-tempo-dev");
    assert_eq!(wf.workflow.db, PathBuf::from("./tempo.db"));
    assert_eq!(wf.workflow.port, 4820);
    assert_eq!(wf.workflow.ask_timeout_minutes, 30);
    let planner = &wf.agents[&AgentId("planner".into())];
    assert_eq!(planner.model.as_deref(), Some("opus"));
    assert!(planner.auto_clear);
    assert_eq!(planner.permission_mode, None);
    let builder = &wf.agents[&AgentId("builder".into())];
    assert_eq!(builder.permission_mode.as_deref(), Some("acceptEdits"));
    // roster order is lexicographic (BTreeMap)
    let ids: Vec<_> = wf.agents.keys().map(|a| a.0.as_str()).collect();
    assert_eq!(ids, ["builder", "planner"]);
}

#[test]
fn edge_kinds_parse_including_loop() {
    let wf: WorkflowFile = toml::from_str(
        "[workflow]\nname = \"x\"\n\
         [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
         edges = [{ to = \"b\", kind = \"ask\" }, { to = \"b\", kind = \"send\" }, \
         { to = \"b\", kind = \"loop\" }]\n\
         [agents.b]\ndir = \"/tmp\"\nprompt = \"p\"\n",
    )
    .unwrap();
    let kinds: Vec<EdgeKind> = wf.agents[&AgentId("a".into())]
        .edges
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds, [EdgeKind::Ask, EdgeKind::Send, EdgeKind::Loop]);
}

#[test]
fn loop_edge_max_rounds_parses_and_defaults() {
    let wf: WorkflowFile = toml::from_str(
        "[workflow]\nname = \"x\"\n\
         [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
         edges = [{ to = \"b\", kind = \"loop\", max_rounds = 3 }, \
         { to = \"c\", kind = \"loop\" }]\n\
         [agents.b]\ndir = \"/tmp\"\nprompt = \"p\"\n\
         [agents.c]\ndir = \"/tmp\"\nprompt = \"p\"\n",
    )
    .unwrap();
    let edges = &wf.agents[&AgentId("a".into())].edges;
    assert_eq!(edges[0].max_rounds, Some(3));
    assert_eq!(edges[0].effective_max_rounds(), 3);
    assert_eq!(edges[1].max_rounds, None);
    assert_eq!(edges[1].effective_max_rounds(), 10, "default cap is 10");
}

#[test]
fn defaults_apply() {
    let wf: WorkflowFile =
        toml::from_str("[workflow]\nname = \"x\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n")
            .unwrap();
    assert_eq!(wf.workflow.db, PathBuf::from("./tempo.db"));
    assert_eq!(wf.workflow.port, 4820);
    assert_eq!(wf.workflow.ask_timeout_minutes, 30);
    assert!((wf.workflow.idle_debounce_seconds - 2.0).abs() < f64::EPSILON);
    assert!(wf.agents[&AgentId("a".into())].auto_clear);
    assert!(wf.server.bind.is_none());
    assert!(wf.server.allowed_origins.is_empty());
}

#[test]
fn scrollback_defaults_to_5000() {
    let wf: WorkflowFile =
        toml::from_str("[workflow]\nname = \"x\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n")
            .unwrap();
    assert_eq!(wf.workflow.scrollback, 5_000);
}

#[test]
fn scrollback_is_settable() {
    let wf: WorkflowFile = toml::from_str(
        "[workflow]\nname = \"x\"\nscrollback = 20000\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n",
    )
    .unwrap();
    assert_eq!(wf.workflow.scrollback, 20_000);
}

#[test]
fn unknown_fields_rejected() {
    let err = toml::from_str::<WorkflowFile>(
        "[workflow]\nname = \"x\"\nbogus = 1\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("bogus"));
}

#[test]
fn workflow_file_serializes_to_json() {
    let wf: WorkflowFile = toml::from_str(SPEC_TOML).unwrap();
    let json = serde_json::to_value(&wf).unwrap();
    assert_eq!(json["workflow"]["port"], 4820);
    assert_eq!(json["agents"]["planner"]["model"], "opus");
}
