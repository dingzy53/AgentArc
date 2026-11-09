//! Aggregation of usage events into the views used by `stats` and the TUI.

use crate::index::SessionInfo;
use crate::model::{Agent, Cost, Tokens, UsageEvent};
use crate::pricing::Pricing;
use crate::util;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    Day,
    Week,
    Month,
    Agent,
    Model,
    Project,
    Session,
}

impl GroupBy {
    pub fn parse(s: &str) -> Option<GroupBy> {
        match s.trim().to_ascii_lowercase().as_str() {
            "day" => Some(GroupBy::Day),
            "week" => Some(GroupBy::Week),
            "month" => Some(GroupBy::Month),
            "agent" => Some(GroupBy::Agent),
            "model" => Some(GroupBy::Model),
            "project" => Some(GroupBy::Project),
            "session" => Some(GroupBy::Session),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GroupBy::Day => "day",
            GroupBy::Week => "week",
            GroupBy::Month => "month",
            GroupBy::Agent => "agent",
            GroupBy::Model => "model",
            GroupBy::Project => "project",
            GroupBy::Session => "session",
        }
    }

    pub fn from_tui(n: usize) -> GroupBy {
        match n {
            0 => GroupBy::Day,
            1 => GroupBy::Week,
            _ => GroupBy::Month,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    Today,
    Last7,
    Last30,
    ThisMonth,
    ThisYear,
    All,
}

impl Range {
    pub const ALL: [Range; 6] = [
        Range::Today,
        Range::Last7,
        Range::Last30,
        Range::ThisMonth,
        Range::ThisYear,
        Range::All,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Range::Today => "today",
            Range::Last7 => "7d",
            Range::Last30 => "30d",
            Range::ThisMonth => "this month",
            Range::ThisYear => "this year",
            Range::All => "all",
        }
    }

    pub fn next(self) -> Range {
        let idx = Range::ALL.iter().position(|r| *r == self).unwrap_or(0);
        Range::ALL[(idx + 1) % Range::ALL.len()]
    }

    pub fn start_ms(self, tz: &TimeZone, now: i64) -> Option<i64> {
        match self {
            Range::Today => Some(util::today_start_ms(tz, now)),
            Range::Last7 => Some(util::days_ago_ms(7, now)),
            Range::Last30 => Some(util::days_ago_ms(30, now)),
            Range::ThisMonth => Some(util::month_start_ms(tz, now)),
            Range::ThisYear => Some(util::year_start_ms(tz, now)),
            Range::All => None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroupRow {
    pub key: String,
    pub tokens: Tokens,
    pub cost: Cost,
    pub sessions: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatsResult {
    pub group_by: String,
    pub rows: Vec<GroupRow>,
    pub totals: GroupRow,
    pub unpriced_events: u64,
    pub unknown_models: BTreeMap<String, i64>,
}

pub fn compute(
    events: &[UsageEvent],
    sessions: &HashMap<String, SessionInfo>,
    group: GroupBy,
    tz: &TimeZone,
    pricing: &Pricing,
) -> StatsResult {
    let mut rows: BTreeMap<String, (GroupRow, HashSet<String>)> = BTreeMap::new();
    let mut totals = GroupRow::default();
    let mut total_sessions: HashSet<String> = HashSet::new();
    let mut unknown_models: BTreeMap<String, i64> = BTreeMap::new();
    let mut unpriced_events = 0u64;

    for event in events {
        let key = group_key(event, group, tz, sessions, pricing);
        let cost = match pricing.cost_event(event) {
            Some(usd) => Cost {
                usd,
                unknown_events: 0,
            },
            None => {
                unpriced_events += 1;
                *unknown_models.entry(event.model_raw.clone()).or_insert(0) += event.total();
                Cost {
                    usd: 0.0,
                    unknown_events: 1,
                }
            }
        };
        let entry = rows.entry(key).or_insert_with(|| {
            (
                GroupRow {
                    key: String::new(),
                    ..Default::default()
                },
                HashSet::new(),
            )
        });
        entry.0.tokens.add(event);
        entry.0.cost.add(cost);
        entry.1.insert(event.session_key.clone());

        totals.tokens.add(event);
        totals.cost.add(cost);
        total_sessions.insert(event.session_key.clone());
    }

    totals.sessions = total_sessions.len() as u64;
    let mut out: Vec<GroupRow> = rows
        .into_iter()
        .map(|(key, (mut row, set))| {
            row.key = key;
            row.sessions = set.len() as u64;
            row
        })
        .collect();
    match group {
        GroupBy::Day | GroupBy::Week | GroupBy::Month => {
            out.sort_by(|a, b| a.key.cmp(&b.key));
        }
        _ => {
            out.sort_by(|a, b| {
                b.tokens
                    .total
                    .cmp(&a.tokens.total)
                    .then_with(|| a.key.cmp(&b.key))
            });
        }
    }
    StatsResult {
        group_by: group.as_str().to_string(),
        rows: out,
        totals,
        unpriced_events,
        unknown_models,
    }
}

pub fn group_key(
    event: &UsageEvent,
    group: GroupBy,
    tz: &TimeZone,
    sessions: &HashMap<String, SessionInfo>,
    pricing: &Pricing,
) -> String {
    match group {
        GroupBy::Day => util::date_str(event.ts, tz),
        GroupBy::Week => util::week_str(event.ts, tz),
        GroupBy::Month => util::month_str(event.ts, tz),
        GroupBy::Agent => event.agent.display_name().to_string(),
        GroupBy::Model => {
            if event.model_raw.is_empty() {
                "(unknown)".to_string()
            } else {
                pricing.normalize_model(&event.model_raw)
            }
        }
        GroupBy::Project => sessions
            .get(&event.session_key)
            .map(|s| {
                if s.project.is_empty() {
                    "(no project)".to_string()
                } else {
                    s.project.clone()
                }
            })
            .unwrap_or_else(|| "(no project)".to_string()),
        GroupBy::Session => sessions
            .get(&event.session_key)
            .map(|s| {
                let short = if s.session_id.len() > 8 {
                    &s.session_id[s.session_id.len() - 8..]
                } else {
                    s.session_id.as_str()
                };
                format!("{} [{}]", s.display_title(), short)
            })
            .unwrap_or_else(|| event.session_key.clone()),
    }
}

/// Token/cost total per session key.
pub fn session_totals(events: &[UsageEvent], pricing: &Pricing) -> HashMap<String, (Tokens, Cost)> {
    let mut out: HashMap<String, (Tokens, Cost)> = HashMap::new();
    for event in events {
        let e = out.entry(event.session_key.clone()).or_default();
        e.0.add(event);
        let cost = match pricing.cost_event(event) {
            Some(usd) => Cost {
                usd,
                unknown_events: 0,
            },
            None => Cost {
                usd: 0.0,
                unknown_events: 1,
            },
        };
        e.1.add(cost);
    }
    out
}

/// Per-agent totals, sorted by token count.
pub fn agent_totals(events: &[UsageEvent], pricing: &Pricing) -> Vec<(Agent, Tokens, Cost, u64)> {
    let mut map: BTreeMap<Agent, (Tokens, Cost, HashSet<String>)> = BTreeMap::new();
    for e in events {
        let entry = map.entry(e.agent).or_default();
        entry.0.add(e);
        let cost = pricing.cost_event(e).map_or(
            Cost {
                usd: 0.0,
                unknown_events: 1,
            },
            |usd| Cost {
                usd,
                unknown_events: 0,
            },
        );
        entry.1.add(cost);
        entry.2.insert(e.session_key.clone());
    }
    let mut out: Vec<(Agent, Tokens, Cost, u64)> = map
        .into_iter()
        .map(|(a, (t, c, s))| (a, t, c, s.len() as u64))
        .collect();
    out.sort_by_key(|a| std::cmp::Reverse(a.1.total));
    out
}

/// Daily token and cost series for a heatmap (UTC date buckets in `tz`).
pub fn daily_series(
    events: &[UsageEvent],
    tz: &TimeZone,
    pricing: &Pricing,
) -> BTreeMap<String, (i64, f64)> {
    let mut map: BTreeMap<String, (i64, f64)> = BTreeMap::new();
    for e in events {
        let key = util::date_str(e.ts, tz);
        let entry = map.entry(key).or_default();
        entry.0 += e.total();
        entry.1 += pricing.cost_event(e).unwrap_or(0.0);
    }
    map
}

pub fn parse_range_date(s: &str, tz: &TimeZone, end: bool) -> Option<i64> {
    util::parse_date_ms(s, tz, end)
}
