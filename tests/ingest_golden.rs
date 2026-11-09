use agentarc::config::Config;
use agentarc::index::{Filters, Index, IngestOptions};
use agentarc::model::Agent;
use rusqlite::{Connection, params};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn field_varint(number: u32, value: u64) -> Vec<u8> {
    let mut out = varint((number as u64) << 3);
    out.extend(varint(value));
    out
}

fn field_bytes(number: u32, value: &[u8]) -> Vec<u8> {
    let mut out = varint(((number as u64) << 3) | 2);
    out.extend(varint(value.len() as u64));
    out.extend_from_slice(value);
    out
}

fn varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            return out;
        }
    }
}

fn kv(key: &str, value: &str) -> Vec<u8> {
    let mut out = field_bytes(1, key.as_bytes());
    out.extend(field_bytes(2, value.as_bytes()));
    out
}

fn antigravity_blob(request_id: &str) -> Vec<u8> {
    // ModelUsageStats: enum=1318, input=100, output=30, cache write=5,
    // cache read=50, thinking=20, response=10, response id.
    let mut stats = field_varint(1, 1318);
    stats.extend(field_varint(2, 100));
    stats.extend(field_varint(3, 30));
    stats.extend(field_varint(4, 5));
    stats.extend(field_varint(5, 50));
    stats.extend(field_varint(9, 20));
    stats.extend(field_varint(10, 10));
    stats.extend(field_bytes(11, b"resp-ag-1"));

    let mut msg = field_bytes(4, &stats);
    msg.extend(field_bytes(19, b"gemini-3.8-flash-n"));
    msg.extend(field_bytes(20, &kv("request_id", request_id)));
    msg.extend(field_bytes(20, &kv("cascade_id", "conv-1")));

    field_bytes(1, &msg)
}

fn make_antigravity_db(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0, PRIMARY KEY(idx));",
    )
    .unwrap();
    let blob = antigravity_blob("req-ag-1");
    conn.execute(
        "INSERT INTO gen_metadata(idx,data,size) VALUES(0,?1,?2)",
        params![blob, blob.len() as i64],
    )
    .unwrap();
}

fn make_summaries_db(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE conversation_summaries(
            conversation_id text, title text NOT NULL DEFAULT '', preview text NOT NULL DEFAULT '',
            step_count integer NOT NULL DEFAULT 0, last_modified_time datetime NOT NULL,
            workspace_uris text NOT NULL, last_user_input_time datetime NOT NULL,
            parent_conversation_id text NOT NULL DEFAULT '');",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversation_summaries(conversation_id,title,step_count,last_modified_time,workspace_uris,last_user_input_time,parent_conversation_id)
         VALUES('conv-1','Antigravity fixture',3,'2026-04-05 06:07:08+00:00','[\"file:///home/tester/proj\"]','2026-04-05 06:00:00+00:00','')",
        [],
    )
    .unwrap();
}

fn test_config(archive: &Path) -> Config {
    let mut cfg = Config {
        archive_dir: archive.to_string_lossy().to_string(),
        host_name: "testhost".to_string(),
        sync_on_start: false,
        ..Default::default()
    };
    cfg.projects
        .aliases
        .insert("~/proj".to_string(), "fixture-project".to_string());
    cfg
}

