//! Local derived SQLite index. The index lives in the cache directory (never in
//! the synced archive), can be deleted at any time, and is rebuilt from the
//! archive by `agentarc reindex`.
//!
//! Dedup is global: usage rows are stored per `(dedup_key, archive_path)` and
//! queries pick one row per `dedup_key`, so an incremental re-parse can never
//! lose an event that was also copied into another session file.

use crate::adapters::antigravity;
use crate::adapters::{self, ParseContext};
use crate::archive::{self, sqlite_backup};
use crate::config::Config;
use crate::model::{Agent, PARSER_VERSION, ParsedFile, Session, UsageEvent};
use crate::util;
use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;
use walkdir::WalkDir;

pub const SCHEMA_VERSION: i64 = 1;
/// Bump when metadata post-processing rules change so existing indexes get
/// refreshed even without archive changes.
pub const POST_PROCESS_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS files (
    archive_path TEXT PRIMARY KEY,
    host TEXT NOT NULL,
    agent TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    parsed_at INTEGER NOT NULL,
    parser_version INTEGER NOT NULL,
    warnings TEXT NOT NULL DEFAULT '[]',
    raw_usage_count INTEGER NOT NULL DEFAULT 0,
    unique_usage_count INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS sessions (
    key TEXT PRIMARY KEY,
    agent TEXT NOT NULL,
    host TEXT NOT NULL,
    source_path TEXT NOT NULL DEFAULT '',
    archive_path TEXT NOT NULL DEFAULT '',
    session_id TEXT NOT NULL DEFAULT '',
    title TEXT,
    cwd TEXT,
    project TEXT NOT NULL DEFAULT '',
    started_at INTEGER NOT NULL DEFAULT 0,
    ended_at INTEGER NOT NULL DEFAULT 0,
    models TEXT NOT NULL DEFAULT '[]',
    parent_session TEXT,
    is_subagent INTEGER NOT NULL DEFAULT 0,
    parse_warnings TEXT NOT NULL DEFAULT '[]',
    reported_cost_usd REAL
);
CREATE INDEX IF NOT EXISTS idx_sessions_agent ON sessions(agent);
CREATE INDEX IF NOT EXISTS idx_sessions_host ON sessions(host);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);
CREATE TABLE IF NOT EXISTS usage (
    dedup_key TEXT NOT NULL,
    archive_path TEXT NOT NULL,
    agent TEXT NOT NULL,
    host TEXT NOT NULL,
    session_key TEXT NOT NULL,
    ts INTEGER NOT NULL,
    model_raw TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    provider TEXT,
    fresh_input INTEGER NOT NULL DEFAULT 0,
    cache_read INTEGER NOT NULL DEFAULT 0,
    cache_write_5m INTEGER NOT NULL DEFAULT 0,
    cache_write_1h INTEGER NOT NULL DEFAULT 0,
    output INTEGER NOT NULL DEFAULT 0,
    reasoning INTEGER NOT NULL DEFAULT 0,
    reported_cost_usd REAL,
    PRIMARY KEY (dedup_key, archive_path)
);
CREATE INDEX IF NOT EXISTS idx_usage_ts ON usage(ts);
CREATE INDEX IF NOT EXISTS idx_usage_agent ON usage(agent);
CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_key);
CREATE TABLE IF NOT EXISTS ag_meta (
    conversation_id TEXT NOT NULL,
    agent TEXT NOT NULL,
    host TEXT NOT NULL,
    archive_path TEXT NOT NULL DEFAULT '',
    title TEXT,
    cwd TEXT,
    step_count INTEGER NOT NULL DEFAULT 0,
    start_ms INTEGER NOT NULL DEFAULT 0,
    end_ms INTEGER NOT NULL DEFAULT 0,
    layout TEXT NOT NULL DEFAULT '',
    parent_conversation_id TEXT,
    PRIMARY KEY (conversation_id, agent, host)
);
CREATE TABLE IF NOT EXISTS codex_threads (
    thread_id TEXT PRIMARY KEY,
    title TEXT,
    cwd TEXT,
    model TEXT,
    tokens_used INTEGER
);
"#;

#[derive(Clone, Debug, Default)]
pub struct Filters {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub agents: Option<Vec<Agent>>,
    pub hosts: Option<Vec<String>>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub search: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionInfo {
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
    pub reported_cost_usd: Option<f64>,
}

impl SessionInfo {
    pub fn display_title(&self) -> String {
        self.title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| util::truncate_chars(&self.session_id, 24))
    }
}

