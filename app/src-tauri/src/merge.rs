//! Format-preserving tempo.toml merge (spec §4). The graph editor never
//! regenerates the file: untouched keys, comments, and layout stay
//! byte-identical; only genuinely changed values alter the text.

use coretempo_core::types::config::{
    AgentConfig, Edge, OutputConfig, TriggerConfig, TriggerType, WorkflowFile,
};
use coretempo_core::workflow::validate_workflow;
use toml_edit::{DocumentMut, Item, Table, Value};

/// # Errors
/// A human/LLM-readable reason: base text unparsable, or an edited value that
/// cannot be represented (never expected for validated models).
pub fn merge_workflow(text: &str, model: &WorkflowFile) -> Result<String, String> {
    let old = validate_workflow(text).map_err(|issues| {
        format!(
            "cannot merge into an invalid file; fix these first: {}",
            issues
                .iter()
                .map(|i| format!("{}: {}", i.path, i.message))
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| format!("cannot parse the current file as TOML: {e}"))?;

    merge_workflow_section(&mut doc, &old, model)?;
    merge_server_section(&mut doc, &old, model)?;
    merge_agents(&mut doc, &old, model)?;
    merge_trigger_section(&mut doc, &old, model)?;
    Ok(doc.to_string())
}

/// `toml_edit::Item::as_table_mut` fails only if a key holds something other
/// than a table (e.g. an inline table). Every key this module targets is one
/// it created as a table itself, so this is a defensive, never-expected path.
fn as_table<'a>(item: &'a mut Item, key: &str) -> Result<&'a mut Table, String> {
    item.as_table_mut()
        .ok_or_else(|| format!("'{key}' must be a table, not an inline table or array"))
}

/// Write only when the effective value changed; omitted defaults stay omitted.
fn set_scalar<T: PartialEq, F: Fn(&T) -> Value>(
    table: &mut Table,
    key: &str,
    old: &T,
    new: &T,
    to_value: F,
) {
    if old != new {
        table[key] = Item::Value(to_value(new));
    }
}

/// `Option` sibling of [`set_scalar`]: `Some` writes/updates the key, `None`
/// removes it, no change leaves the key (and any comment on it) untouched.
fn set_option<T: PartialEq, F: Fn(&T) -> Value>(
    table: &mut Table,
    key: &str,
    old: Option<&T>,
    new: Option<&T>,
    to_value: F,
) {
    if old == new {
        return;
    }
    match new {
        Some(v) => table[key] = Item::Value(to_value(v)),
        None => {
            table.remove(key);
        }
    }
}

fn merge_workflow_section(
    doc: &mut DocumentMut,
    old: &WorkflowFile,
    model: &WorkflowFile,
) -> Result<(), String> {
    let old_w = &old.workflow;
    let new_w = &model.workflow;
    if old_w == new_w {
        return Ok(());
    }
    let table = as_table(
        doc["workflow"].or_insert(Item::Table(Table::new())),
        "workflow",
    )?;
    set_scalar(table, "name", &old_w.name, &new_w.name, |s| {
        Value::from(s.as_str())
    });
    set_scalar(table, "db", &old_w.db, &new_w.db, |p| {
        Value::from(p.display().to_string())
    });
    set_scalar(table, "port", &old_w.port, &new_w.port, |p| {
        Value::from(i64::from(*p))
    });
    set_scalar(
        table,
        "ask_timeout_minutes",
        &old_w.ask_timeout_minutes,
        &new_w.ask_timeout_minutes,
        |v| Value::from(i64::try_from(*v).unwrap_or(i64::MAX)),
    );
    set_scalar(
        table,
        "idle_debounce_seconds",
        &old_w.idle_debounce_seconds,
        &new_w.idle_debounce_seconds,
        |v| Value::from(*v),
    );
    Ok(())
}

