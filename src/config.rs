//! Configuration: load/init `~/.config/agentarc/config.toml` and resolve source
//! roots, archive path, host name and time zone.

use crate::model::Agent;
use crate::util;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub archive_dir: String,
    /// Must be explicitly enabled before the archive may live in a cloud-sync
    /// folder (OneDrive, Dropbox, CloudStorage, Syncthing).
    pub allow_cloud_archive: bool,
    pub host_name: String,
    pub sync_on_start: bool,
    pub timezone: String,
    pub sources: Sources,
    pub pricing: PricingConfig,
    pub projects: ProjectsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            archive_dir: "~/AgentArchive".to_string(),
            allow_cloud_archive: false,
            host_name: String::new(),
            sync_on_start: true,
            timezone: String::new(),
            sources: Sources::default(),
            pricing: PricingConfig::default(),
            projects: ProjectsConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub claude: SourceConfig,
    pub codex: SourceConfig,
    pub pi: SourceConfig,
    pub dsh: SourceConfig,
    pub antigravity_cli: SourceConfig,
    pub antigravity_ide: SourceConfig,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            claude: SourceConfig {
                enabled: true,
                paths: vec!["~/.claude/projects".into()],
            },
            codex: SourceConfig {
                enabled: true,
                paths: vec![
                    "~/.codex/sessions".into(),
                    "~/.codex/archived_sessions".into(),
                ],
            },
            pi: SourceConfig {
                enabled: true,
                paths: vec!["~/.pi/agent/sessions".into()],
            },
            dsh: SourceConfig {
                enabled: true,
                paths: vec!["~/.dsh/sessions".into()],
            },
            antigravity_cli: SourceConfig {
                enabled: true,
                paths: vec!["~/.gemini/antigravity-cli".into()],
            },
            antigravity_ide: SourceConfig {
                enabled: true,
                paths: vec![
                    "~/.gemini/antigravity".into(),
                    "~/.gemini/antigravity-ide".into(),
                ],
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceConfig {
    pub enabled: bool,
    pub paths: Vec<String>,
}

impl Default for SourceConfig {
    /// A source table that only specifies `paths` stays enabled.
    fn default() -> Self {
        Self {
            enabled: true,
            paths: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PricingConfig {
    pub refresh_days: u64,
    /// Model id (normalized) -> public base model id.
    pub aliases: BTreeMap<String, String>,
    /// Model id -> USD per 1M tokens.
    pub overrides: BTreeMap<String, PriceOverride>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        let mut aliases = BTreeMap::new();
        aliases.insert("gemini-3.8-flash-n".into(), "gemini-3.8-flash".into());
        aliases.insert("gemini-4-argon-trial".into(), "gemini-4-pro".into());
        aliases.insert("deepseek-flash".into(), "deepseek-v4-flash".into());
        aliases.insert(
            "deepseek-v4.1-flash-expires-on-0910".into(),
            "deepseek-v4.1-flash".into(),
        );
        aliases.insert("claude-opus-5-5".into(), "claude-opus-5.5".into());
        let mut overrides = BTreeMap::new();
        // Locally used alpha model that is not in LiteLLM; "free" is part of
        // its name. Override rather than silently pricing it at $0.
        overrides.insert(
            "ox-alpha-free".to_string(),
            PriceOverride {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                cache_write_1h: None,
            },
        );
        Self {
            refresh_days: 7,
            aliases,
            overrides,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PriceOverride {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cache_write_1h: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectsConfig {
    pub aliases: BTreeMap<String, String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = util::config_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading config {}", path.display())),
        }
    }

    pub fn archive_dir(&self) -> PathBuf {
        if let Ok(v) = std::env::var("AGENTARC_ARCHIVE") {
            util::expand_tilde(&v)
        } else {
            util::expand_tilde(&self.archive_dir)
        }
    }

    pub fn host_name(&self) -> String {
        let h = self.host_name.trim();
        if h.is_empty() {
            util::hostname()
        } else {
            h.to_string()
        }
    }

    pub fn timezone(&self) -> jiff::tz::TimeZone {
        util::resolve_timezone(&self.timezone)
    }

    pub fn index_path(&self) -> PathBuf {
        util::cache_dir().join("index.sqlite")
    }

    pub fn pricing_cache_path(&self) -> PathBuf {
        util::cache_dir().join("pricing.json")
    }

    pub fn log_path(&self) -> PathBuf {
        util::cache_dir().join("agentarc.log")
    }

    pub fn source_config_mut(&mut self, agent: Agent) -> &mut SourceConfig {
        match agent {
            Agent::Claude => &mut self.sources.claude,
            Agent::Codex => &mut self.sources.codex,
            Agent::Pi => &mut self.sources.pi,
            Agent::Dsh => &mut self.sources.dsh,
            Agent::AntigravityCli => &mut self.sources.antigravity_cli,
            Agent::AntigravityIde => &mut self.sources.antigravity_ide,
        }
    }

    pub fn source_config(&self, agent: Agent) -> &SourceConfig {
        match agent {
            Agent::Claude => &self.sources.claude,
            Agent::Codex => &self.sources.codex,
            Agent::Pi => &self.sources.pi,
            Agent::Dsh => &self.sources.dsh,
            Agent::AntigravityCli => &self.sources.antigravity_cli,
            Agent::AntigravityIde => &self.sources.antigravity_ide,
        }
    }

    /// Resolve configured paths, applying the agents' own environment overrides.
    pub fn source_roots(&self, agent: Agent) -> Vec<PathBuf> {
        let cfg = self.source_config(agent);
        if !cfg.enabled {
            return Vec::new();
        }
        let mut paths: Vec<PathBuf> = cfg.paths.iter().map(|p| util::expand_tilde(p)).collect();
        match agent {
            Agent::Claude => {
                if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
                    paths = vec![PathBuf::from(dir).join("projects")];
                }
            }
            Agent::Codex => {
                if let Some(dir) = std::env::var_os("CODEX_HOME") {
                    let home = PathBuf::from(dir);
                    paths = vec![home.join("sessions"), home.join("archived_sessions")];
                }
            }
            _ => {}
        }
        paths
    }

    pub fn codex_home(&self) -> PathBuf {
        if let Some(dir) = std::env::var_os("CODEX_HOME") {
            PathBuf::from(dir)
        } else {
            util::home_dir().join(".codex")
        }
    }

    pub fn enabled_agents(&self) -> Vec<Agent> {
        Agent::ALL
            .into_iter()
            .filter(|a| self.source_config(*a).enabled)
            .collect()
    }

    /// Returns a human-readable reason when the archive lives in a known
    /// cloud-sync folder and `allow_cloud_archive` is not set.
    pub fn cloud_archive_reason(&self) -> Option<String> {
        let dir = self.archive_dir();
        for component in dir.components() {
            let name = component.as_os_str().to_string_lossy();
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("onedrive") {
                return Some(format!("archive_dir is inside {name}"));
            }
            if lower == "dropbox" || lower.starts_with("dropbox ") {
                return Some(format!("archive_dir is inside {name}"));
            }
            if lower == "cloudstorage" {
                return Some("archive_dir is inside ~/Library/CloudStorage".to_string());
            }
        }
        let mut cur = Some(dir.as_path());
        while let Some(p) = cur {
            if p.join(".stfolder").exists() {
                return Some(format!(
                    "archive_dir is inside Syncthing folder {}",
                    p.display()
                ));
            }
            cur = p.parent();
        }
        None
    }

    pub fn ensure_archive_safe(&self) -> Result<()> {
        if let Some(reason) = self.cloud_archive_reason() {
            if !self.allow_cloud_archive {
                bail!(
                    "{reason}; set allow_cloud_archive = true in the config once you are ready to sync"
                );
            }
        }
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String> {
        let body = toml::to_string_pretty(self).context("serializing default config")?;
        Ok(format!(
            "# agentarc configuration\n\
             # archive_dir may be overridden with the AGENTARC_ARCHIVE environment variable.\n\
             # Cloud-sync folders require allow_cloud_archive = true.\n\n{body}"
        ))
    }

    /// Write the current config to the config path (overwriting it).
    pub fn save(&self) -> Result<PathBuf> {
        let path = util::config_path();
        let text = self.to_toml()?;
        util::atomic_write(&path, text.as_bytes())?;
        Ok(path)
    }

    /// Create the config file if it does not exist. Returns the path.
    pub fn init_file(&self) -> Result<PathBuf> {
        let path = util::config_path();
        if path.exists() {
            return Ok(path);
        }
        let text = self.to_toml()?;
        util::atomic_write(&path, text.as_bytes())?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_roundtrips_toml() {
        let cfg = Config::default();
        let text = cfg.to_toml().unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.archive_dir, cfg.archive_dir);
        assert_eq!(parsed.pricing.aliases, cfg.pricing.aliases);
        assert!(parsed.sources.claude.enabled);
    }
}
