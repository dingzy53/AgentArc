//! Pi (`~/.pi/agent/sessions/--<cwd-slug>--/*.jsonl`) adapter.
//!
//! Pi sessions are a tree of entries (`id`/`parentId`). Forks copy entries into
//! other session files, so dedup is by `responseId` (or a content hash when it
//! is missing). Every branch counts: each assistant message was a real API call.

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

fn message_ts(entry: &Value, message: &Value) -> i64 {
    if let Some(n) = message.get("timestamp").and_then(|v| v.as_i64()) {
        if n > 100_000_000_000 {
            return n;
        }
        if n > 1_000_000_000 {
            return n * 1000;
        }
    }
    ts_from_value(entry, &["timestamp"]).unwrap_or(0)
}

pub fn parse(path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let (records, mut warnings) = read_jsonl(path)?;
    let mut session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut cwd: Option<String> = None;
    let mut header_ts: Option<i64> = None;
    let mut started = i64::MAX;
    let mut ended = 0i64;
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut usage_events: Vec<UsageEvent> = Vec::new();
    let mut raw_usage = 0u64;
    let mut title: Option<String> = None;
    let mut first_user: Option<String> = None;

    for entry in &records {
        let ty = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "session" => {
                if let Some(id) = get_str(entry, &["id"]) {
                    if !id.is_empty() {
                        session_id = id.to_string();
                    }
                }
                if let Some(c) = get_str(entry, &["cwd"]) {
                    cwd = Some(c.to_string());
                }
                header_ts = ts_from_value(entry, &["timestamp"]);
            }
            "session_info" => {
                if let Some(t) = get_str(entry, &["title"]).or_else(|| get_str(entry, &["name"])) {
                    if !t.is_empty() {
                        title = Some(t.to_string());
                    }
                }
            }
            "message" => {
                let Some(message) = entry.get("message") else {
                    continue;
                };
                let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let ts = message_ts(entry, message);
                if ts > 0 {
                    started = started.min(ts);
                    ended = ended.max(ts);
                }
                if role == "user" && first_user.is_none() {
                    let mut text = String::new();
                    collect_text(message, &mut text, &mut String::new());
                    let t = util::collapse_ws(&text);
                    if !t.is_empty() {
                        first_user = Some(util::truncate_chars(&t, 72));
                    }
                }
                if role != "assistant" {
                    continue;
                }
                let Some(usage) = message.get("usage") else {
                    continue;
                };
                let input = get_i64(usage, &["input"]);
                let output = get_i64(usage, &["output"]);
                let cache_read = get_i64(usage, &["cacheRead"]);
                let cache_write = get_i64(usage, &["cacheWrite"]);
                let reasoning = get_i64(usage, &["reasoning"]);
                if input + output + cache_read + cache_write == 0 {
                    continue;
                }
                raw_usage += 1;
                let provider = get_str(message, &["provider"]).map(|s| s.to_string());
                let model = get_str(message, &["model"]).unwrap_or("").to_string();
                if !model.is_empty() {
                    models.insert(model.clone());
                }
                let response_id = get_str(message, &["responseId"]).unwrap_or("").to_string();
                let dedup_key = if !response_id.is_empty() {
                    format!("pi:{response_id}")
                } else {
                    let fingerprint = format!(
                        "{}|{}|{}|{input}|{output}|{cache_read}|{cache_write}",
                        ts,
                        provider.as_deref().unwrap_or(""),
                        model
                    );
                    format!("pi:hash:{}", util::hash_bytes(fingerprint.as_bytes()))
                };
                let reported = usage
                    .get("cost")
                    .and_then(|c| c.get("total"))
                    .and_then(|v| v.as_f64())
                    .filter(|c| *c > 0.0);
                usage_events.push(UsageEvent {
                    dedup_key,
                    agent: Agent::Pi,
                    host: ctx.host.to_string(),
                    session_key: String::new(),
                    ts,
                    model_raw: model,
                    provider,
                    fresh_input: input,
                    cache_read,
                    cache_write_5m: cache_write,
                    cache_write_1h: 0,
                    output,
                    reasoning,
                    reported_cost_usd: reported,
                });
            }
            _ => {}
        }
    }

    let id = session_id.clone();
    let key = session_key(ctx.host, Agent::Pi, &id);
    for event in usage_events.iter_mut() {
        event.session_key = key.clone();
    }
    if usage_events.is_empty() {
        warnings.push("session has no assistant usage records".to_string());
    }

    let started_at = header_ts
        .or(if started == i64::MAX {
            None
        } else {
            Some(started)
        })
        .unwrap_or_else(|| file_mtime_ms(path));
    let ended_at = ended.max(started_at);
    let mut session = Session {
        key,
        agent: Agent::Pi,
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
    for entry in &records {
        if entry.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        let role = message.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let ts = message_ts(entry, message);
        match role {
            "user" => {
                let mut text = String::new();
                let mut thinking = String::new();
                let tools = collect_text(message, &mut text, &mut thinking);
                if !text.trim().is_empty() {
                    out.push(Message {
                        ts,
                        role: Role::User,
                        text: text.trim().to_string(),
                        tool_name: None,
                        tool_args_short: None,
                        thinking: None,
                    });
                }
                for (name, args) in tools {
                    out.push(Message {
                        ts,
                        role: Role::Tool,
                        text: String::new(),
                        tool_name: Some(name),
                        tool_args_short: Some(args),
                        thinking: None,
                    });
                }
            }
            "assistant" => {
                let mut text = String::new();
                let mut thinking = String::new();
                let tools = collect_text(message, &mut text, &mut thinking);
                if !text.trim().is_empty() || !thinking.trim().is_empty() {
                    out.push(Message {
                        ts,
                        role: Role::Assistant,
                        text: text.trim().to_string(),
                        tool_name: None,
                        tool_args_short: None,
                        thinking: if thinking.trim().is_empty() {
                            None
                        } else {
                            Some(thinking.trim().to_string())
                        },
                    });
                }
                for (name, args) in tools {
                    out.push(Message {
                        ts,
                        role: Role::Tool,
                        text: String::new(),
                        tool_name: Some(name),
                        tool_args_short: Some(args),
                        thinking: None,
                    });
                }
            }
            "toolResult" => {
                let mut text = String::new();
                let mut thinking = String::new();
                collect_text(message, &mut text, &mut thinking);
                let t = util::truncate_chars(text.trim(), 2000);
                out.push(Message {
                    ts,
                    role: Role::Tool,
                    text: t,
                    tool_name: Some("result".to_string()),
                    tool_args_short: None,
                    thinking: None,
                });
            }
            "system" => {
                let mut text = String::new();
                collect_text(message, &mut text, &mut String::new());
                out.push(Message {
                    ts,
                    role: Role::System,
                    text: util::truncate_chars(text.trim(), 4000),
                    tool_name: None,
                    tool_args_short: None,
                    thinking: None,
                });
            }
            _ => {}
        }
    }
    Ok(out)
}
