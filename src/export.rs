//! Markdown export of a session transcript.

use crate::index::SessionInfo;
use crate::model::{Cost, Message, Role, Tokens};
use crate::util;

#[derive(Clone, Copy, Debug, Default)]
pub struct ExportOptions {
    pub with_thinking: bool,
    pub with_tool_output: bool,
}

pub fn export_markdown(
    session: &SessionInfo,
    messages: &[Message],
    tokens: Tokens,
    cost: Cost,
    opts: ExportOptions,
    tz: &jiff::tz::TimeZone,
) -> String {
    let mut out = String::new();
    let title = session.display_title();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str("| field | value |\n|---|---|\n");
    out.push_str(&format!("| agent | {} |\n", session.agent.display_name()));
    out.push_str(&format!("| host | {} |\n", session.host));
    out.push_str(&format!("| session id | `{}` |\n", session.session_id));
    if !session.project.is_empty() {
        out.push_str(&format!("| project | {} |\n", session.project));
    }
    if let Some(cwd) = &session.cwd {
        out.push_str(&format!("| cwd | `{cwd}` |\n"));
    }
    out.push_str(&format!(
        "| started | {} |\n",
        util::fmt_ts(session.started_at, tz, "%Y-%m-%d %H:%M:%S %Z")
    ));
    out.push_str(&format!(
        "| ended | {} |\n",
        util::fmt_ts(session.ended_at, tz, "%Y-%m-%d %H:%M:%S %Z")
    ));
    if !session.models.is_empty() {
        out.push_str(&format!("| models | {} |\n", session.models.join(", ")));
    }
    out.push_str(&format!(
        "| tokens | {} fresh · {} cache read · {} cache write · {} output |\n",
        tokens.fresh_input, tokens.cache_read, tokens.cache_write, tokens.output
    ));
    out.push_str(&format!("| total tokens | {} |\n", tokens.total));
    out.push_str(&format!("| cost | {} |\n", util::human_usd(cost.usd)));
    if cost.unknown_events > 0 {
        out.push_str(&format!("| unpriced events | {} |\n", cost.unknown_events));
    }
    if let Some(reported) = session.reported_cost_usd {
        out.push_str(&format!(
            "| reported cost | {} (computed {}, delta {}) |\n",
            util::human_usd(reported),
            util::human_usd(cost.usd),
            util::human_usd(cost.usd - reported)
        ));
    }
    if !session.parse_warnings.is_empty() {
        out.push_str(&format!(
            "| warnings | {} |\n",
            session.parse_warnings.join("; ")
        ));
    }
    out.push_str("\n---\n\n");

    if messages.is_empty() {
        out.push_str("_No readable transcript was archived for this session._\n");
        return out;
    }

    for m in messages {
        match m.role {
            Role::User => {
                out.push_str("## User\n\n");
                out.push_str(m.text.trim());
                out.push_str("\n\n");
            }
            Role::Assistant => {
                if opts.with_thinking {
                    if let Some(t) = &m.thinking {
                        out.push_str("<details><summary>thinking</summary>\n\n");
                        out.push_str(t.trim());
                        out.push_str("\n\n</details>\n\n");
                    }
                }
                if !m.text.trim().is_empty() {
                    out.push_str("## Assistant\n\n");
                    out.push_str(m.text.trim());
                    out.push_str("\n\n");
                }
            }
            Role::Tool => {
                if let Some(name) = &m.tool_name {
                    if m.text.trim().is_empty() {
                        out.push_str(&format!(
                            "⚙ `{}`{}\n\n",
                            name,
                            m.tool_args_short
                                .as_deref()
                                .map(|a| format!(" {a}"))
                                .unwrap_or_default()
                        ));
                    } else if opts.with_tool_output {
                        out.push_str(&format!(
                            "⚙ `{name}` result:\n\n```\n{}\n```\n\n",
                            m.text.trim()
                        ));
                    } else {
                        out.push_str(&format!(
                            "⚙ `{name}` ({} chars of tool output hidden)\n\n",
                            m.text.chars().count()
                        ));
                    }
                }
            }
            Role::System => {
                out.push_str("> system: ");
                out.push_str(&util::collapse_ws(&m.text));
                out.push_str("\n\n");
            }
        }
    }
    out
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if (c.is_whitespace() || c == '-' || c == '_' || c == '/') && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    util::truncate_chars(&trimmed, 60)
}

pub fn default_export_name(session: &SessionInfo) -> String {
    let slug = slugify(&session.display_title());
    let id = &session.session_id;
    if slug.is_empty() {
        format!("agentarc-{id}.md")
    } else {
        format!("agentarc-{slug}-{}.md", util::truncate_chars(id, 12))
    }
}
