//! Adapters turn archived agent files into the normalized model. Every parser is
//! lenient: unknown record types are ignored, broken trailing lines produce a
//! warning instead of an error.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod dsh;
pub mod pi;
pub mod protowire;

use crate::model::{Agent, Message, ParsedFile, Role};
use crate::util;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct ParseContext<'a> {
    pub host: &'a str,
    pub archive_rel: &'a str,
    pub aliases: &'a BTreeMap<String, String>,
}

pub fn parse_file(agent: Agent, path: &Path, ctx: &ParseContext<'_>) -> Result<ParsedFile> {
    let mut parsed = match agent {
        Agent::Claude => claude::parse(path, ctx)?,
        Agent::Codex => codex::parse(path, ctx)?,
        Agent::Pi => pi::parse(path, ctx)?,
        Agent::Dsh => dsh::parse(path, ctx)?,
        Agent::AntigravityCli => antigravity::parse_cli(path, ctx)?,
        Agent::AntigravityIde => antigravity::parse_ide(path, ctx)?,
    };
    parsed.finish_counts();
    Ok(parsed)
}

pub fn session_key(host: &str, agent: Agent, id: &str) -> String {
    format!("{}:{}:{}", host, agent.as_str(), id)
}

// ---------------------------------------------------------------------------
// Reading helpers
// ---------------------------------------------------------------------------

/// Read a plain file or a multi-frame zstd container. Zstd files are decoded one
/// frame at a time; a torn final frame (crash, zero padding) keeps everything
/// decoded so far and records a warning.
pub fn read_maybe_zstd(path: &Path, warnings: &mut Vec<String>) -> Result<Vec<u8>> {
    let is_zstd = path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.ends_with(".zstd") || n.ends_with(".zst"));
    if !is_zstd {
        return std::fs::read(path).with_context(|| format!("reading {}", path.display()));
    }
    let file = File::open(path)?;
    let mut decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
        .with_context(|| format!("opening zstd {}", path.display()))?;
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => {
                warnings.push(format!(
                    "zstd decode stopped after {} bytes: {e} (torn tail tolerated)",
                    out.len()
                ));
                break;
            }
        }
    }
    // A multi-frame file may have a zero-padded tail; strip trailing NULs.
    while out.last() == Some(&0) {
        out.pop();
    }
    Ok(out)
}

pub fn parse_jsonl_bytes(bytes: &[u8], path: &Path, warnings: &mut Vec<String>) -> Vec<Value> {
    let mut out = Vec::new();
    let text = String::from_utf8_lossy(bytes);
    let total_lines = text.lines().count();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push(v),
            Err(e) => {
                if idx + 1 == total_lines {
                    warnings.push(format!(
                        "{}: last line is not valid JSON ({e}); torn tail ignored",
                        path.display()
                    ));
                } else {
                    warnings.push(format!(
                        "{}: line {} is not valid JSON ({e})",
                        path.display(),
                        idx + 1
                    ));
                }
            }
        }
    }
    out
}

pub fn read_jsonl(path: &Path) -> Result<(Vec<Value>, Vec<String>)> {
    let mut warnings = Vec::new();
    let bytes = read_maybe_zstd(path, &mut warnings)?;
    let values = parse_jsonl_bytes(&bytes, path, &mut warnings);
    Ok((values, warnings))
}

/// Read newline-delimited JSON from a file that is known to be plain text.
pub fn read_jsonl_plain(path: &Path) -> Result<Vec<Value>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut warnings = Vec::new();
    let mut out = Vec::new();
    let mut count = 0usize;
    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        count += 1;
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push(v),
            Err(e) => warnings.push(format!("line {count}: {e}")),
        }
    }
    let _ = warnings;
    Ok(out)
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

pub fn get_str<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str()
}

