use agentarc::config::{Config, PriceOverride};
use agentarc::export::{ExportOptions, export_markdown};
use agentarc::index::SessionInfo;
use agentarc::model::{Agent, Cost, Message, Role, Tokens, UsageEvent};
use agentarc::pricing::Pricing;

const TABLE: &str = r#"{
  "gpt-5": {
    "input_cost_per_token": 0.000001,
    "output_cost_per_token": 0.00001,
    "cache_read_input_token_cost": 0.0000001,
    "cache_creation_input_token_cost": 0.000002
  },
  "claude-opus-5-5": {
    "input_cost_per_token": 0.000004,
    "output_cost_per_token": 0.00002,
    "cache_read_input_token_cost": 0.0000002,
    "cache_creation_input_token_cost": 0.000005,
    "cache_creation_input_token_cost_above_1hr": 0.000008
  }
}"#;

fn event(model: &str) -> UsageEvent {
    UsageEvent {
        dedup_key: "x".into(),
        agent: Agent::Pi,
        host: "test".into(),
        session_key: "s".into(),
        ts: 0,
        model_raw: model.into(),
        provider: None,
        fresh_input: 1_000_000,
        cache_read: 1_000_000,
        cache_write_5m: 1_000_000,
        cache_write_1h: 0,
        output: 1_000_000,
        reasoning: 0,
        reported_cost_usd: None,
    }
}

#[test]
fn prices_known_models_through_provider_and_date_suffixes() {
    let cfg = Config::default();
    let pricing = Pricing::from_parts(&cfg, TABLE);
    let cost = pricing
        .cost_event(&event("openai/gpt-5-2026-07-07"))
        .expect("known model");
    assert!((cost - 13.1).abs() < 1e-9, "cost={cost}");
    assert!(
        pricing
            .cost_event(&event("totally-unknown-model"))
            .is_none()
    );
}

#[test]
fn aliases_and_overrides_are_applied() {
    let mut cfg = Config::default();
    cfg.pricing
        .aliases
        .insert("friendly-alias".into(), "gpt-5".into());
    cfg.pricing.overrides.insert(
        "private-model".into(),
        PriceOverride {
            input: 2.0,
            output: 4.0,
            cache_read: 0.5,
            cache_write: 1.0,
            cache_write_1h: None,
        },
    );
    let pricing = Pricing::from_parts(&cfg, TABLE);
    let cost = pricing
        .cost_event(&event("friendly-alias"))
        .expect("alias resolved");
    assert!((cost - 13.1).abs() < 1e-9, "alias cost={cost}");
    let cost = pricing
        .cost_event(&event("private-model"))
        .expect("override resolved");
    // per 1M: 2 input + 4 output + 0.5 cache read + 1 cache write = 7.5
    assert!((cost - 7.5).abs() < 1e-9, "override cost={cost}");
}

#[test]
fn reported_cost_wins_and_zero_falls_back() {
    let cfg = Config::default();
    let pricing = Pricing::from_parts(&cfg, TABLE);
    let mut e = event("gpt-5");
    e.reported_cost_usd = Some(0.5);
    assert_eq!(pricing.cost_event(&e), Some(0.5));
    e.reported_cost_usd = Some(0.0);
    assert!(pricing.cost_event(&e).unwrap() > 1.0);
}

#[test]
fn markdown_export_hides_thinking_by_default() {
    let session = SessionInfo {
        key: "test:pi:s".into(),
        agent: Agent::Pi,
        host: "test".into(),
        source_path: String::new(),
        archive_path: String::new(),
        session_id: "s".into(),
        title: Some("Test session".into()),
        cwd: Some("/home/tester/proj".into()),
        project: "~/proj".into(),
        started_at: 1_770_000_000_000,
        ended_at: 1_770_000_100_000,
        models: vec!["mimo-v2.5-pro".into()],
        parent_session: None,
        is_subagent: false,
        parse_warnings: Vec::new(),
        reported_cost_usd: Some(0.02),
    };
    let messages = vec![
        Message {
            ts: 1_770_000_000_000,
            role: Role::User,
            text: "hello".into(),
            ..Default::default()
        },
        Message {
            ts: 1_770_000_001_000,
            role: Role::Assistant,
            text: "hi".into(),
            thinking: Some("secret thoughts".into()),
            ..Default::default()
        },
        Message {
            ts: 1_770_000_002_000,
            role: Role::Tool,
            text: "hidden output".into(),
            tool_name: Some("bash".into()),
            tool_args_short: Some("{\"command\":\"id\"}".into()),
            ..Default::default()
        },
    ];
    let md = export_markdown(
        &session,
        &messages,
        Tokens {
            fresh_input: 10,
            cache_read: 20,
            cache_write: 5,
            output: 3,
            reasoning: 1,
            total: 38,
        },
        Cost {
            usd: 0.01,
            unknown_events: 0,
        },
        ExportOptions::default(),
        &jiff::tz::TimeZone::UTC,
    );
    assert!(md.contains("# Test session"));
    assert!(md.contains("## User"));
    assert!(md.contains("## Assistant"));
    assert!(!md.contains("secret thoughts"));
    assert!(md.contains("⚙ `bash`"));
    assert!(!md.contains("hidden output"));
    assert!(md.contains("reported cost"));

    let md = export_markdown(
        &session,
        &messages,
        Tokens::default(),
        Cost::default(),
        ExportOptions {
            with_thinking: true,
            with_tool_output: true,
        },
        &jiff::tz::TimeZone::UTC,
    );
    assert!(md.contains("secret thoughts"));
    assert!(md.contains("hidden output"));
}
