//! DeepSeek Harness (`~/.dsh/sessions/.../session.v4.jsonl.zstd`) adapter.
//!
//! The container is multi-frame zstd; `read_maybe_zstd` tolerates a torn tail.
//! The same usage numbers also appear inside `data.stream[]` as a `usage` chunk,
//! so only `data.usage` is read. Dedup key: `session-id:turn:step[:attempt]`.

use super::{
    ParseContext, collect_text, finalize_session, get_i64, get_str, parse_jsonl_bytes,
    read_maybe_zstd, session_key,
};
use crate::model::{Agent, Message, ParsedFile, Role, Session, UsageEvent};
use crate::util;
use anyhow::Result;
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

pub fn parse(path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let mut warnings = Vec::new();
    let bytes = read_maybe_zstd(path, &mut warnings)?;
    let records = parse_jsonl_bytes(&bytes, path, &mut warnings);

    let mut session_id = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut cwd: Option<String> = None;
    let mut created_at: Option<i64> = None;
    let mut delegation_depth = 0i64;
    let mut title: Option<String> = None;
    let mut started = i64::MAX;
    let mut ended = 0i64;
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut usage_events: Vec<UsageEvent> = Vec::new();
    let mut raw_usage = 0u64;
    let mut first_user: Option<String> = None;

    for record in &records {
        let ty = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let time = record.get("time").and_then(|v| v.as_i64()).unwrap_or(0);
        if time > 0 {
            started = started.min(time);
            ended = ended.max(time);
        }
        match ty {
            "session" => {
                let data = record.get("data").unwrap_or(record);
                if let Some(id) = get_str(data, &["id"]).or_else(|| get_str(record, &["id"])) {
                    if !id.is_empty() {
                        session_id = id.to_string();
                    }
                }
                if let Some(c) = get_str(data, &["cwd"]).or_else(|| get_str(record, &["cwd"])) {
                    cwd = Some(c.to_string());
                }
                created_at = data
                    .get("createdAt")
                    .and_then(|v| v.as_i64())
                    .or_else(|| record.get("createdAt").and_then(|v| v.as_i64()));
                delegation_depth = data
                    .get("delegationDepth")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
            }
            "session/title" => {
                if let Some(t) = get_str(record, &["data", "title"]) {
                    if !t.is_empty() {
                        title = Some(t.to_string());
                    }
                }
            }
            "user/message" => {
                if first_user.is_none() {
                    let mut text = String::new();
                    collect_text(
                        record.get("data").unwrap_or(record),
                        &mut text,
                        &mut String::new(),
                    );
                    let t = util::collapse_ws(&text);
                    if !t.is_empty() {
                        first_user = Some(util::truncate_chars(&t, 72));
                    }
                }
            }
            "assistant/message" => {
                let data = record.get("data").unwrap_or(record);
                let Some(usage) = data.get("usage") else {
                    continue;
                };
                let input = get_i64(usage, &["inputTokens"]);
                let output = get_i64(usage, &["outputTokens"]);
                let cache_read = get_i64(usage, &["cacheReadTokens"]);
                let reasoning = get_i64(usage, &["reasoningTokens"]);
                if input + output + cache_read == 0 {
                    continue;
                }
                raw_usage += 1;
                let provider =
                    get_str(data, &["message", "source", "provider"]).map(|s| s.to_string());
                let model = get_str(data, &["message", "source", "model"])
                    .unwrap_or("")
                    .to_string();
                if !model.is_empty() {
                    models.insert(model.clone());
                }
                let turn = data.get("turn").and_then(|v| v.as_i64()).unwrap_or(0);
                let step = data.get("step").and_then(|v| v.as_i64()).unwrap_or(0);
                let attempt = data.get("attempt").and_then(|v| v.as_i64());
                let seq = record.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
                let suffix = match attempt {
                    Some(a) => format!("{turn}:{step}:{a}"),
                    None => format!("{turn}:{step}"),
                };
                let dedup_key = if turn == 0 && step == 0 {
                    format!("dsh:{session_id}:seq:{seq}")
                } else {
                    format!("dsh:{session_id}:{suffix}")
                };
                usage_events.push(UsageEvent {
                    dedup_key,
                    agent: Agent::Dsh,
                    host: ctx.host.to_string(),
                    session_key: String::new(),
                    ts: time,
                    model_raw: model,
                    provider,
                    fresh_input: input,
                    cache_read,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                    output,
                    reasoning,
                    reported_cost_usd: None,
                });
            }
            _ => {}
        }
    }

    let id = session_id.clone();
    let key = session_key(ctx.host, Agent::Dsh, &id);
    for event in usage_events.iter_mut() {
        event.session_key = key.clone();
    }
    if usage_events.is_empty() {
        warnings.push("session has no assistant usage records".to_string());
    }

    let started_at = created_at
        .or(if started == i64::MAX {
            None
        } else {
            Some(started)
        })
        .unwrap_or_else(|| file_mtime_ms(path));
    let ended_at = ended.max(started_at);
    let mut session = Session {
        key,
        agent: Agent::Dsh,
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
        is_subagent: delegation_depth > 0,
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
    let mut warnings = Vec::new();
    let bytes = read_maybe_zstd(path, &mut warnings)?;
    let records = parse_jsonl_bytes(&bytes, path, &mut warnings);
    let mut out = Vec::new();
    for record in &records {
        let ty = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = record.get("time").and_then(|v| v.as_i64()).unwrap_or(0);
        let data = record.get("data").unwrap_or(record);
        match ty {
            "user/message" => {
                let mut text = String::new();
                let tools = collect_text(data, &mut text, &mut String::new());
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
            "assistant/message" => {
                let message = data.get("message").unwrap_or(data);
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
            "tool/result" => {
                if let Some(text) = data
                    .get("result")
                    .and_then(|r| r.get("text").or_else(|| r.get("content")))
                    .and_then(|v| v.as_str())
                {
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
            "system/message" => {
                let mut text = String::new();
                collect_text(data, &mut text, &mut String::new());
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
