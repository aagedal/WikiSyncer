# Diagnostics and incident collection

`wikisync doctor` collects a bounded, offline, allowlist-only diagnostic report. It
can print human-readable output or JSON and can create a private JSON bundle without
overwriting an existing file:

```sh
wikisync --library /absolute/path/to/library doctor
wikisync --library /absolute/path/to/library doctor --json
wikisync --library /absolute/path/to/library doctor --bundle ./wikisync-doctor.json
```

The bundle is created with mode `0600`. It contains versions, OS/architecture,
filesystem and database-file aggregates, source/collection/schedule counts, bounded
run/error aggregates, local control-plane state, and a quick logical-object integrity
summary. It omits source endpoints, titles, collection names, paths, raw errors,
content, object IDs, environment variables, and logs. Review every bundle before
sharing it. Never attach the library database, canonical objects, article/search
text, socket files, or an unreviewed service log to a public issue.

## Local checks

These commands inspect daemon control state without contacting MediaWiki:

```sh
/absolute/path/to/wikisyncd --library /absolute/path/to/library health
/absolute/path/to/wikisyncd --library /absolute/path/to/library status
wikisync --library /absolute/path/to/library status --json
wikisync --library /absolute/path/to/library doctor --json
```

When connected, daemon status reports its PID, uptime, successful mutation count, and
current handler state/detail; health checks whether the local control plane responds.
When the daemon is stopped they fail with connection-not-found or connection-refused,
rather than returning an offline status. CLI status separately reports durable sync
checkpoints, recent runs, job counts, and the latest recorded error. None of these is
a whole-library integrity check or proof of upstream completeness. A daemon state of
`idle` means that IPC and application dispatch are ready; it does not mean scheduling
is configured or that the archive is current. Doctor opens the database as an
immutable read-only checkpointed snapshot so it creates no WAL/SHM files or migrations;
while a daemon has uncheckpointed WAL data, its catalog and run counts may lag live
state. Section failures are reported as redacted error codes rather than raw details.

Inspect the service manager and recent logs:

```sh
# macOS
launchctl print "gui/$(id -u)/org.wikisync.WikiSyncer"
tail -n 200 "$HOME/Library/Logs/WikiSyncer/wikisyncd-error.log"

# Linux
systemctl --user status wikisyncd.service
journalctl --user -u wikisyncd.service --since today --no-pager
systemctl --user list-timers wikisyncd-health.timer
```

Service logs and recorded error messages can include source endpoints, page/job
identifiers, filesystem paths, and upstream response details. Redact usernames, home
paths, query/title text, operator contact data, IP addresses, tokens, and material
that may have been suppressed upstream. Preserve timestamps, versions, exit status,
error class, retryability, and aggregate counts where possible.

## Filesystem and permissions

With the daemon stopped, record metadata rather than contents:

```sh
ls -ld /absolute/path/to/library
ls -l /absolute/path/to/library/library.sqlite3*
df -h /absolute/path/to/library
```

The library root and application-created directories should be mode `0700`; the
SQLite database, WAL/SHM files, and daemon sockets should be `0600`. A permissive or
unexpected owner is an incident to correct before restarting. Also check free space:
SQLite and object installation require room for temporary and durable files.

Do not delete `.wikisyncd.sock`, `.wikisync-writer.sock`, `.wikisync-ipc.lock`, `tmp/`,
loose objects, packs, or WAL files as a generic repair step. Normal IPC shutdown
removes both sockets. After a crash, cooperating WikiSyncer processes serialize
startup through the private advisory lock and replace only a nonresponsive socket
whose device/inode identity remains unchanged. Active sockets, symlinks, and regular
files are preserved and cause startup to fail safely. Manual socket removal should
therefore be unnecessary; investigate any repeated recovery failure before changing
the library.

## Database checks versus canonical verification

After taking a whole-library backup and stopping all WikiSyncer processes, an operator
with `sqlite3` can run a read-only structural check:

```sh
sqlite3 -readonly /absolute/path/to/library/library.sqlite3 'PRAGMA quick_check;'
```

An `ok` result covers SQLite structure only. It does not read, reconstruct, or hash
the canonical content objects. Use the GUI's **Verify full library** action for the
implemented logical-object verification. It checks every catalogued object through
the store's normal loose/pack reconstruction and identity validation. The current
verifier does not yet validate every planned manifest, revision-chain, search-pointer,
or cache invariant, so report its exact scope rather than saying the archive is
universally verified.

If verification reports corruption, stop synchronization, keep the original library
unchanged, and make a permission-preserving copy for investigation. Restoring a known
good whole-library backup is safer than manually replacing individual pack, index,
database, or loose-object files.

## Minimal issue report

After redaction, a useful report contains:

- operating system and architecture;
- WikiSyncer and Rust build versions or commit;
- install method and service-template revision;
- whether the failure followed sleep, wake, upgrade, cancellation, disk pressure, or
  an unclean shutdown;
- daemon `health`/`status` exit results and CLI status aggregate counts;
- service-manager state and a short, reviewed log excerpt;
- library filesystem type, free space, owner, and permission modes; and
- whether SQLite quick-check and full logical-object verification passed, failed, or
  were not run.

Do not reproduce a network failure against a live source solely for a report. Routine
project tests use loopback fixtures, and an offline failure should remain offline
until the evidence shows that source reachability is relevant.
