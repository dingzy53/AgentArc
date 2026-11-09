//! OpenAI Codex CLI (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`) adapter.
//!
//! OpenAI semantics: `input_tokens` *includes* cached tokens and `output_tokens`
//! includes reasoning. We normalize to fresh input + cache read. `token_count`
//! events can repeat the same cumulative totals, so only changed totals count.
//! Sessions whose turns failed before reaching a model legitimately have no
//! `token_count`; they are still listed (with 0 tokens) and flagged.

use super::{
    ParseContext, collect_text, finalize_session, get_i64, get_str, read_jsonl, session_key,
    ts_from_value,
};
use crate::model::{Agent, Message, ParsedFile, Role, Session, UsageEvent};
use crate::util;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

fn file_mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(util::now_ms)
}

fn session_id_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // rollout-2026-10-12T15-58-18-<uuid>
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        // The UUID is the final five dash-separated groups.
        let start = parts.len().saturating_sub(5);
        return parts[start..].join("-");
    }
    stem
}

#[derive(Clone, Copy, Default)]
struct Totals {
    input: i64,
    cached: i64,
    output: i64,
    reasoning: i64,
}

impl Totals {
    fn from_value(v: &Value) -> Totals {
        Totals {
            input: get_i64(v, &["input_tokens"]),
            cached: get_i64(v, &["cached_input_tokens"]),
            output: get_i64(v, &["output_tokens"]),
            reasoning: get_i64(v, &["reasoning_output_tokens"]),
        }
    }
    fn same(&self, o: &Totals) -> bool {
        self.input == o.input
            && self.cached == o.cached
            && self.output == o.output
            && self.reasoning == o.reasoning
    }
    fn saturating_sub(&self, o: &Totals) -> Totals {
        Totals {
            input: (self.input - o.input).max(0),
            cached: (self.cached - o.cached).max(0),
            output: (self.output - o.output).max(0),
            reasoning: (self.reasoning - o.reasoning).max(0),
        }
    }
}

