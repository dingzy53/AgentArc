//! Claude Code (`~/.claude/projects/<slug>/<session>.jsonl`) adapter.
//!
//! Dedup: one API response is written as several `assistant` lines (one per
//! content block) with the same `message.id` + `requestId`; resumed/forked
//! sessions copy history into new files. The dedup key is therefore global.
//!
//! Advisor tokens: when `usage.iterations` exists, each iteration is counted as
//! its own usage row with its own model. The top-level usage is then ignored.

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

fn usage_numbers(v: &Value) -> (i64, i64, i64, i64, i64) {
    let fresh = get_i64(v, &["input_tokens"]);
    let cache_read = get_i64(v, &["cache_read_input_tokens"]);
    let cache_write = get_i64(v, &["cache_creation_input_tokens"]);
    let output = get_i64(v, &["output_tokens"]);
    let reasoning = get_i64(v, &["output_tokens_details", "thinking_tokens"]);
    let w5 = get_i64(v, &["cache_creation", "ephemeral_5m_input_tokens"]);
    let w1h = get_i64(v, &["cache_creation", "ephemeral_1h_input_tokens"]);
    let (w5, w1h) = if w5 + w1h > 0 {
        (w5, w1h)
    } else {
        (cache_write, 0)
    };
    (fresh, cache_read, w5, w1h, output.max(reasoning))
}

