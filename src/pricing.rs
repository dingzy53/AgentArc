//! Pricing: LiteLLM's `model_prices_and_context_window.json` with offline
//! cache, an embedded snapshot, config aliases/overrides and cost calculation.
//!
//! Unknown models are never silently priced at $0: they are reported as
//! unpriced so `doctor` can surface them.

use crate::config::Config;
use crate::model::UsageEvent;
use crate::util;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const EMBEDDED: &str = include_str!("../assets/pricing_snapshot.json");

#[derive(Clone, Debug, Default)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: Option<f64>,
    pub input_above_200k: Option<f64>,
    pub output_above_200k: Option<f64>,
    pub cache_read_above_200k: Option<f64>,
    pub cache_write_above_200k: Option<f64>,
}

impl ModelPrice {
    fn from_json(v: &Value) -> Option<ModelPrice> {
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64());
        let input = f("input_cost_per_token")?;
        let output = f("output_cost_per_token")?;
        Some(ModelPrice {
            input,
            output,
            cache_read: f("cache_read_input_token_cost")
                .or_else(|| f("input_cost_per_token_cache_hit"))
                .unwrap_or(0.0),
            cache_write_5m: f("cache_creation_input_token_cost").unwrap_or(0.0),
            cache_write_1h: f("cache_creation_input_token_cost_above_1hr")
                .or_else(|| f("cache_creation_input_token_cost_above_12hr")),
            input_above_200k: f("input_cost_per_token_above_200k_tokens")
                .or_else(|| f("input_cost_per_token_above_100k_tokens")),
            output_above_200k: f("output_cost_per_token_above_200k_tokens")
                .or_else(|| f("output_cost_per_token_above_100k_tokens")),
            cache_read_above_200k: f("cache_read_input_token_cost_above_200k_tokens")
                .or_else(|| f("cache_read_input_token_cost_above_100k_tokens")),
            cache_write_above_200k: f("cache_creation_input_token_cost_above_200k_tokens")
                .or_else(|| f("cache_creation_input_token_cost_above_100k_tokens")),
        })
    }

    pub fn per_million(&self) -> (f64, f64, f64, f64) {
        (
            self.input * 1e6,
            self.output * 1e6,
            self.cache_read * 1e6,
            self.cache_write_5m * 1e6,
        )
    }
}

#[derive(Clone, Debug)]
pub struct Pricing {
    table: BTreeMap<String, ModelPrice>,
    aliases: BTreeMap<String, String>,
    /// Overrides are stored per token, converted from the config's per-million
    /// values.
    overrides: BTreeMap<String, ModelPrice>,
    pub source: String,
    pub fetched_at: Option<i64>,
    pub cache_path: PathBuf,
    refresh_days: u64,
    /// Resolved price per raw model id. Price lookup is a hot path in the TUI
    /// and in stats (three groupings over every event).
    lookup_cache: RefCell<HashMap<String, Option<(ModelPrice, String)>>>,
}

impl Pricing {
    pub fn load(cfg: &Config, auto_refresh: bool) -> Pricing {
        let cache_path = cfg.pricing_cache_path();
        let mut table = parse_table(EMBEDDED).unwrap_or_default();
        let mut source = "embedded".to_string();
        let mut fetched_at = None;
        if let Ok(text) = std::fs::read_to_string(&cache_path) {
            if let Some(t) = parse_table(&text) {
                if !t.is_empty() {
                    table = t;
                    source = "cache".to_string();
                }
            }
            fetched_at = cache_mtime(&cache_path);
        }
        let mut pricing = Pricing {
            table,
            aliases: cfg.pricing.aliases.clone(),
            overrides: BTreeMap::new(),
            source,
            fetched_at,
            cache_path,
            refresh_days: cfg.pricing.refresh_days,
            lookup_cache: RefCell::new(HashMap::new()),
        };
        pricing.apply_overrides(&cfg.pricing.overrides);
        let offline = std::env::var_os("AGENTARC_OFFLINE").is_some();
        if auto_refresh && !offline && pricing.is_stale() {
            if let Ok(text) = fetch_litellm_timeout(5) {
                if let Some(t) = parse_table(&text) {
                    if !t.is_empty() {
                        pricing.table = t;
                        pricing.source = "updated".to_string();
                        pricing.fetched_at = Some(util::now_ms());
                        let _ = util::atomic_write(&pricing.cache_path, text.as_bytes());
                        pricing.apply_overrides(&cfg.pricing.overrides);
                    }
                }
            }
        }
        pricing
    }

    /// Construct a pricing view from an explicit table (tests and embedded
    /// snapshot use). No cache or network access.
    pub fn from_parts(cfg: &Config, table_json: &str) -> Pricing {
        let table = parse_table(table_json).unwrap_or_default();
        let mut pricing = Pricing {
            table,
            aliases: cfg.pricing.aliases.clone(),
            overrides: BTreeMap::new(),
            source: "test".to_string(),
            fetched_at: None,
            cache_path: cfg.pricing_cache_path(),
            refresh_days: cfg.pricing.refresh_days,
            lookup_cache: RefCell::new(HashMap::new()),
        };
        pricing.apply_overrides(&cfg.pricing.overrides);
        pricing
    }

