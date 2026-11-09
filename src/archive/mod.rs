//! Archive: discovery of source files, an incremental manifest and safe copying
//! into `hosts/<host>/<agent>/...`.
//!
//! The archive is the source of truth. Sources are opened read-only; when a
//! source shrinks or is rewritten, the previously archived copy is preserved as
//! a versioned snapshot before it is replaced.

pub mod sqlite_backup;

use crate::config::Config;
use crate::model::Agent;
use crate::util;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Jsonl,
    Zstd,
    Sqlite,
    Protobuf,
    Text,
}

impl FileKind {
    pub fn from_path(path: &Path) -> FileKind {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".zstd") || name.ends_with(".zst") {
            FileKind::Zstd
        } else if name.ends_with(".jsonl") {
            FileKind::Jsonl
        } else if name.ends_with(".db") || name.ends_with(".sqlite") {
            FileKind::Sqlite
        } else if name.ends_with(".pb") {
            FileKind::Protobuf
        } else {
            FileKind::Text
        }
    }
}

#[derive(Clone, Debug)]
pub struct Discovered {
    pub agent: Agent,
    pub src: PathBuf,
    /// Path relative to the agent's archive directory
    /// (e.g. `projects/-home-user/<uuid>.jsonl`).
    pub rel: PathBuf,
    pub kind: FileKind,
}

#[derive(Default, Debug)]
pub struct Discovery {
    pub files: Vec<Discovered>,
    pub errors: Vec<String>,
}

impl Discovery {
    pub fn count_for(&self, agent: Agent) -> usize {
        self.files.iter().filter(|f| f.agent == agent).count()
    }
}

pub fn discover(cfg: &Config, agents: &[Agent]) -> Discovery {
    let mut out = Discovery::default();
    for &agent in agents {
        if !cfg.source_config(agent).enabled {
            continue;
        }
        match agent {
            Agent::Claude => discover_claude(cfg, &mut out),
            Agent::Codex => discover_codex(cfg, &mut out),
            Agent::Pi => discover_simple(cfg, agent, "sessions", &mut out),
            Agent::Dsh => discover_dsh(cfg, &mut out),
            Agent::AntigravityCli => discover_antigravity(cfg, agent, false, &mut out),
            Agent::AntigravityIde => discover_antigravity(cfg, agent, true, &mut out),
        }
    }
    out.files.sort_by(|a, b| a.src.cmp(&b.src));
    out
}

fn walk_files(root: &Path) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Never descend into the archive itself if someone points a source
            // at a parent directory, and skip temp/lock noise.
            let name = e.file_name().to_string_lossy();
            !(name == ".git"
                || name == "node_modules"
                || name.starts_with(".tmp-")
                || name.ends_with(".lock")
                || name.ends_with(".tmp"))
        })
}

fn push_file(out: &mut Discovery, agent: Agent, src: PathBuf, rel: PathBuf) {
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return;
    }
    out.files.push(Discovered {
        agent,
        kind: FileKind::from_path(&src),
        src,
        rel,
    });
}

fn discover_claude(cfg: &Config, out: &mut Discovery) {
    for root in cfg.source_roots(Agent::Claude) {
        if !root.is_dir() {
            continue;
        }
        for entry in walk_files(&root) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&root) else {
                continue;
            };
            push_file(
                out,
                Agent::Claude,
                path.to_path_buf(),
                Path::new("projects").join(rel),
            );
        }
    }
}

fn discover_codex(cfg: &Config, out: &mut Discovery) {
    for root in cfg.source_roots(Agent::Codex) {
        if !root.is_dir() {
            continue;
        }
        let label = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "sessions".to_string());
        for entry in walk_files(&root) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&root) else {
                continue;
            };
            push_file(
                out,
                Agent::Codex,
                path.to_path_buf(),
                Path::new(&label).join(rel),
            );
        }
    }
    // state_*.sqlite holds thread titles/cwd/tokens_used. Archive the highest
    // numbered snapshot under codex/state.sqlite.
    if !cfg.source_config(Agent::Codex).enabled {
        return;
    }
    let home = cfg.codex_home();
    if home.is_dir() {
        let mut best: Option<(u64, PathBuf)> = None;
        if let Ok(rd) = fs::read_dir(&home) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let Some(rest) = name.strip_prefix("state_") else {
                    continue;
                };
                let Some(num) = rest.strip_suffix(".sqlite") else {
                    continue;
                };
                let Ok(n) = num.parse::<u64>() else { continue };
                if best.as_ref().is_none_or(|(b, _)| n > *b) {
                    best = Some((n, entry.path()));
                }
            }
        }
        if let Some((_n, path)) = best {
            push_file(out, Agent::Codex, path, PathBuf::from("state.sqlite"));
        }
    }
}

