# Configuration

The config file is TOML at:

- Linux/macOS: `~/.config/agentarc/config.toml`
- override with `AGENTARC_CONFIG=/path/to/config.toml`

`agentarc config init` writes the default file, `agentarc config edit` opens it
in `$EDITOR`, and `agentarc config path` prints the path. The TUI also has a
configuration page (`c`) which opens automatically on first run when the config
file does not exist yet.

## Full example

```toml
archive_dir   = "~/AgentArchive"
allow_cloud_archive = false
host_name     = ""            # default: short hostname
sync_on_start = true
timezone      = ""            # default: system tz, e.g. "Asia/Kuala_Lumpur"

[sources.claude]
enabled = true
paths   = ["~/.claude/projects"]          # $CLAUDE_CONFIG_DIR respected

[sources.codex]
enabled = true
paths   = ["~/.codex/sessions", "~/.codex/archived_sessions"]   # $CODEX_HOME respected

[sources.pi]
enabled = true
paths   = ["~/.pi/agent/sessions"]        # add legacy/fork layouts here if needed

[sources.dsh]
enabled = true
paths   = ["~/.dsh/sessions"]

[sources.antigravity_cli]
enabled = true
paths   = ["~/.gemini/antigravity-cli"]

[sources.antigravity_ide]
enabled = true
paths   = ["~/.gemini/antigravity", "~/.gemini/antigravity-ide"]

[pricing]
refresh_days = 7

[pricing.aliases]
"gemini-3.8-flash-n"   = "gemini-3.8-flash"
"gemini-4-argon-trial" = "gemini-4-pro"
"deepseek-flash"       = "deepseek-v4-flash"
"deepseek-v4.1-flash-expires-on-0910" = "deepseek-v4.1-flash"
"claude-opus-5-5"      = "claude-opus-5.5"

# USD per 1M tokens. Add entries for private/internal models that are not in
# the LiteLLM table. Unknown models are reported in `doctor`, never priced at $0.
[pricing.overrides."some-model"]
input = 0.0
output = 0.0
cache_read = 0.0
cache_write = 0.0
# cache_write_1h = 0.0       # optional, if the model has a 1h cache write price

[projects.aliases]
"~/Obsidian" = "obsidian-vault"
```

## Fields

### General

| key | default | meaning |
|---|---|---|
| `archive_dir` | `~/AgentArchive` | archive root; every machine writes under `hosts/<host>/` |
| `allow_cloud_archive` | `false` | required if `archive_dir` is inside OneDrive, Dropbox, `~/Library/CloudStorage` or a Syncthing folder |
| `host_name` | short hostname | name of this machine's subtree; keep it stable and unique per machine |
| `sync_on_start` | `true` | run sync + index in the background when the TUI starts |
| `timezone` | system zone | IANA name used for day/week/month buckets and `--since/--until` |

If `archive_dir` is inside a known cloud-sync folder and
`allow_cloud_archive = false`, sync refuses to run before copying anything. The
index is always local (`~/.cache/agentarc/index.sqlite`) and is never put in the
synced folder.

### Sources

Each `[sources.<agent>]` section has:

```toml
enabled = true
paths   = ["~/path/to/root"]
```

The defaults are shown in the full example. A disabled source is not discovered,
but **already archived files for that agent are still indexed and counted**.
This is intentional for agents you stopped using.

Environment overrides:

- `CLAUDE_CONFIG_DIR` replaces the Claude root with `$CLAUDE_CONFIG_DIR/projects`
- `CODEX_HOME` replaces the Codex roots with `$CODEX_HOME/sessions` and
  `$CODEX_HOME/archived_sessions`, and `$CODEX_HOME/state_*.sqlite` for titles

### Pricing

- `pricing.refresh_days` — auto-refresh age. `0` disables automatic refresh.
- `pricing.aliases` — map a raw/internal model id to a public base model id.
- `pricing.overrides` — explicit USD per **1 million** tokens for a model:
  `input`, `output`, `cache_read`, `cache_write`, optional `cache_write_1h`.

Pricing source order: cache at `~/.cache/agentarc/pricing.json` → embedded
snapshot → downloaded refresh. `agentarc pricing update` refreshes explicitly;
`agentarc pricing show <model>` shows the resolved price and match.

### Projects

`projects.aliases` maps path prefixes to display names after home-directory
normalization. Example: `~/Obsidian` → `obsidian-vault`, so
`~/Obsidian/Math` becomes `obsidian-vault/Math` on all hosts.

## TUI configuration page

- Opens automatically on first run when no config file exists.
- Reopen with `c`; `Esc`/`c` returns without saving.
- Editable fields: archive folder, host name, timezone, sync on start,
  allow-cloud-archive, per-agent enable toggles.
- `Save` writes the TOML and stays on the page.
- `Save & sync` writes the TOML, returns to the tabs and starts a background
  sync.
- While the page is open, `Tab`/`Shift+Tab` move between fields. In the main
  view, `Tab`/`Shift+Tab` loop Overview → Timeline → Models.

Advanced source paths and pricing overrides are edited in the TOML file; the
config page intentionally exposes only the common first-run settings.

## Environment variables

| variable | effect |
|---|---|
| `AGENTARC_CONFIG` | config file path |
| `AGENTARC_ARCHIVE` | archive root (overrides `archive_dir`) |
| `AGENTARC_CACHE_DIR` | index/pricing cache directory (default `~/.cache/agentarc`) |
| `AGENTARC_OFFLINE` | if set, never auto-refresh pricing |
| `CLAUDE_CONFIG_DIR` | Claude config root |
| `CODEX_HOME` | Codex home root |
| `EDITOR` / `VISUAL` | editor used by `agentarc config edit` |

If your home directory is not writable (containers, sandboxes), point all three
`AGENTARC_*` variables at a writable directory.
