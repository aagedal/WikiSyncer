# User service management

`wikisyncd --library <absolute-path> run` is a foreground process for one library. It
exposes local control commands through `<library>/.wikisyncd.sock` and holds exclusive
cooperative writer ownership through `<library>/.wikisync-writer.sock`. Both sockets
are created with user-only permissions. `status`, `health`, and `shutdown` use the
same `--library` argument. `WIKISYNC_LIBRARY` is also accepted, but the templates
deliberately use an explicit absolute path.

This checkpoint provides the single-writer IPC foundation and application dispatch,
not the finished scheduler. GUI and CLI clients can forward collection sync, logical-
object verification, and compaction to the daemon; those operations open the store
and synchronization contacts the configured MediaWiki source. Nothing runs
unattended until scheduling is implemented and configured.

The service integration in `packaging/` is opt-in and per-user. Do not install it as
root or convert it into a system service: the daemon and library should have the same
unprivileged owner.

## Before installing

1. Build or obtain a trusted `wikisyncd` executable and record its absolute path.
2. Create or open the target library through WikiSyncer. Confirm that
   `library.sqlite3` exists at its root.
3. Stop any manually started daemon for that library:

   ```sh
   /absolute/path/to/wikisyncd --library /absolute/path/to/library shutdown
   ```

4. Ensure the library is on a local filesystem that supports Unix permissions and
   local Unix sockets. Network shares, cloud-synchronized folders, and removable
   volumes that disappear during use are not supported operating locations. The
   hardened systemd template also uses a private temporary directory, so do not put
   its library or executable below `/tmp`.
5. Confirm that the resolved paths do not pass through a directory writable by
   another user. WikiSyncer hardens the library directories to `0700`, the SQLite
   database and its WAL files to `0600`, and daemon sockets to `0600` when it opens
   them. Parent-directory and backup permissions remain the operator's responsibility.

The template grants write access only to the complete library tree because the daemon
creates its two sockets there and owns library mutation. Its network sandbox permits
outbound IPv4/IPv6 for explicitly requested synchronization. It does not need root,
inbound network, or access to unrelated home-directory content. The local reader is
a separate loopback-only process; these templates do not expose it.

## macOS launchd user agent

Copy `packaging/launchd/org.wikisync.WikiSyncer.plist.in` to a private working file.
Replace `@WIKISYNCD@`, `@LIBRARY@`, and `@LOG_DIRECTORY@` with absolute paths. Create
the log directory with mode `0700`, validate the result, then place it in the user
agent directory:

```sh
mkdir -p "$HOME/Library/Logs/WikiSyncer" "$HOME/Library/LaunchAgents"
chmod 700 "$HOME/Library/Logs/WikiSyncer" "$HOME/Library/LaunchAgents"
plutil -lint /path/to/rendered/org.wikisync.WikiSyncer.plist
install -m 600 /path/to/rendered/org.wikisync.WikiSyncer.plist \
  "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer.plist"
launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer.plist"
```

The agent starts at login and restarts after unexpected failure. Inspect it without
making a network request:

```sh
/absolute/path/to/wikisyncd --library /absolute/path/to/library status
/absolute/path/to/wikisyncd --library /absolute/path/to/library health
launchctl print "gui/$(id -u)/org.wikisync.WikiSyncer"
```

The current daemon does not handle `SIGTERM` or `SIGINT`. A direct `launchctl bootout`
or logout can therefore terminate it without the cooperative IPC shutdown path. This
is a known Milestone 4 gap, not a graceful-shutdown guarantee. Before uninstalling,
upgrading, or deliberately stopping the agent, request IPC shutdown, wait for the
process to exit successfully, and only then unload its definition:

```sh
/absolute/path/to/wikisyncd --library /absolute/path/to/library shutdown
launchctl bootout "gui/$(id -u)/org.wikisync.WikiSyncer"
rm "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer.plist"
```

