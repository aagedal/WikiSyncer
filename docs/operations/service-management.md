# User service management

`wikisyncd --library <absolute-path> run` is a foreground process for one library. It
exposes local control commands through `<library>/.wikisyncd.sock` and holds exclusive
cooperative writer ownership through `<library>/.wikisync-writer.sock`. Both sockets
are created with user-only permissions. `status`, `health`, and `shutdown` use the
same `--library` argument. `WIKISYNC_LIBRARY` is also accepted, but the templates
deliberately use an explicit absolute path.

The daemon executes durable per-collection interval and daily-UTC schedules configured
in the GUI. Each occurrence receives deterministic delay-only jitter. The next run is
advanced atomically before synchronization starts, so restart or wake coalesces missed
occurrences into one resumable run instead of a catch-up storm. Manual and paused
schedules never start automatically. GUI and CLI clients can also forward collection
sync, logical-object verification, and compaction to the daemon.

The service integration in `packaging/` is opt-in and per-user. Do not install it as
root or convert it into a system service: the daemon and library should have the same
unprivileged owner.

Every production MediaWiki and dump request uses
`WikiSyncer/<version> (<contact>)`. By default, `<contact>` is the public project
repository URL. Set `WIKISYNC_OPERATOR_CONTACT` to a public email address, URL, or
similar contact when an operator-specific value is appropriate. The value is sent to
the configured source: do not put a token, private address, or other secret there. It
must be 1–256 visible ASCII bytes without surrounding whitespace, parentheses, or
backslashes; an invalid value fails before a request is made and is not copied into
the error message.

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

The macOS distribution has a primary agent plus a log-maintenance companion. Copy the
two plist templates, the `newsyslog` template, and `wikisync-log-maintenance.sh` to a
private working directory. Replace `@WIKISYNCD@`, `@LIBRARY@`, `@LOG_DIRECTORY@`,
`@LOG_MAINTENANCE_SCRIPT@`, `@NEWSYSLOG_CONFIG@`, and `@SERVICE_PLIST@` with absolute
paths; replace `@UID@` and `@GID@` with `id -u` and `id -g`. The log-directory path
must not contain whitespace because `newsyslog.conf` is whitespace-delimited. Create
the directories with mode `0700`, validate both plists, then install all four files as
the same unprivileged user:

```sh
mkdir -p "$HOME/Library/Logs/WikiSyncer" \
  "$HOME/Library/Application Support/WikiSyncer/service" \
  "$HOME/Library/LaunchAgents"
chmod 700 "$HOME/Library/Logs/WikiSyncer" \
  "$HOME/Library/Application Support/WikiSyncer" \
  "$HOME/Library/Application Support/WikiSyncer/service" \
  "$HOME/Library/LaunchAgents"
plutil -lint /path/to/rendered/org.wikisync.WikiSyncer.plist
plutil -lint /path/to/rendered/org.wikisync.WikiSyncer-log-maintenance.plist
install -m 600 /path/to/rendered/org.wikisync.WikiSyncer.plist \
  "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer.plist"
install -m 600 /path/to/rendered/org.wikisync.WikiSyncer-log-maintenance.plist \
  "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer-log-maintenance.plist"
install -m 600 /path/to/rendered/wikisync-newsyslog.conf \
  "$HOME/Library/Application Support/WikiSyncer/service/newsyslog.conf"
install -m 700 packaging/launchd/wikisync-log-maintenance.sh \
  "$HOME/Library/Application Support/WikiSyncer/service/wikisync-log-maintenance.sh"
launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer.plist"
launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer-log-maintenance.plist"
```

To configure an operator-specific contact for the unattended daemon, add an
`EnvironmentVariables` dictionary containing `WIKISYNC_OPERATOR_CONTACT` to the
rendered primary plist before `plutil -lint`. XML-escape the public value. Leave the
dictionary out to use the repository-URL default; the maintenance companion does not
need the value because it performs no network requests.

The companion checks hourly. When either stream is at least 10 MiB, it confirms the
daemon control plane is responsive, unloads the primary agent so launchd closes its
output descriptors, waits up to ten minutes for cooperative termination, evaluates
both streams with `/usr/sbin/newsyslog`, retains four gzip archives per stream, and reloads
the primary agent. It never rotates a live or symlinked stream. If shutdown or rotation
fails, inspect `launchctl print` for the companion; the helper always attempts to
reload an agent it successfully unloaded.

Four archives bound retained generations, not instantaneous bytes. Each current file
can exceed 10 MiB by output written between hourly checks, and compression ratios vary.
This is a retention policy, not a hard disk quota. Use a filesystem quota or disk-space
monitor when an absolute storage ceiling is required.

The agent starts at login and restarts after unexpected failure. Inspect it without
making a network request:

```sh
/absolute/path/to/wikisyncd --library /absolute/path/to/library status
/absolute/path/to/wikisyncd --library /absolute/path/to/library health
launchctl print "gui/$(id -u)/org.wikisync.WikiSyncer"
launchctl print "gui/$(id -u)/org.wikisync.WikiSyncer-log-maintenance"
```

