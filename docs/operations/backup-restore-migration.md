# Backup, restore, and migration

WikiSyncer does not yet publish a stable backup-file contract. The safe beta procedure
is a quiescent, whole-directory copy of the library. This preserves the SQLite
database, its WAL state, immutable loose objects, packfiles and indexes, and any
future manifest or export directories together.

Never treat a copy of `library.sqlite3` alone as a complete backup. Canonical revision
bytes live below `objects/`, and a live SQLite database may also have `-wal` and
`-shm` files. Derived cache and exports are not substitutes for canonical objects.

## Create a backup

1. Record the WikiSyncer binary version and the library path.
2. Stop the daemon cooperatively and stop any GUI, CLI writer, or reader using the
   library. `systemctl --user stop wikisyncd.service` invokes cooperative shutdown.
   On macOS, or for a manually started daemon, call shutdown directly before unloading
   the launchd agent:

   ```sh
   /absolute/path/to/wikisyncd --library /absolute/path/to/library shutdown
   # For the installed macOS user agent, unload it after shutdown succeeds:
   launchctl bootout "gui/$(id -u)/org.wikisync.WikiSyncer"
   ```

3. Confirm the daemon is stopped. If the control socket remains after an unclean
   crash, the next start intentionally fails closed. Do not delete the socket until
   service-manager state and process inspection establish that no process owns the
   library. A stopped daemon's `status` command normally returns a connection error.
4. Copy the library root to a new, user-only destination on a filesystem that
   preserves permissions. For example:

   ```sh
   umask 077
   mkdir -p /absolute/path/to/backup-parent
   cp -a /absolute/path/to/library \
     /absolute/path/to/backup-parent/library-YYYYMMDD-HHMMSS
   ```

5. Compare the copied file count and allocated size with the source. Open the copy
   with the same WikiSyncer version, inspect `wikisync ... status`, and run the GUI's
   full-library verification. Full verification reconstructs and hashes logical
   canonical objects; a quick SQLite check alone does not.
6. Restart the original service only after the copy has finished.

Backups can contain material later deleted or suppressed upstream, public editor
identifiers, and exact article history. Store them with user-only permissions and
full-disk encryption where available. A separately encrypted, offline copy protects
against disk loss; ordinary cloud sync is not a safe live-library location.

## Restore without overwriting the existing library

Restore into a new empty destination so the existing library remains recoverable:

1. Stop all WikiSyncer processes for both source and destination paths.
2. Copy the backup directory to a new local path using a permission-preserving tool.
3. Set directory access to the owning user. WikiSyncer re-applies `0700` to library
   directories and `0600` to SQLite files it opens, but operators must also secure
   parent directories and any other copied files.
4. Reject a restore whose `library.sqlite3` is a symbolic link. Do not combine a
   database from one backup with `objects/` from another.
5. Open the restored path with the same binary version first, inspect status, and run
   full object and manifest-chain verification.
6. Render a new service definition with the restored absolute path. Stop and remove
   the old service definition before enabling the new one.

A restored library whose full check passes demonstrates internal object and manifest-
chain consistency since capture. Without an externally retained signed manifest head,
it cannot detect replacement by an older internally consistent backup or prove
completeness relative to Wikimedia. No current command certifies that the backup
includes every filesystem artifact expected by a future stable format.

## Upgrade and schema migration

`Library::open` applies forward SQLite migrations automatically. At this checkpoint
the repository schema version is 6, but that number and the migration contract are
not yet declared stable. Merely running an offline CLI command such as `status` opens
the library and can migrate it.

For an upgrade:

1. Stop the daemon and make a tested whole-library backup with the old binary.
2. Keep the old binary and backup together. Do not test a new binary against the only
   copy.
3. Restore the backup to a staging path and open that copy with the new binary.
4. Inspect status, exercise representative search/show/history operations, and run
   full verification before upgrading the production path.
5. Stop the old service, update its executable path only after the checks pass, and
   start it once. Do not run old and new daemons concurrently.

Downgrades are not guaranteed. If rollback is required, stop the new binary and
restore the pre-upgrade directory as a unit; do not attempt to edit `PRAGMA
user_version` or reverse migrations manually.

## Move to another machine or filesystem

Use the restore procedure rather than moving a live directory. Preserve ownership and
permissions, install a compatible executable for the destination platform, and
re-render service templates with destination-specific absolute paths. Unix sockets
are runtime files and must not be used as evidence that a copied daemon is alive.
Start only one destination after verification to avoid two independent writers being
mistaken for one synchronized service.