fn merge_server_section(
    doc: &mut DocumentMut,
    old: &WorkflowFile,
    model: &WorkflowFile,
) -> Result<(), String> {
    let old_s = &old.server;
    let new_s = &model.server;
    if old_s == new_s {
        return Ok(());
    }
    let table = as_table(doc["server"].or_insert(Item::Table(Table::new())), "server")?;
    set_option(
        table,
        "bind",
        old_s.bind.as_ref(),
        new_s.bind.as_ref(),
        |ip| Value::from(ip.to_string()),
    );
    set_option(
        table,
        "token_file",
        old_s.token_file.as_ref(),
        new_s.token_file.as_ref(),
        |p| Value::from(p.display().to_string()),
    );
    set_option(table, "log", old_s.log.as_ref(), new_s.log.as_ref(), |s| {
        Value::from(s.as_str())
    });
    if old_s.allowed_origins != new_s.allowed_origins {
        if new_s.allowed_origins.is_empty() {
            table.remove("allowed_origins");
        } else {
            table["allowed_origins"] = Item::Value(Value::from_iter(new_s.allowed_origins.clone()));
        }
    }
    if table.is_empty() {
        doc.remove("server");
    }
    Ok(())
}

fn merge_agents(
    doc: &mut DocumentMut,
    old: &WorkflowFile,
    model: &WorkflowFile,
) -> Result<(), String> {
    let agents_table = as_table(doc["agents"].or_insert(Item::Table(Table::new())), "agents")?;

    for id in old.agents.keys() {
        if !model.agents.contains_key(id) {
            agents_table.remove(&id.0);
        }
    }

    for (id, new_agent) in &model.agents {
        if let Some(old_agent) = old.agents.get(id) {
            let table = as_table(&mut agents_table[id.0.as_str()], &id.0)?;
            merge_agent_fields(table, old_agent, new_agent)?;
        } else {
            let table = new_agent_table(new_agent)?;
            agents_table[id.0.as_str()] = Item::Table(table);
        }
    }
    Ok(())
}

fn merge_agent_fields(
    table: &mut Table,
    old: &AgentConfig,
    new: &AgentConfig,
) -> Result<(), String> {
    set_scalar(table, "dir", &old.dir, &new.dir, |p| {
        Value::from(p.display().to_string())
    });
    if old.prompt != new.prompt {
        table["prompt"] = Item::Value(prompt_value(&new.prompt));
    }
    set_option(
        table,
        "model",
        old.model.as_ref(),
        new.model.as_ref(),
        |s| Value::from(s.as_str()),
    );
    set_option(
        table,
        "permission_mode",
        old.permission_mode.as_ref(),
        new.permission_mode.as_ref(),
        |s| Value::from(s.as_str()),
    );
    set_scalar(table, "auto_clear", &old.auto_clear, &new.auto_clear, |b| {
        Value::from(*b)
    });
    if old.edges != new.edges {
        if new.edges.is_empty() {
            table.remove("edges");
        } else {
            table["edges"] = Item::Value(edges_value(&new.edges)?);
        }
    }
    if old.tools != new.tools {
        if new.tools.is_empty() {
            table.remove("tools");
        } else {
            table["tools"] = Item::Value(Value::from_iter(new.tools.clone()));
        }
    }
    Ok(())
}

/// Template field order for a brand-new `[agents.<id>]` table: `dir`, `prompt`,
/// then optionals only when they carry a non-default value.
fn new_agent_table(agent: &AgentConfig) -> Result<Table, String> {
    let mut table = Table::new();
    table["dir"] = Item::Value(Value::from(agent.dir.display().to_string()));
    table["prompt"] = Item::Value(prompt_value(&agent.prompt));
    if let Some(model) = &agent.model {
        table["model"] = Item::Value(Value::from(model.as_str()));
    }
    if let Some(permission_mode) = &agent.permission_mode {
        table["permission_mode"] = Item::Value(Value::from(permission_mode.as_str()));
    }
    if !agent.auto_clear {
        table["auto_clear"] = Item::Value(Value::from(false));
    }
    if !agent.edges.is_empty() {
        table["edges"] = Item::Value(edges_value(&agent.edges)?);
    }
    if !agent.tools.is_empty() {
        table["tools"] = Item::Value(Value::from_iter(agent.tools.clone()));
    }
    Ok(table)
}