    fn apply_overrides(&mut self, overrides: &BTreeMap<String, crate::config::PriceOverride>) {
        for (model, o) in overrides {
            // Config values are USD per 1M tokens.
            self.overrides.insert(
                model.to_ascii_lowercase(),
                ModelPrice {
                    input: o.input / 1e6,
                    output: o.output / 1e6,
                    cache_read: o.cache_read / 1e6,
                    cache_write_5m: o.cache_write / 1e6,
                    cache_write_1h: o.cache_write_1h.map(|v| v / 1e6),
                    input_above_200k: None,
                    output_above_200k: None,
                    cache_read_above_200k: None,
                    cache_write_above_200k: None,
                },
            );
        }
    }

    pub fn is_stale(&self) -> bool {
        let Some(mtime) = self.fetched_at else {
            return true;
        };
        if self.refresh_days == 0 {
            return false;
        }
        util::now_ms() - mtime > (self.refresh_days as i64) * 86_400_000
    }

    pub fn model_count(&self) -> usize {
        self.table.len()
    }

    pub fn overrides_len(&self) -> usize {
        self.overrides.len()
    }

    /// Candidates for a raw model id, in lookup order: the id as-is, after
    /// provider/date normalization, and alias targets.
    pub fn candidates(&self, raw: &str) -> Vec<String> {
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: String| {
            if !s.is_empty() && !out.contains(&s) {
                out.push(s);
            }
        };
        push(raw.clone());
        let stripped = strip_model_suffix(&raw);
        push(stripped.clone());
        for base in [&raw, &stripped] {
            if let Some(target) = self.aliases.get(base) {
                push(target.to_ascii_lowercase());
            }
        }
        // Providers use `/`, `@`, `:` and dots for variants.
        if let Some((_provider, rest)) = raw.rsplit_once('/') {
            push(rest.to_string());
            let stripped = strip_model_suffix(rest);
            push(stripped.clone());
            if let Some(target) = self.aliases.get(&stripped) {
                push(target.to_ascii_lowercase());
            }
        }
        out
    }

    pub fn normalize_model(&self, raw: &str) -> String {
        let candidates = self.candidates(raw);
        for cand in &candidates {
            if self.overrides.contains_key(cand) || self.table.contains_key(cand) {
                return cand.clone();
            }
        }
        candidates.last().cloned().unwrap_or_default()
    }

    pub fn price_for(&self, raw: &str) -> Option<(ModelPrice, String)> {
        if let Some(hit) = self.lookup_cache.borrow().get(raw) {
            return hit.clone();
        }
        let resolved = self.resolve_price(raw);
        self.lookup_cache
            .borrow_mut()
            .insert(raw.to_string(), resolved.clone());
        resolved
    }

    fn resolve_price(&self, raw: &str) -> Option<(ModelPrice, String)> {
        let candidates = self.candidates(raw);
        for cand in &candidates {
            if let Some(p) = self.overrides.get(cand) {
                return Some((p.clone(), format!("override:{cand}")));
            }
            if let Some(p) = self.table.get(cand) {
                return Some((p.clone(), format!("pricing:{cand}")));
            }
        }
        // Longest matching prefix (e.g. a dated variant of a known base model).
        for cand in &candidates {
            let mut best: Option<(&String, &ModelPrice)> = None;
            for (key, price) in &self.table {
                if cand == key {
                    return Some((price.clone(), format!("pricing:{key}")));
                }
                let prefix_match = cand.strip_prefix(key.as_str()).is_some_and(|rest| {
                    rest.starts_with('-')
                        || rest.starts_with('.')
                        || rest.starts_with(':')
                        || rest.starts_with('_')
                });
                if prefix_match && best.is_none_or(|(b, _)| key.len() > b.len()) {
                    best = Some((key, price));
                }
            }
            if let Some((key, price)) = best {
                return Some((price.clone(), format!("prefix:{key}")));
            }
        }
        None
    }