pub fn get_i64(v: &Value, path: &[&str]) -> i64 {
    let mut cur = v;
    for key in path {
        let Some(next) = cur.get(*key) else {
            return 0;
        };
        cur = next;
    }
    cur.as_i64()
        .or_else(|| cur.as_u64().map(|n| n as i64))
        .or_else(|| cur.as_f64().map(|n| n as i64))
        .unwrap_or(0)
}

pub fn get_f64(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_f64()
}

pub fn json_line(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

pub fn ts_from_value(v: &Value, path: &[&str]) -> Option<i64> {
    let s = get_str(v, path)?;
    util::parse_ts_ms(s)
}

pub fn first_user_title(values: &[Value]) -> Option<String> {
    for v in values {
        let role = get_str(v, &["type"])
            .or_else(|| get_str(v, &["message", "role"]))
            .or_else(|| get_str(v, &["data", "message", "role"]));
        if role == Some("user") {
            let mut text = String::new();
            collect_text(v.get("message").unwrap_or(v), &mut text, &mut String::new());
            if text.trim().is_empty() {
                collect_text(v, &mut text, &mut String::new());
            }
            let t = util::collapse_ws(&text);
            if !t.is_empty() {
                return Some(util::truncate_chars(&t, 72));
            }
        }
    }
    None
}

pub fn collect_text(v: &Value, text: &mut String, thinking: &mut String) -> Vec<(String, String)> {
    // `v` may be a message wrapper, a content array or a bare string.
    if v.is_array() || v.is_string() {
        return collect_content(v, text, thinking);
    }
    let content = v
        .get("content")
        .or_else(|| v.get("message").and_then(|m| m.get("content")));
    let Some(content) = content else {
        if let Some(s) = v.get("text").and_then(|t| t.as_str()) {
            text.push_str(s);
        }
        return Vec::new();
    };
    collect_content(content, text, thinking)
}

fn collect_content(
    content: &Value,
    text: &mut String,
    thinking: &mut String,
) -> Vec<(String, String)> {
    let mut tools = Vec::new();
    match content {
        Value::String(s) => text.push_str(s),
        Value::Array(items) => {
            for item in items {
                let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ty {
                    "text" | "input_text" | "output_text" => {
                        if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                            text.push_str(s);
                            text.push('\n');
                        }
                    }
                    "thinking" | "reasoning" => {
                        if let Some(s) = item
                            .get("thinking")
                            .or_else(|| item.get("text"))
                            .and_then(|t| t.as_str())
                        {
                            if !thinking.is_empty() {
                                thinking.push('\n');
                            }
                            thinking.push_str(s);
                        }
                    }
                    "tool_use" | "toolCall" | "tool-call" | "function_call" => {
                        let name = item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let args = item
                            .get("input")
                            .or_else(|| item.get("arguments"))
                            .map(|a| util::truncate_chars(&json_line(a), 60))
                            .unwrap_or_default();
                        tools.push((name, args));
                    }
                    _ => {
                        if let Some(s) = item.get("text").and_then(|t| t.as_str()) {
                            text.push_str(s);
                            text.push('\n');
                        }
                    }
                }
            }
        }
        Value::Object(obj) => {
            if let Some(s) = obj.get("text").and_then(|t| t.as_str()) {
                text.push_str(s);
            }
        }
        _ => {}
    }
    tools
}

pub fn make_message(ts: i64, role: Role, text: String, thinking: String) -> Option<Message> {
    let text = text.trim().to_string();
    if text.is_empty() && thinking.trim().is_empty() {
        return None;
    }
    Some(Message {
        ts,
        role,
        text,
        tool_name: None,
        tool_args_short: None,
        thinking: if thinking.trim().is_empty() {
            None
        } else {
            Some(thinking.trim().to_string())
        },
    })
}

/// Fill in project derived fields for a session that an adapter built.
pub fn finalize_session(session: &mut crate::model::Session, aliases: &BTreeMap<String, String>) {
    session.project = util::project_for_cwd(session.cwd.as_deref(), aliases);
    session.models.sort();
    session.models.dedup();
    if session.ended_at < session.started_at {
        session.ended_at = session.started_at;
    }
}