/// Multiline prompts render as `"""` blocks when that round-trips exactly;
/// anything else falls back to `toml_edit`'s escaped single-line string.
fn prompt_value(prompt: &str) -> Value {
    if prompt.contains('\n') {
        let candidate = format!("\"\"\"\n{prompt}\"\"\"");
        if let Ok(v) = candidate.parse::<Value>()
            && v.as_str() == Some(prompt)
        {
            return v;
        }
    }
    Value::from(prompt)
}

/// `AgentId` chars ([a-z0-9_-]) and the two kind literals never need escaping, so
/// building this text and parsing it back is exact.
fn edge_inline(edge: &Edge) -> String {
    match edge.max_rounds {
        Some(max) => format!(
            "{{ to = \"{}\", kind = \"{}\", max_rounds = {max} }}",
            edge.to.0,
            edge.kind.as_str()
        ),
        None => format!(
            "{{ to = \"{}\", kind = \"{}\" }}",
            edge.to.0,
            edge.kind.as_str()
        ),
    }
}

/// Spec §1.1 inline-table-array form.
fn edges_value(edges: &[Edge]) -> Result<Value, String> {
    let items = edges.iter().map(edge_inline).collect::<Vec<_>>().join(", ");
    format!("[{items}]")
        .parse::<Value>()
        .map_err(|e| format!("cannot render edges as TOML: {e}"))
}

/// The trigger's single edge: the same inline table, unwrapped from the array.
fn trigger_edge_value(edge: &Edge) -> Result<Value, String> {
    edge_inline(edge)
        .parse::<Value>()
        .map_err(|e| format!("cannot render the trigger edge as TOML: {e}"))
}

/// The TOML literal behind `#[serde(rename_all = "snake_case")]` on `TriggerType`.
fn trigger_type_str(trigger_type: TriggerType) -> &'static str {
    match trigger_type {
        TriggerType::OnStart => "on_start",
        TriggerType::Webhook => "webhook",
    }
}

/// `[trigger]` is add/update/remove as a whole: absent in the model means the section
/// goes, since the trigger *is* its one edge and a half-written one will not validate.
fn merge_trigger_section(
    doc: &mut DocumentMut,
    old: &WorkflowFile,
    model: &WorkflowFile,
) -> Result<(), String> {
    if old.trigger == model.trigger {
        return Ok(());
    }
    let Some(new_trigger) = &model.trigger else {
        doc.remove("trigger");
        return Ok(());
    };
    let Some(old_trigger) = &old.trigger else {
        doc["trigger"] = Item::Table(new_trigger_table(new_trigger)?);
        return Ok(());
    };
    let table = as_table(&mut doc["trigger"], "trigger")?;
    set_scalar(
        table,
        "type",
        &old_trigger.trigger_type,
        &new_trigger.trigger_type,
        |t| Value::from(trigger_type_str(*t)),
    );
    if old_trigger.edge != new_trigger.edge {
        table["edge"] = Item::Value(trigger_edge_value(&new_trigger.edge)?);
    }
    set_option(
        table,
        "message",
        old_trigger.message.as_ref(),
        new_trigger.message.as_ref(),
        |s| prompt_value(s),
    );
    if old_trigger.output != new_trigger.output {
        match &new_trigger.output {
            None => {
                table.remove("output");
            }
            Some(output) => table["output"] = Item::Table(output_table(output)?),
        }
    }
    Ok(())
}

/// Template field order for a brand-new `[trigger]` table: `type`, `edge`, then the
/// `on_start` message and output contract when there are any.
fn new_trigger_table(trigger: &TriggerConfig) -> Result<Table, String> {
    let mut table = Table::new();
    table["type"] = Item::Value(Value::from(trigger_type_str(trigger.trigger_type)));
    table["edge"] = Item::Value(trigger_edge_value(&trigger.edge)?);
    if let Some(message) = &trigger.message {
        table["message"] = Item::Value(prompt_value(message));
    }
    if let Some(output) = &trigger.output {
        table["output"] = Item::Table(output_table(output)?);
    }
    Ok(table)
}

