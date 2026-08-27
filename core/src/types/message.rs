//! Canonical message record and origin types (contracts §2.2).

use serde::{Deserialize, Serialize};

use crate::time::Timestamp;
use crate::types::id::{AgentId, MessageId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Ask,
    Send,
}

impl MessageKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Ask => "ask",
            MessageKind::Send => "send",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid message kind '{0}'; valid kinds: ask, send")]
pub struct ParseKindError(String);

impl std::str::FromStr for MessageKind {
    type Err = ParseKindError;

    fn from_str(s: &str) -> Result<MessageKind, ParseKindError> {
        match s {
            "ask" => Ok(MessageKind::Ask),
            "send" => Ok(MessageKind::Send),
            other => Err(ParseKindError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Queued,
    Injected,
    Working,
    Replied,
    Done,
    Failed,
}

impl MessageStatus {
    /// `replied` (ask) | `done` (send) | `failed` end the lifecycle.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self {
            MessageStatus::Replied | MessageStatus::Done | MessageStatus::Failed => true,
            MessageStatus::Queued | MessageStatus::Injected | MessageStatus::Working => false,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MessageStatus::Queued => "queued",
            MessageStatus::Injected => "injected",
            MessageStatus::Working => "working",
            MessageStatus::Replied => "replied",
            MessageStatus::Done => "done",
            MessageStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid message status '{0}'; valid: queued, injected, working, replied, done, failed")]
pub struct ParseStatusError(String);

impl std::str::FromStr for MessageStatus {
    type Err = ParseStatusError;

    fn from_str(s: &str) -> Result<MessageStatus, ParseStatusError> {
        match s {
            "queued" => Ok(MessageStatus::Queued),
            "injected" => Ok(MessageStatus::Injected),
            "working" => Ok(MessageStatus::Working),
            "replied" => Ok(MessageStatus::Replied),
            "done" => Ok(MessageStatus::Done),
            "failed" => Ok(MessageStatus::Failed),
            other => Err(ParseStatusError(other.to_string())),
        }
    }
}

/// Serializes as a plain string: `agent:planner` | `user` | `http:1f2e3d4c`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Origin {
    Agent(AgentId),
    User,
    /// A plain authenticated API call without `X-CoreTempo-Agent` (a script,
    /// a human's `tempo ask` from a shell); the id is the request id.
    Http(String),
    /// A flow kickoff: the webhook, `run --flow` and desktop fire paths. The
    /// id is the trigger hub id minus `t-`, so observers correlate the
    /// kickoff to its trigger without mistaking any other HTTP message for
    /// one (#24). Agents see it as `http` in the injected header.
    Trigger(String),
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Agent(id) => write!(f, "agent:{id}"),
            Origin::User => f.write_str("user"),
            Origin::Http(req) => write!(f, "http:{req}"),
            Origin::Trigger(hex) => write!(f, "trigger:{hex}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid origin '{0}': expected 'agent:<id>', 'user', 'http:<req-id>', or 'trigger:<hex>'")]
pub struct OriginParseError(pub String);

impl std::str::FromStr for Origin {
    type Err = OriginParseError;

    fn from_str(s: &str) -> Result<Origin, OriginParseError> {
        if s == "user" {
            return Ok(Origin::User);
        }
        if let Some(id) = s.strip_prefix("agent:")
            && !id.is_empty()
        {
            return Ok(Origin::Agent(AgentId(id.to_string())));
        }
        if let Some(req) = s.strip_prefix("http:")
            && !req.is_empty()
        {
            return Ok(Origin::Http(req.to_string()));
        }
        if let Some(hex) = s.strip_prefix("trigger:")
            && !hex.is_empty()
        {
            return Ok(Origin::Trigger(hex.to_string()));
        }
        Err(OriginParseError(s.to_string()))
    }
}

impl Serialize for Origin {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Origin {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Origin, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Canonical message record (§3.2). Nullable fields serialize as explicit `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: MessageId,
    pub kind: MessageKind,
    pub from: Origin,
    pub to: AgentId,
    pub body: String,
    pub status: MessageStatus,
    pub code: Option<u8>,
    pub reply: Option<String>,
    pub created_at: Timestamp,
    pub injected_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    /// Why the message failed (human text naming the fix); `None` unless
    /// `status == Failed` (spec 2026-08-17 §4.3).
    #[serde(default)]
    pub reason: Option<String>,
    /// Machine-readable failure kind: `timeout`, `blocked_on_permission`,
    /// `agent_exited`, `agent_restarted`, `orphaned`.
    #[serde(default)]
    pub reason_code: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::time::Timestamp;
    use crate::types::id::{AgentId, MessageId};
    use crate::types::message::{MessageKind, MessageRecord, MessageStatus, Origin};

    #[test]
    fn record_matches_canonical_json() {
        let record = MessageRecord {
            id: MessageId("m-a3f91c2e".into()),
            kind: MessageKind::Ask,
            from: Origin::Agent(AgentId("planner".into())),
            to: AgentId("builder".into()),
            body: "Is the schema migration done?".into(),
            status: MessageStatus::Replied,
            code: Some(0),
            reply: Some("Yes, migration 004 applied and tested.".into()),
            created_at: Timestamp("2026-08-01T17:03:11Z".into()),
            injected_at: Some(Timestamp("2026-08-01T17:03:12Z".into())),
            completed_at: Some(Timestamp("2026-08-01T17:04:40Z".into())),
            reason: None,
            reason_code: None,
        };
        let expected = serde_json::json!({
            "id": "m-a3f91c2e", "kind": "ask", "from": "agent:planner", "to": "builder",
            "body": "Is the schema migration done?", "status": "replied", "code": 0,
            "reply": "Yes, migration 004 applied and tested.",
            "created_at": "2026-08-01T17:03:11Z", "injected_at": "2026-08-01T17:03:12Z",
            "completed_at": "2026-08-01T17:04:40Z", "reason": null, "reason_code": null
        });
        assert_eq!(serde_json::to_value(&record).unwrap(), expected);
        let back: MessageRecord = serde_json::from_value(expected).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn queued_record_serializes_explicit_nulls() {
        let record = MessageRecord {
            id: MessageId("m-b7c2aaaa".into()),
            kind: MessageKind::Send,
            from: Origin::User,
            to: AgentId("builder".into()),
            body: "go".into(),
            status: MessageStatus::Queued,
            code: None,
            reply: None,
            created_at: Timestamp("2026-08-01T17:03:11Z".into()),
            injected_at: None,
            completed_at: None,
            reason: None,
            reason_code: None,
        };
        let v = serde_json::to_value(&record).unwrap();
        assert!(v.get("code").unwrap().is_null());
        assert!(v.get("reply").unwrap().is_null());
        assert!(v.get("injected_at").unwrap().is_null());
        assert!(v.get("completed_at").unwrap().is_null());
        assert!(v.get("reason").unwrap().is_null());
        assert!(v.get("reason_code").unwrap().is_null());
    }

    #[test]
    fn origin_round_trips() {
        for s in ["agent:planner", "user", "http:1f2e3d4c", "trigger:1f2e3d4c"] {
            let o: Origin = s.parse().unwrap();
            assert_eq!(o.to_string(), s);
        }
        assert!("bogus".parse::<Origin>().is_err());
    }

    #[test]
    fn origin_rejects_invalid_forms() {
        for bad in [
            "", "agent:", "http:", "trigger:", "robot:x", "User", "agent",
        ] {
            assert!(Origin::from_str(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn kind_and_status_str_round_trip() {
        for kind in [MessageKind::Ask, MessageKind::Send] {
            assert_eq!(MessageKind::from_str(kind.as_str()).unwrap(), kind);
        }
        let statuses = [
            MessageStatus::Queued,
            MessageStatus::Injected,
            MessageStatus::Working,
            MessageStatus::Replied,
            MessageStatus::Done,
            MessageStatus::Failed,
        ];
        for status in statuses {
            assert_eq!(MessageStatus::from_str(status.as_str()).unwrap(), status);
        }
        assert!(MessageKind::from_str("shout").is_err());
        assert!(MessageStatus::from_str("pending").is_err());
    }

    #[test]
    fn terminal_statuses() {
        assert!(MessageStatus::Replied.is_terminal());
        assert!(MessageStatus::Done.is_terminal());
        assert!(MessageStatus::Failed.is_terminal());
        assert!(!MessageStatus::Queued.is_terminal());
        assert!(!MessageStatus::Injected.is_terminal());
        assert!(!MessageStatus::Working.is_terminal());
    }
}
