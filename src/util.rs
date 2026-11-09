//! Small shared helpers: paths, time zones, atomic file writes and formatting.

use anyhow::{Context, Result};
use jiff::civil::{Date, DateTime, Time};
use jiff::{Timestamp, tz::TimeZone};
use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand_tilde(p: &str) -> PathBuf {
    let p = p.trim();
    if p == "~" {
        home_dir()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(p)
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(dir).join("agentarc")
    } else {
        home_dir().join(".config/agentarc")
    }
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("AGENTARC_CONFIG") {
        PathBuf::from(p)
    } else {
        config_dir().join("config.toml")
    }
}

pub fn cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("AGENTARC_CACHE_DIR") {
        PathBuf::from(p)
    } else if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(dir).join("agentarc")
    } else {
        home_dir().join(".cache/agentarc")
    }
}

pub fn hostname() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .split('.')
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_string()
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn ms_from_secs(secs: i64) -> i64 {
    secs.saturating_mul(1000)
}

/// Parse an RFC 3339-ish timestamp into Unix milliseconds. Returns `None` when
/// the value is empty or malformed (adapters treat that as a warning, not an
/// error).
pub fn parse_ts_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Some(ts.as_millisecond());
    }
    // Some agents emit whole seconds or milliseconds as an integer.
    if let Ok(n) = s.parse::<i64>() {
        if n > 100_000_000_000 {
            return Some(n);
        }
        if n > 1_000_000_000 {
            return Some(n * 1000);
        }
    }
    None
}

pub fn fmt_ts(ms: i64, tz: &TimeZone, fmt: &str) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => ts.to_zoned(tz.clone()).strftime(fmt).to_string(),
        Err(_) => String::new(),
    }
}

pub fn today_start_ms(tz: &TimeZone, now: i64) -> i64 {
    match Timestamp::from_millisecond(now) {
        Ok(ts) => {
            let z = ts.to_zoned(tz.clone());
            let d = z.date();
            Date::new(d.year(), d.month(), d.day())
                .ok()
                .and_then(|d| d.to_zoned(tz.clone()).ok())
                .map(|z| z.timestamp().as_millisecond())
                .unwrap_or(now)
        }
        Err(_) => now,
    }
}

pub fn month_start_ms(tz: &TimeZone, now: i64) -> i64 {
    match Timestamp::from_millisecond(now) {
        Ok(ts) => {
            let z = ts.to_zoned(tz.clone());
            let d = z.date();
            Date::new(d.year(), d.month(), 1)
                .ok()
                .and_then(|d| d.to_zoned(tz.clone()).ok())
                .map(|z| z.timestamp().as_millisecond())
                .unwrap_or(now)
        }
        Err(_) => now,
    }
}

pub fn year_start_ms(tz: &TimeZone, now: i64) -> i64 {
    match Timestamp::from_millisecond(now) {
        Ok(ts) => {
            let z = ts.to_zoned(tz.clone());
            Date::new(z.date().year(), 1, 1)
                .ok()
                .and_then(|d| d.to_zoned(tz.clone()).ok())
                .map(|z| z.timestamp().as_millisecond())
                .unwrap_or(now)
        }
        Err(_) => now,
    }
}

pub fn days_ago_ms(days: i64, now: i64) -> i64 {
    now.saturating_sub(days.saturating_mul(86_400_000))
}

/// Parse a `YYYY-MM-DD` date at start (or end) of day in `tz`.
pub fn parse_date_ms(s: &str, tz: &TimeZone, end_of_day: bool) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i16 = parts[0].parse().ok()?;
    let m: i8 = parts[1].parse().ok()?;
    let d: i8 = parts[2].parse().ok()?;
    let date = Date::new(y, m, d).ok()?;
    let time = if end_of_day {
        Time::new(23, 59, 59, 999_999_999).ok()?
    } else {
        Time::new(0, 0, 0, 0).ok()?
    };
    DateTime::from_parts(date, time)
        .to_zoned(tz.clone())
        .ok()
        .map(|z| z.timestamp().as_millisecond())
}

pub fn date_str(ms: i64, tz: &TimeZone) -> String {
    fmt_ts(ms, tz, "%Y-%m-%d")
}

pub fn month_str(ms: i64, tz: &TimeZone) -> String {
    fmt_ts(ms, tz, "%Y-%m")
}

pub fn week_str(ms: i64, tz: &TimeZone) -> String {
    fmt_ts(ms, tz, "%G-W%V")
}

pub fn system_timezone() -> TimeZone {
    TimeZone::system()
}

/// Resolve a configured timezone name, falling back to the system zone.
pub fn resolve_timezone(name: &str) -> TimeZone {
    let name = name.trim();
    if name.is_empty() {
        system_timezone()
    } else {
        TimeZone::get(name).unwrap_or_else(|_| system_timezone())
    }
}

