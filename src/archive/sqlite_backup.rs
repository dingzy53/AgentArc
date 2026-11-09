//! Safe snapshots of live SQLite databases.
//!
//! Agents write their conversation databases in WAL mode. Copying a `.db` with
//! `cp` can capture a torn state, so we use the SQLite online backup API: open
//! the source read-only, stream it into a fresh database, then atomically rename
//! that snapshot into the archive.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, backup::Backup};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn open_read_only(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Ok(conn) = Connection::open_with_flags(path, flags) {
        return Ok(conn);
    }
    // Read-only filesystems may refuse to create the WAL `-shm` file. The
    // immutable URI tells SQLite the file is not being written while we read it;
    // this is the best available fallback for a source that is on read-only
    // media.
    let encoded = path
        .to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3F");
    let uri = format!("file:{encoded}?immutable=1");
    Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)
        .with_context(|| format!("opening SQLite database {}", path.display()))
}

/// Snapshot `src` into `dest`. `dest` must not exist. Returns a cheap content
/// counter (rows in the main metadata table, or tables as a fallback) so callers
/// can detect a database that got smaller.
pub fn backup_to(src: &Path, dest: &Path) -> Result<u64> {
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // A WAL database on read-only media cannot create its `-shm` sidecar, so
    // the first attempt may fail. Retry with SQLite's immutable URI; the backup
    // API still produces a consistent snapshot of the bytes we can read.
    let first = open_read_only(src).and_then(|conn| do_backup(conn, dest));
    match first {
        Ok(n) => Ok(n),
        Err(first_err) => {
            if dest.exists() {
                let _ = fs::remove_file(dest);
            }
            let conn = open_immutable(src).with_context(|| {
                format!(
                    "opening SQLite database {} (after: {first_err:#})",
                    src.display()
                )
            })?;
            do_backup(conn, dest)
        }
    }
}

fn do_backup(conn: Connection, dest: &Path) -> Result<u64> {
    let mut dst =
        Connection::open(dest).with_context(|| format!("creating snapshot {}", dest.display()))?;
    {
        let backup = Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(512, Duration::from_millis(2), None)?;
    }
    drop(dst);
    drop(conn);
    let check = Connection::open(dest)?;
    let rows = content_count(&check).unwrap_or(0);
    drop(check);
    Ok(rows)
}

fn open_immutable(path: &Path) -> Result<Connection> {
    let encoded = path
        .to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3F");
    let uri = format!("file:{encoded}?immutable=1");
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening SQLite database {} (immutable)", path.display()))
}

/// Cheap measure of how much content a database holds. Prefers agent-specific
/// tables; falls back to the table count so an unknown schema still gets a
/// monotonic-ish check.
pub fn row_count(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let conn = open_read_only(path)?;
    let n = content_count(&conn).unwrap_or(0);
    Ok(n)
}

fn content_count(conn: &Connection) -> Result<u64> {
    for table in ["gen_metadata", "steps", "threads"] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if exists {
            let sql = format!("SELECT count(*) FROM \"{table}\"");
            let n: i64 = conn.query_row(&sql, [], |r| r.get(0))?;
            return Ok(n.max(0) as u64);
        }
    }
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table'",
        [],
        |r| r.get(0),
    )?;
    Ok(n.max(0) as u64)
}

/// Read a single integer/string value from the archived Codex state database.
pub fn query_text_rows(path: &Path, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let conn = open_read_only(path)?;
    let mut stmt = conn.prepare(sql)?;
    let cols = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut vals = Vec::with_capacity(cols);
        for i in 0..cols {
            let v: Option<String> = row
                .get::<_, Option<String>>(i)
                .or_else(|_| {
                    row.get::<_, Option<i64>>(i)
                        .map(|v| v.map(|n| n.to_string()))
                })
                .unwrap_or(None);
            vals.push(v);
        }
        out.push(vals);
    }
    Ok(out)
}

/// Copy a database snapshot to a new path (used by tests).
pub fn copy_snapshot(src: &Path, dest: &Path) -> Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", dest.display()));
    backup_to(src, &tmp)?;
    fs::rename(&tmp, dest)?;
    Ok(())
}
