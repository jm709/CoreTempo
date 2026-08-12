//! DDL for the v1 tables (spec §11): `messages` (the canonical record, §3.2), `runs`,
//! and `agent_events`.

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
  id           TEXT PRIMARY KEY,
  kind         TEXT NOT NULL CHECK (kind IN ('ask', 'send')),
  from_origin  TEXT NOT NULL,
  to_agent     TEXT NOT NULL,
  body         TEXT NOT NULL,
  status       TEXT NOT NULL CHECK
               (status IN ('queued', 'injected', 'working', 'replied', 'done', 'failed')),
  code         INTEGER CHECK (code IN (0, 1)),
  reply        TEXT,
  created_at   TEXT NOT NULL,
  injected_at  TEXT,
  completed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_messages_created ON messages (created_at);
CREATE INDEX IF NOT EXISTS idx_messages_to_status ON messages (to_agent, status);

CREATE TABLE IF NOT EXISTS runs (
  run_id        TEXT PRIMARY KEY,
  workflow_name TEXT NOT NULL,
  workflow_hash TEXT NOT NULL,
  started_at    TEXT NOT NULL,
  stopped_at    TEXT
);

CREATE TABLE IF NOT EXISTS agent_events (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  agent     TEXT NOT NULL,
  state     TEXT NOT NULL,
  exit_code INTEGER,
  ts        TEXT NOT NULL
);
";