/// Normalize a cwd into a cross-host project name:
/// `/home/alice/GitHub/x` and `/Users/alice/GitHub/x` both become `~/GitHub/x`.
pub fn normalize_project_path(cwd: &str) -> String {
    let cwd = cwd.trim();
    if cwd.is_empty() {
        return String::new();
    }
    let mut path = cwd.to_string();
    for prefix in ["/Users/", "/home/", "/var/home/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if let Some((_user, tail)) = rest.split_once('/') {
                path = format!("~/{tail}");
            } else {
                path = "~".to_string();
            }
            break;
        }
    }
    if path == "/root" {
        path = "~".to_string();
    } else if let Some(rest) = path.strip_prefix("/root/") {
        path = format!("~/{rest}");
    }
    path
}

/// If a git repository is detectable from `cwd` (on this machine), use its root
/// so that subdirectories of one project are grouped together.
pub fn git_root(cwd: &str) -> Option<PathBuf> {
    let mut cur = PathBuf::from(cwd);
    if !cur.is_dir() {
        return None;
    }
    for _ in 0..64 {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

pub fn project_for_cwd(cwd: Option<&str>, aliases: &BTreeMap<String, String>) -> String {
    let Some(cwd) = cwd.map(str::trim).filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let root = git_root(cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string());
    let mut name = normalize_project_path(&root);
    // Longest alias prefix wins.
    let mut best: Option<(&String, &String)> = None;
    for (from, to) in aliases {
        let from_norm = normalize_project_path(from.trim());
        let matches = name == from_norm
            || name
                .strip_prefix(&from_norm)
                .is_some_and(|rest| rest.starts_with('/'));
        if matches && best.is_none_or(|(b, _)| from_norm.len() > b.len()) {
            best = Some((from, to));
        }
    }
    if let Some((_from, to)) = best {
        let from_norm = normalize_project_path(name_for_alias(aliases, _from));
        name = if name == from_norm {
            to.clone()
        } else {
            format!("{}{}", to, &name[from_norm.len()..])
        };
    }
    name
}

fn name_for_alias<'a>(aliases: &'a BTreeMap<String, String>, from: &'a String) -> &'a str {
    let _ = aliases;
    from
}

pub fn human_tokens(n: i64) -> String {
    let n = n.max(0);
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let v = n as f64;
    if v >= GB {
        format!("{:.2} GB", v / GB)
    } else if v >= MB {
        format!("{:.1} MB", v / MB)
    } else if v >= KB {
        format!("{:.0} KB", v / KB)
    } else {
        format!("{n} B")
    }
}

pub fn human_usd(v: f64) -> String {
    if v == 0.0 {
        "$0.00".to_string()
    } else if v < 0.01 {
        format!("${v:.4}")
    } else if v < 1000.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.1}")
    }
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn temp_path_for(path: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let tmp = format!(".{name}.tmp-{}-{n}", std::process::id());
    path.with_file_name(tmp)
}

/// Write bytes atomically: temp file in the same directory, fsync, rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let tmp = temp_path_for(path);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating temporary file {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    restrict_permissions(path);
    Ok(())
}

pub fn atomic_copy(src: &Path, dest: &Path) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_path_for(dest);
    let mut input = File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let n = std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    fs::rename(&tmp, dest)?;
    restrict_permissions(dest);
    if let Ok(meta) = input.metadata() {
        let _ = set_mtime(dest, meta.modified().ok());
    }
    Ok(n)
}

/// Archives contain full transcripts (code, possibly secrets). Keep newly
/// written files private to the user where the platform supports it.
pub fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub fn set_mtime(path: &Path, modified: Option<SystemTime>) -> Result<()> {
    let Some(mtime) = modified else {
        return Ok(());
    };
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_times(FileTimes::new().set_modified(mtime))?;
    Ok(())
}

/// BLAKE3 hash of at most `len` bytes from the start of a file.
pub fn hash_head(path: &Path, len: u64) -> Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = len;
    let mut buf = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn file_entropy(path: &Path) -> Result<f64> {
    let data = fs::read(path)?;
    Ok(entropy(&data))
}

pub fn entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for b in data {
        counts[*b as usize] += 1;
    }
    let len = data.len() as f64;
    let mut h = 0.0;
    for c in counts {
        if c > 0 {
            let p = c as f64 / len;
            h -= p * p.log2();
        }
    }
    h
}

/// Timestamp suffix safe for filenames: `20261109T214225Z`.
pub fn utc_stamp(ms: i64) -> String {
    let tz = TimeZone::UTC;
    fmt_ts(ms, &tz, "%Y%m%dT%H%M%SZ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_home_paths() {
        assert_eq!(normalize_project_path("/home/alice/GitHub/x"), "~/GitHub/x");
        assert_eq!(
            normalize_project_path("/Users/alice/GitHub/x"),
            "~/GitHub/x"
        );
        assert_eq!(normalize_project_path("/root/x"), "~/x");
        assert_eq!(normalize_project_path("/tmp/x"), "/tmp/x");
    }

    #[test]
    fn aliases_apply_to_prefix() {
        let mut aliases = BTreeMap::new();
        aliases.insert("~/Obsidian".to_string(), "obsidian-vault".to_string());
        assert_eq!(
            project_for_cwd(Some("/home/alice/Obsidian/Math"), &aliases),
            "obsidian-vault/Math"
        );
    }
}
