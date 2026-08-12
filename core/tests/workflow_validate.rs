use coretempo_core::types::config::EdgeKind;
use coretempo_core::types::config::TriggerType;
use coretempo_core::types::id::AgentId;
use coretempo_core::workflow::validate_workflow;

const VALID: &str =
    "[workflow]\nname = \"dev\"\n[agents.planner]\ndir = \"/tmp\"\nprompt = \"plan\"\n";

#[test]
fn valid_file_passes() {
    let wf = validate_workflow(VALID).unwrap();
    assert_eq!(wf.workflow.name, "dev");
}

#[test]
fn toml_parse_error_is_one_issue_with_empty_path() {
    let issues = validate_workflow("not [ toml").unwrap_err();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "");
    assert!(!issues[0].message.is_empty());
}

#[test]
fn no_agents_is_an_issue() {
    let issues = validate_workflow("[workflow]\nname = \"dev\"\n[agents]\n").unwrap_err();
    assert!(issues.iter().any(|i| i.path == "agents"));
}

#[test]
fn bad_agent_id_is_an_issue() {
    let text =
        "[workflow]\nname = \"dev\"\n[agents.\"Bad_Agent!\"]\ndir = \"/tmp\"\nprompt = \"p\"\n";
    let issues = validate_workflow(text).unwrap_err();
    let issue = issues
        .iter()
        .find(|i| i.path == "agents.Bad_Agent!")
        .unwrap();
    assert!(issue.message.contains("^[a-z0-9][a-z0-9_-]{0,31}$"));
}

#[test]
fn empty_prompt_and_dir_are_issues() {
    let text = "[workflow]\nname = \"dev\"\n[agents.a]\ndir = \"\"\nprompt = \"  \"\n";
    let issues = validate_workflow(text).unwrap_err();
    assert!(issues.iter().any(|i| i.path == "agents.a.dir"));
    assert!(issues.iter().any(|i| i.path == "agents.a.prompt"));
}

#[test]
fn bad_numeric_settings_are_issues() {
    let text = "[workflow]\nname = \"dev\"\nport = 0\nask_timeout_minutes = 0\n\
                idle_debounce_seconds = -1.0\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n";
    let issues = validate_workflow(text).unwrap_err();
    assert!(issues.iter().any(|i| i.path == "workflow.port"));
    assert!(
        issues
            .iter()
            .any(|i| i.path == "workflow.ask_timeout_minutes")
    );
    assert!(
        issues
            .iter()
            .any(|i| i.path == "workflow.idle_debounce_seconds")
    );
}

#[test]
fn empty_name_is_an_issue() {
    let text = "[workflow]\nname = \"\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n";
    let issues = validate_workflow(text).unwrap_err();
    assert!(issues.iter().any(|i| i.path == "workflow.name"));
}

/// The example shipped at the repo root must always load: it is what `./dev`
/// tells a new user to copy.
#[test]
fn shipped_example_workflow_is_valid() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../tempo.example.toml");
    let text = std::fs::read_to_string(path).expect("tempo.example.toml is missing");
    let wf = validate_workflow(&text).expect("tempo.example.toml must validate");
    assert_eq!(wf.workflow.name, "example");
    assert_eq!(wf.agents.len(), 2);
}

const EDGED: &str = r#"
[workflow]
name = "edged"

[agents.planner]
dir = "/tmp"
prompt = "plan"
edges = [
  { to = "builder", kind = "ask" },
  { to = "notifier", kind = "send" },
]

[agents.builder]
dir = "/tmp"
prompt = "build"

[agents.notifier]
dir = "/tmp"
prompt = "notify"
"#;

#[test]
fn edges_parse_in_order() {
    let file = validate_workflow(EDGED).expect("valid");
    let planner = &file.agents[&AgentId("planner".into())];
    let steps: Vec<(&str, EdgeKind)> = planner
        .edges
        .iter()
        .map(|e| (e.to.0.as_str(), e.kind))
        .collect();
    assert_eq!(
        steps,
        vec![("builder", EdgeKind::Ask), ("notifier", EdgeKind::Send)]
    );
    assert!(file.agents[&AgentId("builder".into())].edges.is_empty());
}

#[test]
fn edge_to_unknown_agent_is_rejected_with_roster() {
    let text = EDGED.replace("to = \"builder\"", "to = \"bilder\"");
    let issues = validate_workflow(&text).expect_err("must reject");
    let issue = issues
        .iter()
        .find(|i| i.path == "agents.planner.edges")
        .expect("edges issue");
    assert!(issue.message.contains("bilder"), "names the bad target");
    assert!(issue.message.contains("builder"), "names the roster");
}

#[test]
fn self_edge_is_rejected() {
    let text = EDGED.replace("to = \"builder\"", "to = \"planner\"");
    let issues = validate_workflow(&text).expect_err("must reject");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "agents.planner.edges" && i.message.contains("itself"))
    );
}