pub fn parse(path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let (records, mut warnings) = read_jsonl(path)?;
    let file_stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let is_subagent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s == "subagents")
        .unwrap_or(false);
    let parent_dir = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // `<session-uuid>/subagents/agent-*.jsonl`
    let grandparent = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut session_id = String::new();
    let mut subagent_id = if is_subagent {
        file_stem.clone()
    } else {
        String::new()
    };
    let mut cwd: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut started = i64::MAX;
    let mut ended = 0i64;
    let mut reported_cost: Option<f64> = None;
    let mut models: BTreeSet<String> = BTreeSet::new();
    let mut usage_events: Vec<UsageEvent> = Vec::new();
    let mut raw_usage = 0u64;
    let mut saw_usage = false;

    for (line_no, rec) in records.iter().enumerate() {
        let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(sid) = get_str(rec, &["sessionId"]) {
            if session_id.is_empty() {
                session_id = sid.to_string();
            }
        }
        if let Some(c) = get_str(rec, &["cwd"]) {
            if cwd.is_none() && !c.is_empty() {
                cwd = Some(c.to_string());
            }
        }
        if let Some(t) = get_str(rec, &["timestamp"]) {
            if let Some(ms) = util::parse_ts_ms(t) {
                started = started.min(ms);
                ended = ended.max(ms);
            }
        }
        match ty {
            "custom-title" => {
                if let Some(t) = get_str(rec, &["customTitle"]) {
                    if !t.is_empty() {
                        custom_title = Some(t.to_string());
                    }
                }
            }
            "ai-title" => {
                if let Some(t) = get_str(rec, &["aiTitle"]) {
                    if !t.is_empty() {
                        ai_title = Some(t.to_string());
                    }
                }
            }
            "cost-state" => {
                let c = rec
                    .get("totalCostUSD")
                    .and_then(|v| v.as_f64())
                    .or_else(|| {
                        rec.get("totalCostUSD")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                    });
                if let Some(c) = c {
                    reported_cost = Some(reported_cost.map_or(c, |old: f64| old.max(c)));
                }
            }
            "assistant" => {
                let usage = rec.get("message").and_then(|m| m.get("usage"));
                let Some(usage) = usage else { continue };
                let ts = ts_from_value(rec, &["timestamp"]).unwrap_or(0);
                let model = get_str(rec, &["message", "model"])
                    .or_else(|| get_str(rec, &["requestedModel"]))
                    .unwrap_or("")
                    .to_string();
                let id = get_str(rec, &["message", "id"])
                    .or_else(|| get_str(rec, &["uuid"]))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("line{line_no}"));
                let request = get_str(rec, &["requestId"])
                    .or_else(|| get_str(rec, &["requestRef"]))
                    .or_else(|| get_str(rec, &["message", "id"]))
                    .unwrap_or("")
                    .to_string();
                let iterations = usage.get("iterations").and_then(|v| v.as_array());
                raw_usage += 1;
                took_usage(
                    usage,
                    iterations,
                    &model,
                    &id,
                    &request,
                    ts,
                    &mut usage_events,
                    &mut models,
                    &mut saw_usage,
                );
            }
            _ => {}
        }
    }

    if session_id.is_empty() {
        // A subagent file lives inside `<session-uuid>/subagents/`.
        if is_subagent && !grandparent.is_empty() && grandparent != "subagents" {
            session_id = grandparent.clone();
        } else if is_subagent && parent_dir != "subagents" {
            session_id = parent_dir.clone();
        } else {
            session_id = file_stem.clone();
        }
    }
    if subagent_id.is_empty() {
        subagent_id = session_id.clone();
    }
    let id = if is_subagent {
        format!("{session_id}#subagent#{subagent_id}")
    } else {
        session_id.clone()
    };
    let key = session_key(ctx.host, Agent::Claude, &id);

    let title = custom_title
        .or(ai_title)
        .or_else(|| first_user_title(&records));
    let started_at = if started == i64::MAX {
        file_mtime_ms(path)
    } else {
        started
    };
    let ended_at = ended.max(started_at);

    for event in usage_events.iter_mut() {
        event.session_key = key.clone();
        event.host = ctx.host.to_string();
        event.agent = Agent::Claude;
    }

    if !saw_usage {
        warnings.push("session has no assistant usage records".to_string());
    }

    let mut session = Session {
        key,
        agent: Agent::Claude,
        host: ctx.host.to_string(),
        source_path: String::new(),
        archive_path: ctx.archive_rel.to_string(),
        session_id: id,
        title,
        cwd,
        project: String::new(),
        started_at,
        ended_at,
        models: models.into_iter().collect(),
        parent_session: is_subagent.then(|| session_id.clone()),
        is_subagent,
        parse_warnings: warnings.clone(),
        reported_cost_usd: reported_cost,
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

#[allow(clippy::too_many_arguments)]
fn took_usage(
    usage: &Value,
    iterations: Option<&Vec<Value>>,
    model: &str,
    id: &str,
    request: &str,
    ts: i64,
    out: &mut Vec<UsageEvent>,
    models: &mut BTreeSet<String>,
    saw_usage: &mut bool,
) {
    let mut push = |usage: &Value, model: &str, suffix: String| {
        let (fresh, cache_read, w5, w1h, output) = usage_numbers(usage);
        if fresh + cache_read + w5 + w1h + output == 0 {
            return;
        }
        *saw_usage = true;
        if !model.is_empty() {
            models.insert(model.to_string());
        }
        out.push(UsageEvent {
            dedup_key: format!("claude:{id}:{request}:{suffix}"),
            agent: Agent::Claude,
            host: String::new(),
            session_key: String::new(),
            ts,
            model_raw: model.to_string(),
            provider: Some("anthropic".to_string()),
            fresh_input: fresh,
            cache_read,
            cache_write_5m: w5,
            cache_write_1h: w1h,
            output,
            reasoning: get_i64(usage, &["output_tokens_details", "thinking_tokens"]),
            reported_cost_usd: None,
        });
    };

    if let Some(iters) = iterations {
        if !iters.is_empty() {
            for (i, iter) in iters.iter().enumerate() {
                let iter_model = iter
                    .get("model")
                    .and_then(|m| m.as_str())
                    .filter(|m| !m.is_empty())
                    .unwrap_or(model);
                push(iter, iter_model, format!("it{i}"));
            }
            return;
        }
    }
    push(usage, model, "top".to_string());
}

fn first_user_title(records: &[Value]) -> Option<String> {
    for rec in records {
        if rec.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        if rec.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let Some(message) = rec.get("message") else {
            continue;
        };
        let Some(content) = message.get("content") else {
            continue;
        };
        let mut text = String::new();
        let mut thinking = String::new();
        collect_text(content, &mut text, &mut thinking);
        let t = util::collapse_ws(&text);
        if !t.is_empty() {
            return Some(util::truncate_chars(&t, 72));
        }
    }
    None
}

pub fn read_messages(path: &Path) -> Result<Vec<Message>> {
    let (records, _warnings) = read_jsonl(path)?;
    let mut out = Vec::new();
    for rec in &records {
        let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = ts_from_value(rec, &["timestamp"]).unwrap_or(0);
        match ty {
            "user" => {
                let Some(message) = rec.get("message") else {
                    continue;
                };
                let content = message.get("content").unwrap_or(&Value::Null);
                let mut text = String::new();
                let mut thinking = String::new();
                let tools = collect_text(content, &mut text, &mut thinking);
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
            }
            "assistant" => {
                let Some(message) = rec.get("message") else {
                    continue;
                };
                let content = message.get("content").unwrap_or(&Value::Null);
                let mut text = String::new();
                let mut thinking = String::new();
                let tools = collect_text(content, &mut text, &mut thinking);
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
            _ => {}
        }
    }
    Ok(out)
}
