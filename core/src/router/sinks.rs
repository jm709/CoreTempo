//! Injection templates (contracts §3.2, exact format strings) and per-origin sink helpers.
//! The full protocol primer lives in each agent's generated `--append-system-prompt`;
//! injected messages stay short.

use crate::types::config::{Edge, EdgeKind};
use crate::types::id::{AgentId, MessageId};
use crate::types::message::Origin;

/// `{sender}` rendering (contracts §3.2): `Agent(id)` → bare id, `User` → `user`,
/// `Http(_)` → `http`.
pub(crate) fn origin_label(origin: &Origin) -> String {
    match origin {
        Origin::Agent(id) => id.0.clone(),
        Origin::User => "user".to_string(),
        Origin::Http(_) => "http".to_string(),
    }
}

/// One injection: two lines joined by `\n`.
pub(crate) fn render_ask(id: &MessageId, from: &Origin, body: &str) -> String {
    format!(
        "[CoreTempo {id} from {sender} — reply expected] {body}\nReply first with: tempo \
         reply {id} --code 0 '<answer>' (--code 1 on failure), then continue.",
        id = id.0,
        sender = origin_label(from),
    )
}

pub(crate) fn render_send(id: &MessageId, from: &Origin, body: &str) -> String {
    format!("[CoreTempo {} from {}] {}", id.0, origin_label(from), body)
}

pub(crate) fn render_reply(id: &MessageId, replier: &AgentId, code: u8, body: &str) -> String {
    format!(
        "[CoreTempo reply to {} from {} — code {}] {}",
        id.0, replier.0, code, body
    )
}

/// Obligation nudge (spec §2): names the exact missing commands. Typed by the
/// queue worker like any injection — not a `MessageRecord`.
pub(crate) fn render_nudge(unmet: &[Edge]) -> String {
    let steps = unmet
        .iter()
        .map(|e| match e.kind {
            EdgeKind::Ask => format!("tempo ask {} \"<your delegation>\"", e.to.0),
            EdgeKind::Send => format!("tempo send {} \"<your message>\"", e.to.0),
            EdgeKind::Loop => format!(
                "tempo ask {} \"<next round>\" (or `tempo done {}` if the task is complete)",
                e.to.0, e.to.0
            ),
        })
        .collect::<Vec<_>>()
        .join(", then ");
    format!(
        "[CoreTempo] You have not completed your required workflow steps. \
         Run now, in order: {steps}."
    )
}

/// Owed-reply nudge: the agent went idle without answering an ask it received.
/// Takes precedence over [`render_nudge`] — someone is blocked on this agent.
pub(crate) fn render_reply_nudge(owed: &[MessageId]) -> String {
    let ids = owed
        .iter()
        .map(|id| id.0.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // Runnable as typed, for the first one owed: a placeholder id is one more
    // thing to get wrong when the agent is already off the rails.
    let first = owed.first().map_or("<id>", |id| id.0.as_str());
    format!(
        "[CoreTempo] You still owe a reply to: {ids}. Run \
         `tempo reply {first} --code 0 '<answer>'` (or --code 1 with why you \
         could not) before anything else. The asker is blocked on you."
    )
}

/// One-shot nudge when a loop's round cap is reached (edge-semantics spec):
/// never force-done — the owner decides between ending and escalating.
pub(crate) fn render_cap_nudge(capped: &[(AgentId, u32)]) -> String {
    let parts = capped
        .iter()
        .map(|(to, max)| {
            format!(
                "the loop with {to} hit its round cap ({max}) — run `tempo done {to}` \
                 if the task is complete, or report your state to whoever kicked you off",
                to = to.0,
                max = max
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("[CoreTempo] {parts}.")
}

#[cfg(test)]
mod tests {
    use crate::router::sinks::{origin_label, render_ask, render_reply, render_send};
    use crate::types::id::{AgentId, MessageId};
    use crate::types::message::Origin;

    #[test]
    fn origin_labels() {
        assert_eq!(
            origin_label(&Origin::Agent(AgentId("planner".to_string()))),
            "planner"
        );
        assert_eq!(origin_label(&Origin::User), "user");
        assert_eq!(origin_label(&Origin::Http("1f2e3d4c".to_string())), "http");
    }

    #[test]
    fn ask_template_is_two_lines_with_reply_instruction() {
        let text = render_ask(
            &MessageId("m-a3f91c2e".to_string()),
            &Origin::Agent(AgentId("planner".to_string())),
            "Is the schema migration done?",
        );
        assert_eq!(
            text,
            "[CoreTempo m-a3f91c2e from planner — reply expected] Is the schema migration \
             done?\nReply first with: tempo reply m-a3f91c2e --code 0 '<answer>' (--code 1 \
             on failure), then continue."
        );
    }

    #[test]
    fn send_template() {
        let text = render_send(
            &MessageId("m-b7c2d1e0".to_string()),
            &Origin::Agent(AgentId("planner".to_string())),
            "Deploy is green.",
        );
        assert_eq!(text, "[CoreTempo m-b7c2d1e0 from planner] Deploy is green.");
    }

    #[test]
    fn send_template_from_user_and_http() {
        let id = MessageId("m-b7c2d1e0".to_string());
        assert_eq!(
            render_send(&id, &Origin::User, "hi"),
            "[CoreTempo m-b7c2d1e0 from user] hi"
        );
        assert_eq!(
            render_send(&id, &Origin::Http("1f2e3d4c".to_string()), "hi"),
            "[CoreTempo m-b7c2d1e0 from http] hi"
        );
    }

    #[test]
    fn reply_template() {
        let text = render_reply(
            &MessageId("m-a3f91c2e".to_string()),
            &AgentId("builder".to_string()),
            0,
            "Yes, migration 004 applied and tested.",
        );
        assert_eq!(
            text,
            "[CoreTempo reply to m-a3f91c2e from builder — code 0] Yes, migration 004 \
             applied and tested."
        );
    }
}
