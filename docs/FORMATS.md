# Agent formats and normalization

This page documents what each adapter reads, the dedup key it uses and the
pitfalls that cause incorrect token counts. Formats drift; every adapter is
lenient about unknown fields, and `PARSER_VERSION` in `src/model.rs` forces a
re-index when interpretation changes.

Normalized token columns:

```text
total = fresh_input + cache_read + cache_write_5m + cache_write_1h + output
reasoning ⊂ output   (never added on top)
```

`doctor` shows raw usage-bearing lines vs unique dedup keys per agent so
duplicate-heavy data is visible.

## Claude Code

**Files**

- `~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl`
- `~/.claude/projects/<slug>/<session-uuid>/subagents/agent-*.jsonl`
- `$CLAUDE_CONFIG_DIR/projects/...` when the environment variable is set.

**Records**

- `type: assistant` with `message.{id, model, usage}`, `requestId`, `timestamp`,
  `cwd`, `sessionId`, `isSidechain`.
- `usage`: `input_tokens` (fresh, excludes cache),
  `cache_creation_input_tokens` (+ `cache_creation.ephemeral_5m/1h_input_tokens`),
  `cache_read_input_tokens`, `output_tokens` (includes thinking).
- `usage.iterations[]`: per-iteration usage, including `advisor_message` entries
  with their own `model`.
- `type: cost-state`: session-level `totalCostUSD` and `modelUsage`.
- `type: custom-title` / `ai-title` for titles.

**Dedup key**

`message.id:requestId`, plus `:it<N>` when `usage.iterations[]` exists.

**Pitfalls**

1. One API response is written as several `assistant` lines (one per content
   block) with the same `message.id` + `requestId`. Dedup is global because
   resumed/forked sessions copy history into new files.
2. Top-level usage excludes advisor iterations. If `iterations` exists, count
   each iteration as its own usage row using the iteration's model (fallback:
   message model) and ignore the top-level usage. Never count both.
3. Cache writes have two prices; use the `ephemeral_5m/1h` split when present.
4. Subagent files live under `<session-uuid>/subagents/` and are stored as
   separate sessions tagged `is_subagent`, with `parent_session` set.

## OpenAI Codex CLI

**Files**

- `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`
- `~/.codex/archived_sessions/...`
- `~/.codex/state_*.sqlite` (highest version): `threads` table for titles/cwd/
  `tokens_used` cross-check.
- `$CODEX_HOME` replaces `~/.codex`.

**Records**

- `session_meta` (`id`, `cwd`, `model_provider`, `originator`).
- `turn_context` (`model`).
- `event_msg` with `payload.type = token_count | task_started | task_complete | …`.
- `response_item` messages/function calls.
- `payload.info.{total_token_usage,last_token_usage}`:
  `{input_tokens, cached_input_tokens, output_tokens,
  reasoning_output_tokens, total_tokens}`.

**Normalization and dedup**

OpenAI semantics: `input_tokens` includes cached tokens and `output_tokens`
includes reasoning. Normalize to:

```text
fresh_input = input_tokens - cached_input_tokens
cache_read  = cached_input_tokens
```

`token_count` events can repeat identical cumulative totals. Count an event only
when `total_token_usage` changes. Use `last_token_usage`; if absent, use the
delta of cumulative totals. The model comes from the latest `turn_context` before
the event, because `token_count` does not carry it. Dedup key includes the
session id and cumulative totals.

A session with no `token_count` and turns ending in `task_complete.error` is
"failed, 0 tokens". A session with no `token_count` and no errors is reported as
"no usage data" in `doctor`, not as $0.

## Pi (badlogic/pi-mono)

**Files**

- `~/.pi/agent/sessions/--<cwd-slug>--/<ISO-ts>_<uuidv7>.jsonl`

**Records**

- Header `{"type":"session","version":3,"id","cwd"}`.
- A tree of entries (`id`, `parentId`): `message` (`user|assistant|toolResult`),
  `model_change`, `thinking_level_change`, compaction, etc.
- `message.usage = {input, output, cacheRead, cacheWrite, reasoning,
  totalTokens, cost:{input,output,cacheRead,cacheWrite,total}}`, plus
  `message.{provider, model, responseId, timestamp}`.

`input` already excludes cache; verified `input + output + cacheRead + cacheWrite
= totalTokens`.

**Dedup and branches**

Forked/branched sessions copy entries into other files. Dedup on `responseId`;
if it is missing, hash `(timestamp, provider, model, usage)`. Count **all**
branches of the tree, not only the active path — each was a real API call.

`usage.cost.total` is per-message and used as reported cost. If it is absent or
zero while tokens > 0, fall back to the pricing table.