`SIGTERM`, `SIGINT`, and the IPC `shutdown` command all request cooperative shutdown
after the active operation completes. Before uninstalling, upgrading, or deliberately
stopping the agent, request IPC shutdown, wait for the process to exit successfully,
and only then unload its definition:

```sh
/absolute/path/to/wikisyncd --library /absolute/path/to/library shutdown
launchctl bootout \
  "gui/$(id -u)/org.wikisync.WikiSyncer-log-maintenance"
launchctl bootout "gui/$(id -u)/org.wikisync.WikiSyncer"
rm "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer.plist" \
  "$HOME/Library/LaunchAgents/org.wikisync.WikiSyncer-log-maintenance.plist"
```

After shutdown, `status` and `health` fail with a connection-not-found or
connection-refused error; use `launchctl print` to distinguish an unloaded agent from
one that is repeatedly failing. Removing the service does not remove the library or
logs. The installed maintenance script, configuration, current logs, and numbered
archives remain until the operator reviews and removes them separately.

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

For an operator-specific contact, add a private user-unit drop-in before starting the
service, then verify the merged unit. Omit the drop-in to use the repository-URL
default:

```ini
# ~/.config/systemd/user/wikisyncd.service.d/operator-contact.conf
[Service]
Environment="WIKISYNC_OPERATOR_CONTACT=mailto:operator@example.invalid"
```

The persistent service restarts only after an unexpected failure. `systemctl stop`
invokes the local `shutdown` command, then waits for the main PID to exit and release
its sockets. The combined stop sequence allows up to 60 seconds for the active
request to finish; `SIGTERM` also enters the same cooperative path. Enable the
optional health probe independently:

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

The unit explicitly sends stdout/stderr to journald and limits acceptance from the
daemon to 1,000 messages per 30 seconds (the health probe is limited to 100). This
limits a noisy unit's message rate but does not bound journal storage. journald storage
and age limits normally cover the whole system journal and require administrator
policy. Inspect the effective policy and current use before enabling unattended sync:

```sh
systemd-analyze cat-config systemd/journald.conf
journalctl --disk-usage
```

If the existing administrator policy is not bounded appropriately, an administrator
can add a drop-in such as the following and restart journald after assessing the
system-wide effect. These values are examples, not settings installed by WikiSyncer:

```ini
# /etc/systemd/journald.conf.d/60-journal-retention.conf
[Journal]
SystemMaxUse=512M
RuntimeMaxUse=128M
MaxRetentionSec=30day
```

`SystemMaxUse` and `RuntimeMaxUse` retain free-space headroom according to journald's
own rules; archived journals are removed to approach those limits. The limits include
other services, so do not describe them as a WikiSyncer-specific quota. Where an
administrator declines to set a bounded journal policy, external disk monitoring is
required and this service-log retention checkpoint is not operationally satisfied.

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
`status`. The durable schedule cursor starts at most one overdue run and moves its
next occurrence into the future. Long-gap reconciliation then compares stable page
IDs and captures intermediate revisions before advancing its checkpoint. The daemon
applies bounded retry/backoff and a shared circuit breaker to transient source
throttling. A real-daemon restart gate proves a retryable partial failure retains its
old head/checkpoint, reuses the durable intermediate revision, and resumes the same
run. Cooperative signal cancellation exits at bounded request/transaction boundaries
and leaves the run resumable. Each synchronization client and its clones enforce a
shared aggregate response-body budget (512 MiB by default) and at most four in-flight
requests by default. Migration 9 adds one durable library-wide transfer policy for
request concurrency, an optional aggregate downloaded-byte rate, and metered-network
avoidance. The GUI can edit that policy whether it owns the direct writer or forwards
through the daemon, and every new synchronization client applies it across clones and
retry responses. New sync runs snapshot the policy in their immutable configuration
hash.

On Linux, metered-network avoidance uses a bounded, local NetworkManager `nmcli`
probe. A connection reported as metered prevents foreground synchronization and leaves
an overdue scheduled occurrence unclaimed so it can run later. Conflicting, missing,
timed-out, or malformed probe results are reported as unknown and do not silently
claim that the connection is unmetered. macOS currently reports unsupported/unknown
because WikiSyncer does not yet have a reliable safe system API integration there.

Before shutting down the computer, moving the library, or taking a backup, request a
cooperative stop and confirm both that the service manager has no daemon process and
that `status` can no longer connect. A forced kill should be reserved for a daemon
that cannot respond; durable sync jobs are designed to resume, but an offline whole-
library copy must not be taken while files are changing.

An ordinary IPC shutdown removes both socket files. After a crash, startup is
serialized by `.wikisync-ipc.lock`; a nonresponsive Unix socket is removed only after
its device/inode identity is rechecked under that advisory lock, and the replacement
is bound before releasing the lock. Active sockets and unexpected paths are never
removed. This coordinates WikiSyncer processes inside the owner-private library; it
does not protect against a hostile process running as the same OS account. Never
treat socket removal as a repair for database or object-store errors.