fn discover_simple(cfg: &Config, agent: Agent, prefix: &str, out: &mut Discovery) {
    for root in cfg.source_roots(agent) {
        if !root.is_dir() {
            continue;
        }
        for entry in walk_files(&root) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&root) else {
                continue;
            };
            push_file(out, agent, path.to_path_buf(), Path::new(prefix).join(rel));
        }
    }
}

fn discover_dsh(cfg: &Config, out: &mut Discovery) {
    for root in cfg.source_roots(Agent::Dsh) {
        if !root.is_dir() {
            continue;
        }
        // One session directory may hold both session.jsonl.zstd (v0) and
        // session.v4.jsonl.zstd. Archive/index only the highest version.
        let mut by_dir: BTreeMap<PathBuf, (u32, PathBuf)> = BTreeMap::new();
        for entry in walk_files(&root) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.starts_with("session") || !name.ends_with(".jsonl.zstd") {
                continue;
            }
            let version = name
                .strip_prefix("session.v")
                .and_then(|s| s.split('.').next())
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let dir = path.parent().unwrap_or(&root).to_path_buf();
            match by_dir.get(&dir) {
                Some((v, _)) if *v >= version => {}
                _ => {
                    by_dir.insert(dir, (version, path.to_path_buf()));
                }
            }
        }
        for (_dir, (_v, path)) in by_dir {
            let Ok(rel) = path.strip_prefix(&root) else {
                continue;
            };
            push_file(
                out,
                Agent::Dsh,
                path.clone(),
                Path::new("sessions").join(rel),
            );
        }
    }
}

fn discover_antigravity(cfg: &Config, agent: Agent, ide: bool, out: &mut Discovery) {
    for root in cfg.source_roots(agent) {
        if !root.is_dir() {
            continue;
        }
        // Prefix with the root's name for the IDE because both the unified and
        // the legacy layout may exist.
        let prefix = if ide {
            root.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "antigravity".to_string())
        } else {
            String::new()
        };
        for entry in walk_files(&root) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(&root) else {
                continue;
            };
            if !is_antigravity_artifact(rel) {
                continue;
            }
            let full_rel = if prefix.is_empty() {
                rel.to_path_buf()
            } else {
                Path::new(&prefix).join(rel)
            };
            push_file(out, agent, path.to_path_buf(), full_rel);
        }
    }
}

fn is_antigravity_artifact(rel: &Path) -> bool {
    let s = rel.to_string_lossy().replace('\\', "/");
    let name = rel.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if s.starts_with("conversations/") {
        return name.ends_with(".db") || name.ends_with(".pb");
    }
    if name == "conversation_summaries.db" {
        return true;
    }
    if name.ends_with("_summaries_proto.pb") {
        return true;
    }
    if s.starts_with("annotations/") && name.ends_with(".pbtxt") {
        return true;
    }
    if s.starts_with("brain/") && s.ends_with("/.system_generated/logs/transcript_full.jsonl") {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestEntry {
    #[serde(default)]
    pub rel: String,
    #[serde(default)]
    pub agent: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub inode: u64,
    /// BLAKE3 of the complete source file at the time it was archived.
    #[serde(default)]
    pub content_hash: String,
    pub archived_at: i64,
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub source_missing_since: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "manifest_version")]
    pub version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, ManifestEntry>,
}

fn manifest_version() -> u32 {
    1
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: 1,
            files: BTreeMap::new(),
        }
    }
}

impl Manifest {
    pub fn path(archive: &Path, host: &str) -> PathBuf {
        archive.join("hosts").join(host).join("manifest.json")
    }

    pub fn load(archive: &Path, host: &str) -> Result<Self> {
        let path = Self::path(archive, host);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing manifest {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading manifest {}", path.display())),
        }
    }

    pub fn save(&self, archive: &Path, host: &str) -> Result<()> {
        let path = Self::path(archive, host);
        let text = serde_json::to_string_pretty(self)?;
        util::atomic_write(&path, text.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub dry_run: bool,
    pub agents: Vec<Agent>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            agents: Agent::ALL.to_vec(),
        }
    }
}

