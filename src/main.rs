//! agentarc — archive AI-agent sessions and compute trustworthy token/cost
//! statistics.

use agentarc::archive;
use agentarc::config::Config;
use agentarc::doctor;
use agentarc::export;
use agentarc::index::{self, Filters, Index, IngestOptions};
use agentarc::model::Agent;
use agentarc::pricing::Pricing;
use agentarc::stats::{self, GroupBy};
use agentarc::tui;
use agentarc::util;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "agentarc",
    version,
    about = "Archive AI-agent sessions and compute correct token/cost statistics"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Archive new/changed session files into the archive and re-index.
    Sync {
        /// Show what would be copied without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Comma-separated agents (claude,codex,pi,dsh,antigravity-cli,antigravity-ide).
        #[arg(long)]
        agent: Option<String>,
        /// Skip the local index update.
        #[arg(long)]
        no_index: bool,
    },
    /// Compute token/cost statistics from the archive.
    Stats {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value = "agent")]
        by: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        csv: bool,
    },
    /// Export one session transcript to Markdown.
    Export {
        /// Session key or session-id prefix.
        session: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        with_thinking: bool,
        #[arg(long)]
        with_tool_output: bool,
    },
    /// Diagnose sources, archive, index, dedup and pricing.
    Doctor,
    /// Manage the LiteLLM pricing cache.
    Pricing {
        #[command(subcommand)]
        command: PricingCommand,
    },
    /// Drop and rebuild the local index from the archive (all hosts).
    Reindex,
    /// Config file helpers.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand, Debug)]
enum PricingCommand {
    /// Download the LiteLLM pricing table into the cache.
    Update,
    /// Show the resolved price for one model.
    Show { model: String },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print the config file path.
    Path,
    /// Create a default config if it does not exist.
    Init,
    /// Open the config in $EDITOR (creating a default first).
    Edit,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("agentarc: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;
    match cli.command {
        None => tui::run(cfg),
        Some(Command::Sync {
            dry_run,
            agent,
            no_index,
        }) => cmd_sync(cfg, dry_run, agent, no_index),
        Some(Command::Stats {
            since,
            until,
            by,
            agent,
            host,
            json,
            csv,
        }) => cmd_stats(cfg, since, until, by, agent, host, json, csv),
        Some(Command::Export {
            session,
            output,
            with_thinking,
            with_tool_output,
        }) => cmd_export(cfg, session, output, with_thinking, with_tool_output),
        Some(Command::Doctor) => cmd_doctor(cfg),
        Some(Command::Pricing { command }) => cmd_pricing(cfg, command),
        Some(Command::Reindex) => cmd_reindex(cfg),
        Some(Command::Config { command }) => cmd_config(cfg, command),
    }
}

fn parse_agents(s: &str) -> Result<Vec<Agent>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some(agent) = Agent::parse(part) else {
            bail!(
                "unknown agent {part:?} (expected one of: claude, codex, pi, dsh, antigravity-cli, antigravity-ide)"
            );
        };
        out.push(agent);
    }
    if out.is_empty() {
        Ok(Agent::ALL.to_vec())
    } else {
        Ok(out)
    }
}

fn parse_hosts(s: &str) -> Vec<String> {
    s.split(',')
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect()
}