    /// Cost of one usage event. Per-message reported cost wins (Pi); otherwise
    /// the pricing table is used. Returns `None` when the model is unpriced.
    pub fn cost_event(&self, e: &UsageEvent) -> Option<f64> {
        if let Some(c) = e.reported_cost_usd {
            if c > 0.0 || e.total() == 0 {
                return Some(c);
            }
        }
        let (price, _why) = self.price_for(&e.model_raw)?;
        let total_input = e.fresh_input + e.cache_read + e.cache_write_5m + e.cache_write_1h;
        let tiered = total_input > 200_000;
        let input_rate = if tiered {
            price.input_above_200k.unwrap_or(price.input)
        } else {
            price.input
        };
        let output_rate = if tiered {
            price.output_above_200k.unwrap_or(price.output)
        } else {
            price.output
        };
        let cache_read_rate = if tiered {
            price.cache_read_above_200k.unwrap_or(price.cache_read)
        } else {
            price.cache_read
        };
        let cache_write_rate = if tiered {
            price.cache_write_above_200k.unwrap_or(price.cache_write_5m)
        } else {
            price.cache_write_5m
        };
        let cache_write_1h_rate = price.cache_write_1h.unwrap_or(cache_write_rate);
        let usd = e.fresh_input as f64 * input_rate
            + e.cache_read as f64 * cache_read_rate
            + e.cache_write_5m as f64 * cache_write_rate
            + e.cache_write_1h as f64 * cache_write_1h_rate
            + e.output as f64 * output_rate;
        Some(usd)
    }

    pub fn find_for_display(&self, model: &str) -> Option<(String, ModelPrice)> {
        self.price_for(model).map(|(p, why)| (why, p))
    }

    pub fn table_keys(&self) -> impl Iterator<Item = &String> {
        self.table.keys()
    }

    pub fn update_from_config(cfg: &Config) -> Result<usize> {
        let text =
            fetch_litellm().context("downloading LiteLLM pricing table (network unavailable?)")?;
        let table = parse_table(&text)
            .context("the downloaded pricing file is not a JSON object of models")?;
        if table.is_empty() {
            bail!("downloaded pricing table is empty");
        }
        let path = cfg.pricing_cache_path();
        util::atomic_write(&path, text.as_bytes())?;
        Ok(table.len())
    }
}

fn cache_mtime(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

pub fn parse_table(text: &str) -> Option<BTreeMap<String, ModelPrice>> {
    let v: Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    let mut out = BTreeMap::new();
    for (key, val) in obj {
        if let Some(price) = ModelPrice::from_json(val) {
            out.insert(key.to_ascii_lowercase(), price);
        }
    }
    // LiteLLM's table mostly uses provider-prefixed ids (azure/..., vertex_ai/...).
    // Add the bare model id as a fallback so logs that only record the model
    // name still resolve. Unprefixed entries win.
    let bases: Vec<(String, ModelPrice)> = out
        .iter()
        .filter_map(|(key, price)| {
            key.rsplit_once('/')
                .map(|(_, base)| (base.to_string(), price.clone()))
        })
        .collect();
    for (base, price) in bases {
        out.entry(base).or_insert(price);
    }
    Some(out)
}

fn strip_model_suffix(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();
    // Remove `@2026-01-01`, `:0`, `-v1:0` style suffixes.
    if let Some((base, _)) = s.rsplit_once('@') {
        s = base.to_string();
    }
    if let Some((base, suffix)) = s.rsplit_once(':') {
        if suffix.chars().all(|c| c.is_ascii_digit()) {
            s = base.to_string();
        }
    }
    let mut parts: Vec<String> = s.split('-').map(|p| p.to_string()).collect();
    loop {
        let n = parts.len();
        if n == 0 {
            break;
        }
        let last = parts[n - 1].as_str();
        let is_num = |v: &str| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit());
        // `-YYYY-MM-DD`
        if n >= 3
            && parts[n - 3].len() == 4
            && is_num(&parts[n - 3])
            && is_num(&parts[n - 2])
            && is_num(last)
        {
            parts.truncate(n - 3);
            continue;
        }
        // `-YYYYMMDD`
        if last.len() == 8 && is_num(last) {
            parts.truncate(n - 1);
            continue;
        }
        if matches!(last, "latest" | "v1" | "v2" | "v3") {
            parts.truncate(n - 1);
            continue;
        }
        break;
    }
    if parts.is_empty() { s } else { parts.join("-") }
}

pub fn fetch_litellm() -> Result<String> {
    fetch_litellm_timeout(30)
}

pub fn fetch_litellm_timeout(secs: u64) -> Result<String> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(secs)))
        .build();
    let agent: ureq::Agent = config.into();
    let mut resp = agent
        .get(LITELLM_URL)
        .header("User-Agent", "agentarc")
        .call()
        .context("HTTP request for LiteLLM pricing failed")?;
    let text = resp
        .body_mut()
        .read_to_string()
        .context("reading LiteLLM pricing response")?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_dates_and_variants() {
        assert_eq!(strip_model_suffix("gpt-5-2026-07-07"), "gpt-5");
        assert_eq!(strip_model_suffix("claude-opus-5-5"), "claude-opus-5-5");
        assert_eq!(strip_model_suffix("gpt-5:0"), "gpt-5");
    }

    #[test]
    fn embedded_pricing_has_known_models() {
        let table = parse_table(EMBEDDED).unwrap();
        assert!(table.contains_key("gpt-5"));
        assert!(table.contains_key("claude-opus-5-5"));
    }
}
