# Service templates

These templates run `wikisyncd` as the logged-in user. They do not install files,
create a library, configure synchronization schedules, or grant system-wide access.
The daemon remains in the foreground under the service manager and is the long-lived
writer for one library.

The current daemon has a graceful IPC `shutdown` command but no `SIGTERM`/`SIGINT`
handler. The systemd unit uses `ExecStop` to request IPC shutdown. launchd has no
equivalent stop hook, so operators must call `shutdown` before `bootout`; logout and
other signal-driven termination remain an explicit beta limitation.

Replace every template token before installation:

- `@WIKISYNCD@`: absolute path to the `wikisyncd` executable;
- `@LIBRARY@`: absolute path to an existing WikiSyncer library;
- `@LOG_DIRECTORY@`: existing user-only log directory (launchd only); and
- `@DOCUMENTATION_DIRECTORY@`: absolute path to `docs/operations` (systemd only).

Do not put shell syntax, environment variables, `~`, XML entities, or systemd `%`
specifiers in a replacement. Service managers do not perform ordinary shell
expansion here. Paths containing `@`, a newline, or a double quote are not supported
by this simple substitution format. The launchd template additionally requires XML
escaping for `&`, `<`, and `>`; choosing ordinary absolute paths avoids that issue.

`wikisyncd.service.in` is the persistent Linux service. The health service/timer is
optional and only runs the local `health` command every 15 minutes. It is not a sync
schedule, does not contact MediaWiki, and does not restart an intentionally disabled
daemon. Explicit GUI/CLI requests can forward synchronization, verification, and
compaction through the daemon, but schedule dispatch remains unfinished.

See [service-management.md](../docs/operations/service-management.md) for a cautious,
manual installation procedure. Packaging automation should perform the same token
validation and install user-owned files with mode `0600`.
