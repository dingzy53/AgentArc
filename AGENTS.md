# AGENTS.md

Guidance for AI coding agents working on this repository.

## What this is

`agentarc` is a Rust CLI + ratatui TUI for:

1. incrementally archiving AI-agent session files into a portable, append-only
   archive (one host subtree per machine), and
2. computing normalized token/cost statistics from that archive.

It supports Claude Code, OpenAI Codex CLI, Pi, DeepSeek Harness, and Google
Antigravity CLI/IDE on Linux and macOS. Windows is not supported.

The archive is the source of truth. The SQLite index is a disposable cache.

## Commands

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets
cargo build --release
./target/release/agentarc --help
```

If `~/.cargo` is read-only in the sandbox, set a workspace-local cargo home:

```bash
export CARGO_HOME="$PWD/.cargo-home"   # gitignored
export PATH="$HOME/.cargo/bin:$PATH"
```

This environment is a **single binary + library** crate. `src/lib.rs` exposes the
modules; `src/main.rs` is the clap entry point.

## Repository map

| Path | Purpose |
|---|---|
| `src/main.rs` | clap subcommands, CLI output |
| `src/lib.rs` | public module tree |
| `src/model.rs` | normalized `Session` / `UsageEvent` / `Message`, agent enum |
| `src/config.rs` | TOML config, source roots, env overrides, config save |
| `src/archive/mod.rs` | discovery, manifest, incremental sync, versions |
| `src/archive/sqlite_backup.rs` | SQLite online-backup snapshots |
| `src/adapters/` | one module per agent + hand-written protobuf walker |
| `src/index.rs` | schema, parallel ingest, global-dedup queries, transcript load |
| `src/pricing.rs` | LiteLLM cache/embedded snapshot, aliases, overrides, cost |
| `src/stats.rs` | grouping/ranges/aggregation |
| `src/tui/mod.rs` | 3-tab TUI, config page, background sync progress |
| `src/export.rs` | Markdown export |
| `src/doctor.rs` | diagnostics |
| `tests/fixtures/` | small per-agent fixtures |
| `docs/` | architecture, formats, configuration, scheduling, restore |
| `.github/workflows/` | CI and release builds |

## Hard invariants

Read these before changing parsers or the index:

1. **Never modify source agent directories.** Source files are opened read-only.
   Tests assert source mtime/content is unchanged after a sync.
2. **Archive = plain files under `hosts/<host>/<agent>/...`.** A machine writes
   only its own host subtree. Deleted sources stay archived with
   `source_missing_since`.
3. **Token invariant:**
   `total = fresh_input + cache_read + cache_write_5m + cache_write_1h + output`.
   `reasoning` is a subset of `output` and is never added again.
4. **Dedup is global.** Usage rows are stored per `(dedup_key, archive_path)` and
   collapsed at query time. Do not change this to a single-column primary key;
   incremental re-parsing would lose events supplied by another file.
5. **Unknown model prices are never $0.** They are reported as unpriced (`?`) in
   `stats`/TUI and listed in `doctor`.
6. **No decryption.** Encrypted Antigravity `.pb` conversations are archived and
   listed (if a summary exists) but never counted.
7. **TUI tabs are exactly 1–3:** Overview, Timeline, Models. `Tab`/`Shift+Tab`
   loop them. The configuration page is separate and opened with `c`; it opens
   automatically when no config file exists.
8. **Sync/index must not block the TUI.** Both run in a background thread and
   report progress through `SyncUpdate` channels. Keep the progress window and
   footer status working.
9. **Safety:** if `archive_dir` is inside OneDrive/Dropbox/CloudStorage/Syncthing,
   sync refuses unless `allow_cloud_archive = true`.

## Parser changes

- Be lenient: unknown record types/fields are ignored; broken trailing JSON or a
  torn zstd tail produces a warning, not a hard failure.
- Every adapter emits normalized `UsageEvent`s. It must set a stable global
  `dedup_key` and a `session_key` formatted as `host:agent:id`.
- If interpretation changes, bump `PARSER_VERSION` in `src/model.rs` so existing
  indexes re-parse the affected files.
- Add/update a golden test in `tests/` or a unit test in the adapter. Test both
  token totals and dedup behavior.
- Keep `src/adapters/protowire.rs` dependency-free; it intentionally has no
  generated protobuf schema.

### Adding an agent adapter

1. Add a variant to `Agent` in `src/model.rs` (`as_str`, `display_name`,
   `parse`, `ALL`).
2. Add a `SourceConfig` field in `src/config.rs` (defaults + `source_config(_mut)`),
   discovery in `src/archive/mod.rs`, and parse dispatch in `src/adapters/mod.rs`.
3. Implement `parse()` and `read_messages()` in a new `src/adapters/<agent>.rs`.
4. Add archive-path parsing/classification in `src/index.rs`
   (`is_parseable`, `is_meta_file`).
5. Add a small fixture and golden test; add a row to `README.md`'s agent table
   and `docs/FORMATS.md`.
6. If it needs metadata files (titles/cwd), add a table + post-processing in
   `src/index.rs` rather than querying source dirs during stats.

## Index and database

- Schema lives in `src/index.rs` (`SCHEMA`). Bump `SCHEMA_VERSION` for
  incompatible schema changes; bump `POST_PROCESS_VERSION` when metadata
  post-processing rules change.
- `Index::ingest` parses files in parallel with rayon, then writes in one thread.
- Re-parsing a file deletes its old usage/session rows; usage dedup relies on the
  composite primary key. Tests in `tests/ingest_golden.rs` cover re-parse safety.
- The archive is not the index: `agentarc reindex` must always rebuild from
  files under `hosts/`.

## Pricing

- `agentarc pricing update` writes `~/.cache/agentarc/pricing.json`; the embedded
  fallback is `assets/pricing_snapshot.json`.
- `parse_table` adds bare model ids for provider-prefixed LiteLLM keys.
  Preserve this behavior.
- Config prices are USD per **1M** tokens and converted to per-token internally.
- Add aliases/overrides to the default config only when they are principled;
  do not hide genuinely unknown models.

## TUI

- ratatui + crossterm; no animations. Redraw on events and on a ~200 ms poll for
  background progress.
- Table widths use `Constraint::Percentage`, not fixed widths, so 80-column
  terminals keep all columns visible.
- The footer is two lines: status first, key hints second. Keep status visible
  during sync/index.
- All text editing in the config page is append/backspace only; `Enter` commits,
  `Esc` cancels an edit, `Tab` moves fields.
- TUI code lives in `src/tui/mod.rs`; if it grows, split it into `ui/` and `app/`
  submodules rather than adding more unrelated state.

## Tests

- Use `tempfile` and synthetic fixtures; never touch real agent dirs or
  `~/AgentArchive`.
- Archive tests must cover unchanged/grow/rewrite/shrink/delete and source
  read-only safety.
- Run `cargo fmt --check`, `cargo test`, and ideally `cargo clippy --all-targets`
  before finishing.
- Do not commit `target/`, `.cargo-home/`, `.dev-*`, `.test-*`, throwaway
  archives, logs, or generated pricing caches.

## Releasing

- The tag has no `v` prefix and must equal `version` in `Cargo.toml`.
- `git tag 0.0.1 && git push --follow-tags origin main`.
- `.github/workflows/release.yml` builds:
  - `x86_64-unknown-linux-musl` (static),
  - `aarch64-apple-darwin`,
  - `x86_64-apple-darwin` (cross-compiled on the arm64 macOS runner).
- Each asset is `agentarc-<tag>-<target>.tar.gz`, next to a `.sha256` file.
- Push the workflow changes before the first tag, so the tag run finds the
  workflow.
- CI runs `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` on
  Linux and macOS.

## Documentation

Keep these in sync with behavior:

- `README.md` — user-facing quick start, commands, TUI keys, limitations
- `docs/ARCHITECTURE.md` — pipeline, archive/index design, safety
- `docs/FORMATS.md` — per-agent formats and dedup/token rules
- `docs/CONFIGURATION.md` — full config reference and env vars
- `docs/SCHEDULING.md` — systemd/launchd/cron examples
- `docs/RESTORE.md` — restoring or exporting sessions

The docs above are the current design reference.

## Known limitations

- Antigravity IDE encrypted `.pb` has no token stats by design.
- Internal/trial model ids need aliases/overrides; `doctor` shows what is unknown.
- No full-text search across sessions; the TUI no longer has a session browser
  (use `stats --by session` and `export`).
- Pricing is a fallback/sanity estimate; Claude `cost-state` and Pi per-message
  costs are the agent-reported references.
