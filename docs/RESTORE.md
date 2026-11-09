# Restoring a session from the archive

The archive mirrors source paths and records the original source path for every
file in `hosts/<host>/manifest.json`.

1. Find the session key or session id:

   ```bash
   agentarc stats --by session --json
   # or filter by project/agent/model with the other --by values
   ```

2. Query the archived file path and original source:

   ```bash
   sqlite3 ~/.cache/agentarc/index.sqlite \
     "select host, archive_path, source_path from sessions where key='<key>';"
   ```

   `archive_path` is relative to `hosts/<host>/`.

3. The archived file is `~/AgentArchive/hosts/<host>/<archive_path>`. An older
   copy preserved before a shrink/rewrite is under
   `~/AgentArchive/hosts/<host>/.versions/<agent>/...`.

4. Copy it back to the original path **only while the agent is not running**,
   and keep a backup of the current file. For SQLite files (Antigravity), stop
   the app before restoring.

For reading or sharing a session, prefer:

```bash
agentarc export <session-key-or-id-prefix> -o session.md
agentarc export <session-key-or-id-prefix> --with-thinking --with-tool-output
```

The CLI `export` command works even though the TUI intentionally has only
Overview, Timeline and Models tabs (see [ARCHITECTURE.md](ARCHITECTURE.md)).