fn cmd_sync(cfg: Config, dry_run: bool, agent: Option<String>, no_index: bool) -> Result<()> {
    let agents = match agent {
        Some(s) => parse_agents(&s)?,
        None => Agent::ALL.to_vec(),
    };
    let opts = archive::SyncOptions {
        dry_run,
        agents: agents.clone(),
    };
    let progress_tty = std::io::stderr().is_terminal();
    let last_shown = std::sync::Mutex::new(0u64);
    let summary = archive::run_sync_with_progress(&cfg, &opts, |p| {
        if p.phase == "discovering" {
            if progress_tty {
                eprint!("\rsync: discovering source files…");
                let _ = std::io::stderr().flush();
            }
            return;
        }
        if p.phase == "done" {
            if progress_tty {
                eprint!("\r\x1b[K");
                let _ = std::io::stderr().flush();
            }
            return;
        }
        let mut last = match last_shown.lock() {
            Ok(v) => v,
            Err(_) => return,
        };
        let should_show = p.processed == p.total || *last == 0 || p.processed - *last >= 25;
        if !should_show {
            return;
        }
        *last = p.processed;
        if progress_tty {
            eprint!(
                "\rsync: [{}/{}] {} copied · {} · {}",
                p.processed,
                p.total,
                p.copied,
                util::human_bytes(p.bytes),
                util::truncate_chars(&p.current, 50)
            );
            let _ = std::io::stderr().flush();
        } else if p.processed == p.total || p.processed % 250 == 0 {
            eprintln!(
                "sync: [{}/{}] {} copied · {}",
                p.processed,
                p.total,
                p.copied,
                util::human_bytes(p.bytes)
            );
        }
    })?;
    if progress_tty {
        eprintln!();
    }
    println!("{}", summary.summary_line());
    if !summary.errors.is_empty() {
        eprintln!("{} warning(s):", summary.errors.len());
        for e in summary.errors.iter().take(20) {
            eprintln!("  - {e}");
        }
    }
    if summary.sources_missing > 0 {
        println!(
            "{} source file(s) are missing now; their archived copies were kept.",
            summary.sources_missing
        );
    }
    if dry_run {
        println!("(dry run: nothing was written)");
        return Ok(());
    }
    if no_index {
        return Ok(());
    }
    let mut idx = Index::open(&cfg.index_path())?;
    let last_index = std::sync::Mutex::new(0u64);
    let ingest = idx.ingest_with_progress(
        &cfg,
        &IngestOptions {
            force: false,
            agents: if agents.len() == Agent::ALL.len() {
                None
            } else {
                Some(agents)
            },
            hosts: None,
        },
        |p| {
            if p.phase != "indexing" {
                return;
            }
            let mut last = match last_index.lock() {
                Ok(v) => v,
                Err(_) => return,
            };
            let should_show = p.done == 0 || p.done == p.total || p.done - *last >= 50;
            if !should_show {
                return;
            }
            *last = p.done;
            if progress_tty {
                eprint!(
                    "\rindex: [{}/{}] {}",
                    p.done,
                    p.total,
                    util::truncate_chars(&p.current, 60)
                );
                let _ = std::io::stderr().flush();
            } else if p.done == p.total {
                eprintln!("index: [{}/{}] done", p.done, p.total);
            }
        },
    )?;
    if progress_tty {
        eprintln!("\r\x1b[K");
    }
    println!(
        "index: {} file(s) parsed, {} skipped, {} session(s), {} usage event(s) in {:.1}s",
        ingest.files_parsed,
        ingest.files_skipped,
        ingest.sessions,
        ingest.usage,
        ingest.duration_ms as f64 / 1000.0
    );
    if !ingest.errors.is_empty() {
        for e in ingest.errors.iter().take(20) {
            eprintln!("  index error: {e}");
        }
    }
    idx.set_meta(
        "last_sync",
        &serde_json::json!({
            "at": util::now_ms(),
            "host": cfg.host_name(),
            "summary": summary.summary_line(),
        })
        .to_string(),
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_stats(
    cfg: Config,
    since: Option<String>,
    until: Option<String>,
    by: String,
    agent: Option<String>,
    host: Option<String>,
    json: bool,
    csv: bool,
) -> Result<()> {
    let tz = cfg.timezone();
    let group = GroupBy::parse(&by).with_context(|| {
        format!("unknown --by value {by:?}; expected day/week/month/agent/model/project/session")
    })?;
    let since_ms = match since {
        Some(s) => Some(
            stats::parse_range_date(&s, &tz, false)
                .context("invalid --since date (expected YYYY-MM-DD)")?,
        ),
        None => None,
    };
    let until_ms = match until {
        Some(s) => Some(
            stats::parse_range_date(&s, &tz, true)
                .context("invalid --until date (expected YYYY-MM-DD)")?
                + 1,
        ),
        None => None,
    };
    let mut idx = Index::open(&cfg.index_path())?;
    if !cfg.archive_dir().is_dir() {
        eprintln!(
            "archive {} does not exist yet; run `agentarc sync` first",
            cfg.archive_dir().display()
        );
    } else {
        idx.ingest(&cfg, &IngestOptions::default())?;
    }
    let filters = Filters {
        since: since_ms,
        until: until_ms,
        agents: agent.map(|s| parse_agents(&s)).transpose()?,
        hosts: host.map(|s| parse_hosts(&s)),
        ..Default::default()
    };
    let events = idx.load_events(&filters)?;
    let sessions = idx.load_sessions(&filters)?;
    let session_map: std::collections::HashMap<String, index::SessionInfo> = sessions
        .iter()
        .map(|s| (s.key.clone(), s.clone()))
        .collect();
    let pricing = Pricing::load(&cfg, true);
    let result = stats::compute(&events, &session_map, group, &tz, &pricing);

    if json {
        let payload = serde_json::json!({
            "generated_at": util::now_ms(),
            "group_by": result.group_by,
            "since": since_ms,
            "until": until_ms,
            "timezone": if cfg.timezone.is_empty() { "system".to_string() } else { cfg.timezone.clone() },
            "totals": result.totals,
            "rows": result.rows,
            "unpriced_events": result.unpriced_events,
            "unknown_models": result.unknown_models,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if csv {
        println!("group,tokens,fresh_input,cache_read,cache_write,output,cost_usd,sessions");
        for row in &result.rows {
            println!(
                "{},{},{},{},{},{},{:.6},{}",
                csv_escape(&row.key),
                row.tokens.total,
                row.tokens.fresh_input,
                row.tokens.cache_read,
                row.tokens.cache_write,
                row.tokens.output,
                row.cost.usd,
                row.sessions
            );
        }
        return Ok(());
    }

    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>9}",
        "group", "tokens", "fresh", "cache rd", "cache wr", "output", "cost", "sessions"
    );
    println!("{}", "-".repeat(112));
    for row in &result.rows {
        println!(
            "{:<28} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>9}",
            util::truncate_chars(&row.key, 28),
            util::human_tokens(row.tokens.total),
            util::human_tokens(row.tokens.fresh_input),
            util::human_tokens(row.tokens.cache_read),
            util::human_tokens(row.tokens.cache_write),
            util::human_tokens(row.tokens.output),
            util::human_usd(row.cost.usd),
            row.sessions
        );
    }
    println!("{}", "-".repeat(112));
    let t = &result.totals;
    println!(
        "{:<28} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>9}",
        "TOTAL",
        util::human_tokens(t.tokens.total),
        util::human_tokens(t.tokens.fresh_input),
        util::human_tokens(t.tokens.cache_read),
        util::human_tokens(t.tokens.cache_write),
        util::human_tokens(t.tokens.output),
        util::human_usd(t.cost.usd),
        t.sessions
    );
    if result.unpriced_events > 0 {
        eprintln!(
            "warning: {} usage event(s) have no price. Run `agentarc doctor` or add [pricing.overrides].",
            result.unpriced_events
        );
        for (model, tokens) in &result.unknown_models {
            eprintln!("  ? {model} ({} tokens)", util::human_tokens(*tokens));
        }
    }
    Ok(())
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn cmd_export(
    cfg: Config,
    key: String,
    output: Option<PathBuf>,
    with_thinking: bool,
    with_tool_output: bool,
) -> Result<()> {
    let mut idx = Index::open(&cfg.index_path())?;
    if cfg.archive_dir().is_dir() {
        let _ = idx.ingest(&cfg, &IngestOptions::default());
    }
    let Some(session) = idx.session(&key)? else {
        bail!(
            "no unique session matching {key:?}; try the Sessions tab or `agentarc stats --by session`"
        );
    };
    let events = idx.load_events(&Filters::default())?;
    let pricing = Pricing::load(&cfg, false);
    let totals = stats::session_totals(&events, &pricing);
    let (tokens, cost) = totals.get(&session.key).cloned().unwrap_or_default();
    let messages = index::load_transcript(&cfg, &session).unwrap_or_default();
    let md = export::export_markdown(
        &session,
        &messages,
        tokens,
        cost,
        export::ExportOptions {
            with_thinking,
            with_tool_output,
        },
        &cfg.timezone(),
    );
    match output {
        Some(path) => {
            std::fs::write(&path, md).with_context(|| format!("writing {}", path.display()))?;
            println!("exported {} → {}", session.key, path.display());
        }
        None => {
            print!("{md}");
        }
    }
    Ok(())
}

fn cmd_doctor(cfg: Config) -> Result<()> {
    let mut idx = Index::open(&cfg.index_path())?;
    if cfg.archive_dir().is_dir() {
        let _ = idx.ingest(&cfg, &IngestOptions::default());
    }
    let pricing = Pricing::load(&cfg, false);
    print!("{}", doctor::run_doctor(&cfg, &idx, &pricing)?);
    Ok(())
}

fn cmd_pricing(cfg: Config, command: PricingCommand) -> Result<()> {
    match command {
        PricingCommand::Update => {
            let n = Pricing::update_from_config(&cfg)?;
            println!(
                "pricing: {} model(s) cached at {}",
                n,
                cfg.pricing_cache_path().display()
            );
        }
        PricingCommand::Show { model } => {
            let pricing = Pricing::load(&cfg, false);
            match pricing.find_for_display(&model) {
                Some((matched, price)) => {
                    let (input, output, cache_read, cache_write) = price.per_million();
                    println!("model:       {model}");
                    println!("matched:     {matched}");
                    println!("input:       ${input:.4} / 1M tokens");
                    println!("output:      ${output:.4} / 1M tokens");
                    println!("cache read:  ${cache_read:.4} / 1M tokens");
                    println!("cache write: ${cache_write:.4} / 1M tokens");
                    if let Some(cw1h) = price.cache_write_1h {
                        println!("cache write 1h: ${:.4} / 1M tokens", cw1h * 1e6);
                    }
                }
                None => {
                    bail!(
                        "no price for {model:?}; add [pricing.overrides.\"{model}\"] or [pricing.aliases]"
                    );
                }
            }
        }
    }
    Ok(())
}

fn cmd_reindex(cfg: Config) -> Result<()> {
    let stats = index::rebuild(&cfg)?;
    let idx = Index::open(&cfg.index_path())?;
    let (files, sessions, usage_rows, usage_unique) = idx.counts()?;
    println!(
        "reindex: {} file(s) parsed, {} session(s), {} usage rows ({} unique) in {:.1}s",
        stats.files_parsed,
        sessions,
        usage_rows,
        usage_unique,
        stats.duration_ms as f64 / 1000.0
    );
    println!("index: {}", cfg.index_path().display());
    let _ = files;
    Ok(())
}

fn cmd_config(cfg: Config, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => {
            println!("{}", util::config_path().display());
        }
        ConfigCommand::Init => {
            let path = cfg.init_file()?;
            println!("config: {}", path.display());
            if path.exists() {
                println!("(already existed; left untouched)");
            }
        }
        ConfigCommand::Edit => {
            let path = cfg.init_file()?;
            let editor = std::env::var("EDITOR")
                .or_else(|_| std::env::var("VISUAL"))
                .unwrap_or_else(|_| "vi".to_string());
            let status = std::process::Command::new(&editor)
                .arg(&path)
                .status()
                .with_context(|| format!("running editor {editor:?}"))?;
            if !status.success() {
                bail!("editor exited with {status}");
            }
            println!("edited {}", path.display());
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn print_status(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{msg}");
}