#[derive(Clone, Debug, Default)]
pub struct IngestOptions {
    pub force: bool,
    pub agents: Option<Vec<Agent>>,
    pub hosts: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct IngestProgress {
    pub phase: String,
    pub total: u64,
    pub done: u64,
    pub current: String,
}

#[derive(Clone, Debug, Default)]
pub struct IngestStats {
    pub files_parsed: u64,
    pub files_skipped: u64,
    pub metadata_files: u64,
    pub sessions: u64,
    pub usage: u64,
    pub warnings: u64,
    pub errors: Vec<String>,
    pub duration_ms: u128,
}

pub struct Index {
    conn: Connection,
    pub path: PathBuf,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening index {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        let idx = Index {
            conn,
            path: path.to_path_buf(),
        };
        idx.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(idx)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Index {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .ok()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Ingest
    // -----------------------------------------------------------------------

    pub fn ingest(&mut self, cfg: &Config, opts: &IngestOptions) -> Result<IngestStats> {
        self.ingest_with_progress(cfg, opts, |_| {})
    }

    pub fn ingest_with_progress<F>(
        &mut self,
        cfg: &Config,
        opts: &IngestOptions,
        progress: F,
    ) -> Result<IngestStats>
    where
        F: Fn(&IngestProgress) + Send + Sync,
    {
        let emit = |p: IngestProgress| progress(&p);
        let started = Instant::now();
        let archive = cfg.archive_dir();
        let mut stats = IngestStats::default();
        let mut changed = false;
        if !archive.is_dir() {
            stats.duration_ms = started.elapsed().as_millis();
            return Ok(stats);
        }

        let mut source_map: HashMap<(String, String, String), String> = HashMap::new();
        let mut hosts: Vec<String> = Vec::new();
        let hosts_dir = archive.join("hosts");
        if hosts_dir.is_dir() {
            for e in std::fs::read_dir(&hosts_dir)?.flatten() {
                if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let host = e.file_name().to_string_lossy().to_string();
                if let Some(want) = &opts.hosts {
                    if !want.contains(&host) {
                        continue;
                    }
                }
                hosts.push(host.clone());
                if let Ok(m) = archive::Manifest::load(&archive, &host) {
                    for (_src, entry) in m.files {
                        source_map
                            .insert((host.clone(), entry.agent.clone(), entry.rel.clone()), _src);
                    }
                }
            }
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut tasks: Vec<ParseTask> = Vec::new();
        for host in &hosts {
            let host_dir = hosts_dir.join(host);
            for entry in WalkDir::new(&host_dir).follow_links(false) {
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                let Ok(rel) = path.strip_prefix(&host_dir) else {
                    continue;
                };
                let rel_str = rel.to_string_lossy().to_string();
                if rel_str.starts_with(".versions/")
                    || rel_str == "manifest.json"
                    || rel_str == "manifest.summary.json"
                {
                    continue;
                }
                let mut comps = rel.components();
                let Some(agent_dir) = comps
                    .next()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                else {
                    continue;
                };
                let Some(agent) = Agent::from_archive_dir(&agent_dir) else {
                    continue;
                };
                if let Some(want) = &opts.agents {
                    if !want.contains(&agent) {
                        continue;
                    }
                }
                let agent_rel = comps.as_path().to_string_lossy().to_string();
                let archive_rel = rel_str.clone();
                seen.insert(archive_rel.clone());

                let meta = match std::fs::metadata(path) {
                    Ok(m) => m,
                    Err(e) => {
                        stats.errors.push(format!("{archive_rel}: {e}"));
                        continue;
                    }
                };
                use std::os::unix::fs::MetadataExt;
                let mtime = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
                let size = meta.len();

                if Self::is_meta_file(agent, &agent_rel) {
                    if !opts.force && self.file_unchanged(&archive_rel, size, mtime) {
                        stats.files_skipped += 1;
                        continue;
                    }
                    let r = self.ingest_meta_file(agent, host, path, &archive_rel);
                    match r {
                        Ok((_n, warns)) => {
                            stats.metadata_files += 1;
                            stats.warnings += warns;
                            changed = true;
                        }
                        Err(e) => stats.errors.push(format!("{archive_rel}: {e:#}")),
                    }
                    self.record_file(&archive_rel, host, agent, size, mtime, 0, 0, &[], "meta")?;
                    continue;
                }

                if !Self::is_parseable(agent, &agent_rel) {
                    continue;
                }
                if !opts.force && self.file_unchanged(&archive_rel, size, mtime) {
                    stats.files_skipped += 1;
                    continue;
                }
                let source_path = source_map
                    .get(&(host.clone(), agent.as_str().to_string(), agent_rel.clone()))
                    .cloned()
                    .unwrap_or_default();
                tasks.push(ParseTask {
                    path: path.to_path_buf(),
                    host: host.clone(),
                    agent,
                    archive_rel,
                    source_path,
                    size,
                    mtime,
                });
            }
        }

        emit(IngestProgress {
            phase: "parsing".to_string(),
            total: tasks.len() as u64,
            done: 0,
            current: String::new(),
        });
        // Parse transcripts in parallel; SQLite writes remain single-threaded.
        let parsed: Vec<(ParseTask, Result<ParsedFile>)> = tasks
            .into_par_iter()
            .map(|task| {
                let ctx = ParseContext {
                    host: &task.host,
                    archive_rel: &task.archive_rel,
                    aliases: &cfg.projects.aliases,
                };
                let result = adapters::parse_file(task.agent, &task.path, &ctx);
                (task, result)
            })
            .collect();
        let total_parsed = parsed.len() as u64;
        for (idx, (task, result)) in parsed.into_iter().enumerate() {
            emit(IngestProgress {
                phase: "indexing".to_string(),
                total: total_parsed,
                done: idx as u64,
                current: task.archive_rel.clone(),
            });
            match result {
                Ok(mut p) => {
                    for s in p.sessions.iter_mut() {
                        if s.source_path.is_empty() {
                            s.source_path = task.source_path.clone();
                        }
                    }
                    self.insert_usage(&p, &task.archive_rel)?;
                    self.record_file(
                        &task.archive_rel,
                        &task.host,
                        task.agent,
                        task.size,
                        task.mtime,
                        p.raw_usage_count,
                        p.unique_usage_count,
                        &p.warnings,
                        "conversation",
                    )?;
                    stats.files_parsed += 1;
                    stats.sessions += p.sessions.len() as u64;
                    stats.usage += p.usage.len() as u64;
                    stats.warnings += p.warnings.len() as u64;
                    changed = true;
                }
                Err(e) => stats.errors.push(format!("{}: {e:#}", task.archive_rel)),
            }
        }

        // Drop index rows for archive files that no longer exist. Never prune
        // after a filtered ingest: an agent/host filter would otherwise delete
        // the rows of every other agent.
        if opts.agents.is_none() && opts.hosts.is_none() {
            self.prune_missing(&seen)?;
        }
        let post_done = self
            .get_meta("post_process_version")
            .and_then(|v| v.parse::<i64>().ok())
            == Some(POST_PROCESS_VERSION);
        if changed || opts.force || !post_done {
            self.post_process(cfg)?;
            self.set_meta("post_process_version", &POST_PROCESS_VERSION.to_string())?;
        }
        stats.duration_ms = started.elapsed().as_millis();
        self.set_meta("last_index", &util::now_ms().to_string())?;
        self.set_meta(
            "last_ingest_errors",
            &serde_json::to_string(&stats.errors).unwrap_or_else(|_| "[]".to_string()),
        )?;
        emit(IngestProgress {
            phase: "done".to_string(),
            total: total_parsed,
            done: total_parsed,
            current: String::new(),
        });
        Ok(stats)
    }

    fn file_unchanged(&self, archive_path: &str, size: u64, mtime: i64) -> bool {
        let row: Option<(i64, i64, i64)> = self
            .conn
            .query_row(
                "SELECT size, mtime, parser_version FROM files WHERE archive_path=?1",
                [archive_path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        row.is_some_and(|(s, m, v)| s == size as i64 && m == mtime && v == PARSER_VERSION)
    }

    fn is_parseable(agent: Agent, rel: &str) -> bool {
        match agent {
            Agent::Claude => rel.ends_with(".jsonl"),
            Agent::Pi => rel.ends_with(".jsonl"),
            Agent::Dsh => rel.ends_with(".jsonl.zstd") || rel.ends_with(".jsonl.zst"),
            Agent::Codex => {
                (rel.ends_with(".jsonl")) || rel == "state.sqlite" || rel.ends_with("/state.sqlite")
            }
            Agent::AntigravityCli | Agent::AntigravityIde => {
                let r = strip_ide_prefix(rel);
                (r.starts_with("conversations/") && (r.ends_with(".db") || r.ends_with(".pb")))
                    || r == "conversation_summaries.db"
                    || r.ends_with("_summaries_proto.pb")
            }
        }
    }

    fn is_meta_file(agent: Agent, rel: &str) -> bool {
        match agent {
            Agent::Codex => rel == "state.sqlite" || rel.ends_with("/state.sqlite"),
            Agent::AntigravityCli | Agent::AntigravityIde => {
                let r = strip_ide_prefix(rel);
                r == "conversation_summaries.db" || r.ends_with("_summaries_proto.pb")
            }
            _ => false,
        }
    }

    fn ingest_meta_file(
        &mut self,
        agent: Agent,
        host: &str,
        path: &Path,
        archive_rel: &str,
    ) -> Result<(u64, u64)> {
        match agent {
            Agent::Codex => {
                let rows = read_codex_threads(path)?;
                let tx = self.conn.transaction()?;
                tx.execute("DELETE FROM codex_threads", [])?;
                for (id, title, cwd, model, tokens) in &rows {
                    tx.execute(
                        "INSERT OR REPLACE INTO codex_threads(thread_id,title,cwd,model,tokens_used)
                         VALUES(?1,?2,?3,?4,?5)",
                        params![id, title, cwd, model, tokens],
                    )?;
                }
                tx.commit()?;
                Ok((rows.len() as u64, 0))
            }
            Agent::AntigravityCli | Agent::AntigravityIde => {
                let metas = antigravity::parse_meta_file(agent, path, host)?;
                let tx = self.conn.transaction()?;
                tx.execute("DELETE FROM ag_meta WHERE archive_path=?1", [archive_rel])?;
                for m in &metas {
                    tx.execute(
                        "INSERT OR REPLACE INTO ag_meta(conversation_id,agent,host,archive_path,title,cwd,step_count,start_ms,end_ms,layout,parent_conversation_id)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![
                            m.conversation_id,
                            m.agent,
                            m.host,
                            archive_rel,
                            m.title,
                            m.cwd,
                            m.step_count,
                            m.start_ms,
                            m.end_ms,
                            m.layout,
                            m.parent_conversation_id
                        ],
                    )?;
                }
                tx.commit()?;
                Ok((metas.len() as u64, 0))
            }
            _ => Ok((0, 0)),
        }
    }

    fn insert_session(&self, s: &Session) -> Result<()> {
        let models = serde_json::to_string(&s.models)?;
        let warnings = serde_json::to_string(&s.parse_warnings)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions(
                key,agent,host,source_path,archive_path,session_id,title,cwd,project,
                started_at,ended_at,models,parent_session,is_subagent,parse_warnings,reported_cost_usd)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                s.key,
                s.agent.as_str(),
                s.host,
                s.source_path,
                s.archive_path,
                s.session_id,
                s.title,
                s.cwd,
                s.project,
                s.started_at,
                s.ended_at,
                models,
                s.parent_session,
                s.is_subagent as i64,
                warnings,
                s.reported_cost_usd,
            ],
        )?;
        Ok(())
    }

    fn insert_usage(&mut self, parsed: &ParsedFile, archive_rel: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM usage WHERE archive_path=?1", [archive_rel])?;
        tx.execute("DELETE FROM sessions WHERE archive_path=?1", [archive_rel])?;
        for s in &parsed.sessions {
            let models = serde_json::to_string(&s.models)?;
            let warnings = serde_json::to_string(&s.parse_warnings)?;
            tx.execute(
                "INSERT OR REPLACE INTO sessions(
                    key,agent,host,source_path,archive_path,session_id,title,cwd,project,
                    started_at,ended_at,models,parent_session,is_subagent,parse_warnings,reported_cost_usd)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                params![
                    s.key,
                    s.agent.as_str(),
                    s.host,
                    s.source_path,
                    s.archive_path,
                    s.session_id,
                    s.title,
                    s.cwd,
                    s.project,
                    s.started_at,
                    s.ended_at,
                    models,
                    s.parent_session,
                    s.is_subagent as i64,
                    warnings,
                    s.reported_cost_usd,
                ],
            )?;
        }
        for u in &parsed.usage {
            tx.execute(
                "INSERT OR IGNORE INTO usage(
                    dedup_key,archive_path,agent,host,session_key,ts,model_raw,model,provider,
                    fresh_input,cache_read,cache_write_5m,cache_write_1h,output,reasoning,reported_cost_usd)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                params![
                    u.dedup_key,
                    archive_rel,
                    u.agent.as_str(),
                    u.host,
                    u.session_key,
                    u.ts,
                    u.model_raw,
                    u.model_raw,
                    u.provider,
                    u.fresh_input,
                    u.cache_read,
                    u.cache_write_5m,
                    u.cache_write_1h,
                    u.output,
                    u.reasoning.min(u.output),
                    u.reported_cost_usd,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_file(
        &self,
        archive_rel: &str,
        host: &str,
        agent: Agent,
        size: u64,
        mtime: i64,
        raw: u64,
        unique: u64,
        warnings: &[String],
        kind: &str,
    ) -> Result<()> {
        let w = serde_json::to_string(warnings)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO files(
                archive_path,host,agent,size,mtime,parsed_at,parser_version,warnings,
                raw_usage_count,unique_usage_count,kind)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                archive_rel,
                host,
                agent.as_str(),
                size as i64,
                mtime,
                util::now_ms(),
                PARSER_VERSION,
                w,
                raw as i64,
                unique as i64,
                kind
            ],
        )?;
        Ok(())
    }

    fn prune_missing(&self, seen: &HashSet<String>) -> Result<()> {
        let mut stmt = self.conn.prepare("SELECT archive_path FROM files")?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for path in rows {
            if !seen.contains(&path) {
                self.conn
                    .execute("DELETE FROM usage WHERE archive_path=?1", [&path])?;
                self.conn
                    .execute("DELETE FROM sessions WHERE archive_path=?1", [&path])?;
                self.conn
                    .execute("DELETE FROM files WHERE archive_path=?1", [&path])?;
            }
        }
        Ok(())
    }

    fn post_process(&mut self, cfg: &Config) -> Result<()> {
        // Antigravity: apply summary metadata (title/cwd/time) to parsed
        // conversations, and create sessions for conversations we only know
        // from the summary index.
        let metas = self.load_ag_meta()?;
        for m in &metas {
            let project = util::project_for_cwd(m.cwd.as_deref(), &cfg.projects.aliases);
            let existing: Option<String> = self
                .conn
                .query_row(
                    "SELECT key FROM sessions WHERE session_id=?1 AND agent=?2 AND host=?3",
                    params![m.conversation_id, m.agent, m.host],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(key) = existing {
                self.conn.execute(
                    "UPDATE sessions SET
                        title=COALESCE(NULLIF(title,''), ?2),
                        cwd=COALESCE(cwd, ?3),
                        project=CASE WHEN ?4 <> '' THEN ?4 ELSE project END,
                        started_at=CASE WHEN started_at<=0 THEN ?5 WHEN ?5>0 AND ?5<started_at THEN ?5 ELSE started_at END,
                        ended_at=CASE WHEN ?6>ended_at THEN ?6 ELSE ended_at END
                     WHERE key=?1",
                    params![
                        key,
                        m.title,
                        m.cwd,
                        project,
                        m.start_ms,
                        m.end_ms
                    ],
                )?;
            } else {
                let Some(agent) = Agent::parse(&m.agent) else {
                    continue;
                };
                let key = adapters::session_key(&m.host, agent, &m.conversation_id);
                let warnings = vec![format!(
                    "summary-only ({}) : conversation body not archived (tokens n/a)",
                    m.layout
                )];
                let session = Session {
                    key,
                    agent,
                    host: m.host.clone(),
                    source_path: String::new(),
                    archive_path: m.archive_path.clone(),
                    session_id: m.conversation_id.clone(),
                    title: m.title.clone(),
                    cwd: m.cwd.clone(),
                    project: project.clone(),
                    started_at: m.start_ms,
                    ended_at: m.end_ms,
                    models: Vec::new(),
                    parent_session: m.parent_conversation_id.clone(),
                    is_subagent: m.parent_conversation_id.is_some(),
                    parse_warnings: warnings,
                    reported_cost_usd: None,
                };
                self.insert_session(&session)?;
            }
        }

        // Codex: titles/cwd from state_*.sqlite.
        let threads = self.load_codex_threads()?;
        for (id, title, cwd, _model, _tokens) in threads {
            let project = util::project_for_cwd(cwd.as_deref(), &cfg.projects.aliases);
            self.conn.execute(
                "UPDATE sessions SET
                    title=COALESCE(?2, NULLIF(title,'')),
                    cwd=COALESCE(?3, cwd),
                    project=CASE WHEN ?4 <> '' THEN ?4 ELSE project END
                 WHERE agent='codex' AND session_id=?1",
                params![id, title, cwd, project],
            )?;
        }

        // Any session with a cwd but no project (e.g. metadata supplied later).
        let mut stmt = self
            .conn
            .prepare("SELECT key, cwd FROM sessions WHERE project='' AND cwd IS NOT NULL")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        for (key, cwd) in rows {
            let p = util::project_for_cwd(Some(&cwd), &cfg.projects.aliases);
            if !p.is_empty() {
                self.conn.execute(
                    "UPDATE sessions SET project=?2 WHERE key=?1",
                    params![key, p],
                )?;
            }
        }
        Ok(())
    }

    fn load_ag_meta(&self) -> Result<Vec<AgMetaRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT conversation_id,agent,host,archive_path,title,cwd,start_ms,end_ms,layout,parent_conversation_id FROM ag_meta",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(AgMetaRow {
                conversation_id: r.get(0)?,
                agent: r.get(1)?,
                host: r.get(2)?,
                archive_path: r.get(3)?,
                title: r.get(4)?,
                cwd: r.get(5)?,
                start_ms: r.get(6)?,
                end_ms: r.get(7)?,
                layout: r.get(8)?,
                parent_conversation_id: r.get(9)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    #[allow(clippy::type_complexity)]
    fn load_codex_threads(
        &self,
    ) -> Result<
        Vec<(
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        )>,
    > {
        let mut stmt = self
            .conn
            .prepare("SELECT thread_id,title,cwd,model,tokens_used FROM codex_threads")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Store normalized model ids on usage rows (pricing's normalization is
    /// applied lazily at query time; this keeps the raw id available).
    pub fn apply_model_normalization(&mut self) -> Result<()> {
        self.conn
            .execute("UPDATE usage SET model=model_raw WHERE model=''", [])?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    pub fn load_events(&self, filters: &Filters) -> Result<Vec<UsageEvent>> {
        let mut sql = String::from(
            "SELECT u.dedup_key,u.agent,u.host,u.session_key,u.ts,u.model_raw,u.model,u.provider,
                    u.fresh_input,u.cache_read,u.cache_write_5m,u.cache_write_1h,u.output,u.reasoning,u.reported_cost_usd
             FROM usage u
             JOIN (SELECT dedup_key, MIN(rowid) AS r FROM usage GROUP BY dedup_key) d
               ON d.r = u.rowid
             WHERE 1=1",
        );
        let mut vals: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filters.since {
            sql.push_str(" AND u.ts >= ?");
            vals.push(since.into());
        }
        if let Some(until) = filters.until {
            sql.push_str(" AND u.ts < ?");
            vals.push(until.into());
        }
        if let Some(agents) = &filters.agents {
            if !agents.is_empty() {
                let placeholders = vec!["?"; agents.len()].join(",");
                sql.push_str(&format!(" AND u.agent IN ({placeholders})"));
                for a in agents {
                    vals.push(a.as_str().to_string().into());
                }
            }
        }
        if let Some(hosts) = &filters.hosts {
            if !hosts.is_empty() {
                let placeholders = vec!["?"; hosts.len()].join(",");
                sql.push_str(&format!(" AND u.host IN ({placeholders})"));
                for h in hosts {
                    vals.push(h.clone().into());
                }
            }
        }
        if let Some(model) = &filters.model {
            sql.push_str(" AND u.model_raw = ?");
            vals.push(model.clone().into());
        }
        sql.push_str(" ORDER BY u.ts");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(vals), |r| {
            let agent: String = r.get(1)?;
            Ok(UsageEvent {
                dedup_key: r.get(0)?,
                agent: Agent::parse(&agent).unwrap_or(Agent::Claude),
                host: r.get(2)?,
                session_key: r.get(3)?,
                ts: r.get(4)?,
                model_raw: r.get(5)?,
                // model is filled by the caller with pricing normalization.
                provider: r.get(7)?,
                fresh_input: r.get(8)?,
                cache_read: r.get(9)?,
                cache_write_5m: r.get(10)?,
                cache_write_1h: r.get(11)?,
                output: r.get(12)?,
                reasoning: r.get(13)?,
                reported_cost_usd: r.get(14)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn load_sessions(&self, filters: &Filters) -> Result<Vec<SessionInfo>> {
        let mut sql = String::from(
            "SELECT key,agent,host,source_path,archive_path,session_id,title,cwd,project,
                    started_at,ended_at,models,parent_session,is_subagent,parse_warnings,reported_cost_usd
             FROM sessions WHERE 1=1",
        );
        let mut vals: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(since) = filters.since {
            sql.push_str(" AND ended_at >= ?");
            vals.push(since.into());
        }
        if let Some(until) = filters.until {
            sql.push_str(" AND started_at < ?");
            vals.push(until.into());
        }
        if let Some(agents) = &filters.agents {
            if !agents.is_empty() {
                let placeholders = vec!["?"; agents.len()].join(",");
                sql.push_str(&format!(" AND agent IN ({placeholders})"));
                for a in agents {
                    vals.push(a.as_str().to_string().into());
                }
            }
        }
        if let Some(hosts) = &filters.hosts {
            if !hosts.is_empty() {
                let placeholders = vec!["?"; hosts.len()].join(",");
                sql.push_str(&format!(" AND host IN ({placeholders})"));
                for h in hosts {
                    vals.push(h.clone().into());
                }
            }
        }
        if let Some(project) = &filters.project {
            sql.push_str(" AND project = ?");
            vals.push(project.clone().into());
        }
        sql.push_str(" ORDER BY started_at DESC");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(vals), |r| {
            let agent: String = r.get(1)?;
            let models: String = r.get(11)?;
            let warnings: String = r.get(14)?;
            Ok(SessionInfo {
                key: r.get(0)?,
                agent: Agent::parse(&agent).unwrap_or(Agent::Claude),
                host: r.get(2)?,
                source_path: r.get(3)?,
                archive_path: r.get(4)?,
                session_id: r.get(5)?,
                title: r.get(6)?,
                cwd: r.get(7)?,
                project: r.get(8)?,
                started_at: r.get(9)?,
                ended_at: r.get(10)?,
                models: serde_json::from_str(&models).unwrap_or_default(),
                parent_session: r.get(12)?,
                is_subagent: r.get::<_, i64>(13)? != 0,
                parse_warnings: serde_json::from_str(&warnings).unwrap_or_default(),
                reported_cost_usd: r.get(15)?,
            })
        })?;
        let mut out: Vec<SessionInfo> = rows.filter_map(|r| r.ok()).collect();
        if let Some(search) = &filters.search {
            let needle = search.to_ascii_lowercase();
            out.retain(|s| {
                s.display_title().to_ascii_lowercase().contains(&needle)
                    || s.project.to_ascii_lowercase().contains(&needle)
                    || s.session_id.to_ascii_lowercase().contains(&needle)
                    || s.models
                        .iter()
                        .any(|m| m.to_ascii_lowercase().contains(&needle))
            });
        }
        Ok(out)
    }

    pub fn session(&self, key_or_prefix: &str) -> Result<Option<SessionInfo>> {
        let filter = Filters::default();
        let sessions = self.load_sessions(&filter)?;
        if let Some(exact) = sessions.iter().find(|s| s.key == key_or_prefix) {
            return Ok(Some(exact.clone()));
        }
        // A full session id should win over its subagent transcripts (which
        // share the same id as a prefix).
        if let Some(exact) = sessions
            .iter()
            .find(|s| s.session_id == key_or_prefix && !s.is_subagent)
            .or_else(|| sessions.iter().find(|s| s.session_id == key_or_prefix))
        {
            return Ok(Some(exact.clone()));
        }
        let matches: Vec<&SessionInfo> = sessions
            .iter()
            .filter(|s| s.session_id.starts_with(key_or_prefix) || s.key.contains(key_or_prefix))
            .collect();
        Ok(match matches.len() {
            1 => Some(matches[0].clone()),
            _ => None,
        })
    }

    /// Distinct models with token totals (for `doctor`).
    pub fn models(&self) -> Result<Vec<(String, String, u64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT u.model_raw, u.model, COUNT(*), SUM(u.fresh_input+u.cache_read+u.cache_write_5m+u.cache_write_1h+u.output)
             FROM usage u
             JOIN (SELECT dedup_key, MIN(rowid) AS r FROM usage GROUP BY dedup_key) d ON d.r=u.rowid
             GROUP BY u.model_raw, u.model
             ORDER BY 4 DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)?,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn dedup_stats(&self) -> Result<Vec<(String, u64, u64, u64)>> {
        // agent, raw lines, unique dedup keys, stored rows
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT agent, SUM(raw_usage_count), SUM(unique_usage_count) FROM files GROUP BY agent",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut per_agent: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for row in rows.flatten() {
            let entry = per_agent.entry(row.0).or_default();
            entry.0 = row.1.max(0) as u64;
            entry.1 = row.2.max(0) as u64;
        }
        drop(stmt);
        let mut stmt = self.conn.prepare(
            "SELECT agent, COUNT(*) FROM usage u
             JOIN (SELECT dedup_key, MIN(rowid) AS r FROM usage GROUP BY dedup_key) d ON d.r=u.rowid
             GROUP BY agent",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows.flatten() {
            per_agent.entry(row.0).or_default().2 = row.1.max(0) as u64;
        }
        for (agent, (raw, unique_local, unique_global)) in per_agent {
            out.push((agent, raw, unique_local.max(unique_global), unique_global));
        }
        out.sort();
        Ok(out)
    }

    /// Count Antigravity conversation bodies by layout, as detected on disk:
    /// `(db, pb_plain, pb_encrypted, summary_only)`.
    pub fn antigravity_layout_counts(&self) -> Result<(u64, u64, u64, u64)> {
        let count_files = |pattern: &str| -> Result<u64> {
            let n: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM files WHERE agent LIKE 'antigravity%'
                 AND archive_path LIKE ?1",
                [pattern],
                |r| r.get(0),
            )?;
            Ok(n.max(0) as u64)
        };
        let db = count_files("%conversations/%.db")?;
        let pb = count_files("%conversations/%.pb")?;
        let encrypted = self.warning_mentions("pb-encrypted")?;
        let plain = pb.saturating_sub(encrypted);
        let summary_only: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE parse_warnings LIKE '%summary-only%'",
            [],
            |r| r.get(0),
        )?;
        Ok((db, plain, encrypted, summary_only.max(0) as u64))
    }

    pub fn ag_layout_counts(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT layout, COUNT(*) FROM ag_meta GROUP BY layout ORDER BY layout")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn warning_mentions(&self, needle: &str) -> Result<u64> {
        let mut stmt = self
            .conn
            .prepare("SELECT parse_warnings FROM sessions WHERE parse_warnings LIKE ?1")?;
        let pattern = format!("%{needle}%");
        let rows = stmt.query_map([pattern], |r| r.get::<_, String>(0))?;
        Ok(rows
            .filter_map(|r| r.ok())
            .filter(|w| w.contains(needle))
            .count() as u64)
    }

    pub fn file_warnings(&self) -> Result<Vec<(String, String, Vec<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT archive_path, agent, warnings FROM files WHERE warnings <> '[]'")?;
        let rows = stmt.query_map([], |r| {
            let w: String = r.get(2)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                serde_json::from_str::<Vec<String>>(&w).unwrap_or_default(),
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    #[allow(clippy::type_complexity)]
    pub fn sessions_without_usage(&self) -> Result<Vec<(String, String, String, Vec<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.key, s.agent, COALESCE(s.title,''), s.parse_warnings
             FROM sessions s
             WHERE NOT EXISTS (SELECT 1 FROM usage u WHERE u.session_key=s.key)
             ORDER BY s.started_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let w: String = r.get(3)?;
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                serde_json::from_str::<Vec<String>>(&w).unwrap_or_default(),
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn counts(&self) -> Result<(u64, u64, u64, u64)> {
        let files: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?
            as u64;
        let sessions: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))?
            as u64;
        let usage_rows: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get::<_, i64>(0))?
            as u64;
        let usage_unique: u64 =
            self.conn
                .query_row("SELECT COUNT(DISTINCT dedup_key) FROM usage", [], |r| {
                    r.get::<_, i64>(0)
                })? as u64;
        Ok((files, sessions, usage_rows, usage_unique))
    }

    pub fn last_index_ms(&self) -> i64 {
        self.get_meta("last_index")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    pub fn ensure_index_ready(&self) -> Result<()> {
        let _: String = self.conn.query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )?;
        Ok(())
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM;")?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct ParseTask {
    path: PathBuf,
    host: String,
    agent: Agent,
    archive_rel: String,
    source_path: String,
    size: u64,
    mtime: i64,
}

#[derive(Clone, Debug)]
struct AgMetaRow {
    conversation_id: String,
    agent: String,
    host: String,
    archive_path: String,
    title: Option<String>,
    cwd: Option<String>,
    start_ms: i64,
    end_ms: i64,
    layout: String,
    parent_conversation_id: Option<String>,
}

fn strip_ide_prefix(rel: &str) -> &str {
    if let Some(rest) = rel.strip_prefix("antigravity/") {
        rest
    } else if let Some(rest) = rel.strip_prefix("antigravity-ide/") {
        rest
    } else {
        rel
    }
}

#[allow(clippy::type_complexity)]
fn read_codex_threads(
    path: &Path,
) -> Result<
    Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    )>,
> {
    let conn = sqlite_backup::open_read_only(path)?;
    let mut stmt = conn
        .prepare("SELECT id, title, cwd, model, tokens_used FROM threads")
        .context("selecting from Codex threads table")?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get::<_, Option<String>>(0)?.unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        out.push((
            id,
            row.get::<_, Option<String>>(1).unwrap_or(None),
            row.get::<_, Option<String>>(2).unwrap_or(None),
            row.get::<_, Option<String>>(3).unwrap_or(None),
            row.get::<_, Option<i64>>(4).unwrap_or(None),
        ));
    }
    Ok(out)
}

/// Delete and recreate the index from the archive (all hosts).
pub fn rebuild(cfg: &Config) -> Result<IngestStats> {
    let path = cfg.index_path();
    if path.exists() {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }
    let mut index = Index::open(&path)?;
    index.ingest(
        cfg,
        &IngestOptions {
            force: true,
            agents: None,
            hosts: None,
        },
    )
}

/// Load the archived transcript for a session and parse it into messages.
pub fn load_transcript(cfg: &Config, session: &SessionInfo) -> Result<Vec<crate::model::Message>> {
    // archive_path is stored relative to the host directory, e.g.
    // `claude/projects/...`.
    let path = cfg
        .archive_dir()
        .join("hosts")
        .join(&session.host)
        .join(&session.archive_path);
    match session.agent {
        Agent::Claude => crate::adapters::claude::read_messages(&path),
        Agent::Codex => crate::adapters::codex::read_messages(&path),
        Agent::Pi => crate::adapters::pi::read_messages(&path),
        Agent::Dsh => crate::adapters::dsh::read_messages(&path),
        Agent::AntigravityCli | Agent::AntigravityIde => {
            if let Some(transcript) = find_antigravity_transcript(cfg, session) {
                antigravity::read_transcript(&transcript)
            } else {
                Ok(Vec::new())
            }
        }
    }
}

fn find_antigravity_transcript(cfg: &Config, session: &SessionInfo) -> Option<PathBuf> {
    let host_root = cfg.archive_dir().join("hosts").join(&session.host);
    let agent_root = host_root.join(session.agent.archive_dir());
    let rel_parent = Path::new(&session.archive_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut candidates = Vec::new();
    for prefix in ["", "antigravity/", "antigravity-ide/"] {
        candidates.push(
            agent_root
                .join(prefix)
                .join("brain")
                .join(&session.session_id)
                .join(".system_generated/logs/transcript_full.jsonl"),
        );
    }
    let _ = rel_parent;
    candidates.into_iter().find(|p| p.exists())
}
