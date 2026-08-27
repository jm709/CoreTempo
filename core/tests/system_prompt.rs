#![expect(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use coretempo_core::types::id::AgentId;
use coretempo_core::workflow::{load_workflow, validate_workflow};

static DIR_N: AtomicU64 = AtomicU64::new(0);

/// Per-call unique: tests run concurrently, and a shared path lets one test read
/// the file while another is mid-`write`.
fn temp_dir(tag: &str) -> PathBuf {
    let n = DIR_N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("coretempo-{tag}-{}-{n}", std::process::id()))
}

fn frozen() -> coretempo_core::types::config::FrozenWorkflow {
    let text = "[workflow]\nname = \"dev\"\n\
                [agents.planner]\ndir = \"/tmp\"\nprompt = \"You are the planning agent.\"\n\
                [agents.builder]\ndir = \"/tmp\"\nprompt = \"You build.\"\n\
                [agents.reviewer]\ndir = \"/tmp\"\nprompt = \"You review.\"\n";
    let dir = temp_dir("primer");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(&path, text).unwrap();
    // validate first so a test failure points at the right layer
    validate_workflow(text).unwrap();
    load_workflow(&path).unwrap().1
}

#[test]
fn primer_contains_role_protocol_and_roster() {
    let wf = frozen();
    let prompt = wf.system_prompt(&AgentId("planner".into())).unwrap();
    // role prompt first
    assert!(prompt.starts_with("You are the planning agent."));
    // identity + workflow
    assert!(prompt.contains("agent 'planner'"));
    assert!(prompt.contains("workflow 'dev'"));
    // roster excludes self, lexicographic
    assert!(prompt.contains("builder, reviewer"));
    assert!(!prompt.contains("planner, builder"));
    // protocol essentials
    assert!(prompt.contains("tempo ask"));
    assert!(prompt.contains("tempo send"));
    assert!(prompt.contains("tempo reply <id> --code 0"));
    assert!(prompt.contains("reply expected"));
    assert!(prompt.contains("End your turn after asking"));
    // Amendment 31: the primer has to say what a flow-labelled header means,
    // or the label the kickoff now carries is noise the agent cannot act on.
    assert!(
        prompt.contains("[CoreTempo <id> from http, flow <name> — reply expected]"),
        "the primer shows the labelled header: {prompt}"
    );
    assert!(
        prompt.contains("A header without a flow is not a kickoff"),
        "and says what an unlabelled one means: {prompt}"
    );
}

#[test]
fn unknown_agent_returns_none() {
    assert!(frozen().system_prompt(&AgentId("ghost".into())).is_none());
}

fn frozen_with_edges() -> coretempo_core::types::config::FrozenWorkflow {
    let text = "[workflow]\nname = \"dev\"\n\
                [agents.planner]\ndir = \"/tmp\"\nprompt = \"You are the planning agent.\"\n\
                edges = [\n\
                \x20 { to = \"builder\", kind = \"ask\" },\n\
                \x20 { to = \"notifier\", kind = \"send\" },\n\
                ]\n\
                [agents.builder]\ndir = \"/tmp\"\nprompt = \"You build.\"\n\
                [agents.notifier]\ndir = \"/tmp\"\nprompt = \"You notify.\"\n";
    let dir = temp_dir("edges");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(&path, text).unwrap();
    // validate first so a test failure points at the right layer
    validate_workflow(text).unwrap();
    load_workflow(&path).unwrap().1
}

#[test]
fn edged_agent_prompt_lists_steps_in_order() {
    // Build a FrozenWorkflow the same way the file's existing tests do, with
    // planner.edges = [ask builder, send notifier].
    let wf = frozen_with_edges();
    let prompt = wf
        .system_prompt(&AgentId("planner".into()))
        .expect("known agent");
    let steps_at = prompt
        .find("Required workflow steps")
        .expect("steps block present");
    let ask_at = prompt.find("1. tempo ask builder").expect("ask step");
    let send_at = prompt.find("2. tempo send notifier").expect("send step");
    assert!(steps_at < ask_at && ask_at < send_at, "steps in edge order");
    assert!(
        prompt.contains("end your turn; the reply will arrive as a new prompt"),
        "ask steps must never say to wait"
    );
    assert!(prompt.contains("Do not skip these steps."));
}

#[test]
fn edge_free_agent_prompt_is_unchanged() {
    let wf = frozen_with_edges();
    let prompt = wf
        .system_prompt(&AgentId("builder".into()))
        .expect("known agent");
    assert!(!prompt.contains("Required workflow steps"));
}

#[test]
fn every_prompt_carries_the_quiet_protocol() {
    // Edge-semantics spec (2026-08-05): politeness wastes tokens and re-opens
    // turns. Every agent's primer forbids acknowledgement traffic.
    let wf = frozen_with_edges();
    for id in ["planner", "builder", "notifier"] {
        let prompt = wf.system_prompt(&AgentId(id.into())).expect("known agent");
        assert!(
            prompt.contains(
                "Never send acknowledgements, sign-offs, or thanks. If a message \
                 requires no new work from you, do nothing and end your turn."
            ),
            "{id} prompt missing the quiet-protocol line"
        );
        assert!(
            prompt.contains("write durable state to files"),
            "{id} prompt missing the durable-state guidance"
        );
    }
}