/// Coarse progress emitted while scanning/copying source files.
#[derive(Clone, Debug, Default)]
pub struct SyncProgress {
    pub phase: String,
    pub total: u64,
    pub processed: u64,
    pub copied: u64,
    pub versioned: u64,
    pub bytes: u64,
    pub current: String,
    pub elapsed_ms: u128,
}

#[derive(Clone, Debug, Default)]
pub struct AgentSyncCounts {
    pub new: u64,
    pub grown: u64,
    pub rewritten: u64,
    pub versioned: u64,
    pub unchanged: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SyncSummary {
    pub host: String,
    pub archive: PathBuf,
    pub per_agent: BTreeMap<Agent, AgentSyncCounts>,
    pub errors: Vec<String>,
    pub duration_ms: u128,
    pub dry_run: bool,
    pub sources_missing: u64,
}

impl SyncSummary {
    pub fn total_new(&self) -> u64 {
        self.per_agent.values().map(|a| a.new).sum()
    }
    pub fn total_grown(&self) -> u64 {
        self.per_agent.values().map(|a| a.grown).sum()
    }
    pub fn total_rewritten(&self) -> u64 {
        self.per_agent.values().map(|a| a.rewritten).sum()
    }
    pub fn total_versioned(&self) -> u64 {
        self.per_agent.values().map(|a| a.versioned).sum()
    }
    pub fn total_unchanged(&self) -> u64 {
        self.per_agent.values().map(|a| a.unchanged).sum()
    }
    pub fn total_bytes(&self) -> u64 {
        self.per_agent.values().map(|a| a.bytes).sum()
    }

    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        for (agent, c) in &self.per_agent {
            if c.new + c.grown + c.rewritten + c.versioned + c.unchanged == 0 {
                continue;
            }
            parts.push(format!(
                "{} +{} new, {} grown, {} rewritten, {} versioned",
                agent.as_str(),
                c.new,
                c.grown,
                c.rewritten,
                c.versioned
            ));
        }
        if parts.is_empty() {
            parts.push("nothing to do".to_string());
        }
        format!(
            "sync: {} · {} file(s) checked in {:.1} s",
            parts.join(" · "),
            self.total_new()
                + self.total_grown()
                + self.total_rewritten()
                + self.total_versioned()
                + self.total_unchanged(),
            self.duration_ms as f64 / 1000.0
        )
    }
}

fn stat_parts(meta: &fs::Metadata) -> (u64, i64, u64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.len(),
        meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        meta.ino(),
    )
}

/// A live SQLite database in WAL mode may be written to the `-wal` sidecar
/// without touching the main file's mtime. Fold the sidecar into the change
/// detection key so we do not miss those writes.
fn source_identity(path: &Path, kind: FileKind) -> Result<(u64, i64, u64)> {
    let meta = fs::metadata(path)?;
    let (mut size, mut mtime_ns, inode) = stat_parts(&meta);
    if kind == FileKind::Sqlite {
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        if let Ok(wmeta) = fs::metadata(&wal) {
            let (ws, wm, _) = stat_parts(&wmeta);
            size = size.saturating_add(ws);
            mtime_ns = mtime_ns.max(wm);
        }
    }
    Ok((size, mtime_ns, inode))
}

fn sqlite_source_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let main = fs::metadata(path).and_then(|m| m.modified()).ok();
    let wal = fs::metadata(format!("{}-wal", path.display()))
        .and_then(|m| m.modified())
        .ok();
    match (main, wal) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

pub fn run_sync(cfg: &Config, opts: &SyncOptions) -> Result<SyncSummary> {
    run_sync_with_progress(cfg, opts, |_| {})
}

