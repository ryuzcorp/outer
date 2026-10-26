---
title: Back up the database
description: Daily D1 dumps, retention, on-demand backup, download, and restore — in Console and over MCP.
---

An app with a database gets a daily dump automatically once it has been active in the prior 24 hours (a request or a successful deploy). Each dump is a full `.sql` export stored off to the side, never inside the app's own files. Dumps are kept 3 days on Free, 30 on Pro with any-day restore. The person-facing guide is [Back up and restore your data](/backend/backups).

## Restore from Console

1. Open **Data → D1 → Backups** for the dump list.
2. Use **Back up now** to take a dump on demand.
3. Use **Download** to save a dump as `.sql`.
4. Use **Restore** to replace the live tables with that dump.

Restore takes a fresh backup first (behind a confirmation that names the backup), so a bad restore is itself restorable.

## Operate over MCP

You get the same three operations over MCP. All are owner-only:

| Tool | Effect | Args |
| --- | --- | --- |
| `list_backups` | Read the dump list | `binding` optional, defaults to the app's only D1 binding |
| `backup_database` | Take a dump now | `binding` optional, defaults to the app's only D1 binding |
| `restore_backup` | Replace live tables with a dump | `confirm` must equal the backup id; `binding` optional, defaults to the app's only D1 binding |
