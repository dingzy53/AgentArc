# agentarc

[![Release](https://img.shields.io/github/v/release/dingzy53/AgentArc)](https://github.com/dingzy53/AgentArc/releases)
[![Downloads](https://img.shields.io/github/downloads/dingzy53/AgentArc/total)](https://github.com/dingzy53/AgentArc/releases)
[![CI](https://github.com/dingzy53/AgentArc/actions/workflows/ci.yml/badge.svg)](https://github.com/dingzy53/AgentArc/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/dingzy53/AgentArc)](LICENSE)

Archive AI-agent session files and compute token/cost statistics you can trust.

`agentarc` is a single Rust binary for **Linux and macOS**. It copies session
transcripts from the agents you use into one portable archive, keeps older
copies when a source is rewritten, and builds a local index for a keyboard-driven
TUI and a few script-friendly CLI commands.

- **Archive first, statistics second.** The archive is plain files and is the
  source of truth. The SQLite index is derived and can be deleted at any time.
- **Incremental, read-only sync.** Source files are never modified. Unchanged
  files are skipped by size/mtime/inode; grown append-only files are copied;
  shrunk or rewritten files keep a versioned snapshot under `.versions/`.
- **Correct dedup.** Streaming duplicates, resumed/forked sessions, repeated
  cumulative counters and advisor iterations are handled per agent.
- **Honest pricing.** Uses per-message agent-reported cost when available,
  otherwise a cached LiteLLM pricing table. Unknown models are shown as
  unpriced, never silently priced at $0.
- **Multi-host.** One archive, one `hosts/<host>/` subtree per machine. Stats
  merge all hosts; a sync tool never sees conflicting writes.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — pipeline, archive and index design
- [Agent formats](docs/FORMATS.md) — per-agent records, dedup and token rules
- [Configuration](docs/CONFIGURATION.md) — full config reference, env vars
- [Scheduling](docs/SCHEDULING.md) — systemd/launchd/cron examples
- [Restoring sessions](docs/RESTORE.md) — restore or export from the archive
- [AGENTS.md](AGENTS.md) — development guidelines for AI coding agents

## Status

Implemented:

- config + `doctor` source discovery
- incremental archive sync (JSONL, multi-frame zstd, SQLite online backup API)
- adapters for Claude Code, Codex CLI, Pi, DeepSeek Harness, Antigravity CLI
  and Antigravity IDE (`db` / plain `pb` / encrypted `pb`)
- local SQLite index with global dedup
- LiteLLM pricing with embedded snapshot, cache, aliases and overrides
- CLI: `sync`, `stats`, `export`, `doctor`, `pricing`, `reindex`, `config`
- ratatui TUI: Overview/heatmap, Timeline and Models, first-run configuration,
  and background sync/index progress
- multi-host archive layout and cross-host project normalization
- tests for sync grow/shrink/rewrite/delete, source read-only safety, golden
  tokens for every adapter, dedup survival, pricing and export

## Install

### Prebuilt binary

Download the archive for your platform from
[Releases](https://github.com/dingzy53/AgentArc/releases):

| Platform | Asset |
|---|---|
| Linux x86_64 | `agentarc-<version>-x86_64-unknown-linux-musl.tar.gz` |
| macOS Apple silicon | `agentarc-<version>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `agentarc-<version>-x86_64-apple-darwin.tar.gz` |

Each release also publishes a `.sha256` file. Then:

```bash
tar -xzf agentarc-<version>-<target>.tar.gz
install -m 755 agentarc-<version>-<target>/agentarc ~/.local/bin/agentarc
```

The Linux build is static and needs no system libraries. The macOS builds are
unsigned; if Gatekeeper blocks the binary, allow it in
**System Settings → Privacy & Security**.

### From source

Prerequisites: Rust stable and a C toolchain (bundled SQLite and zstd need a C
compiler).

Fedora:

```bash
sudo dnf install -y rustup gcc
rustup-init -y --profile default
source "$HOME/.cargo/env"
```

macOS:

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

Build:

```bash
cargo build --release
# binary at target/release/agentarc
```

Install on `PATH` if desired:

```bash
install -m 755 target/release/agentarc ~/.local/bin/agentarc
```

## Quick start

```bash
agentarc                      # first run opens the Configuration page
agentarc doctor               # show detected sources, archive, index, pricing
agentarc sync                 # archive changes and update the index
agentarc                      # TUI
```

On first run, the Configuration page lets you set the archive folder, host name,
timezone, `sync_on_start` and the agents you want to sync, then **Save** or
**Save & sync**. Equivalent CLI setup:

```bash
agentarc config init          # writes ~/.config/agentarc/config.toml
agentarc config edit          # open it in $EDITOR
```

A throwaway manual archive is easy:

```bash
AGENTARC_ARCHIVE=~/AgentArchive-dev agentarc sync
AGENTARC_ARCHIVE=~/AgentArchive-dev agentarc
```

## Commands

```text
agentarc                         TUI (default)
agentarc sync [--dry-run] [--agent claude,pi] [--no-index]
agentarc stats [--since 2026-10-01] [--until 2026-10-31]
               [--by day|week|month|agent|model|project|session]
               [--agent …] [--host …] [--json|--csv]
agentarc export <session-key|id-prefix> [-o file.md]
               [--with-thinking] [--with-tool-output]
agentarc doctor                  sources, archive, index, dedup, unknown models,
                                 sessions without usage, parse warnings
agentarc pricing update
agentarc pricing show <model>
agentarc reindex                 drop + rebuild the local index from the archive
agentarc config path|init|edit
```

`sync` prints a live single-line progress display on a TTY. `stats` filters dates
in the configured timezone. `export` prints Markdown to stdout unless `-o` is
given. Session ids and keys both work; a full session id is preferred over its
subagent transcripts.

## Archive layout

```text
~/AgentArchive/
  README.txt
  hosts/
    <hostname>/
      manifest.json                 source path → size/mtime/inode/hash/versions
      claude/projects/<slug>/<uuid>.jsonl
      claude/projects/<slug>/<uuid>/subagents/agent-*.jsonl
      codex/sessions/YYYY/MM/DD/rollout-*.jsonl
      codex/state.sqlite             online-backup snapshot (titles/cwd)
      pi/sessions/--<slug>--/*.jsonl
      dsh/sessions/--<slug>--/session-*/session*.jsonl.zstd
      antigravity-cli/conversations/<uuid>.db
      antigravity-cli/conversation_summaries.db
      antigravity-cli/brain/<uuid>/.system_generated/logs/transcript_full.jsonl
      antigravity-ide/…
      .versions/<agent>/<rel-path>.<UTC-timestamp>
```

Each host writes only under its own `hosts/<host>/`, so a sync tool never sees
conflicting writes. Statistics merge all hosts. A deleted source is kept in the
archive and marked `source_missing_since` in the manifest.

### Cloud-sync safety

If `archive_dir` resolves inside `~/OneDrive*`, `~/Library/CloudStorage/*`,
`~/Dropbox`, or a Syncthing folder, sync refuses to run unless
`allow_cloud_archive = true` is set. Flip that only after you have finished
testing locally. The index lives in `~/.cache/agentarc/index.sqlite`, never in
the synced folder (SQLite over OneDrive would corrupt).

The first-run Configuration page warns about this; the full reference is in
[Configuration](docs/CONFIGURATION.md).

## Token correctness

Normalized token columns are always:

```text
total = fresh_input + cache_read + cache_write + output
```

`reasoning` is a subset of `output` and is never added again.

| Agent | Dedup | Notes |
|---|---|---|
| Claude Code | `message.id:requestId`, suffix per `usage.iterations[]` | Streaming content-block lines share one id; iterations/advisor tokens are counted with their own model; 5m/1h cache split; `cost-state` shown as reported cost. |
| Codex CLI | changed cumulative `total_token_usage` | `input` includes cache and `output` includes reasoning; `fresh = input − cached`; repeated totals skipped; failed 0-token turns listed. |
| Pi | `responseId`, else hash(ts, provider, model, usage) | Forks copy entries between files, so dedup is global; all tree branches count; `usage.cost.total` is reported cost. |
| DeepSeek Harness | `session-id:turn:step[:attempt]` | Multi-frame zstd with torn-tail tolerance; only `data.usage` is read; highest session version wins. |
| Antigravity CLI | `request_id`, else `cascade_id:idx` | `gen_metadata` protobuf `1.4` ModelUsageStats; SQLite snapshots via the online backup API. |
| Antigravity IDE | same | `.db` like the CLI; plain `.pb` parsed; encrypted `.pb` archived and listed with tokens **n/a (encrypted)**. |

`doctor` reports raw usage lines vs unique dedup keys per agent, unknown models,
sessions without usage, parse warnings and last ingest errors. A healthy run
shows 0 unknown models and no unexplained parse errors. Full details:
[Agent formats](docs/FORMATS.md).

## TUI

```text
Tab      next tab (loops Overview → Timeline → Models → Overview)
Shift+Tab previous tab
1–3      jump to Overview / Timeline / Models
c        configuration page (archive folder, host, agents to sync…)
r        range: today / 7d / 30d / this month / this year / all
a / h    toggle agents / hosts
g        Timeline grouping: day / week / month
s        sync now (background, with a progress window)
t        heatmap metric: tokens or cost
↑↓/PgUp/PgDn  scroll the current table
?        help
q / Esc  quit
```

The TUI defaults to **this year**, and Timeline defaults to **group by month**.

On first run (no config file yet) the TUI opens the **Configuration** page
automatically. It lets you edit the archive folder, host name, timezone,
`sync_on_start`, cloud-archive permission and per-agent `enabled` toggles, then
**Save** or **Save & sync**. `c` reopens the page at any time; `Esc`/`c` goes
back without saving. The configuration page is not one of the three tabs, so
`Tab` always loops the three main tabs when the page is closed. While the page
is open, `Tab`/`Shift+Tab` move between form fields.

The Overview heatmap is 53 weeks of daily tokens (or cost), shaded by quantile:
`· ░ ▒ ▓ █`.

`sync` and the index rebuild run in a background thread. A progress window shows
the phase, a progress bar, copied files/bytes and the current file, so a long
first archive never looks frozen. The footer status line also stays visible.

The TUI intentionally has no session browser; use `agentarc stats --by session`
and `agentarc export` for reading or sharing individual sessions.

## Scheduling

See [Scheduling](docs/SCHEDULING.md) for Fedora systemd user timer, macOS launchd
and cron examples. Ready-made units are in
[`docs/agentarc-sync.service`](docs/agentarc-sync.service),
[`docs/agentarc-sync.timer`](docs/agentarc-sync.timer) and
[`docs/com.agentarc.sync.plist`](docs/com.agentarc.sync.plist).

## Development

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets
cargo build --release
```

Tests use temp directories only. They never touch real agent directories or
`~/AgentArchive`; the archive sync test additionally asserts that source mtimes
and hashes are unchanged. Fixtures live in `tests/fixtures/`.

See [AGENTS.md](AGENTS.md) for module layout, invariants and the adapter checklist.

### Releasing

Tags have no `v` prefix and must equal `version` in `Cargo.toml`.

```bash
# bump version in Cargo.toml, refresh Cargo.lock with cargo build
git tag 0.0.1
git push --follow-tags origin main
```

[`.github/workflows/release.yml`](.github/workflows/release.yml) checks the tag,
builds the static Linux binary and both macOS binaries, and attaches them to the
GitHub release.

## Limitations

- No Windows support.
- No full-text search across sessions.
- Antigravity IDE encrypted `conversations/*.pb` cannot be counted (no
  decryption is attempted; the key is held by the app/keychain).
- Internal/trial model ids need `[pricing.aliases]` or `[pricing.overrides]`;
  `doctor` lists anything still unknown.
- The archive contains full transcripts (code, possibly secrets). Keep it out
  of public/shared folders. Credentials files are never copied.

## License

[MIT](LICENSE) © 2026 Ding Zheng Yuan
