#![expect(
    clippy::expect_used,
    reason = "flow_issues asserts invalidity outside a #[test] fn"
)]

use coretempo_core::types::config::EdgeKind;
use coretempo_core::types::config::TriggerType;
use coretempo_core::types::id::{AgentId, FlowName};
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
        issue.message.contains("loop cycle: a → b → a"),
        "traces the cycle back to where it closes: {}",
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

[flows.main]
agents = ["planner"]

[flows.main.trigger]
type = "on_start"
edge = { to = "planner", kind = "ask" }
message = "Plan the implementation"

[agents.planner]
dir = "/tmp"
prompt = "plan"
"#;

#[test]
fn on_start_flow_parses() {
    let file = validate_workflow(TRIGGERED).expect("valid");
    let flow = &file.flows[&FlowName("main".into())];
    assert_eq!(flow.agents, vec![AgentId("planner".into())]);
    assert_eq!(flow.trigger.trigger_type, TriggerType::OnStart);
    assert_eq!(flow.trigger.edge.to, AgentId("planner".into()));
    assert_eq!(flow.trigger.edge.kind, EdgeKind::Ask);
    assert_eq!(
        flow.trigger.message.as_deref(),
        Some("Plan the implementation")
    );
}

#[test]
fn loop_trigger_edge_is_rejected() {
    let text = TRIGGERED.replace("kind = \"ask\"", "kind = \"loop\"");
    let issues = validate_workflow(&text).expect_err("must reject");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "flows.main.trigger.edge" && i.message.contains("loop")),
        "wanted a flows.main.trigger.edge issue naming loop, got: {issues:?}"
    );
}

#[test]
fn on_start_requires_nonempty_message() {
    for replacement in ["message = \"  \"", ""] {
        let text = TRIGGERED.replace("message = \"Plan the implementation\"", replacement);
        let issues = validate_workflow(&text).expect_err("must reject");
        assert!(
            issues
                .iter()
                .any(|i| i.path == "flows.main.trigger.message" && i.message.contains("on_start")),
            "{issues:?}"
        );
    }
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
fn allow_entries_must_not_be_blank() {
    let toml = r#"
        [workflow]
        name = "t"
        [agents.pa]
        dir = "~/x"
        prompt = "p"
        allow = ["WebSearch", "", "   "]
    "#;
    let issues = validate_workflow(toml).unwrap_err();
    let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(paths, vec!["agents.pa.allow", "agents.pa.allow"]);
    assert!(
        issues[0].message.contains("permission rule"),
        "{}",
        issues[0].message
    );
    assert!(
        issues[0].message.contains("WebSearch"),
        "message names a valid example"
    );
}

#[test]
fn allow_rules_pass_through_validation_verbatim() {
    let toml = r#"
        [workflow]
        name = "t"
        [agents.pa]
        dir = "~/x"
        prompt = "p"
        allow = ["WebSearch", "WebFetch", "Read(//data/**)", "Bash(git log:*)"]
    "#;
    let file = validate_workflow(toml).expect("valid");
    let pa = &file.agents[&AgentId("pa".into())];
    assert_eq!(
        pa.allow,
        [
            "WebSearch",
            "WebFetch",
            "Read(//data/**)",
            "Bash(git log:*)"
        ]
    );
}

/// A one-agent webhook flow whose `[flows.hook.output]` body the caller supplies.
fn with_output(output: &str) -> Vec<coretempo_core::types::ValidationIssue> {
    let text = format!(
        "[workflow]\nname = \"x\"\n[agents.a]\ndir = \"~/p\"\nprompt = \"p\"\n\
         [flows.hook]\nagents = [\"a\"]\n\
         trigger = {{ type = \"webhook\", edge = {{ to = \"a\", kind = \"ask\" }} }}\n\
         [flows.hook.output]\n{output}"
    );
    coretempo_core::workflow::validate_workflow(&text).expect_err("fixture must be invalid")
}

#[test]
fn output_requires_exactly_one_schema_source() {
    let both = with_output("schema = { type = \"object\" }\nschema_file = \"s.json\"\n");
    assert!(
        both.iter()
            .any(|i| i.path == "flows.hook.output" && i.message.contains("exactly one")),
        "{both:?}"
    );
    let neither = with_output("max_repairs = 1\n");
    assert!(
        neither
            .iter()
            .any(|i| i.path == "flows.hook.output" && i.message.contains("exactly one")),
        "{neither:?}"
    );
}

#[test]
fn output_empty_schema_file_is_rejected() {
    let issues = with_output("schema_file = \"\"\n");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "flows.hook.output" && i.message.contains("empty")),
        "{issues:?}"
    );
}