#[test]
fn indexes_every_adapter_with_global_dedup() {
    let temp = tempdir().unwrap();
    let archive = temp.path().join("archive");
    let host = archive.join("hosts/testhost");
    fs::create_dir_all(host.join("claude/projects/p1")).unwrap();
    fs::copy(
        "tests/fixtures/claude/basic.jsonl",
        host.join("claude/projects/p1/s1.jsonl"),
    )
    .unwrap();
    // A subagent transcript stays a separate, tagged session.
    fs::create_dir_all(host.join("claude/projects/p1/s1/subagents")).unwrap();
    let sub = fs::read_to_string("tests/fixtures/claude/basic.jsonl")
        .unwrap()
        .replace("sess-1", "sess-sub")
        .replace("msg-1", "msg-sub")
        .replace("req-1", "req-sub");
    fs::write(
        host.join("claude/projects/p1/s1/subagents/agent-abc.jsonl"),
        sub,
    )
    .unwrap();

    fs::create_dir_all(host.join("pi/sessions/--x--")).unwrap();
    fs::copy(
        "tests/fixtures/pi/basic.jsonl",
        host.join("pi/sessions/--x--/a.jsonl"),
    )
    .unwrap();
    fs::copy(
        "tests/fixtures/pi/duplicate.jsonl",
        host.join("pi/sessions/--x--/b.jsonl"),
    )
    .unwrap();

    fs::create_dir_all(host.join("codex/sessions/2026/03/04")).unwrap();
    fs::copy(
        "tests/fixtures/codex/basic.jsonl",
        host.join("codex/sessions/2026/03/04/rollout-x.jsonl"),
    )
    .unwrap();

    let dsh_bytes = fs::read("tests/fixtures/dsh/basic.jsonl").unwrap();
    let split = dsh_bytes.len() / 2;
    let mut frames = zstd::stream::encode_all(&dsh_bytes[..split], 3).unwrap();
    frames.extend(zstd::stream::encode_all(&dsh_bytes[split..], 3).unwrap());
    frames.extend_from_slice(&[0, 0, 0, 0]); // torn tail
    let dsh_path = host.join("dsh/sessions/--x--/session-fixture/session.v4.jsonl.zstd");
    fs::create_dir_all(dsh_path.parent().unwrap()).unwrap();
    fs::write(&dsh_path, frames).unwrap();

    let ag_db = host.join("antigravity-cli/conversations/conv-1.db");
    make_antigravity_db(&ag_db);
    make_summaries_db(&host.join("antigravity-cli/conversation_summaries.db"));

    let cfg = test_config(&archive);
    let index_path = temp.path().join("index.sqlite");
    let mut index = Index::open(&index_path).unwrap();
    let stats = index.ingest(&cfg, &IngestOptions::default()).unwrap();
    assert!(stats.errors.is_empty(), "errors: {:?}", stats.errors);

    let (files, sessions, usage_rows, usage_unique) = index.counts().unwrap();
    assert!(files >= 6, "files={files}");
    assert_eq!(
        sessions, 7,
        "claude main+subagent, 2 pi, codex, dsh, antigravity"
    );
    assert_eq!(
        usage_unique, 9,
        "4 claude + 1 pi + 2 codex + 1 dsh + 1 antigravity"
    );
    assert!(usage_rows >= usage_unique);

    let events = index.load_events(&Filters::default()).unwrap();
    assert_eq!(events.len(), usage_unique as usize);

    let claude: Vec<_> = events.iter().filter(|e| e.agent == Agent::Claude).collect();
    assert_eq!(
        claude.len(),
        4,
        "two iterations × main and subagent file, deduped"
    );
    let fresh: i64 = claude.iter().map(|e| e.fresh_input).sum();
    let cache_read: i64 = claude.iter().map(|e| e.cache_read).sum();
    let cache_write: i64 = claude.iter().map(|e| e.cache_write()).sum();
    let output: i64 = claude.iter().map(|e| e.output).sum();
    assert_eq!(fresh, 220);
    assert_eq!(cache_read, 410);
    assert_eq!(cache_write, 100);
    assert_eq!(output, 64);
    assert!(
        claude.iter().any(|e| e.model_raw == "claude-haiku-5-5"),
        "advisor iteration must be counted with its own model"
    );

    let pi: Vec<_> = events.iter().filter(|e| e.agent == Agent::Pi).collect();
    assert_eq!(pi.len(), 1, "responseId dedups the forked copy");
    assert_eq!(pi[0].fresh_input, 100);
    assert_eq!(pi[0].cache_read, 300);
    assert_eq!(pi[0].cache_write_5m, 5);
    assert_eq!(pi[0].output, 20);
    assert_eq!(pi[0].reasoning, 10);
    assert_eq!(pi[0].reported_cost_usd, Some(0.01));

    let codex: Vec<_> = events.iter().filter(|e| e.agent == Agent::Codex).collect();
    assert_eq!(
        codex.len(),
        2,
        "repeated cumulative totals are not double-counted"
    );
    assert_eq!(codex.iter().map(|e| e.fresh_input).sum::<i64>(), 300);
    assert_eq!(codex.iter().map(|e| e.cache_read).sum::<i64>(), 1200);
    assert_eq!(codex.iter().map(|e| e.output).sum::<i64>(), 300);

    let dsh: Vec<_> = events.iter().filter(|e| e.agent == Agent::Dsh).collect();
    assert_eq!(dsh.len(), 1);
    assert_eq!(dsh[0].fresh_input, 100);
    assert_eq!(dsh[0].cache_read, 300);
    assert_eq!(
        dsh[0].output, 20,
        "usage inside data.stream must be ignored"
    );

    let ag: Vec<_> = events
        .iter()
        .filter(|e| e.agent == Agent::AntigravityCli)
        .collect();
    assert_eq!(ag.len(), 1);
    assert_eq!(ag[0].fresh_input, 100);
    assert_eq!(ag[0].cache_read, 50);
    assert_eq!(ag[0].cache_write(), 5);
    assert_eq!(ag[0].output, 30);
    assert_eq!(ag[0].dedup_key, "ag:req-ag-1");

    let sessions = index.load_sessions(&Filters::default()).unwrap();
    let ag_session = sessions
        .iter()
        .find(|s| s.agent == Agent::AntigravityCli)
        .unwrap();
    assert_eq!(ag_session.title.as_deref(), Some("Antigravity fixture"));
    assert_eq!(ag_session.project, "fixture-project");
    let claude_sessions: Vec<_> = sessions
        .iter()
        .filter(|s| s.agent == Agent::Claude)
        .collect();
    assert_eq!(claude_sessions.len(), 2);
    assert!(claude_sessions.iter().any(|s| s.is_subagent));
    assert!(
        claude_sessions
            .iter()
            .any(|s| s.reported_cost_usd == Some(1.23))
    );

    // Incremental: an unchanged re-ingest parses nothing.
    let again = index.ingest(&cfg, &IngestOptions::default()).unwrap();
    assert_eq!(again.files_parsed, 0);
    assert!(again.files_skipped >= 6);

    // Append one assistant response and re-ingest: counts change.
    let mut claude_text = fs::read_to_string(host.join("claude/projects/p1/s1.jsonl")).unwrap();
    claude_text.push_str("{\"type\":\"assistant\",\"sessionId\":\"sess-1\",\"cwd\":\"/home/tester/proj\",\"timestamp\":\"2026-01-02T03:05:00.000Z\",\"requestId\":\"req-2\",\"message\":{\"id\":\"msg-2\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":7,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":3}}}\n");
    fs::write(host.join("claude/projects/p1/s1.jsonl"), claude_text).unwrap();
    index.ingest(&cfg, &IngestOptions::default()).unwrap();
    let events = index.load_events(&Filters::default()).unwrap();
    let claude: Vec<_> = events.iter().filter(|e| e.agent == Agent::Claude).collect();
    assert_eq!(claude.len(), 5);
}