pub fn run_sync_with_progress<F>(
    cfg: &Config,
    opts: &SyncOptions,
    progress: F,
) -> Result<SyncSummary>
where
    F: Fn(&SyncProgress) + Send + Sync,
{
    let emit = |p: SyncProgress| progress(&p);
    let started = Instant::now();
    cfg.ensure_archive_safe()?;
    let archive = cfg.archive_dir();
    let host = cfg.host_name();
    let mut summary = SyncSummary {
        host: host.clone(),
        archive: archive.clone(),
        dry_run: opts.dry_run,
        ..Default::default()
    };

    if !opts.dry_run {
        write_readme_if_missing(&archive, &host)?;
    }

    emit(SyncProgress {
        phase: "discovering".to_string(),
        ..Default::default()
    });
    let mut manifest = Manifest::load(&archive, &host)?;
    let discovery = discover(cfg, &opts.agents);
    summary.errors.extend(discovery.errors);

    let host_dir = archive.join("hosts").join(&host);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut processed = 0u64;
    let total_files = discovery.files.len() as u64;
    let mut copied = 0u64;
    let mut versioned_total = 0u64;
    let mut copied_bytes = 0u64;
    emit(SyncProgress {
        phase: "syncing".to_string(),
        total: total_files,
        ..Default::default()
    });

    for (idx, file) in discovery.files.iter().enumerate() {
        let checked = idx as u64 + 1;
        emit(SyncProgress {
            phase: "syncing".to_string(),
            total: total_files,
            processed: checked,
            copied,
            versioned: versioned_total,
            bytes: copied_bytes,
            current: file.rel.to_string_lossy().to_string(),
            elapsed_ms: started.elapsed().as_millis(),
        });
        let key = file.src.to_string_lossy().to_string();
        seen.insert(key.clone());
        let counts = summary.per_agent.entry(file.agent).or_default();
        let (size, mtime_ns, inode) = match source_identity(&file.src, file.kind) {
            Ok(v) => v,
            Err(e) => {
                summary.errors.push(format!("{}: {e}", file.src.display()));
                continue;
            }
        };
        if let Some(entry) = manifest.files.get(&key) {
            if entry.size == size && entry.mtime_ns == mtime_ns && entry.inode == inode {
                counts.unchanged += 1;
                continue;
            }
        }
        let dest = host_dir.join(file.agent.archive_dir()).join(&file.rel);

        if opts.dry_run {
            counts.new += 1;
            continue;
        }

        let outcome = match file.kind {
            FileKind::Sqlite => sync_sqlite(&file.src, &dest, &host_dir, file.agent, &file.rel),
            _ => sync_text(&file.src, &dest, &host_dir, file.agent, &file.rel),
        };

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                summary
                    .errors
                    .push(format!("{}: {e:#}", file.src.display()));
                continue;
            }
        };

        match outcome.action {
            CopyAction::New => counts.new += 1,
            CopyAction::Grown => counts.grown += 1,
            CopyAction::Rewritten => counts.rewritten += 1,
            CopyAction::Identical => counts.unchanged += 1,
        }
        counts.versioned += outcome.versioned;
        counts.bytes += outcome.bytes;
        copied += 1;
        versioned_total += outcome.versioned;
        copied_bytes += outcome.bytes;

        let content_hash = outcome
            .content_hash
            .unwrap_or_else(|| util::hash_head(&file.src, size).unwrap_or_default());
        let entry = manifest.files.entry(key).or_insert_with(|| ManifestEntry {
            rel: file.rel.to_string_lossy().to_string(),
            agent: file.agent.as_str().to_string(),
            size,
            mtime_ns,
            inode,
            content_hash: content_hash.clone(),
            archived_at: util::now_ms(),
            versions: Vec::new(),
            source_missing_since: None,
        });
        entry.rel = file.rel.to_string_lossy().to_string();
        entry.agent = file.agent.as_str().to_string();
        entry.size = size;
        entry.mtime_ns = mtime_ns;
        entry.inode = inode;
        entry.content_hash = content_hash;
        entry.archived_at = util::now_ms();
        entry.source_missing_since = None;
        if !outcome.versioned_rel.is_empty() {
            entry.versions.push(outcome.versioned_rel);
        }

        processed += 1;
        if processed.is_multiple_of(25) {
            let _ = manifest.save(&archive, &host);
        }
    }

    // Mark sources that disappeared. Deleted sources are never removed from the
    // archive.
    let now = util::now_ms();
    for (key, entry) in manifest.files.iter_mut() {
        let Some(agent) = Agent::parse(&entry.agent) else {
            continue;
        };
        if !opts.agents.contains(&agent) {
            continue;
        }
        if !cfg.source_config(agent).enabled {
            continue;
        }
        if seen.contains(key) {
            continue;
        }
        if !Path::new(key).exists() && entry.source_missing_since.is_none() {
            entry.source_missing_since = Some(now);
            summary.sources_missing += 1;
        }
    }

    if !opts.dry_run {
        manifest.save(&archive, &host)?;
        // A manifest key list makes stat-only diffing fast for humans.
        let _ = write_manifest_summary(&archive, &host, &manifest);
    }

    summary.duration_ms = started.elapsed().as_millis();
    emit(SyncProgress {
        phase: "done".to_string(),
        total: total_files,
        processed: total_files,
        copied,
        versioned: versioned_total,
        bytes: copied_bytes,
        current: String::new(),
        elapsed_ms: started.elapsed().as_millis(),
    });
    Ok(summary)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyAction {
    New,
    Grown,
    Rewritten,
    Identical,
}

