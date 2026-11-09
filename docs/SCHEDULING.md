# Scheduling sync

`agentarc sync` is safe to run from a timer. It is incremental, atomic per file
and never modifies source directories. Do not run two syncs for the same host at
the same time.

Find the binary path first:

```bash
command -v agentarc          # e.g. /home/you/.cargo/bin/agentarc
```

The examples below assume `~/.cargo/bin/agentarc`; adjust if you installed it
elsewhere.

## Fedora / systemd user timer

Copy the unit files:

```bash
mkdir -p ~/.config/systemd/user
cp docs/agentarc-sync.service docs/agentarc-sync.timer ~/.config/systemd/user/
```

If your binary is not at `~/.cargo/bin/agentarc`, edit `ExecStart` in
`~/.config/systemd/user/agentarc-sync.service`.

Enable and start:

```bash
systemctl --user daemon-reload
systemctl --user enable --now agentarc-sync.timer
systemctl --user list-timers agentarc-sync.timer
```

Run once manually or inspect logs:

```bash
systemctl --user start agentarc-sync.service
journalctl --user -u agentarc-sync.service -n 100
```

The provided timer runs daily with a 5-minute randomized delay and
`Persistent=true`, so a missed run happens after the next boot/login.

`agentarc` can auto-refresh pricing during a scheduled sync. If you want the
timer to stay fully offline, uncomment this line in the service:

```ini
Environment=AGENTARC_OFFLINE=1
```

## macOS / launchd

Copy the plist and replace the placeholder path:

```bash
cp docs/com.agentarc.sync.plist ~/Library/LaunchAgents/
# edit the ProgramArguments path
launchctl load -w ~/Library/LaunchAgents/com.agentarc.sync.plist
launchctl list | grep agentarc
```

The example runs every 6 hours (`StartInterval = 21600`) and at login
(`RunAtLoad`). Logs go to `/tmp/agentarc-sync.out.log` and
`/tmp/agentarc-sync.err.log`; change those paths if you prefer
`~/Library/Logs/agentarc/`.

To unload:

```bash
launchctl unload ~/Library/LaunchAgents/com.agentarc.sync.plist
```

## Cron alternative

If you prefer cron, run the same command and redirect output:

```cron
17 */6 * * * /home/you/.cargo/bin/agentarc sync >> /tmp/agentarc-sync.log 2>&1
```

## Multi-machine setup

1. Give each machine a stable, unique `host_name` in its config.
2. Point `archive_dir` at the shared folder on both machines **after** local
   testing, and set `allow_cloud_archive = true`.
3. Let each machine write only its own `hosts/<host>/` subtree.
4. Keep the local index out of the shared folder (the default cache path already
   does this).
5. Run `agentarc reindex` on a machine after it receives new host subtrees from
   another machine, or just let the next `sync`/TUI index them incrementally.

See [CONFIGURATION.md](CONFIGURATION.md) for the cloud-sync guard and the
`AGENTARC_*` environment variables.
