//! Google Antigravity adapters — CLI (`agy`) and IDE/desktop.
//!
//! Per-conversation SQLite databases store a `gen_metadata.data` protobuf blob
//! per generation. The token numbers live at message path `1.4`
//! (`ModelUsageStats`); model id at `1.19`, display name at `1.21` and key/value
//! pairs at `1.20`.
//!
//! IDE `conversations/*.pb` files may be encrypted. We never attempt to decrypt
//! them: they are archived, listed (when a summary exists) and explicitly marked
//! as `pb-encrypted`, which excludes them from token/cost totals.

use super::protowire::{self, Field, WireValue};
use super::{ParseContext, finalize_session, get_str, session_key};
use crate::archive::sqlite_backup;
use crate::model::{Agent, Message, ParsedFile, Role, Session, UsageEvent};
use crate::util;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgMeta {
    pub conversation_id: String,
    pub agent: String,
    pub host: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub step_count: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub layout: String,
    pub parent_conversation_id: Option<String>,
}

fn file_mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(util::now_ms)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn decode_workspace_uris(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let first = v.as_array()?.first()?.as_str()?;
    let path = first.strip_prefix("file://").unwrap_or(first);
    let mut decoded = percent_decode(path);
    // file:///home/... leaves a leading slash after the scheme.
    if !decoded.starts_with('/') && first.starts_with("file:///") {
        decoded = format!("/{decoded}");
    }
    Some(decoded)
}

/// Parse SQLite's `YYYY-MM-DD HH:MM:SS(.frac)+00:00` timestamps.
fn parse_sqlite_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let base = &s[..19].replace(' ', "T");
    let iso = format!("{base}Z");
    util::parse_ts_ms(&iso)
}

// ---------------------------------------------------------------------------
// Summary metadata
// ---------------------------------------------------------------------------

pub fn parse_meta_file(agent: Agent, path: &Path, host: &str) -> Result<Vec<AgMeta>> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with("_summaries_proto.pb") {
        return parse_summaries_pb(agent, path, host);
    }
    if name == "conversation_summaries.db" {
        return parse_summaries_db(agent, path, host);
    }
    Ok(Vec::new())
}

fn parse_summaries_db(agent: Agent, path: &Path, host: &str) -> Result<Vec<AgMeta>> {
    let conn = sqlite_backup::open_read_only(path)?;
    let mut out = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT conversation_id, title, step_count, last_modified_time, workspace_uris,
                last_user_input_time, parent_conversation_id
         FROM conversation_summaries",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let title: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
        let steps: i64 = row.get::<_, Option<i64>>(2)?.unwrap_or(0);
        let modified: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
        let workspaces: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
        let last_input: String = row.get::<_, Option<String>>(5)?.unwrap_or_default();
        let parent: String = row.get::<_, Option<String>>(6)?.unwrap_or_default();
        let end = parse_sqlite_time(&modified)
            .filter(|ms| *ms > 946_684_800_000)
            .unwrap_or_else(|| file_mtime_ms(path));
        let start = parse_sqlite_time(&last_input)
            .filter(|ms| *ms > 946_684_800_000 && *ms <= end)
            .unwrap_or(end);
        out.push(AgMeta {
            conversation_id: id,
            agent: agent.as_str().to_string(),
            host: host.to_string(),
            title: (!title.is_empty()).then_some(title),
            cwd: decode_workspace_uris(&workspaces),
            step_count: steps,
            start_ms: start.min(end),
            end_ms: end,
            layout: "db".to_string(),
            parent_conversation_id: (!parent.is_empty()).then_some(parent),
        });
    }
    Ok(out)
}

