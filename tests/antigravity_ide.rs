use agentarc::adapters::antigravity;
use agentarc::adapters::{ParseContext, parse_file};
use agentarc::model::Agent;
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

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
fn fv(n: u32, v: u64) -> Vec<u8> {
    let mut o = varint((n as u64) << 3);
    o.extend(varint(v));
    o
}
fn fb(n: u32, b: &[u8]) -> Vec<u8> {
    let mut o = varint(((n as u64) << 3) | 2);
    o.extend(varint(b.len() as u64));
    o.extend_from_slice(b);
    o
}
fn kv(k: &str, v: &str) -> Vec<u8> {
    let mut o = fb(1, k.as_bytes());
    o.extend(fb(2, v.as_bytes()));
    o
}

fn usage_blob() -> Vec<u8> {
    let mut stats = fv(1, 1318);
    stats.extend(fv(2, 1000));
    stats.extend(fv(3, 200));
    stats.extend(fv(5, 300));
    stats.extend(fv(9, 50));
    stats.extend(fv(10, 150));
    let mut msg = fb(4, &stats);
    msg.extend(fb(19, b"gemini-3.8-flash-n"));
    msg.extend(fb(20, &kv("request_id", "req-pb-1")));
    fb(1, &msg)
}

#[test]
fn plain_pb_is_parsed_and_encrypted_pb_is_archived_only() {
    let temp = tempdir().unwrap();
    let host = "testhost";
    let ctx = ParseContext {
        host,
        archive_rel: "hosts/testhost/antigravity-ide/antigravity/conversations/x.pb",
        aliases: &BTreeMap::new(),
    };

    let plain = temp.path().join("plain.pb");
    fs::write(&plain, usage_blob()).unwrap();
    let parsed = parse_file(Agent::AntigravityIde, &plain, &ctx).unwrap();
    assert_eq!(parsed.usage.len(), 1);
    assert_eq!(parsed.usage[0].fresh_input, 1000);
    assert_eq!(parsed.usage[0].output, 200);
    assert_eq!(parsed.usage[0].cache_read, 300);
    assert_eq!(parsed.usage[0].dedup_key, "ag:req-pb-1");
    assert!(
        !parsed.sessions[0]
            .parse_warnings
            .iter()
            .any(|w| w.contains("encrypted"))
    );

    // Deterministic high-entropy blob: does not decode as protobuf.
    let mut encrypted = Vec::new();
    let mut x: u64 = 0x1234_5678_9abc_def0;
    for _ in 0..4096 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        encrypted.push((x & 0xff) as u8);
    }
    let enc = temp.path().join("encrypted.pb");
    fs::write(&enc, encrypted).unwrap();
    let parsed = parse_file(Agent::AntigravityIde, &enc, &ctx).unwrap();
    assert!(parsed.usage.is_empty());
    assert_eq!(parsed.sessions.len(), 1);
    assert!(
        parsed.sessions[0]
            .parse_warnings
            .iter()
            .any(|w| w.contains("pb-encrypted"))
    );
}

#[test]
fn summaries_proto_and_db_are_decoded() {
    let temp = tempdir().unwrap();
    let mut inner = fb(1, b"Summary title");
    inner.extend(fv(2, 12));
    let mut time = fv(1, 1770000000);
    time.extend(fv(2, 123));
    inner.extend(fb(3, &time));
    inner.extend(fb(9, &fb(1, b"file:///home/tester/proj%20x")));
    let mut entry = fb(1, b"conv-summary-1");
    entry.extend(fb(2, &inner));
    let path = temp.path().join("summary_summaries_proto.pb");
    fs::write(&path, fb(1, &entry)).unwrap();

    let metas = antigravity::parse_meta_file(Agent::AntigravityIde, &path, "testhost").unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].conversation_id, "conv-summary-1");
    assert_eq!(metas[0].title.as_deref(), Some("Summary title"));
    assert_eq!(metas[0].step_count, 12);
    assert_eq!(metas[0].end_ms, 1_770_000_000_000);
    assert_eq!(metas[0].cwd.as_deref(), Some("/home/tester/proj x"));

    let db = temp.path().join("conversation_summaries.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE conversation_summaries(
         conversation_id text, title text, step_count integer, last_modified_time datetime,
         workspace_uris text, last_user_input_time datetime, parent_conversation_id text);
         INSERT INTO conversation_summaries VALUES(
         'conv-db-1','DB title',5,'2026-04-05 06:07:08+00:00',
         '[\"file:///home/tester/My%20Project\"]','2026-04-05 06:00:00+00:00','');",
    )
    .unwrap();
    let metas = antigravity::parse_meta_file(Agent::AntigravityCli, &db, "testhost").unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].title.as_deref(), Some("DB title"));
    assert_eq!(metas[0].cwd.as_deref(), Some("/home/tester/My Project"));
    assert_eq!(metas[0].layout, "db");
}

#[test]
fn transcript_is_readable() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("transcript_full.jsonl");
    fs::write(
        &path,
        "{\"step_index\":0,\"type\":\"USER_INPUT\",\"created_at\":\"2026-04-05T06:00:00Z\",\"content\":\"hello\"}\n\
         {\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-04-05T06:00:05Z\",\"content\":\"world\"}\n",
    )
    .unwrap();
    let messages = antigravity::read_transcript(&path).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text, "hello");
    assert_eq!(messages[1].text, "world");
}
