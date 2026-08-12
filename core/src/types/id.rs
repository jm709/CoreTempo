//! Id newtypes. Formats (frozen): agent ids match `^[a-z0-9][a-z0-9_-]{0,31}$`;
//! message ids are `m-` + 8 lowercase hex; run ids `r-` + 8 lowercase hex;
//! tokens are 64 lowercase hex chars (32 random bytes).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(pub String);

impl AgentId {
    /// Validates the frozen agent-id pattern `^[a-z0-9][a-z0-9_-]{0,31}$`.
    #[must_use]
    pub fn is_valid(s: &str) -> bool {
        let bytes = s.as_bytes();
        let Some((first, rest)) = bytes.split_first() else {
            return false;
        };
        if bytes.len() > 32 {
            return false;
        }
        let head_ok = first.is_ascii_lowercase() || first.is_ascii_digit();
        head_ok
            && rest
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "server")]
fn random_hex(byte_count: usize) -> String {
    use rand::Rng;
    use std::fmt::Write;

    let mut bytes = vec![0_u8; byte_count];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(byte_count * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(feature = "server")]
impl MessageId {
    /// `m-` + 8 lowercase hex from 4 random bytes.
    #[must_use]
    pub fn generate() -> MessageId {
        MessageId(format!("m-{}", random_hex(4)))
    }
}

#[cfg(feature = "server")]
impl RunId {
    /// `r-` + 8 lowercase hex from 4 random bytes.
    #[must_use]
    pub fn generate() -> RunId {
        RunId(format!("r-{}", random_hex(4)))
    }
}

#[cfg(feature = "server")]
impl Token {
    /// 32 random bytes as 64 lowercase hex chars.
    #[must_use]
    pub fn generate() -> Token {
        Token(random_hex(32))
    }
}

#[cfg(test)]
mod tests {
    use crate::types::id::{AgentId, MessageId, RunId, Token};

    #[test]
    fn agent_id_validation() {
        assert!(AgentId::is_valid("planner"));
        assert!(AgentId::is_valid("a"));
        assert!(AgentId::is_valid("0agent-x_1"));
        assert!(AgentId::is_valid(&"a".repeat(32)));
        assert!(!AgentId::is_valid(""));
        assert!(!AgentId::is_valid(&"a".repeat(33)));
        assert!(!AgentId::is_valid("-planner")); // bad first char
        assert!(!AgentId::is_valid("_planner"));
        assert!(!AgentId::is_valid("Planner")); // uppercase
        assert!(!AgentId::is_valid("plan ner"));
    }

    #[test]
    fn display_is_bare_inner() {
        assert_eq!(AgentId("builder".into()).to_string(), "builder");
        assert_eq!(MessageId("m-a3f91c2e".into()).to_string(), "m-a3f91c2e");
        assert_eq!(RunId("r-1f2e3d4c".into()).to_string(), "r-1f2e3d4c");
    }

    #[test]
    fn generated_ids_have_frozen_shapes() {
        let m = MessageId::generate().0;
        assert_eq!(m.len(), 10);
        assert!(m.starts_with("m-"));
        assert!(
            m[2..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        let r = RunId::generate().0;
        assert_eq!(r.len(), 10);
        assert!(r.starts_with("r-"));

        let t = Token::generate().0;
        assert_eq!(t.len(), 64);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(Token::generate().0, t, "tokens must be random");
    }
}
