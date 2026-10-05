# Beads Coordination Backup Runbook

This repository's implementation tracker lives in the `hud` Beads database, served
by the Dolt server that `.beads/` points at. Treat it as coordination state: blocker notes, deferrals, and
handoff details can exist only in Beads until they are summarized in tracked
docs.

## Current State

As of 2026-06-10, this checkout has no durable Beads backup destination:

```bash
bd backup status --json
bd dolt remote list
bd backup sync
```

Expected result before setup:

- `bd backup status --json` reports `"configured": false`.
- `bd dolt remote list` reports no configured remotes.
- `bd backup sync` fails with `no backup destination configured`.
- `.beads/issues.jsonl` and `.beads/backup/` are ignored local recovery
  artifacts, not durable handoff state.

## Required Durable Path

Use one of these operator-owned destinations. Do not store credentials in this
repository.

### Preferred: Beads Backup Destination

Configure a DoltHub remote or a filesystem path that is already synced
off-machine, then run a backup sync:

```bash
bd backup init https://doltremoteapi.dolthub.com/<owner>/<repo>
DOLT_REMOTE_USER=<owner> DOLT_REMOTE_PASSWORD=<token> bd backup sync
bd backup status --json
```

For filesystem backup, the destination must be durable outside the local
checkout, for example a mounted NAS path or a separate synced backup repo:

```bash
bd backup init /mnt/durable-backups/tze-hud-beads
bd backup sync
bd backup status --json
```

The acceptance signal is `bd backup sync` exiting 0 and
`bd backup status --json` reporting a configured backup plus a fresh timestamp.

### Alternative: Dolt Remote

If the team wants Beads database replication rather than the backup subcommand,
configure a Dolt remote named `origin` and push:

```bash
bd dolt remote add origin <dolt-remote-url>
bd dolt remote list
bd dolt push
```

The acceptance signal is `bd dolt push` exiting 0 and `bd dolt remote list`
showing the configured remote.

## Recovery

When restoring from a Beads backup destination:

```bash
bd backup restore <backup-path-or-url>
bd backup status --json
bd show hud-qdeh8 --json
```

When restoring from a Dolt remote:

```bash
bd dolt remote list
bd dolt pull
bd show hud-qdeh8 --json
```

If Dolt recovery reports `database "hud" not found`, first inspect
`.beads/metadata.json` and restore the `hud` database binding before relying on
new Beads writes.

## Worker Boundary

Agent workers must not invent local-only destinations such as `.beads/backup/`
or a path inside `.worktrees/`. Those paths make `bd backup sync` pass without
solving the durability problem. If no operator-owned destination is available,
report the bead blocked with the exact required destination and leave this
runbook as the recovery path.