/// `[trigger.output]` table in template order: schema source, then the budget.
fn output_table(output: &OutputConfig) -> Result<Table, String> {
    let mut table = Table::new();
    if let Some(path) = &output.schema_file {
        table["schema_file"] = Item::Value(Value::from(path.display().to_string()));
    }
    if let Some(schema) = &output.schema {
        table["schema"] = Item::Value(json_to_toml(schema)?);
    }
    table["max_repairs"] = Item::Value(Value::from(i64::from(output.max_repairs)));
    Ok(table)
}

/// JSON Schema value → TOML value. Null has no TOML form and JSON Schema
/// never needs one at a key position; reject it with the fix.
fn json_to_toml(value: &serde_json::Value) -> Result<Value, String> {
    match value {
        serde_json::Value::Null => {
            Err("JSON null cannot be written to TOML; remove the null from the schema".to_string())
        }
        serde_json::Value::Bool(b) => Ok(Value::from(*b)),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::from)
            .or_else(|| n.as_f64().map(Value::from))
            .ok_or_else(|| format!("number {n} does not fit TOML")),
        serde_json::Value::String(s) => Ok(Value::from(s.as_str())),
        serde_json::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(json_to_toml(item)?);
            }
            Ok(Value::Array(array))
        }
        serde_json::Value::Object(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, item) in map {
                table.insert(key, json_to_toml(item)?);
            }
            Ok(Value::InlineTable(table))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coretempo_core::types::AgentId;
    use coretempo_core::types::config::{EdgeKind, TriggerConfig, TriggerType};

    // `planner` exists so edges-to-planner round-trip through validate_workflow's
    // roster check (edge-target existence is validated on reparse, not by merge).
    const COMMENTED: &str = r#"# Top comment survives.
[workflow]
name = "example"   # trailing comment survives

# Section comment survives.
[agents.builder]
dir = "~/projects/x"
prompt = """
You build things.
"""
# auto_clear stays default

[agents.planner]
dir = "/tmp/planner"
prompt = "You plan things."
"#;

    // `builder` carries a `tools` allowlist so rename/edit/empty scenarios have
    // something real to preserve, mutate, or drop.
    const WITH_TOOLS: &str = r#"[workflow]
name = "example"

[agents.builder]
dir = "/tmp/b"
prompt = "You build things."
tools = ["pat"]
"#;

    // A trigger the merge must leave alone unless the model changed it. The interior comment
    // and the hand-spaced inline edge are what a wholesale rebuild of the table would lose.
    const TRIGGERED: &str = r#"[workflow]
name = "example"

[trigger]
# Why this workflow starts on a webhook.
type = "webhook"
edge = {to = "builder",   kind = "ask"}