#[test]
fn dedup_survives_reparsing_one_of_two_files() {
    let temp = tempdir().unwrap();
    let host = temp.path().join("archive/hosts/testhost");
    fs::create_dir_all(host.join("pi/sessions/--x--")).unwrap();
    fs::copy(
        "tests/fixtures/pi/basic.jsonl",
        host.join("pi/sessions/--x--/a.jsonl"),
    )
    .unwrap();
    fs::copy(
        "tests/fixtures/pi/duplicate.jsonl",
        host.join("pi/sessions/--x--/b.jsonl"),
    )
    .unwrap();
    let cfg = test_config(&temp.path().join("archive"));
    let mut index = Index::open(&temp.path().join("index.sqlite")).unwrap();
    index.ingest(&cfg, &IngestOptions::default()).unwrap();
    assert_eq!(
        index
            .load_events(&Filters::default())
            .unwrap()
            .iter()
            .filter(|e| e.agent == Agent::Pi)
            .count(),
        1
    );
    // Force a re-parse of the first file. The duplicate in file b must keep the
    // event alive (rows are stored per file and deduped at query time).
    fs::write(host.join("pi/sessions/--x--/a.jsonl"), "{}\n").unwrap();
    index
        .ingest(
            &cfg,
            &IngestOptions {
                force: false,
                agents: None,
                hosts: None,
            },
        )
        .unwrap();
    let events = index.load_events(&Filters::default()).unwrap();
    assert_eq!(
        events.iter().filter(|e| e.agent == Agent::Pi).count(),
        1,
        "the other copy still provides the responseId"
    );
}