fn parse_summaries_pb(agent: Agent, path: &Path, host: &str) -> Result<Vec<AgMeta>> {
    let data = std::fs::read(path)?;
    let Some(top) = protowire::parse_ok(&data) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let fallback = file_mtime_ms(path);
    for entry in protowire::all_bytes(&top, 1) {
        let Some(fields) = protowire::parse_ok(entry) else {
            continue;
        };
        let Some(id) = protowire::str_field(&fields, 1).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(inner) = protowire::message(&fields, 2) else {
            continue;
        };
        let title = protowire::str_field(&inner, 1).unwrap_or_default();
        let steps = protowire::varint_field(&inner, 2).unwrap_or(0) as i64;
        let end = protowire::message(&inner, 3)
            .and_then(|t| protowire::varint_field(&t, 1))
            .map(|s| s as i64 * 1000)
            .filter(|ms| *ms > 946_684_800_000)
            .unwrap_or(fallback);
        let cwd = protowire::message(&inner, 9)
            .and_then(|w| protowire::str_field(&w, 1))
            .map(|s| percent_decode(s.strip_prefix("file://").unwrap_or(&s)));
        out.push(AgMeta {
            conversation_id: id,
            agent: agent.as_str().to_string(),
            host: host.to_string(),
            title: (!title.is_empty()).then_some(title),
            cwd,
            step_count: steps,
            start_ms: end,
            end_ms: end,
            layout: "summary-pb".to_string(),
            parent_conversation_id: None,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// ModelUsageStats
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct AgUsage {
    model_id: String,
    model_enum: u64,
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    thinking: i64,
    response_id: String,
    kv: BTreeMap<String, String>,
}

fn kv_pairs(fields: &[Field<'_>]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in protowire::all_bytes(fields, 20) {
        let Some(m) = protowire::parse_ok(raw) else {
            continue;
        };
        if let (Some(k), Some(v)) = (protowire::str_field(&m, 1), protowire::str_field(&m, 2)) {
            out.insert(k, v);
        }
    }
    out
}

fn extract_usage(blob: &[u8]) -> Option<AgUsage> {
    let top = protowire::parse_ok(blob)?;
    let msg = protowire::message(&top, 1).unwrap_or(top.clone());
    let stats = protowire::message(&msg, 4).or_else(|| protowire::message(&top, 4))?;
    let input = protowire::varint_field(&stats, 2).unwrap_or(0) as i64;
    let output = protowire::varint_field(&stats, 3).unwrap_or(0) as i64;
    if input == 0 && output == 0 {
        return None;
    }
    let thinking = protowire::varint_field(&stats, 9).unwrap_or(0) as i64;
    let response_output = protowire::varint_field(&stats, 10).unwrap_or(0) as i64;
    let output = if output > 0 {
        output
    } else {
        thinking + response_output
    };
    Some(AgUsage {
        model_id: protowire::str_field(&msg, 19)
            .or_else(|| protowire::str_field(&top, 19))
            .unwrap_or_default(),
        model_enum: protowire::varint_field(&stats, 1).unwrap_or(0),
        input,
        output,
        cache_write: protowire::varint_field(&stats, 4).unwrap_or(0) as i64,
        cache_read: protowire::varint_field(&stats, 5).unwrap_or(0) as i64,
        thinking,
        response_id: protowire::str_field(&stats, 11).unwrap_or_default(),
        kv: kv_pairs(&msg),
    })
}

fn scan_usages(data: &[u8], depth: usize, out: &mut Vec<AgUsage>) {
    if depth > 5 {
        return;
    }
    if let Some(u) = extract_usage(data) {
        out.push(u);
    }
    let Some(fields) = protowire::parse_ok(data) else {
        return;
    };
    for field in &fields {
        if let WireValue::Bytes(bytes) = field.value {
            // Only recurse into fields that plausibly are submessages.
            if bytes.len() > 2 && protowire::looks_like_plain_protobuf(bytes) {
                scan_usages(bytes, depth + 1, out);
            }
        }
    }
}

fn usage_model(u: &AgUsage) -> String {
    if !u.model_id.is_empty() {
        u.model_id.clone()
    } else if u.model_enum > 0 {
        format!("antigravity-model-{}", u.model_enum)
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

pub fn parse_cli(path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name == "conversation_summaries.db"
        || name.ends_with("_summaries_proto.pb")
        || name.ends_with("transcript_full.jsonl")
        || name.ends_with(".pbtxt")
    {
        return Ok(ParsedFile::default());
    }
    if name.ends_with(".db") {
        return parse_conversation_db(Agent::AntigravityCli, path, ctx);
    }
    Ok(ParsedFile::default())
}

pub fn parse_ide(path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name == "conversation_summaries.db"
        || name.ends_with("_summaries_proto.pb")
        || name.ends_with("transcript_full.jsonl")
        || name.ends_with(".pbtxt")
    {
        return Ok(ParsedFile::default());
    }
    if name.ends_with(".db") {
        return parse_conversation_db(Agent::AntigravityIde, path, ctx);
    }
    if name.ends_with(".pb") {
        return parse_conversation_pb(path, ctx);
    }
    Ok(ParsedFile::default())
}

fn conversation_id_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn parse_conversation_db(agent: Agent, path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let conversation_id = conversation_id_from_path(path);
    let mut warnings = Vec::new();
    let mtime = file_mtime_ms(path);
    let mut usage = Vec::new();
    let mut raw_usage = 0u64;
    let mut models: BTreeSet<String> = BTreeSet::new();

    let conn = sqlite_backup::open_read_only(path)?;
    let mut stmt = match conn.prepare("SELECT idx, data FROM gen_metadata") {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("cannot read gen_metadata: {e}"));
            return finish_ag(
                agent,
                &conversation_id,
                ctx,
                usage,
                models,
                warnings,
                raw_usage,
                mtime,
                "db",
            );
        }
    };
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let idx: i64 = row.get(0).unwrap_or(0);
        let data: Vec<u8> = row.get(1).unwrap_or_default();
        let Some(u) = extract_usage(&data) else {
            continue;
        };
        raw_usage += 1;
        let model = usage_model(&u);
        if !model.is_empty() {
            models.insert(model.clone());
        }
        let request_id =
            u.kv.get("request_id")
                .cloned()
                .unwrap_or_else(|| u.response_id.clone());
        let cascade =
            u.kv.get("cascade_id")
                .cloned()
                .unwrap_or_else(|| conversation_id.clone());
        let dedup_key = if !request_id.is_empty() {
            format!("ag:{request_id}")
        } else {
            format!("ag:{cascade}:{idx}")
        };
        usage.push(UsageEvent {
            dedup_key,
            agent,
            host: ctx.host.to_string(),
            session_key: String::new(),
            ts: mtime,
            model_raw: model,
            provider: Some("google".to_string()),
            fresh_input: u.input,
            cache_read: u.cache_read,
            cache_write_5m: u.cache_write,
            cache_write_1h: 0,
            output: u.output,
            reasoning: u.thinking.min(u.output),
            reported_cost_usd: None,
        });
    }
    drop(rows);
    drop(stmt);
    finish_ag(
        agent,
        &conversation_id,
        ctx,
        usage,
        models,
        warnings,
        raw_usage,
        mtime,
        "db",
    )
}

fn parse_conversation_pb(path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let conversation_id = conversation_id_from_path(path);
    let data = std::fs::read(path)?;
    let mtime = file_mtime_ms(path);
    let entropy = protowire::entropy(&data);
    let plain = protowire::looks_like_plain_protobuf(&data);
    let mut warnings = Vec::new();
    let mut usage = Vec::new();
    let mut models: BTreeSet<String> = BTreeSet::new();
    let layout;

    if !plain {
        layout = "pb-encrypted";
        warnings.push(format!(
            "pb-encrypted (entropy {entropy:.2}); archived only, tokens n/a"
        ));
        return finish_ag(
            Agent::AntigravityIde,
            &conversation_id,
            ctx,
            usage,
            models,
            warnings,
            0,
            mtime,
            layout,
        );
    }

    layout = "pb-plain";
    let mut found = Vec::new();
    scan_usages(&data, 0, &mut found);
    let mut raw_usage = 0u64;
    let mut seen: HashSet<String> = HashSet::new();
    for u in &found {
        let model = usage_model(u);
        if !model.is_empty() {
            models.insert(model.clone());
        }
        let request_id =
            u.kv.get("request_id")
                .cloned()
                .unwrap_or_else(|| u.response_id.clone());
        let cascade =
            u.kv.get("cascade_id")
                .cloned()
                .unwrap_or_else(|| conversation_id.clone());
        let dedup_key = if !request_id.is_empty() {
            format!("ag:{request_id}")
        } else {
            let fingerprint = format!(
                "{}|{}|{}|{}|{}|{}",
                model, u.input, u.output, u.cache_write, u.cache_read, u.thinking
            );
            format!(
                "ag:{cascade}:pb:{}",
                util::hash_bytes(fingerprint.as_bytes())
            )
        };
        if !seen.insert(dedup_key.clone()) {
            continue;
        }
        raw_usage += 1;
        usage.push(UsageEvent {
            dedup_key,
            agent: Agent::AntigravityIde,
            host: ctx.host.to_string(),
            session_key: String::new(),
            ts: mtime,
            model_raw: model,
            provider: Some("google".to_string()),
            fresh_input: u.input,
            cache_read: u.cache_read,
            cache_write_5m: u.cache_write,
            cache_write_1h: 0,
            output: u.output,
            reasoning: u.thinking.min(u.output),
            reported_cost_usd: None,
        });
    }
    if found.is_empty() {
        warnings.push("pb-plain: no ModelUsageStats records found".to_string());
    }
    finish_ag(
        Agent::AntigravityIde,
        &conversation_id,
        ctx,
        usage,
        models,
        warnings,
        raw_usage,
        mtime,
        layout,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_ag(
    agent: Agent,
    conversation_id: &str,
    ctx: &ParseContext<'_>,
    mut usage: Vec<UsageEvent>,
    models: BTreeSet<String>,
    mut warnings: Vec<String>,
    raw_usage: u64,
    mtime: i64,
    layout: &str,
) -> Result<ParsedFile> {
    let key = session_key(ctx.host, agent, conversation_id);
    for e in usage.iter_mut() {
        e.session_key = key.clone();
    }
    if usage.is_empty() && layout != "pb-encrypted" {
        warnings.push("session has no usage records".to_string());
    }
    let mut session = Session {
        key,
        agent,
        host: ctx.host.to_string(),
        source_path: String::new(),
        archive_path: ctx.archive_rel.to_string(),
        session_id: conversation_id.to_string(),
        title: None,
        cwd: None,
        project: String::new(),
        started_at: mtime,
        ended_at: mtime,
        models: models.into_iter().collect(),
        parent_session: None,
        is_subagent: false,
        parse_warnings: warnings.clone(),
        reported_cost_usd: None,
    };
    finalize_session(&mut session, ctx.aliases);
    Ok(ParsedFile {
        sessions: vec![session],
        usage,
        warnings,
        raw_usage_count: raw_usage,
        unique_usage_count: 0,
    })
}

/// Read `brain/<uuid>/.system_generated/logs/transcript_full.jsonl`.
pub fn read_transcript(path: &Path) -> Result<Vec<Message>> {
    let mut out = Vec::new();
    let text = std::fs::read_to_string(path)?;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let ts = get_str(&v, &["created_at"])
            .and_then(util::parse_ts_ms)
            .unwrap_or(0);
        let role = match ty {
            "USER_INPUT" => Role::User,
            "PLANNER_RESPONSE" => Role::Assistant,
            "SYSTEM" => Role::System,
            _ => Role::Tool,
        };
        let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
        if content.trim().is_empty() {
            continue;
        }
        out.push(Message {
            ts,
            role,
            text: util::truncate_chars(content.trim(), 20_000),
            tool_name: if role == Role::Tool {
                Some(ty.to_string())
            } else {
                None
            },
            tool_args_short: None,
            thinking: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_uri() {
        assert_eq!(
            decode_workspace_uris(r#"["file:///home/alice/Obsidian/Math"]"#).as_deref(),
            Some("/home/alice/Obsidian/Math")
        );
        assert_eq!(
            decode_workspace_uris(r#"["file:///home/alice/My%20Docs"]"#).as_deref(),
            Some("/home/alice/My Docs")
        );
    }

    #[test]
    fn parses_sqlite_time() {
        assert!(parse_sqlite_time("2026-08-14 05:16:35.903459748+00:00").is_some());
    }
}
