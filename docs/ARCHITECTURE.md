# Architecture

`agentarc` has four layers:

```text
agent source dirs (read-only)
        │  sync
        ▼
~/AgentArchive/hosts/<host>/<agent>/…      plain files, source of truth
        │  ingest (parser version aware)
        ▼
~/.cache/agentarc/index.sqlite             derived, rebuildable
        │
        ├── TUI (ratatui)
        └── CLI: stats / export / doctor

pricing cache / embedded snapshot ──► cost calculation at query time
```

The archive is portable and safe to sync between machines. The index is local and
disposable: delete it and `agentarc reindex` rebuilds it from the archive.

## Archive is the source of truth

Source directories are opened read-only. A source file is never modified,
renamed or deleted by agentarc.

Every machine writes only under `hosts/<host_name>/`, so a sync tool such as
OneDrive or Syncthing never sees conflicting writes. Statistics merge all host
subtrees. Cross-host paths are normalized (`/home/you/...` and `/Users/you/...`
both become `~/...`).

A source that disappears is marked `source_missing_since` in the host manifest;
its archived copies are kept forever. A source that shrinks or is rewritten has
its previous archived copy moved to `.versions/<agent>/<rel-path>.<UTC-timestamp>`
before the new copy is installed.

## Sync algorithm

1. Discover files per agent using the configured roots and the per-agent
   environment overrides (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
2. Look up the source path in `hosts/<host>/manifest.json`.
3. If size, nanosecond mtime and inode are unchanged, skip without reading.
4. Otherwise classify:
   - append-only text / zstd: if the new file is at least as large as the old
     one and its prefix hashes to the archived content, treat it as grown;
   - otherwise (shrink or rewrite) version the old archive copy first and then
     copy the new one;
   - SQLite: take an online-backup snapshot, compare its content counter, and
     version the old snapshot if the new database has fewer rows/steps.
5. Write atomically (temp file + fsync + rename), keep the source mtime, and
   update the manifest every 25 files and at the end.
6. Keep files modified less than five seconds ago; parsers tolerate torn tails
   and the next sync picks up the rest.

SQLite source change detection includes the `-wal` sidecar because a live WAL
database can change without updating the main file's mtime.

## Index

Tables in `~/.cache/agentarc/index.sqlite`:

- `files` — one row per parsed archive file, with size/mtime/parser version,
  warnings and raw/unique usage counters;
- `sessions` — normalized session metadata;
- `usage` — one row per `(dedup_key, archive_path)`;
- `ag_meta` — Antigravity summary titles/cwd/times;
- `codex_threads` — titles/cwd/tokens from Codex `state_*.sqlite`;
- `meta` — schema/index/sync metadata.

A file is re-parsed when its size/mtime changes or when `PARSER_VERSION` is bumped.
Parsing is parallelized with rayon; SQLite writes are sequential.

**Global dedup:** duplicate usage events are stored once per file (composite
primary key) and collapsed at query time with one row per `dedup_key`. This
makes an incremental re-parse safe: if the same API response is present in two
session files and one file is re-parsed, the other copy still provides the
event.

`doctor` reports raw usage lines vs unique events per agent, parse warnings,
sessions without usage, unknown models and the last ingest errors.

## Pricing

`agentarc pricing update` downloads LiteLLM's pricing table into
`~/.cache/agentarc/pricing.json`. The binary also embeds a compact fallback
snapshot (`assets/pricing_snapshot.json`) so the tool works offline.

Model resolution strips provider prefixes and common date/variant suffixes, then
tries exact matches, `[pricing.aliases]`, `[pricing.overrides]`, and finally the
longest prefix match. Unknown models are never priced at $0; they are counted as
unpriced and surfaced in `doctor`/`stats`.

Cost precedence per usage event:

1. agent-reported per-message cost (Pi `usage.cost.total`), when present and > 0;
2. computed from the pricing table (including cache read/write and tiered
   `*_above_200k_tokens` fields);
3. unknown → `?`/unpriced.

Claude `cost-state.totalCostUSD` is session-level; it is shown next to the
computed cost as a sanity check, not added to usage events.

## TUI

The ratatui UI has three tabs: Overview (totals + per-agent table + 53-week
heatmap), Timeline (day/week/month, defaulting to month) and Models.

`Tab`/`Shift+Tab` loop the three tabs. The configuration page is separate from
the tab set and is opened with `c`; it opens automatically on first run (no
config file). Sync and index jobs run on a background thread and report phase,
progress bar, file counts, bytes and current file in a progress window.

## Safety and privacy

- Archives contain complete transcripts: code, tool output and potentially
  secrets. New archive/config files are created with mode `0600` on Unix.
- Credentials files are never discovered or copied.
- If `archive_dir` is inside a known cloud-sync location, sync refuses to run
  unless `allow_cloud_archive = true` is explicitly set.
- Tests use temp directories and never touch real agent directories or
  `~/AgentArchive`.

See [FORMATS.md](FORMATS.md) for per-agent record formats and dedup rules, and
[CONFIGURATION.md](CONFIGURATION.md) for the configuration reference.