After shutdown, `status` and `health` fail with a connection-not-found or
connection-refused error; use `launchctl print` to distinguish an unloaded agent from
one that is repeatedly failing. Removing the service does not remove the library or
logs. Review and remove the two log files separately only if they are no longer
needed.

## Linux systemd user service

Render `packaging/systemd/wikisyncd.service.in` by replacing `@WIKISYNCD@`,
`@LIBRARY@`, and `@DOCUMENTATION_DIRECTORY@` with absolute paths. Render the optional
health service and timer the same way. Validate rendered units before installing
them. If the installed systemd version does not support one of the hardening
directives, verification fails; review the compatibility tradeoff instead of silently
dropping all hardening:

```sh
systemd-analyze --user verify /path/to/rendered/wikisyncd.service
systemd-analyze --user verify /path/to/rendered/wikisyncd-health.service \
  /path/to/rendered/wikisyncd-health.timer
mkdir -p "$HOME/.config/systemd/user"
chmod 700 "$HOME/.config/systemd" "$HOME/.config/systemd/user"
install -m 600 /path/to/rendered/wikisyncd.service \
  "$HOME/.config/systemd/user/wikisyncd.service"
systemctl --user daemon-reload
systemctl --user enable --now wikisyncd.service
```

The persistent service restarts only after an unexpected failure. `systemctl stop`
invokes the local `shutdown` command, then waits for the main PID to exit and release
its sockets. The combined stop sequence allows up to 60 seconds for the active
request to finish. If that deadline expires, systemd may use an abrupt signal that
the current daemon does not handle gracefully. Enable the optional health probe
independently:

```sh
install -m 600 /path/to/rendered/wikisyncd-health.service \
  "$HOME/.config/systemd/user/wikisyncd-health.service"
install -m 600 /path/to/rendered/wikisyncd-health.timer \
  "$HOME/.config/systemd/user/wikisyncd-health.timer"
systemctl --user daemon-reload
systemctl --user enable --now wikisyncd-health.timer
```

The timer only checks the local Unix socket. It is not a synchronization timer.
Without login lingering, a user manager normally stops at logout; deciding to enable
lingering is an administrator policy choice and is not required by WikiSyncer.

To uninstall, stop and disable the timer and service, remove only their rendered unit
files, then reload the user manager:

```sh
systemctl --user disable --now wikisyncd-health.timer wikisyncd.service
rm "$HOME/.config/systemd/user/wikisyncd.service" \
  "$HOME/.config/systemd/user/wikisyncd-health.service" \
  "$HOME/.config/systemd/user/wikisyncd-health.timer"
systemctl --user daemon-reload
systemctl --user reset-failed
```

## Sleep, wake, and interrupted work

Do not create a second timer that launches another daemon for the same library. The
single daemon is responsible for serializing mutation; its durable run/job records
and checkpoints make interrupted work resumable. A checkpoint advances only after
the corresponding jobs are durably successful.

After a long sleep or an offline interval, first run `health` and inspect daemon
`status`. The presence of a running daemon does not prove that a synchronization ran
or that no revisions were missed. Long-gap reconciliation exists in the sync engine,
but schedule controls and an end-to-end service gate for sleep/wake behavior remain
Milestone 4 work. Do not claim unattended sleep/wake acceptance until that gate passes.

Before shutting down the computer, moving the library, or taking a backup, request a
cooperative stop and confirm both that the service manager has no daemon process and
that `status` can no longer connect. A forced kill should be reserved for a daemon
that cannot respond; durable sync jobs are designed to resume, but the current daemon
does not provide a graceful signal path and an offline whole-library copy must not be
taken while files are changing.

An ordinary IPC shutdown removes both socket files. After a crash, the daemon fails
closed rather than automatically unlinking a socket path, because unlinking after a
racy ownership check could interfere with another process. If a subsequent start
reports a stale socket, keep the service disabled, use service-manager state plus a
tool such as `lsof` to establish that no process owns either socket, and retain a
backup before removing only the confirmed stale socket files. Never treat socket
removal as a repair for database or object-store errors.