#[test]
fn output_max_repairs_range_is_enforced() {
    let issues = with_output("schema = { type = \"object\" }\nmax_repairs = 6\n");
    assert!(
        issues
            .iter()
            .any(|i| i.path == "flows.hook.output" && i.message.contains("0..=5")),
        "{issues:?}"
    );
}

const FLOW_POOL: &str = "[workflow]\nname = \"dev\"\n\
    [agents.smm]\ndir = \"/tmp\"\nprompt = \"p\"\n\
    [agents.kb]\ndir = \"/tmp\"\nprompt = \"p\"\n";

fn flow_issues(flows: &str) -> Vec<coretempo_core::types::ValidationIssue> {
    coretempo_core::workflow::validate_workflow(&format!("{FLOW_POOL}{flows}"))
        .expect_err("fixture must be invalid")
}

#[test]
fn flow_member_must_exist_in_the_pool() {
    let issues = flow_issues(
        "[flows.post]\nagents = [\"nope\", \"smm\"]\n\
         trigger = { type = \"webhook\", edge = { to = \"smm\", kind = \"ask\" } }\n",
    );
    let issue = issues
        .iter()
        .find(|i| i.path == "flows.post.agents")
        .expect("membership issue");
    assert!(issue.message.contains("'nope'"), "{}", issue.message);
    assert!(
        issue.message.contains("kb, smm"),
        "names the pool roster: {}",
        issue.message
    );
}

#[test]
fn flow_members_must_be_unique() {
    let issues = flow_issues(
        "[flows.post]\nagents = [\"smm\", \"kb\", \"smm\"]\n\
         trigger = { type = \"webhook\", edge = { to = \"smm\", kind = \"ask\" } }\n",
    );
    let issue = issues
        .iter()
        .find(|i| i.path == "flows.post.agents")
        .expect("duplicate issue");
    assert!(issue.message.contains("'smm'"), "{}", issue.message);
    assert!(
        issue.message.contains("post"),
        "names the flow: {}",
        issue.message
    );
    assert!(
        issue.message.contains("once"),
        "says each id appears once: {}",
        issue.message
    );
}

#[test]
fn flow_agents_must_be_non_empty() {
    let issues = flow_issues(
        "[flows.post]\nagents = []\n\
         trigger = { type = \"webhook\", edge = { to = \"smm\", kind = \"ask\" } }\n",
    );
    assert!(issues.iter().any(|i| i.path == "flows.post.agents"));
}

#[test]
fn flow_trigger_target_must_be_a_member() {
    // kb is in the pool but not in this flow: still an error.
    let issues = flow_issues(
        "[flows.post]\nagents = [\"smm\"]\n\
         trigger = { type = \"webhook\", edge = { to = \"kb\", kind = \"ask\" } }\n",
    );
    let issue = issues
        .iter()
        .find(|i| i.path == "flows.post.trigger.edge")
        .expect("target issue");
    assert!(issue.message.contains("not a member"), "{}", issue.message);
    assert!(
        issue.message.contains("smm"),
        "names the members: {}",
        issue.message
    );
}

#[test]
fn flow_subset_must_be_edge_closed() {
    // smm delegates to kb, so a flow containing smm must contain kb.
    let text = "[workflow]\nname = \"dev\"\n\
        [agents.smm]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        edges = [{ to = \"kb\", kind = \"ask\" }]\n\
        [agents.kb]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        [flows.post]\nagents = [\"smm\"]\n\
        trigger = { type = \"webhook\", edge = { to = \"smm\", kind = \"ask\" } }\n";
    let issues = coretempo_core::workflow::validate_workflow(text).expect_err("not edge-closed");
    let issue = issues
        .iter()
        .find(|i| i.path == "flows.post.agents")
        .expect("closure issue");
    assert!(issue.message.contains("edge-closed"), "{}", issue.message);
    assert!(
        issue.message.contains("'smm'"),
        "names the edge owner: {}",
        issue.message
    );
    assert!(
        issue.message.contains("'kb'"),
        "names the missing agent: {}",
        issue.message
    );
    assert!(
        issue.message.contains("flows.post.agents"),
        "names the fix location: {}",
        issue.message
    );
}