#[test]
fn duplicate_edge_is_rejected() {
    let text = EDGED.replace(
        "{ to = \"notifier\", kind = \"send\" }",
        "{ to = \"builder\", kind = \"ask\" }",
    );
    let issues = validate_workflow(&text).expect_err("must reject");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "agents.planner.edges" && i.message.contains("duplicate"))
    );
}

#[test]
fn bad_edge_kind_is_a_parse_error() {
    let text = EDGED.replace("kind = \"ask\"", "kind = \"tell\"");
    let issues = validate_workflow(&text).expect_err("must reject");
    assert!(!issues.is_empty(), "serde rejects unknown kinds");
}

#[test]
fn loop_cycle_is_rejected_and_named() {
    let text = "[workflow]\nname = \"cyclic\"\n\
                [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                edges = [{ to = \"b\", kind = \"loop\" }]\n\
                [agents.b]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                edges = [{ to = \"a\", kind = \"loop\" }]\n";
    let issues = validate_workflow(text).expect_err("must reject");
    let issue = issues
        .iter()
        .find(|i| i.message.contains("loop cycle"))
        .expect("cycle issue present");
    assert!(
        issue.message.contains('a') && issue.message.contains('b'),
        "names the cycle members: {}",
        issue.message
    );
    assert!(
        issue.message.contains("ask"),
        "points at ask for request/response pairs: {}",
        issue.message
    );
}

#[test]
fn loop_chain_without_cycle_is_allowed() {
    // a loop b, b send a is feedback, not a loop cycle; and a loop b, b loop c
    // is a chain — both must validate.
    let chain = "[workflow]\nname = \"chained\"\n\
                 [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                 edges = [{ to = \"b\", kind = \"loop\" }]\n\
                 [agents.b]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                 edges = [{ to = \"c\", kind = \"loop\" }, { to = \"a\", kind = \"send\" }]\n\
                 [agents.c]\ndir = \"/tmp\"\nprompt = \"p\"\n";
    validate_workflow(chain).expect("acyclic loop chain validates");
}

#[test]
fn max_rounds_only_on_loop_edges_and_nonzero() {
    let on_ask = "[workflow]\nname = \"x\"\n\
                  [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                  edges = [{ to = \"b\", kind = \"ask\", max_rounds = 3 }]\n\
                  [agents.b]\ndir = \"/tmp\"\nprompt = \"p\"\n";
    let issues = validate_workflow(on_ask).expect_err("must reject");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "agents.a.edges" && i.message.contains("max_rounds")),
        "max_rounds on a non-loop edge is rejected: {issues:?}"
    );
    let zero = on_ask.replace(
        "kind = \"ask\", max_rounds = 3",
        "kind = \"loop\", max_rounds = 0",
    );
    let issues = validate_workflow(&zero).expect_err("must reject");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "agents.a.edges" && i.message.contains("max_rounds")),
        "max_rounds = 0 is rejected: {issues:?}"
    );
}

const TRIGGERED: &str = r#"
[workflow]
name = "triggered"

[trigger]
type = "on_start"
edge = { to = "planner", kind = "ask" }
message = "Plan the implementation"

[agents.planner]
dir = "/tmp"
prompt = "plan"
"#;

#[test]
fn on_start_trigger_parses() {
    let file = validate_workflow(TRIGGERED).expect("valid");
    let trigger = file.trigger.expect("trigger present");
    assert_eq!(trigger.trigger_type, TriggerType::OnStart);
    assert_eq!(trigger.edge.to, AgentId("planner".into()));
    assert_eq!(trigger.edge.kind, EdgeKind::Ask);
    assert_eq!(trigger.message.as_deref(), Some("Plan the implementation"));
}

#[test]
fn loop_trigger_edge_is_rejected() {
    let text = TRIGGERED.replace("kind = \"ask\"", "kind = \"loop\"");
    let issues = validate_workflow(&text).expect_err("must reject");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "trigger.edge" && i.message.contains("loop")),
        "wanted a trigger.edge issue naming loop, got: {issues:?}"
    );
}

#[test]
fn workflow_without_trigger_is_unchanged() {
    let without_trigger = TRIGGERED.replace(
        "[trigger]\ntype = \"on_start\"\nedge = { to = \"planner\", kind = \"ask\" }\n\
         message = \"Plan the implementation\"\n",
        "",
    );
    let file = validate_workflow(&without_trigger).expect("valid");
    assert!(file.trigger.is_none());
}

#[test]
fn trigger_to_unknown_agent_is_rejected_with_roster() {
    let text = TRIGGERED.replace("to = \"planner\"", "to = \"plnner\"");
    let issues = validate_workflow(&text).expect_err("must reject");
    let issue = issues
        .iter()
        .find(|i| i.path == "trigger.edge")
        .expect("issue");
    assert!(issue.message.contains("plnner") && issue.message.contains("planner"));
}