## DeepSeek Harness (`dsh`)

**Files**

- `~/.dsh/sessions/--<cwd-slug>--/session-<uuid>/session.v4.jsonl.zstd` (current)
- older `session.jsonl.zstd` (v0)
- If both exist in one session directory, only the highest version is archived
  and indexed.

**Container**

Multi-frame zstd: one frame appended per flush. The decoder reads all frames and
tolerates a torn tail (zero padding / partial frame after a crash): stop at the
error, keep what decoded, and record a warning.

**Records**

`{"type","seq","time"(ms),"data"}` with:

- header `session` (`id`, `cwd`, `createdAt`, `version`, `delegationDepth`);
- `model/selection`, `turn/start|end`, `step/start|end`;
- `assistant/message`, `user/message`, `tool/call|result`;
- `session/title`.

**Usage**

`assistant/message.data.usage = {inputTokens, outputTokens, cacheReadTokens,
reasoningTokens}`; model at `data.message.source.{provider,model}`.
`inputTokens` excludes cache reads (verified across consecutive steps);
`reasoningTokens` is a subset of `outputTokens`.

**Pitfalls**

- The same usage also appears inside `data.stream[]` as a `usage` chunk. Read
  only `data.usage`.
- Dedup key: `session-id:turn:step` plus `attempt` if present.
- Sub-agents (`delegationDepth > 0`) count normally and are tagged.

## Google Antigravity — CLI (`agy`)

**Files**

- `~/.gemini/antigravity-cli/conversations/<cascade-uuid>.db` (one SQLite DB per
  conversation, WAL mode)
- `conversation_summaries.db` (titles, workspace URIs, timestamps)
- `conversation_summaries` index plus `*_summaries_proto.pb`
- `brain/<uuid>/.system_generated/logs/transcript_full.jsonl`

**SQLite snapshots**

Never copy a WAL database with `cp`. Open read-only and use SQLite's online
backup API. Changes are detected by `(size, mtime)` of `.db` + `.db-wal`, then by
content (row count / max index). On read-only media the code retries with the
SQLite `immutable=1` URI.

**Token data**

Per-conversation `gen_metadata(idx, data)` blobs are protobuf without a schema.
Verified field path `1.4` = `ModelUsageStats`:

| field | meaning |
|---|---|
| 1 | model enum (e.g. 1318 ↔ `MODEL_PLACEHOLDER_M318`) |
| 2 | input tokens (uncached) |
| 3 | output tokens = 9 + 10 |
| 4 | cache write tokens |
| 5 | cache read tokens |
| 9 | thinking output tokens |
| 10 | response output tokens |
| 11 | response id |

The enclosing message has model id at `1.19`, display name at `1.21`, and
key/value pairs at `1.20` (`cascade_id`, `request_id`, `trajectory_id`,
`used_claude`, …). Dedup key: `request_id`, else `cascade_id:idx`.

`docs` are stored per conversation; titles/cwd/times come from the summaries DB
or summary protobuf and are applied during index post-processing. Conversations
known only from a summary are listed as summary-only sessions with no tokens.

## Google Antigravity — IDE/desktop

**Roots**

- `~/.gemini/antigravity/` (unified path in newer builds)
- `~/.gemini/antigravity-ide/` (legacy)

Both are scanned. Layout:

```text
conversations/   <uuid>.pb (older/IDE, possibly encrypted) or <uuid>.db (newer)
implicit/        encrypted implicit context (not archived)
brain/<uuid>/    artifacts, transcript_full.jsonl when present
annotations/     <uuid>.pbtxt
*_summaries_proto.pb   plain protobuf summaries
```

**Detection**

1. `conversations/*.db` → parsed like the CLI, full token stats.
2. `conversations/*.pb`:
   - plain protobuf (low entropy, complete wire decode) → scan for
     `ModelUsageStats`; full token stats when found;
   - encrypted (entropy > 7.2 or decode failure) → **archive only**. A session
     is listed when a summary exists, with tokens shown as **n/a (encrypted)**.
3. No decryption is attempted: no keychain access and no talking to the running
   app.

`doctor` reports `db / pb-plain / pb-encrypted / summary-only` counts so a first
run on a new machine shows what is really there.

## Other notes

- Archived `transcript_full.jsonl` is read for Antigravity transcripts by
  `agentarc export`. No transcript parsing is needed for token stats.
- Codex thread titles come from `state_*.sqlite`; Claude titles from
  `custom-title`/`ai-title`; Pi/DSH from their own title/first-user-message
  fallbacks.
- Project names replace `/home/<user>` and `/Users/<user>` with `~`, use the git
  root when detectable, and apply `[projects.aliases]`.