#[test]
fn an_edge_closed_flow_validates_with_edges_pointing_into_it() {
    // The green side of the check above, with everything an over-strict closure
    // rule would trip on: a two-hop delegation chain inside the flow (smm ask
    // kb, kb send log), a `loop` edge between two members, a pool agent the
    // flow leaves out entirely, and an edge from that outsider *into* a member.
    // Closure is about where a member delegates, not about who delegates to it,
    // so none of these is a violation and the file must validate.
    let text = "[workflow]\nname = \"dev\"\n\
        [agents.smm]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        edges = [{ to = \"kb\", kind = \"ask\" }, { to = \"log\", kind = \"loop\" }]\n\
        [agents.kb]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        edges = [{ to = \"log\", kind = \"send\" }]\n\
        [agents.log]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        [agents.outsider]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        edges = [{ to = \"smm\", kind = \"ask\" }]\n\
        [flows.post]\nagents = [\"smm\", \"kb\", \"log\"]\n\
        trigger = { type = \"webhook\", edge = { to = \"smm\", kind = \"ask\" } }\n\
        [flows.note]\nagents = [\"log\"]\n\
        trigger = { type = \"webhook\", edge = { to = \"log\", kind = \"ask\" } }\n";
    let file = coretempo_core::workflow::validate_workflow(text)
        .unwrap_or_else(|issues| panic!("edge-closed flows must validate: {issues:?}"));
    assert_eq!(
        file.flows[&FlowName("post".into())].agents,
        vec![
            AgentId("smm".into()),
            AgentId("kb".into()),
            AgentId("log".into())
        ]
    );
    // `note` is a single member with no edges of its own, and the members of
    // `post` delegating into it does not drag them in.
    assert_eq!(
        file.flows[&FlowName("note".into())].agents,
        vec![AgentId("log".into())]
    );
}

#[test]
fn flow_trigger_type_rules_apply_per_flow() {
    // on_start without a message; webhook with one.
    let issues = flow_issues(
        "[flows.a]\nagents = [\"smm\"]\n\
         trigger = { type = \"on_start\", edge = { to = \"smm\", kind = \"ask\" } }\n\
         [flows.b]\nagents = [\"kb\"]\n\
         trigger = { type = \"webhook\", edge = { to = \"kb\", kind = \"ask\" }, \
         message = \"no\" }\n",
    );
    assert!(issues.iter().any(|i| i.path == "flows.a.trigger.message"));
    assert!(issues.iter().any(|i| i.path == "flows.b.trigger.message"));
}

#[test]
fn flow_names_use_the_agent_id_charset() {
    let issues = flow_issues(
        "[flows.\"Bad_Name\"]\nagents = [\"smm\"]\n\
         trigger = { type = \"webhook\", edge = { to = \"smm\", kind = \"ask\" } }\n",
    );
    let issue = issues
        .iter()
        .find(|i| i.path == "flows.Bad_Name")
        .expect("charset issue");
    assert!(issue.message.contains("a-z0-9"), "{}", issue.message);
}

#[test]
fn flow_output_shape_rules_apply_per_flow() {
    // output on a send kickoff and on an on_start flow are both rejected.
    let issues = flow_issues(
        "[flows.a]\nagents = [\"smm\"]\n\
         trigger = { type = \"webhook\", edge = { to = \"smm\", kind = \"send\" } }\n\
         [flows.a.output]\nschema = { type = \"object\" }\n\
         [flows.b]\nagents = [\"kb\"]\n\
         trigger = { type = \"on_start\", edge = { to = \"kb\", kind = \"ask\" }, \
         message = \"go\" }\n\
         [flows.b.output]\nschema = { type = \"object\" }\n",
    );
    assert!(issues.iter().any(|i| i.path == "flows.a.output"));
    assert!(issues.iter().any(|i| i.path == "flows.b.output"));
}

#[test]
fn max_concurrent_runs_must_be_1_to_16() {
    for bad in ["0", "17"] {
        let text = format!(
            "[workflow]\nname = \"dev\"\n[server]\nmax_concurrent_runs = {bad}\n\
             [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n"
        );
        let issues = coretempo_core::workflow::validate_workflow(&text).expect_err("out of range");
        let issue = issues
            .iter()
            .find(|i| i.path == "server.max_concurrent_runs")
            .expect("range issue");
        assert!(issue.message.contains("1..=16"), "{}", issue.message);
    }
}

#[test]
fn a_file_with_zero_flows_is_valid() {
    assert!(coretempo_core::workflow::validate_workflow(FLOW_POOL).is_ok());
}

#[test]
fn a_leftover_trigger_section_gets_the_flows_rewrite_hint() {
    let text = "[workflow]\nname = \"dev\"\n\
        [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
        [trigger]\ntype = \"webhook\"\nedge = { to = \"a\", kind = \"ask\" }\n";
    let issues = coretempo_core::workflow::validate_workflow(text).expect_err("must fail");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "");
    let message = &issues[0].message;
    assert!(
        message.contains("unknown field"),
        "keeps the serde diagnosis: {message}"
    );
    assert!(message.contains("[flows."), "shows the rewrite: {message}");
    assert!(
        message.contains("agents ="),
        "shows the subset field: {message}"
    );
    // Other unknown fields must NOT get the trigger hint.
    let other = "[workflow]\nname = \"dev\"\n\
        [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n[bogus]\nx = 1\n";
    let issues = coretempo_core::workflow::validate_workflow(other).expect_err("must fail");
    assert!(
        !issues[0].message.contains("[flows."),
        "{}",
        issues[0].message
    );
}

