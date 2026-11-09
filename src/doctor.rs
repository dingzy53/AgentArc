//! `agentarc doctor`: show exactly what was detected, archived, indexed and
//! counted, including dedup and unknown-model diagnostics.

use crate::archive;
use crate::config::Config;
use crate::index::Index;
use crate::model::Agent;
use crate::pricing::Pricing;
use crate::util;
use anyhow::Result;
use std::fmt::Write;

pub fn run_doctor(cfg: &Config, index: &Index, pricing: &Pricing) -> Result<String> {
    let mut out = String::new();
    let tz = cfg.timezone();
    let archive_dir = cfg.archive_dir();
    let host = cfg.host_name();

    writeln!(out, "agentarc doctor")?;
    writeln!(out, "===============")?;
    writeln!(out, "config:      {}", util::config_path().display())?;
    writeln!(out, "archive:     {}", archive_dir.display())?;
    writeln!(out, "host:        {host}")?;
    writeln!(
        out,
        "timezone:    {} (system fallback if unset)",
        if cfg.timezone.is_empty() {
            "system"
        } else {
            cfg.timezone.as_str()
        }
    )?;
    writeln!(out, "index:       {}", index.path.display())?;
    writeln!(
        out,
        "last index:  {}",
        if index.last_index_ms() > 0 {
            util::fmt_ts(index.last_index_ms(), &tz, "%Y-%m-%d %H:%M:%S")
        } else {
            "never".to_string()
        }
    )?;
    if let Some(reason) = cfg.cloud_archive_reason() {
        writeln!(
            out,
            "cloud guard: {reason} (allow_cloud_archive = {})",
            cfg.allow_cloud_archive
        )?;
    }
    writeln!(out)?;

    writeln!(out, "Sources")?;
    writeln!(out, "-------")?;
    for agent in Agent::ALL {
        let source = cfg.source_config(agent);
        let roots = cfg.source_roots(agent);
        let discovery = archive::discover(cfg, &[agent]);
        writeln!(
            out,
            "  {:<16} {:<8} discovered {:>5} file(s)  roots: {}",
            agent.display_name(),
            if source.enabled {
                "enabled"
            } else {
                "disabled"
            },
            discovery.files.len(),
            if roots.is_empty() {
                "(none)".to_string()
            } else {
                roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )?;
    }
    writeln!(out)?;

    writeln!(out, "Archive")?;
    writeln!(out, "-------")?;
    let counts = archive::archive_file_counts(&archive_dir, None, None)?;
    if counts.is_empty() {
        writeln!(out, "  (no archived files yet — run `agentarc sync`)")?;
    } else {
        writeln!(
            out,
            "  {:<20} {:<18} {:>8} {:>12}",
            "host", "agent", "files", "bytes"
        )?;
        for (host, agent, count, bytes) in &counts {
            writeln!(
                out,
                "  {:<20} {:<18} {:>8} {:>12}",
                host, agent, count, bytes
            )?;
        }
    }
    writeln!(out)?;

    let (files, sessions, usage_rows, usage_unique) = index.counts()?;
    writeln!(out, "Index")?;
    writeln!(out, "-----")?;
    writeln!(out, "  files parsed:        {files}")?;
    writeln!(out, "  sessions:            {sessions}")?;
    writeln!(out, "  usage rows stored:   {usage_rows}")?;
    writeln!(out, "  unique usage events: {usage_unique}")?;
    let ingest_errors: Vec<String> = index
        .get_meta("last_ingest_errors")
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    writeln!(
        out,
        "  ingest errors:       {}",
        if ingest_errors.is_empty() {
            "none".to_string()
        } else {
            format!("{} (see below)", ingest_errors.len())
        }
    )?;
    for e in ingest_errors.iter().take(10) {
        writeln!(out, "    ! {}", util::truncate_chars(e, 180))?;
    }
    let (ag_db, ag_plain, ag_encrypted, ag_summary) = index.antigravity_layout_counts()?;
    if ag_db + ag_plain + ag_encrypted + ag_summary > 0 {
        writeln!(
            out,
            "  Antigravity layouts: db={ag_db}, pb-plain={ag_plain}, pb-encrypted={ag_encrypted}, summary-only={ag_summary}"
        )?;
    }
    let layouts = index.ag_layout_counts()?;
    if !layouts.is_empty() {
        writeln!(
            out,
            "  Antigravity summary rows: {}",
            layouts
                .iter()
                .map(|(l, n)| format!("{l}={n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    writeln!(out)?;

    writeln!(out, "Dedup (raw usage lines vs unique global events)")?;
    writeln!(out, "----------------------------------------------")?;
    let dedup = index.dedup_stats()?;
    if dedup.is_empty() {
        writeln!(out, "  (no usage data)")?;
    }
    for (agent, raw, unique_local, unique_global) in &dedup {
        let ratio = if *unique_global > 0 {
            *raw as f64 / *unique_global as f64
        } else {
            0.0
        };
        writeln!(
            out,
            "  {:<18} {raw:>9} lines → {unique_global:>9} unique ({ratio:.2}×) [per-file distinct {unique_local}]",
            agent
        )?;
    }
    writeln!(out)?;

    writeln!(out, "Unknown / unpriced models")?;
    writeln!(out, "--------------------------")?;
    let models = index.models()?;
    let mut unknown = 0;
    for (raw, _model, events, tokens) in &models {
        if pricing.price_for(raw).is_none() {
            unknown += 1;
            writeln!(
                out,
                "  ? {raw:<44} {events:>7} events {tokens:>12} tokens",
                tokens = util::human_tokens(*tokens)
            )?;
        }
    }
    if unknown == 0 {
        writeln!(out, "  none")?;
    }
    writeln!(out)?;

    writeln!(out, "Sessions without usage data")?;
    writeln!(out, "----------------------------")?;
    let empty = index.sessions_without_usage()?;
    if empty.is_empty() {
        writeln!(out, "  none")?;
    } else {
        for (key, agent, title, warnings) in empty.iter().take(25) {
            writeln!(
                out,
                "  {agent:<18} {:<42} {:<24} {}",
                util::truncate_chars(title, 42),
                util::truncate_chars(key, 24),
                warnings.join("; ")
            )?;
        }
        if empty.len() > 25 {
            writeln!(out, "  … {} more", empty.len() - 25)?;
        }
    }
    writeln!(out)?;

    writeln!(out, "Parse warnings")?;
    writeln!(out, "---------------")?;
    let warnings = index.file_warnings()?;
    if warnings.is_empty() {
        writeln!(out, "  none")?;
    } else {
        for (path, agent, list) in warnings.iter().take(30) {
            writeln!(out, "  [{agent}] {path}")?;
            for w in list {
                writeln!(out, "      - {}", util::truncate_chars(w, 180))?;
            }
        }
        if warnings.len() > 30 {
            writeln!(out, "  … {} more files with warnings", warnings.len() - 30)?;
        }
    }
    writeln!(out)?;

    writeln!(out, "Pricing")?;
    writeln!(out, "-------")?;
    writeln!(
        out,
        "  source: {} ({} models, {} overrides), cache {}",
        pricing.source,
        pricing.model_count(),
        pricing.overrides_len(),
        pricing.cache_path.display()
    )?;
    writeln!(
        out,
        "  cache age: {}",
        match pricing.fetched_at {
            Some(ms) => format!("{} ago", human_age(util::now_ms() - ms)),
            None => "no cache (embedded snapshot in use)".to_string(),
        }
    )?;
    writeln!(
        out,
        "  status: {} unknown model(s), {} session(s) with no usage data",
        unknown,
        empty.len()
    )?;
    Ok(out)
}

fn human_age(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
