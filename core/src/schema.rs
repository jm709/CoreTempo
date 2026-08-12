//! Output contracts for webhook triggers (design 2026-08-06): deterministic
//! repair, JSON parse, and Draft 2020-12 validation. One compiled validator is
//! shared by the router (what it accepts) and the trigger watcher (what it
//! returns).

use serde_json::Value;

use crate::types::AgentId;

pub struct OutputContract {
    /// Raw schema, pretty-printed into the target agent's system prompt.
    pub schema: Value,
    pub target: AgentId,
    /// Rejections allowed before the router accepts and the trigger fails.
    pub max_repairs: u32,
    validator: jsonschema::Validator,
}

impl std::fmt::Debug for OutputContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputContract")
            .field("target", &self.target)
            .field("max_repairs", &self.max_repairs)
            .finish_non_exhaustive()
    }
}

impl OutputContract {
    /// Compiles `schema` under pinned Draft 2020-12. External `$ref`s are
    /// rejected up front: resolution is disabled (no network, no filesystem),
    /// and the compiler's own failure for them is cryptic.
    ///
    /// # Errors
    /// A human-readable compile diagnostic, for `ValidationIssue.message`.
    pub fn compile(
        schema: Value,
        target: AgentId,
        max_repairs: u32,
    ) -> Result<OutputContract, String> {
        if let Some(reference) = find_external_ref(&schema) {
            return Err(format!(
                "schema contains external $ref '{reference}'; only internal \
                 references (starting with '#') are supported — inline the \
                 referenced schema instead"
            ));
        }
        let validator = jsonschema::draft202012::new(&schema)
            .map_err(|e| format!("schema does not compile under draft 2020-12: {e}"))?;
        Ok(OutputContract {
            schema,
            target,
            max_repairs,
            validator,
        })
    }

    /// Repair → parse → validate. `Ok` is the parsed (possibly unwrapped)
    /// value; `Err` lines are LLM-readable, one violation each (max 10).
    ///
    /// # Errors
    /// One line per violation: `at <instance>: <message>  [schema: <pointer>]`.
    pub fn check(&self, raw: &str) -> Result<Value, Vec<String>> {
        let text = repair_text(raw);
        let parsed: Value =
            serde_json::from_str(text).map_err(|e| vec![format!("not valid JSON: {e}")])?;
        let value = self.unwrap_output(parsed);
        let errors: Vec<String> = self
            .validator
            .iter_errors(&value)
            .take(10)
            .map(|error| {
                let instance_path = error.instance_path().to_string();
                let at = pointer_or_root(&instance_path);
                format!("at {at}: {error}  [schema: {}]", error.schema_path())
            })
            .collect();
        if errors.is_empty() {
            Ok(value)
        } else {
            Err(errors)
        }
    }

    /// Unwraps `{"output": {...}}` when the schema's root is an object that
    /// does not declare an `output` property (claude-agent-sdk-python #571:
    /// this exact wrapping otherwise burns every retry).
    fn unwrap_output(&self, value: Value) -> Value {
        let root_is_object = self.schema.get("type").and_then(Value::as_str) == Some("object")
            || self.schema.get("properties").is_some();
        let declares_output = self
            .schema
            .get("properties")
            .and_then(|p| p.get("output"))
            .is_some();
        if !root_is_object || declares_output {
            return value;
        }
        match value {
            Value::Object(mut map) if map.len() == 1 && map.contains_key("output") => {
                match map.remove("output") {
                    Some(inner) => inner,
                    None => Value::Object(map),
                }
            }
            other => other,
        }
    }
}

fn pointer_or_root(pointer: &str) -> &str {
    if pointer.is_empty() {
        "(root)"
    } else {
        pointer
    }
}