#[test]
fn an_unknown_trigger_field_inside_an_agent_table_gets_no_flows_rewrite_hint() {
    let text = "[workflow]\nname = \"dev\"\n\
        [agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\ntrigger = 1\n";
    let issues = coretempo_core::workflow::validate_workflow(text).expect_err("must fail");
    assert_eq!(issues.len(), 1);
    let message = &issues[0].message;
    assert!(
        message.contains("unknown field"),
        "keeps the serde diagnosis: {message}"
    );
    assert!(
        !message.contains("[flows."),
        "an agent-table `trigger` key is not the top-level [trigger] this hint targets: {message}"
    );
}

#[test]
fn blank_mcp_entry_is_an_issue() {
    let text = "[workflow]\nname = \"dev\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                mcp = [\" \"]\n";
    let issues = validate_workflow(text).unwrap_err();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].path, "agents.a.mcp");
    assert!(issues[0].message.contains("blank"), "{}", issues[0].message);
    assert!(
        issues[0].message.contains(".mcp.json"),
        "names the sources: {}",
        issues[0].message
    );
}

#[test]
fn duplicate_mcp_entry_is_an_issue() {
    let text = "[workflow]\nname = \"dev\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                mcp = [\"context7\", \"context7\"]\n";
    let issues = validate_workflow(text).unwrap_err();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].path, "agents.a.mcp");
    assert!(issues[0].message.contains("twice"), "{}", issues[0].message);
}

#[test]
fn mcp_names_are_accepted() {
    let text = "[workflow]\nname = \"dev\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n\
                mcp = [\"context7\", \"mailbox\"]\n";
    let wf = validate_workflow(text).unwrap();
    assert_eq!(
        wf.agents[&AgentId("a".into())].mcp,
        vec!["context7".to_string(), "mailbox".to_string()]
    );
}

#[test]
fn server_trust_agent_dirs_parses_and_defaults_false() {
    let wf = validate_workflow(VALID).unwrap();
    assert!(!wf.server.trust_agent_dirs);
    let text = format!("{VALID}[server]\ntrust_agent_dirs = true\n");
    assert!(validate_workflow(&text).unwrap().server.trust_agent_dirs);
}

fn issues_for(agent_tail: &str) -> Vec<String> {
    let text = format!(
        "[workflow]\nname = \"dev\"\n[agents.a]\ndir = \"/tmp\"\nprompt = \"p\"\n{agent_tail}"
    );
    match validate_workflow(&text) {
        Ok(_) => Vec::new(),
        Err(issues) => issues
            .into_iter()
            .map(|i| format!("{}: {}", i.path, i.message))
            .collect(),
    }
}

#[test]
fn skills_without_isolated_config_is_rejected() {
    let issues = issues_for("skills = [\"./skills/x\"]\n");
    assert_eq!(
        issues,
        vec![
            "agents.a.skills: declared skills reach the agent only through an isolated \
             config dir; set isolated_config = true or drop skills"
                .to_string()
        ]
    );
}

#[test]
fn blank_and_nameless_skill_paths_are_rejected() {
    let issues = issues_for("isolated_config = true\nskills = [\"\", \"..\"]\n");
    assert_eq!(
        issues,
        vec![
            "agents.a.skills[0]: skill path is empty; point it at a directory holding \
             SKILL.md, e.g. \"./skills/handoff\""
                .to_string(),
            "agents.a.skills[1]: skill path '..' has no directory name; Claude Code keys \
             skills by directory name"
                .to_string(),
        ]
    );
}

#[test]
fn duplicate_skill_names_are_rejected() {
    let issues =
        issues_for("isolated_config = true\nskills = [\"./a/handoff\", \"./b/handoff\"]\n");
    assert_eq!(
        issues,
        vec![
            "agents.a.skills: two entries are both named 'handoff' ('./a/handoff', \
             './b/handoff'); Claude Code keys skills by directory name"
                .to_string()
        ]
    );
}

#[test]
fn every_duplicate_skill_name_is_paired_with_the_first_declaration() {
    let issues = issues_for(
        "isolated_config = true\nskills = [\"./a/handoff\", \"./b/handoff\", \"./c/handoff\"]\n",
    );
    assert_eq!(
        issues,
        vec![
            "agents.a.skills: two entries are both named 'handoff' ('./a/handoff', \
             './b/handoff'); Claude Code keys skills by directory name"
                .to_string(),
            "agents.a.skills: two entries are both named 'handoff' ('./a/handoff', \
             './c/handoff'); Claude Code keys skills by directory name"
                .to_string(),
        ]
    );
}

#[test]
fn isolated_config_alone_is_fine() {
    assert!(issues_for("isolated_config = true\n").is_empty());
}
