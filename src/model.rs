//! Normalized data model shared by every adapter, the index and the UI.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Bump this whenever an adapter's interpretation of a format changes. Any file
/// indexed with an older parser version is re-parsed on the next ingest.
pub const PARSER_VERSION: i64 = 1;

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    #[default]
    Claude,
    Codex,
    Pi,
    Dsh,
    AntigravityCli,
    AntigravityIde,
}

impl Agent {
    pub const ALL: [Agent; 6] = [
        Agent::Claude,
        Agent::Codex,
        Agent::Pi,
        Agent::Dsh,
        Agent::AntigravityCli,
        Agent::AntigravityIde,
    ];

    /// Stable id used in SQLite and in CLI filters.
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
            Agent::Dsh => "dsh",
            Agent::AntigravityCli => "antigravity-cli",
            Agent::AntigravityIde => "antigravity-ide",
        }
    }

    /// Directory name under `hosts/<host>/` in the archive.
    pub fn archive_dir(self) -> &'static str {
        self.as_str()
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex CLI",
            Agent::Pi => "Pi",
            Agent::Dsh => "DeepSeek Harness",
            Agent::AntigravityCli => "Antigravity CLI",
            Agent::AntigravityIde => "Antigravity IDE",
        }
    }

    pub fn parse(s: &str) -> Option<Agent> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "pi" => Some(Agent::Pi),
            "dsh" | "deepseek-harness" => Some(Agent::Dsh),
            "antigravity-cli" | "antigravity_cli" | "agy" => Some(Agent::AntigravityCli),
            "antigravity-ide" | "antigravity_ide" | "antigravity" => Some(Agent::AntigravityIde),
            _ => None,
        }
    }

    pub fn from_archive_dir(name: &str) -> Option<Agent> {
        Agent::parse(name)
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A session as emitted by an adapter. Times are Unix milliseconds (UTC).
#[derive(Clone, Debug, Default)]
pub struct Session {
    pub key: String,
    pub agent: Agent,
    pub host: String,
    pub source_path: String,
    pub archive_path: String,
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub project: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub models: Vec<String>,
    pub parent_session: Option<String>,
    pub is_subagent: bool,
    pub parse_warnings: Vec<String>,
    /// Session-level cost reported by the agent itself (Claude `cost-state`).
    pub reported_cost_usd: Option<f64>,
}

/// One normalized usage observation. Reasoning is informational and is always a
/// subset of `output`; it is never added on top.
#[derive(Clone, Debug, Default)]
pub struct UsageEvent {
    pub dedup_key: String,
    pub agent: Agent,
    pub host: String,
    pub session_key: String,
    pub ts: i64,
    pub model_raw: String,
    pub provider: Option<String>,
    pub fresh_input: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub output: i64,
    pub reasoning: i64,
    pub reported_cost_usd: Option<f64>,
}

impl UsageEvent {
    pub fn cache_write(&self) -> i64 {
        self.cache_write_5m + self.cache_write_1h
    }

    pub fn total(&self) -> i64 {
        self.fresh_input + self.cache_read + self.cache_write() + self.output
    }

    pub fn has_tokens(&self) -> bool {
        self.total() > 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    User,
    Assistant,
    Tool,
    System,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::System => "system",
        }
    }
}

/// Transcript message. Only used by the reader/export path, never stored in the
/// index.
#[derive(Clone, Debug, Default)]
pub struct Message {
    pub ts: i64,
    pub role: Role,
    pub text: String,
    pub tool_name: Option<String>,
    pub tool_args_short: Option<String>,
    pub thinking: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ParsedFile {
    pub sessions: Vec<Session>,
    pub usage: Vec<UsageEvent>,
    pub warnings: Vec<String>,
    /// Number of raw usage-bearing lines/records seen before dedup.
    pub raw_usage_count: u64,
    /// Number of distinct dedup keys produced by this file.
    pub unique_usage_count: u64,
}

impl ParsedFile {
    pub fn finish_counts(&mut self) {
        use std::collections::HashSet;
        let set: HashSet<&str> = self.usage.iter().map(|u| u.dedup_key.as_str()).collect();
        self.unique_usage_count = set.len() as u64;
    }
}

/// Aggregate token counts with the invariant
/// `total = fresh_input + cache_read + cache_write + output`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub fresh_input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
    pub total: i64,
}

impl Tokens {
    pub fn add(&mut self, event: &UsageEvent) {
        self.fresh_input += event.fresh_input;
        self.cache_read += event.cache_read;
        self.cache_write += event.cache_write();
        self.output += event.output;
        self.reasoning += event.reasoning;
        self.total += event.total();
    }

    pub fn merge(&mut self, other: &Tokens) {
        self.fresh_input += other.fresh_input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.total += other.total;
    }

    pub fn cache_total(&self) -> i64 {
        self.cache_read + self.cache_write
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Cost {
    pub usd: f64,
    /// Number of usage events whose model had no usable price.
    pub unknown_events: u64,
}

impl Cost {
    pub fn add(&mut self, other: Cost) {
        self.usd += other.usd;
        self.unknown_events += other.unknown_events;
    }

    pub fn is_partial(&self) -> bool {
        self.unknown_events > 0
    }
}