fn frozen_with_loop() -> coretempo_core::types::config::FrozenWorkflow {
    let text = "[workflow]\nname = \"dev\"\n\
                [agents.owner]\ndir = \"/tmp\"\nprompt = \"You supervise.\"\n\
                edges = [{ to = \"worker\", kind = \"loop\" }]\n\
                [agents.worker]\ndir = \"/tmp\"\nprompt = \"You work.\"\n";
    let dir = temp_dir("loop");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(&path, text).unwrap();
    validate_workflow(text).unwrap();
    load_workflow(&path).unwrap().1
}

#[test]
fn loop_edge_composes_the_loop_step() {
    let wf = frozen_with_loop();
    let prompt = wf
        .system_prompt(&AgentId("owner".into()))
        .expect("known agent");
    assert!(prompt.contains("1. Loop with worker:"));
    assert!(prompt.contains("tempo ask worker"));
    assert!(
        prompt.contains("each round message must be self-contained"),
        "rounds must carry their own context: the target resets between rounds"
    );
    assert!(prompt.contains("the target's context resets between rounds"));
    assert!(prompt.contains("tempo done worker"));
    assert!(prompt.contains("Never leave a loop open."));
}

fn frozen_with_output() -> coretempo_core::types::config::FrozenWorkflow {
    let text = "[workflow]\nname = \"dev\"\n\
                [agents.a]\ndir = \"/tmp\"\nprompt = \"You are a.\"\n\
                [agents.b]\ndir = \"/tmp\"\nprompt = \"You are b.\"\n\
                [flows.hook]\nagents = [\"a\", \"b\"]\n\
                trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n\
                [flows.hook.output]\n\
                schema = { type = \"object\", required = [\"name\"] }\n\
                max_repairs = 2\n";
    let dir = temp_dir("output");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(&path, text).unwrap();
    // validate first so a test failure points at the right layer
    validate_workflow(text).unwrap();
    load_workflow(&path).unwrap().1
}

/// Two webhook flows contracting the same agent: the whole-pool warm run
/// composes both schemas into one prompt.
fn frozen_with_two_contracts() -> coretempo_core::types::config::FrozenWorkflow {
    let text = "[workflow]\nname = \"dev\"\n\
                [agents.a]\ndir = \"/tmp\"\nprompt = \"You are a.\"\n\
                [flows.alpha]\nagents = [\"a\"]\n\
                trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n\
                [flows.alpha.output]\n\
                schema = { type = \"object\", required = [\"verdict\"] }\n\
                [flows.beta]\nagents = [\"a\"]\n\
                trigger = { type = \"webhook\", edge = { to = \"a\", kind = \"ask\" } }\n\
                [flows.beta.output]\n\
                schema = { type = \"object\", required = [\"summary\"] }\n";
    let dir = temp_dir("two-contracts");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tempo.toml");
    std::fs::write(&path, text).unwrap();
    validate_workflow(text).unwrap();
    load_workflow(&path).unwrap().1
}

#[test]
fn two_contracts_on_one_agent_are_labelled_by_flow() {
    let prompt = frozen_with_two_contracts()
        .system_prompt(&AgentId("a".into()))
        .expect("known agent");
    // Unlabelled, the two blocks read as contradictory instructions.
    assert!(
        prompt.contains("Output contract for flow 'alpha'"),
        "each block names its flow: {prompt}"
    );
    assert!(
        prompt.contains("Output contract for flow 'beta'"),
        "each block names its flow: {prompt}"
    );
    assert!(prompt.contains("\"verdict\"") && prompt.contains("\"summary\""));
    assert!(
        prompt.contains("Only 'alpha' kickoffs are held to this schema")
            && prompt.contains("Only 'beta' kickoffs are held to this schema"),
        "each block scopes itself to its own flow's kickoff: {prompt}"
    );
    assert!(
        prompt.contains("[CoreTempo <id> from http, flow alpha — reply expected]")
            && prompt.contains("[CoreTempo <id> from http, flow beta — reply expected]"),
        "each block shows the header its own kickoff arrives with (amendment 31): {prompt}"
    );
    assert!(
        prompt.contains("repair it against the validation errors"),
        "the rejection stays the fallback when a labelled reply is refused: {prompt}"
    );
}

#[test]
fn output_contract_appears_in_target_prompt_only() {
    let wf = frozen_with_output();
    let prompt_a = wf.system_prompt(&AgentId("a".into())).expect("known agent");
    assert!(
        prompt_a.contains("Output contract"),
        "target gets the block"
    );
    assert!(prompt_a.contains("\"required\""), "schema text present");
    assert!(prompt_a.contains("--json-file"), "file route offered");
    assert!(prompt_a.contains("--code 1"), "escape hatch named");
    let prompt_b = wf.system_prompt(&AgentId("b".into())).expect("known agent");
    assert!(
        !prompt_b.contains("Output contract"),
        "non-targets unchanged"
    );
}
