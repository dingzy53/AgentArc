use agentarc::archive::{self, FileKind, SyncOptions};
use agentarc::config::Config;
use agentarc::model::Agent;
use agentarc::util;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn test_config(archive: &Path, claude_root: &Path) -> Config {
    let mut cfg = Config {
        archive_dir: archive.to_string_lossy().to_string(),
        host_name: "testhost".to_string(),
        sync_on_start: false,
        ..Default::default()
    };
    cfg.sources.claude.enabled = true;
    cfg.sources.claude.paths = vec![claude_root.to_string_lossy().to_string()];
    cfg.sources.codex.enabled = false;
    cfg.sources.pi.enabled = false;
    cfg.sources.dsh.enabled = false;
    cfg.sources.antigravity_cli.enabled = false;
    cfg.sources.antigravity_ide.enabled = false;
    cfg
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn sync_grows_versions_and_keeps_deleted_sources() {
    let temp = tempdir().unwrap();
    let src_root = temp.path().join("src/projects");
    let session = src_root.join("p1/s1.jsonl");
    fs::create_dir_all(session.parent().unwrap()).unwrap();
    fs::write(&session, "{\"line\":1}\n{\"line\":2}\n").unwrap();
    let archive_dir = temp.path().join("archive");
    let cfg = test_config(&archive_dir, &src_root);

    let before = fs::metadata(&session).unwrap();
    let before_ns = {
        use std::os::unix::fs::MetadataExt;
        before.mtime() * 1_000_000_000 + before.mtime_nsec()
    };
    let before_hash = util::hash_head(&session, fs::metadata(&session).unwrap().len()).unwrap();

    let first = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert_eq!(first.total_new(), 1);
    let archived = archive_dir.join("hosts/testhost/claude/projects/p1/s1.jsonl");
    assert!(archived.exists());
    assert_eq!(read(&archived), "{\"line\":1}\n{\"line\":2}\n");
    assert!(archive_dir.join("hosts/testhost/manifest.json").exists());
    assert!(archive_dir.join("README.txt").exists());

    // The source must never be modified by a sync.
    let after = fs::metadata(&session).unwrap();
    let after_ns = {
        use std::os::unix::fs::MetadataExt;
        after.mtime() * 1_000_000_000 + after.mtime_nsec()
    };
    let after_hash = util::hash_head(&session, fs::metadata(&session).unwrap().len()).unwrap();
    assert_eq!(before_ns, after_ns, "source mtime changed");
    assert_eq!(before_hash, after_hash, "source content changed");

    // Fast path: no change, no reads.
    let second = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert_eq!(second.total_unchanged(), 1);
    assert_eq!(second.total_new(), 0);

    // Append-only growth.
    fs::write(&session, "{\"line\":1}\n{\"line\":2}\n{\"line\":3}\n").unwrap();
    let third = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert_eq!(third.total_grown(), 1);
    assert!(read(&archived).contains("line\":3"));

    // Rewrite/shrink: old copy must be preserved under .versions/.
    fs::write(&session, "{\"line\":9}\n").unwrap();
    let fourth = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert_eq!(fourth.total_rewritten(), 1);
    assert_eq!(fourth.total_versioned(), 1);
    let versions: Vec<_> = walkdir::WalkDir::new(archive_dir.join("hosts/testhost/.versions"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();
    assert_eq!(versions.len(), 1, "one versioned snapshot expected");
    assert!(read(versions[0].path()).contains("line\":3"));

    // Deleting the source keeps the archived file and records the deletion.
    fs::remove_file(&session).unwrap();
    let fifth = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert_eq!(fifth.sources_missing, 1);
    assert!(archived.exists());
    assert!(read(&archived).contains("line\":9"));
    let manifest = archive::Manifest::load(&archive_dir, "testhost").unwrap();
    let entry = manifest.files.values().next().unwrap();
    assert!(entry.source_missing_since.is_some());
    assert_eq!(entry.versions.len(), 1);
}

#[test]
fn dry_run_writes_nothing() {
    let temp = tempdir().unwrap();
    let src_root = temp.path().join("src");
    fs::create_dir_all(&src_root).unwrap();
    fs::write(src_root.join("a.jsonl"), "{}\n").unwrap();
    let archive_dir = temp.path().join("archive");
    let cfg = test_config(&archive_dir, &src_root);
    let opts = SyncOptions {
        dry_run: true,
        agents: vec![Agent::Claude],
    };
    let summary = archive::run_sync(&cfg, &opts).unwrap();
    assert_eq!(summary.total_new(), 1);
    assert!(!archive_dir.exists(), "dry run created the archive");
}

#[test]
fn cloud_guard_refuses_without_opt_in() {
    let temp = tempdir().unwrap();
    let src_root = temp.path().join("src");
    fs::create_dir_all(&src_root).unwrap();
    let cloud = temp.path().join("OneDrive/AgentArchive");
    let cfg = test_config(&cloud, &src_root);
    let err = archive::run_sync(&cfg, &SyncOptions::default()).unwrap_err();
    assert!(format!("{err:#}").contains("cloud"), "{err:#}");
}

#[test]
fn file_kind_detection() {
    assert_eq!(FileKind::from_path(Path::new("x.jsonl")), FileKind::Jsonl);
    assert_eq!(
        FileKind::from_path(Path::new("x.jsonl.zstd")),
        FileKind::Zstd
    );
    assert_eq!(FileKind::from_path(Path::new("x.db")), FileKind::Sqlite);
}

#[test]
fn sqlite_sources_use_online_backup_and_version_on_shrink() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("src/antigravity-cli");
    let conversations = root.join("conversations");
    fs::create_dir_all(&conversations).unwrap();
    let db = conversations.join("conv.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE gen_metadata(idx integer primary key, data blob, size integer);
             INSERT INTO gen_metadata VALUES(0, x'0102', 2);
             INSERT INTO gen_metadata VALUES(1, x'0304', 2);",
        )
        .unwrap();
    }
    let archive_dir = temp.path().join("archive");
    let mut cfg = test_config(&archive_dir, &temp.path().join("unused"));
    cfg.sources.claude.enabled = false;
    cfg.sources.antigravity_cli.enabled = true;
    cfg.sources.antigravity_cli.paths = vec![root.to_string_lossy().to_string()];

    let first = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert_eq!(first.total_new(), 1);
    let archived = archive_dir.join("hosts/testhost/antigravity-cli/conversations/conv.db");
    assert!(archived.exists());
    let snapshot = rusqlite::Connection::open_with_flags(
        &archived,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let rows: i64 = snapshot
        .query_row("SELECT count(*) FROM gen_metadata", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "snapshot must contain the committed rows");

    // Grow the source with a WAL write, then sync again: the archived snapshot
    // must be replaced (not a torn copy).
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("INSERT INTO gen_metadata VALUES(2, x'0506', 2)", [])
            .unwrap();
    }
    let second = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert!(
        second.total_grown() + second.total_rewritten() >= 1,
        "WAL change should be detected"
    );

    // Fewer rows than the archived snapshot: version the old copy first.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DELETE FROM gen_metadata WHERE idx IN (1,2)", [])
            .unwrap();
    }
    let third = archive::run_sync(&cfg, &SyncOptions::default()).unwrap();
    assert_eq!(third.total_versioned(), 1);
    let versions: Vec<_> = walkdir::WalkDir::new(archive_dir.join("hosts/testhost/.versions"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();
    assert_eq!(versions.len(), 1);
}