struct CopyOutcome {
    action: CopyAction,
    versioned: u64,
    versioned_rel: String,
    bytes: u64,
    content_hash: Option<String>,
}

fn sync_text(
    src: &Path,
    dest: &Path,
    host_dir: &Path,
    agent: Agent,
    rel: &Path,
) -> Result<CopyOutcome> {
    let src_meta = fs::metadata(src)?;
    let src_size = src_meta.len();
    let old_size = fs::metadata(dest).map(|m| m.len()).ok();
    let mut versioned = 0u64;
    let mut versioned_rel = String::new();

    if let Some(old_size) = old_size {
        let old_hash = util::hash_head(dest, old_size).ok();
        let new_prefix_hash = util::hash_head(src, old_size.min(src_size)).ok();
        if src_size == old_size && old_hash.is_some() && old_hash == new_prefix_hash {
            return Ok(CopyOutcome {
                action: CopyAction::Identical,
                versioned: 0,
                versioned_rel: String::new(),
                bytes: 0,
                content_hash: old_hash,
            });
        }
        if src_size > old_size && old_hash.is_some() && old_hash == new_prefix_hash {
            // Pure append.
        } else {
            // Rewritten or shrunk: keep the old copy first.
            let (v, p) = version_copy(dest, host_dir, agent, rel)?;
            versioned += v;
            versioned_rel = p;
        }
    }

    let action = if old_size.is_none() {
        CopyAction::New
    } else if versioned > 0 {
        CopyAction::Rewritten
    } else {
        CopyAction::Grown
    };
    let bytes = util::atomic_copy(src, dest)?;
    let content_hash = hash_file_hex(dest).ok();
    let _ = util::set_mtime(dest, src_meta.modified().ok());
    Ok(CopyOutcome {
        action,
        versioned,
        versioned_rel,
        bytes,
        content_hash,
    })
}

fn sync_sqlite(
    src: &Path,
    dest: &Path,
    host_dir: &Path,
    agent: Agent,
    rel: &Path,
) -> Result<CopyOutcome> {
    let src_meta = fs::metadata(src)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_file_name(format!(
        ".{}.snapshot-{}-{}",
        dest.file_name().and_then(|s| s.to_str()).unwrap_or("db"),
        std::process::id(),
        util::now_ms()
    ));
    let tmp_guard = TempGuard(tmp.clone());
    let new_rows = sqlite_backup::backup_to(src, &tmp)?;
    let old_rows = sqlite_backup::row_count(dest).unwrap_or(0);
    let mut versioned = 0u64;
    let mut versioned_rel = String::new();
    let mut action = CopyAction::New;
    if dest.exists() {
        action = CopyAction::Grown;
        if new_rows < old_rows {
            let (v, p) = version_copy(dest, host_dir, agent, rel)?;
            versioned += v;
            versioned_rel = p;
            action = CopyAction::Rewritten;
        }
    }
    fs::rename(&tmp, dest).with_context(|| format!("installing {}", dest.display()))?;
    tmp_guard.disarm();
    util::restrict_permissions(dest);
    let _ = util::set_mtime(
        dest,
        sqlite_source_mtime(src).or_else(|| src_meta.modified().ok()),
    );
    let bytes = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    Ok(CopyOutcome {
        action,
        versioned,
        versioned_rel,
        bytes,
        content_hash: None,
    })
}