#[test]
fn on_start_requires_nonempty_message() {
    for replacement in ["message = \"  \"", ""] {
        let text = TRIGGERED.replace("message = \"Plan the implementation\"", replacement);
        let issues = validate_workflow(&text).expect_err("must reject");
        assert!(
            issues
                .iter()
                .any(|i| i.path == "trigger.message" && i.message.contains("on_start"))
        );
    }
}

#[test]
fn webhook_rejects_message_key() {
    let text = TRIGGERED.replace("type = \"on_start\"", "type = \"webhook\"");
    let issues = validate_workflow(&text).expect_err("must reject");
    assert!(issues.iter().any(|i| i.path == "trigger.message"
        && i.message.contains("webhook")
        && i.message.contains("request body")));
}

#[test]
fn bad_trigger_type_is_a_parse_error() {
    let text = TRIGGERED.replace("type = \"on_start\"", "type = \"cron\"");
    assert!(
        validate_workflow(&text).is_err(),
        "serde rejects unknown types"
    );
}

#[test]
fn tools_must_be_bare_binary_names() {
    let toml = r#"
        [workflow]
        name = "t"
        [agents.pa]
        dir = "~/x"
        prompt = "p"
        tools = ["pat", "/usr/bin/evil", "a b"]
    "#;
    let issues = validate_workflow(toml).unwrap_err();
    let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(paths, vec!["agents.pa.tools", "agents.pa.tools"]);
    assert!(issues[0].message.contains("/usr/bin/evil"));
    assert!(issues[0].message.contains("bare binary name"));
}

#[test]
fn valid_tools_pass_validation() {
    let toml = r#"
        [workflow]
        name = "t"
        [agents.pa]
        dir = "~/x"
        prompt = "p"
        tools = ["pat", "rg2", "my-tool_v2.1"]
    "#;
    assert!(validate_workflow(toml).is_ok());
}

#[test]
fn output_on_send_trigger_is_rejected() {
    let text = r#"
        [workflow]
        name = "x"
        [agents.a]
        dir = "~/p"
        prompt = "p"
        [trigger]
        type = "webhook"
        edge = { to = "a", kind = "send" }
        [trigger.output]
        schema = { type = "object" }
    "#;
    let issues = coretempo_core::workflow::validate_workflow(text).unwrap_err();
    assert!(
        issues
            .iter()
            .any(|i| i.path == "trigger.output" && i.message.contains("ask")),
        "{issues:?}"
    );
}

#[test]
fn output_requires_exactly_one_schema_source() {
    // both keys
    let both = r#"
        [workflow]
        name = "x"
        [agents.a]
        dir = "~/p"
        prompt = "p"
        [trigger]
        type = "webhook"
        edge = { to = "a", kind = "ask" }
        [trigger.output]
        schema = { type = "object" }
        schema_file = "s.json"
    "#;
    let issues = coretempo_core::workflow::validate_workflow(both).unwrap_err();
    assert!(
        issues.iter().any(|i| i.message.contains("exactly one")),
        "{issues:?}"
    );
    // neither key
    let neither = both
        .replace("schema = { type = \"object\" }\n", "")
        .replace("schema_file = \"s.json\"\n", "");
    let issues = coretempo_core::workflow::validate_workflow(&neither).unwrap_err();
    assert!(
        issues.iter().any(|i| i.message.contains("exactly one")),
        "{issues:?}"
    );
}

#[test]
fn output_empty_schema_file_is_rejected() {
    let text = r#"
        [workflow]
        name = "x"
        [agents.a]
        dir = "~/p"
        prompt = "p"
        [trigger]
        type = "webhook"
        edge = { to = "a", kind = "ask" }
        [trigger.output]
        schema_file = ""
    "#;
    let issues = coretempo_core::workflow::validate_workflow(text).unwrap_err();
    assert!(
        issues
            .iter()
            .any(|i| i.path == "trigger.output.schema_file" && i.message.contains("empty")),
        "{issues:?}"
    );
}

#[test]
fn output_on_on_start_trigger_is_rejected() {
    let text = r#"
        [workflow]
        name = "x"
        [agents.a]
        dir = "~/p"
        prompt = "p"
        [trigger]
        type = "on_start"
        message = "go"
        edge = { to = "a", kind = "ask" }
        [trigger.output]
        schema = { type = "object" }
    "#;
    let issues = coretempo_core::workflow::validate_workflow(text).unwrap_err();
    assert!(
        issues
            .iter()
            .any(|i| i.path == "trigger.output" && i.message.contains("webhook")),
        "{issues:?}"
    );
}

#[test]
fn output_max_repairs_range_is_enforced() {
    let text = r#"
        [workflow]
        name = "x"
        [agents.a]
        dir = "~/p"
        prompt = "p"
        [trigger]
        type = "webhook"
        edge = { to = "a", kind = "ask" }
        [trigger.output]
        schema = { type = "object" }
        max_repairs = 6
    "#;
    let issues = coretempo_core::workflow::validate_workflow(text).unwrap_err();
    assert!(
        issues
            .iter()
            .any(|i| i.path == "trigger.output.max_repairs"),
        "{issues:?}"
    );
}