[agents.builder]
dir = "/tmp/b"
prompt = "You build things."
"#;

    fn parsed(text: &str) -> WorkflowFile {
        coretempo_core::workflow::validate_workflow(text).expect("fixture is valid")
    }

    fn toml_str(text: &str) -> WorkflowFile {
        coretempo_core::workflow::validate_workflow(text).expect("merged text reparses")
    }

    fn builder_edges(model: &WorkflowFile) -> Vec<Edge> {
        model.agents[&AgentId("builder".into())].edges.clone()
    }

    #[test]
    fn unchanged_model_is_a_byte_identical_no_op() {
        let model = parsed(COMMENTED);
        let merged = merge_workflow(COMMENTED, &model).expect("merges");
        assert_eq!(merged, COMMENTED);
    }

    #[test]
    fn scalar_change_touches_only_that_key() {
        let mut model = parsed(COMMENTED);
        model.workflow.name = "renamed".to_string();
        let merged = merge_workflow(COMMENTED, &model).expect("merges");
        assert!(merged.contains("name = \"renamed\""));
        assert!(merged.contains("# Top comment survives."));
        assert!(merged.contains("# Section comment survives."));
        assert!(merged.contains("# auto_clear stays default"));
    }

    #[test]
    fn edges_write_in_inline_table_array_form_and_round_trip() {
        let mut model = parsed(COMMENTED);
        let builder = model
            .agents
            .get_mut(&AgentId("builder".into()))
            .expect("builder");
        builder.edges = vec![Edge {
            to: AgentId("planner".into()),
            kind: EdgeKind::Ask,
            max_rounds: None,
        }];
        // (target existence is validate_workflow's job, not merge's)
        let merged = merge_workflow(COMMENTED, &model).expect("merges");
        assert!(merged.contains(r#"edges = [{ to = "planner", kind = "ask" }]"#));
        let reparsed: WorkflowFile = toml_str(&merged);
        assert_eq!(
            reparsed.agents[&AgentId("builder".into())].edges,
            builder_edges(&model)
        );
    }

    #[test]
    fn loop_edges_round_trip_with_max_rounds() {
        let mut model = parsed(COMMENTED);
        let builder = model
            .agents
            .get_mut(&AgentId("builder".into()))
            .expect("builder");
        builder.edges = vec![Edge {
            to: AgentId("planner".into()),
            kind: EdgeKind::Loop,
            max_rounds: Some(4),
        }];
        let merged = merge_workflow(COMMENTED, &model).expect("merges");
        assert!(
            merged.contains(r#"edges = [{ to = "planner", kind = "loop", max_rounds = 4 }]"#),
            "loop kind and cap survive the merge: {merged}"
        );
        let reparsed: WorkflowFile = toml_str(&merged);
        assert_eq!(
            reparsed.agents[&AgentId("builder".into())].edges,
            builder_edges(&model)
        );
    }

    #[test]
    fn new_agent_appends_with_template_field_order() {
        let mut model = parsed(COMMENTED);
        model.agents.insert(
            AgentId("reviewer".into()),
            AgentConfig {
                dir: "/tmp/review".into(),
                prompt: "Line one.\nLine two.".into(),
                model: Some("opus".into()),
                permission_mode: None,
                auto_clear: true,
                edges: Vec::new(),
                tools: Vec::new(),
            },
        );
        let merged = merge_workflow(COMMENTED, &model).expect("merges");
        assert!(merged.contains("[agents.reviewer]"));
        let reparsed: WorkflowFile = toml_str(&merged);
        assert_eq!(
            reparsed.agents[&AgentId("reviewer".into())].prompt,
            "Line one.\nLine two.",
            "multiline prompt round-trips"
        );
        let dir_at = merged.find("[agents.reviewer]").expect("table");
        let d = merged[dir_at..].find("dir =").expect("dir");
        let p = merged[dir_at..].find("prompt =").expect("prompt");
        let m = merged[dir_at..].find("model =").expect("model");
        assert!(
            d < p && p < m,
            "template order: dir, prompt, then optionals"
        );
    }

    #[test]
    fn renaming_an_agent_preserves_its_tools_key() {
        let mut model = parsed(WITH_TOOLS);
        let old_agent = model
            .agents
            .remove(&AgentId("builder".into()))
            .expect("builder");
        model.agents.insert(AgentId("builder2".into()), old_agent);
        let merged = merge_workflow(WITH_TOOLS, &model).expect("merges");
        assert!(
            !merged.contains("[agents.builder]"),
            "old table gone: {merged}"
        );
        assert!(
            merged.contains("[agents.builder2]"),
            "new table present: {merged}"
        );
        let reparsed = toml_str(&merged);
        assert_eq!(
            reparsed.agents[&AgentId("builder2".into())].tools,
            vec!["pat".to_string()],
            "tools survives the rename: {merged}"
        );
    }

    #[test]
    fn editing_tools_in_place_persists() {
        let mut model = parsed(WITH_TOOLS);
        let builder = model
            .agents
            .get_mut(&AgentId("builder".into()))
            .expect("builder");
        builder.tools = vec!["pat".to_string(), "jq".to_string()];
        let merged = merge_workflow(WITH_TOOLS, &model).expect("merges");
        assert!(
            merged.contains(r#"tools = ["pat", "jq"]"#),
            "edited tools list is written: {merged}"
        );
        let reparsed = toml_str(&merged);
        assert_eq!(
            reparsed.agents[&AgentId("builder".into())].tools,
            vec!["pat".to_string(), "jq".to_string()]
        );
    }

    #[test]
    fn emptying_tools_removes_the_key() {
        let mut model = parsed(WITH_TOOLS);
        let builder = model
            .agents
            .get_mut(&AgentId("builder".into()))
            .expect("builder");
        builder.tools = Vec::new();
        let merged = merge_workflow(WITH_TOOLS, &model).expect("merges");
        assert!(!merged.contains("tools ="), "tools key gone: {merged}");
        assert_eq!(
            toml_str(&merged).agents[&AgentId("builder".into())].tools,
            Vec::<String>::new()
        );
    }

    #[test]
    fn removed_agent_table_is_deleted() {
        let two = format!("{COMMENTED}\n[agents.extra]\ndir = \"/tmp\"\nprompt = \"x\"\n");
        let mut model = parsed(&two);
        model.agents.remove(&AgentId("extra".into()));
        let merged = merge_workflow(&two, &model).expect("merges");
        assert!(!merged.contains("[agents.extra]"));
        assert!(merged.contains("[agents.builder]"));
    }

    #[test]
    fn adding_a_trigger_appends_the_section() {
        let mut model = parsed(COMMENTED);
        model.trigger = Some(TriggerConfig {
            trigger_type: TriggerType::OnStart,
            edge: Edge {
                to: AgentId("builder".into()),
                kind: EdgeKind::Ask,
                max_rounds: None,
            },
            message: Some("Go.".into()),
            output: None,
        });
        let merged = merge_workflow(COMMENTED, &model).expect("merges");
        assert!(merged.contains("[trigger]"));
        assert!(merged.contains(r#"type = "on_start""#));
        assert!(merged.contains(r#"edge = { to = "builder", kind = "ask" }"#));
        let reparsed = toml_str(&merged);
        assert_eq!(reparsed.trigger, model.trigger);
        // What `git diff` shows after saving from the canvas: an appended section, nothing else.
        let appended = concat!(
            "\n[trigger]\n",
            "type = \"on_start\"\n",
            "edge = { to = \"builder\", kind = \"ask\" }\n",
            "message = \"Go.\"\n",
        );
        assert_eq!(merged, format!("{COMMENTED}{appended}"));
    }

    #[test]
    fn a_multiline_on_start_message_round_trips() {
        let mut model = parsed(COMMENTED);
        model.trigger = Some(TriggerConfig {
            trigger_type: TriggerType::OnStart,
            edge: Edge {
                to: AgentId("builder".into()),
                kind: EdgeKind::Send,
                max_rounds: None,
            },
            message: Some("Line one.\nLine two.\n".into()),
            output: None,
        });
        let merged = merge_workflow(COMMENTED, &model).expect("merges");
        let reparsed = toml_str(&merged);
        assert_eq!(reparsed.trigger, model.trigger);
    }

    #[test]
    fn removing_the_trigger_deletes_the_section() {
        let mut model = parsed(TRIGGERED);
        model.trigger = None;
        let merged = merge_workflow(TRIGGERED, &model).expect("merges");
        assert!(!merged.contains("[trigger]"), "section gone: {merged}");
        assert!(merged.contains("[agents.builder]"), "agents untouched");
        assert_eq!(toml_str(&merged).trigger, None);
    }

    #[test]
    fn unchanged_trigger_is_byte_identical() {
        let model = parsed(TRIGGERED);
        let merged = merge_workflow(TRIGGERED, &model).expect("merges");
        assert_eq!(merged, TRIGGERED);
    }

    #[test]
    fn retargeting_a_trigger_rewrites_only_the_edge() {
        let two = format!("{TRIGGERED}\n[agents.planner]\ndir = \"/tmp/p\"\nprompt = \"plan\"\n");
        let mut model = parsed(&two);
        let trigger = model.trigger.as_mut().expect("fixture has a trigger");
        trigger.edge = Edge {
            to: AgentId("planner".into()),
            kind: EdgeKind::Send,
            max_rounds: None,
        };
        let merged = merge_workflow(&two, &model).expect("merges");
        assert!(merged.contains(r#"edge = { to = "planner", kind = "send" }"#));
        assert!(merged.contains(r#"type = "webhook""#), "type left alone");
        assert!(merged.contains("# Why this workflow starts on a webhook."));
        assert_eq!(toml_str(&merged).trigger, model.trigger);
    }

    #[test]
    fn switching_a_webhook_to_on_start_writes_the_message() {
        let mut model = parsed(TRIGGERED);
        let trigger = model.trigger.as_mut().expect("fixture has a trigger");
        trigger.trigger_type = TriggerType::OnStart;
        trigger.message = Some("Kick off.".into());
        let merged = merge_workflow(TRIGGERED, &model).expect("merges");
        assert!(merged.contains(r#"type = "on_start""#));
        assert!(merged.contains(r#"message = "Kick off.""#));
        assert_eq!(toml_str(&merged).trigger, model.trigger);
    }

    #[test]
    fn switching_on_start_to_a_webhook_removes_the_message() {
        let on_start = TRIGGERED.replace(
            r#"type = "webhook""#,
            "type = \"on_start\"\nmessage = \"Go.\"",
        );
        let mut model = parsed(&on_start);
        let trigger = model.trigger.as_mut().expect("fixture has a trigger");
        trigger.trigger_type = TriggerType::Webhook;
        trigger.message = None;
        let merged = merge_workflow(&on_start, &model).expect("merges");
        assert!(!merged.contains("message ="), "message key gone: {merged}");
        assert_eq!(toml_str(&merged).trigger, model.trigger);
    }

    #[test]
    fn invalid_base_text_is_an_error_not_a_rewrite() {
        let model = parsed(COMMENTED);
        let err = merge_workflow("not toml [", &model).expect_err("must refuse");
        assert!(err.contains("parse"), "says why: {err}");
    }

    // A webhook trigger with an inline output schema, for the `[trigger.output]`
    // round-trip tests below.
    const WITH_OUTPUT: &str = r#"[workflow]
name = "example"

[trigger]
type = "webhook"
edge = { to = "builder", kind = "ask" }

[trigger.output]
schema = { type = "object", properties = { name = { type = "string" } }, required = ["name"] }
max_repairs = 1

[agents.builder]
dir = "/tmp/b"
prompt = "You build things."
"#;

    #[test]
    fn inline_output_schema_survives_unrelated_merge() {
        let mut model = parsed(WITH_OUTPUT);
        let builder = model
            .agents
            .get_mut(&AgentId("builder".into()))
            .expect("builder");
        builder.dir = "/tmp/b2".into();
        let merged = merge_workflow(WITH_OUTPUT, &model).expect("merges");
        assert!(
            merged.contains(r#"schema = { type = "object", properties = { name = { type = "string" } }, required = ["name"] }"#),
            "schema keys preserved: {merged}"
        );
        assert!(
            merged.contains("max_repairs = 1"),
            "max_repairs preserved: {merged}"
        );
        assert_eq!(toml_str(&merged), model, "reparses to an equal model");
    }

    #[test]
    fn adding_output_via_model_writes_the_section() {
        let mut model = parsed(TRIGGERED);
        let trigger = model.trigger.as_mut().expect("fixture has a trigger");
        trigger.output = Some(OutputConfig {
            schema: None,
            schema_file: Some("schemas/x.json".into()),
            max_repairs: 2,
        });
        let merged = merge_workflow(TRIGGERED, &model).expect("merges");
        assert!(
            merged.contains(r#"schema_file = "schemas/x.json""#),
            "schema_file written: {merged}"
        );
        assert_eq!(toml_str(&merged).trigger, model.trigger);
    }

    #[test]
    fn removing_output_via_model_deletes_the_section() {
        let mut model = parsed(WITH_OUTPUT);
        let trigger = model.trigger.as_mut().expect("fixture has a trigger");
        trigger.output = None;
        let merged = merge_workflow(WITH_OUTPUT, &model).expect("merges");
        assert!(!merged.contains("output"), "output section gone: {merged}");
        assert!(
            merged.contains(r#"edge = { to = "builder", kind = "ask" }"#),
            "rest of [trigger] intact: {merged}"
        );
        assert_eq!(toml_str(&merged).trigger, model.trigger);
    }
}