pub fn parse(path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let (records, mut warnings) = read_jsonl(path)?;
    let mut session_id = session_id_from_path(path);
    let mut cwd: Option<String> = None;
    let mut model = String::new();
    let mut started = i64::MAX;
    let mut ended = 0i64;
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut usage_events: Vec<UsageEvent> = Vec::new();
    let mut raw_usage = 0u64;
    let mut prev_total = Totals::default();
    let mut first_user: Option<String> = None;
    let mut title: Option<String> = None;
    let mut had_error = false;

    for record in &records {
        let ty = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let payload = record.get("payload").unwrap_or(&Value::Null);
        let pty = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(ms) = ts_from_value(record, &["timestamp"]) {
            started = started.min(ms);
            ended = ended.max(ms);
        }
        match ty {
            "session_meta" => {
                if let Some(id) =
                    get_str(payload, &["session_id"]).or_else(|| get_str(payload, &["id"]))
                {
                    if !id.is_empty() {
                        session_id = id.to_string();
                    }
                }
                if let Some(c) = get_str(payload, &["cwd"]) {
                    cwd = Some(c.to_string());
                }
                if let Some(t) = get_str(payload, &["title"]) {
                    title = Some(t.to_string());
                }
            }
            "turn_context" => {
                if let Some(m) = get_str(payload, &["model"]) {
                    if !m.is_empty() {
                        model = m.to_string();
                        models.insert(model.clone());
                    }
                }
            }
            "event_msg" => match pty {
                "token_count" => {
                    let Some(info) = payload.get("info") else {
                        continue;
                    };
                    let total = info
                        .get("total_token_usage")
                        .map(Totals::from_value)
                        .unwrap_or_default();
                    if total.same(&prev_total) {
                        continue;
                    }
                    let usage = info
                        .get("last_token_usage")
                        .map(Totals::from_value)
                        .filter(|t| t.input + t.output > 0)
                        .unwrap_or_else(|| total.saturating_sub(&prev_total));
                    prev_total = total;
                    if usage.input + usage.output + usage.cached == 0 {
                        continue;
                    }
                    raw_usage += 1;
                    let dedup_key = format!(
                        "codex:{session_id}:{}:{}:{}:{}",
                        total.input, total.cached, total.output, total.reasoning
                    );
                    let fresh = (usage.input - usage.cached).max(0);
                    usage_events.push(UsageEvent {
                        dedup_key,
                        agent: Agent::Codex,
                        host: ctx.host.to_string(),
                        session_key: String::new(),
                        ts: ts_from_value(record, &["timestamp"]).unwrap_or(0),
                        model_raw: model.clone(),
                        provider: Some("openai".to_string()),
                        fresh_input: fresh,
                        cache_read: usage.cached,
                        cache_write_5m: 0,
                        cache_write_1h: 0,
                        output: usage.output,
                        reasoning: usage.reasoning,
                        reported_cost_usd: None,
                    });
                }
                "task_complete" => {
                    if payload.get("error").is_some_and(|e| !e.is_null()) {
                        had_error = true;
                    }
                }
                "user_message" if first_user.is_none() => {
                    if let Some(text) = get_str(payload, &["message"]) {
                        let t = util::collapse_ws(text);
                        if !t.is_empty() {
                            first_user = Some(util::truncate_chars(&t, 72));
                        }
                    }
                }
                _ => {}
            },
            "response_item" if pty == "message" => {
                let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "user" && first_user.is_none() {
                    let mut text = String::new();
                    collect_text(payload, &mut text, &mut String::new());
                    let t = util::collapse_ws(&text);
                    if !t.is_empty() {
                        first_user = Some(util::truncate_chars(&t, 72));
                    }
                }
            }
            _ => {}
        }
    }

    if usage_events.is_empty() {
        if had_error {
            warnings.push("all turns failed before reaching the model (0 tokens)".to_string());
        } else {
            warnings.push("no token_count events (no usage data; not counted as $0)".to_string());
        }
    }

    let id = session_id.clone();
    let key = session_key(ctx.host, Agent::Codex, &id);
    for event in usage_events.iter_mut() {
        event.session_key = key.clone();
    }
    let started_at = if started == i64::MAX {
        file_mtime_ms(path)
    } else {
        started
    };
    let ended_at = ended.max(started_at);
    let mut session = Session {
        key,
        agent: Agent::Codex,
        host: ctx.host.to_string(),
        source_path: String::new(),
        archive_path: ctx.archive_rel.to_string(),
        session_id: id,
        title: title.or(first_user),
        cwd,
        project: String::new(),
        started_at,
        ended_at,
        models: models.into_iter().collect(),
        parent_session: None,
        is_subagent: false,
        parse_warnings: warnings.clone(),
        reported_cost_usd: None,
    };
    finalize_session(&mut session, ctx.aliases);

    Ok(ParsedFile {
        sessions: vec![session],
        usage: usage_events,
        warnings,
        raw_usage_count: raw_usage,
        unique_usage_count: 0,
    })
}

pub fn read_messages(path: &Path) -> Result<Vec<Message>> {
    let (records, _warnings) = read_jsonl(path)?;
    let mut out = Vec::new();
    for record in &records {
        let ty = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let payload = record.get("payload").unwrap_or(&Value::Null);
        let pty = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = ts_from_value(record, &["timestamp"]).unwrap_or(0);
        match (ty, pty) {
            ("response_item", "message") => {
                let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let role = match role {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    "system" | "developer" => Role::System,
                    _ => continue,
                };
                let mut text = String::new();
                collect_text(payload, &mut text, &mut String::new());
                if text.trim().is_empty() {
                    continue;
                }
                out.push(Message {
                    ts,
                    role,
                    text: text.trim().to_string(),
                    tool_name: None,
                    tool_args_short: None,
                    thinking: None,
                });
            }
            ("response_item", "function_call") => {
                let name = payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool");
                let args = payload
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                out.push(Message {
                    ts,
                    role: Role::Tool,
                    text: String::new(),
                    tool_name: Some(name.to_string()),
                    tool_args_short: Some(util::truncate_chars(args, 80)),
                    thinking: None,
                });
            }
            ("response_item", "function_call_output") => {
                if let Some(text) = payload.get("output").and_then(|v| v.as_str()) {
                    out.push(Message {
                        ts,
                        role: Role::Tool,
                        text: util::truncate_chars(text, 2000),
                        tool_name: Some("result".to_string()),
                        tool_args_short: None,
                        thinking: None,
                    });
                }
            }
            ("event_msg", "user_message") | ("event_msg", "agent_message") => {
                if let Some(text) = get_str(payload, &["message"]) {
                    out.push(Message {
                        ts,
                        role: if pty == "user_message" {
                            Role::User
                        } else {
                            Role::Assistant
                        },
                        text: text.to_string(),
                        tool_name: None,
                        tool_args_short: None,
                        thinking: None,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}