/// First `$ref` string not starting with `#`, searched recursively.
fn find_external_ref(schema: &Value) -> Option<&str> {
    match schema {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref")
                && !r.starts_with('#')
            {
                return Some(r);
            }
            map.values().find_map(find_external_ref)
        }
        Value::Array(items) => items.iter().find_map(find_external_ref),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

/// Deterministic zero-token repair: trim, strip one markdown fence, and when
/// prose surrounds the JSON take the first balanced `{...}`/`[...]` span
/// (string- and escape-aware). Returns the input unchanged when no candidate
/// span exists — the JSON parse then produces the error.
#[must_use]
pub fn repair_text(raw: &str) -> &str {
    let mut text = raw.trim();
    if let Some(rest) = text.strip_prefix("```") {
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(end) = rest.rfind("```") {
            text = rest[..end].trim();
        }
    }
    if text.starts_with('{') || text.starts_with('[') {
        return text;
    }
    balanced_span(text).unwrap_or(text)
}

/// First balanced JSON object/array span in `text`, respecting strings.
fn balanced_span(text: &str) -> Option<&str> {
    let start = text.find(['{', '['])?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The 422 body for a rejected `tempo reply` (read by the agent as CLI stderr).
#[must_use]
pub fn render_rejection(errors: &[String], attempts_left: u32) -> String {
    let plural = if attempts_left == 1 {
        "attempt"
    } else {
        "attempts"
    };
    format!(
        "tempo reply rejected: the reply body does not match the workflow's \
         output schema.\n- {}\nReply with ONLY the JSON object — no prose, no \
         markdown fences. Consider writing it to a file and using \
         `tempo reply <id> --code 0 --json-file <path>`.\n{attempts_left} \
         {plural} remain; after that the trigger fails and the caller gets \
         these errors.\nIf you cannot produce this shape, reply with --code 1 \
         and a plain-text explanation instead.",
        errors.join("\n- ")
    )
}

/// The trigger-failure reason when the accepted final reply still mismatches.
#[must_use]
pub fn render_trigger_failure(errors: &[String], attempts_used: u32) -> String {
    format!(
        "the agent's reply did not match the output schema after {attempts_used} \
         rejection(s):\n- {}",
        errors.join("\n- ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(schema: serde_json::Value) -> OutputContract {
        OutputContract::compile(schema, AgentId("t".into()), 2)
            .unwrap_or_else(|e| panic!("schema must compile: {e}"))
    }

    fn person_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" }, "age": { "type": "number" } },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    #[test]
    fn repair_strips_fences_and_prose() {
        assert_eq!(repair_text("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(repair_text("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(
            repair_text("Here you go: {\"a\": \"b}\"} done"),
            "{\"a\": \"b}\"}"
        );
        assert_eq!(repair_text("  [1, 2]  "), "[1, 2]");
        assert_eq!(repair_text("no json here"), "no json here");
    }

    #[test]
    fn check_accepts_valid_and_unwraps_output_key() {
        let c = contract(person_schema());
        let v = c
            .check("{\"name\":\"ada\"}")
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(v["name"], "ada");
        // {"output": {...}} unwrap: root is an object schema without an `output` property.
        let v = c
            .check("{\"output\":{\"name\":\"ada\"}}")
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(v["name"], "ada");
    }

    #[test]
    fn check_rejects_with_pointer_lines() {
        let c = contract(person_schema());
        let errors = c.check("{\"age\": 3}").expect_err("missing required name");
        assert!(errors.iter().any(|e| e.contains("name")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("[schema:")), "{errors:?}");
        let errors = c.check("not json at all").expect_err("unparseable");
        assert!(errors[0].contains("not valid JSON"), "{errors:?}");
    }

    #[test]
    fn compile_rejects_external_ref() {
        let schema = serde_json::json!({"$ref": "https://example.com/x.json"});
        let err = OutputContract::compile(schema, AgentId("t".into()), 2)
            .expect_err("external ref must fail");
        assert!(err.contains("$ref"), "{err}");
    }

    #[test]
    fn rejection_text_names_the_escape_hatch() {
        let text = render_rejection(&["at /x: bad".to_string()], 1);
        assert!(text.contains("tempo reply rejected"));
        assert!(text.contains("1 attempt"));
        assert!(text.contains("--code 1"));
    }
}