struct TempGuard(PathBuf);
impl TempGuard {
    fn disarm(mut self) {
        self.0 = PathBuf::new();
        std::mem::forget(self);
    }
}
impl Drop for TempGuard {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

fn version_copy(dest: &Path, host_dir: &Path, agent: Agent, rel: &Path) -> Result<(u64, String)> {
    let stamp = util::utc_stamp(util::now_ms());
    let version_rel = Path::new(".versions")
        .join(agent.archive_dir())
        .join(format!("{}.{}", rel.to_string_lossy(), stamp));
    let target = host_dir.join(&version_rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(dest, &target)
        .with_context(|| format!("versioning {} -> {}", dest.display(), target.display()))?;
    Ok((1, version_rel.to_string_lossy().to_string()))
}

fn hash_file_hex(path: &Path) -> Result<String> {
    let size = fs::metadata(path)?.len();
    util::hash_head(path, size)
}

fn write_readme_if_missing(archive: &Path, host: &str) -> Result<()> {
    let path = archive.join("README.txt");
    if path.exists() {
        return Ok(());
    }
    let text = format!(
        "agentarc archive\n================\n\n\
This folder is an append-only archive of AI agent session transcripts.\n\n\
Layout:\n  hosts/<hostname>/<agent>/...   mirrored source paths\n  hosts/<hostname>/manifest.json per-file sizes, hashes and versions\n  hosts/<hostname>/.versions/... older copies preserved before a rewrite or shrink\n\n\
Each machine writes only under its own hosts/<hostname>/ folder, so a sync tool\n\
(OneDrive, Syncthing, ...) never sees conflicting writes. The archive is plain\n\
files: you can restore any session by copying it back to the original path\n\
listed in that host's manifest.json.\n\n\
This archive was last touched by host `{host}`.\n"
    );
    util::atomic_write(&path, text.as_bytes())
}

#[derive(Serialize)]
struct ManifestSummary<'a> {
    host: &'a str,
    files: usize,
    updated_at: i64,
}

fn write_manifest_summary(archive: &Path, host: &str, manifest: &Manifest) -> Result<()> {
    let summary = ManifestSummary {
        host,
        files: manifest.files.len(),
        updated_at: util::now_ms(),
    };
    let text = serde_json::to_string_pretty(&summary)?;
    util::atomic_write(
        &archive
            .join("hosts")
            .join(host)
            .join("manifest.summary.json"),
        text.as_bytes(),
    )
}

/// Compute the number of files currently in an archive (optionally only for
/// one agent or host).
pub fn archive_file_counts(
    archive: &Path,
    agent: Option<Agent>,
    host: Option<&str>,
) -> Result<Vec<(String, String, u64, u64)>> {
    let hosts_dir = archive.join("hosts");
    let mut out = Vec::new();
    if !hosts_dir.is_dir() {
        return Ok(out);
    }
    for host_entry in fs::read_dir(&hosts_dir)?.flatten() {
        if !host_entry.file_type()?.is_dir() {
            continue;
        }
        let hname = host_entry.file_name().to_string_lossy().to_string();
        if let Some(want) = host {
            if want != hname {
                continue;
            }
        }
        let hpath = host_entry.path();
        for agent_entry in fs::read_dir(&hpath)?.flatten() {
            if !agent_entry.file_type()?.is_dir() {
                continue;
            }
            let aname = agent_entry.file_name().to_string_lossy().to_string();
            if aname == ".versions" {
                continue;
            }
            let Some(a) = Agent::from_archive_dir(&aname) else {
                continue;
            };
            if let Some(want) = agent {
                if want != a {
                    continue;
                }
            }
            let mut count = 0u64;
            let mut bytes = 0u64;
            for e in WalkDir::new(agent_entry.path()).follow_links(false) {
                let Ok(e) = e else { continue };
                if !e.file_type().is_file() {
                    continue;
                }
                count += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
            out.push((hname.clone(), aname, count, bytes));
        }
    }
    out.sort();
    Ok(out)
}

/// Quickly validate that a `hosts/<host>/manifest.json` is readable.
pub fn verify_manifest(archive: &Path, host: &str) -> Result<usize> {
    let m = Manifest::load(archive, host)?;
    Ok(m.files.len())
}

/// Copy the archive (used only by tests/tools) while refusing to run inside a
/// cloud folder.
pub fn ensure_not_cloud(cfg: &Config) -> Result<()> {
    cfg.ensure_archive_safe()?;
    let archive = cfg.archive_dir();
    if archive.exists() && !archive.is_dir() {
        bail!("archive_dir {} is not a directory", archive.display());
    }
    Ok(())
}

/// Duration helper kept public so tests can express their own timeouts.
pub fn small_pause() -> Duration {
    Duration::from_millis(5)
}
