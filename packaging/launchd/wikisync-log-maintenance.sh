#!/bin/sh

# launchd owns the daemon's stdout/stderr descriptors. Rotating their path while
# the agent is running would leave it writing to an archived inode, so this helper
# unloads the agent before invoking newsyslog and always attempts to load it again.

set -u

if [ "$#" -ne 7 ]; then
    echo "usage: $0 WIKISYNCD LIBRARY NEWSYSLOG_CONFIG SERVICE_PLIST UID STDOUT_LOG STDERR_LOG" >&2
    exit 64
fi

daemon=$1
library=$2
newsyslog_config=$3
service_plist=$4
uid=$5
stdout_log=$6
stderr_log=$7

for absolute_path in \
    "$daemon" "$library" "$newsyslog_config" "$service_plist" \
    "$stdout_log" "$stderr_log"
do
    case "$absolute_path" in
        /*) ;;
        *) echo "log maintenance paths must be absolute" >&2; exit 64 ;;
    esac
done
case "$uid" in
    ''|*[!0-9]*) echo "log maintenance UID must be numeric" >&2; exit 64 ;;
esac

if [ ! -f "$newsyslog_config" ] || [ -L "$newsyslog_config" ]; then
    echo "newsyslog configuration must be a regular, non-symlink file" >&2
    exit 66
fi
if [ ! -f "$service_plist" ] || [ -L "$service_plist" ]; then
    echo "service plist must be a regular, non-symlink file" >&2
    exit 66
fi

rotation_bytes=10485760
needs_rotation=0
for log_path in "$stdout_log" "$stderr_log"; do
    if [ -L "$log_path" ]; then
        echo "refusing to rotate a symlink log path" >&2
        exit 66
    fi
    if [ -f "$log_path" ]; then
        log_size=$(/usr/bin/stat -f '%z' "$log_path") || exit 74
        if [ "$log_size" -ge "$rotation_bytes" ]; then
            needs_rotation=1
        fi
    fi
done

if [ "$needs_rotation" -eq 0 ]; then
    exit 0
fi

# If the daemon is not responding, do not assume launchd has closed its inherited
# descriptors. A stopped file cannot grow and can be considered on a later pass.
if ! "$daemon" --library "$library" status >/dev/null 2>&1; then
    exit 0
fi

domain="gui/$uid"
label="org.wikisync.WikiSyncer"
service_unloaded=0

restore_service() {
    if [ "$service_unloaded" -eq 1 ]; then
        /bin/launchctl bootstrap "$domain" "$service_plist" >/dev/null 2>&1 || true
    fi
}
trap restore_service EXIT
trap 'exit 75' HUP INT TERM

# bootout sends SIGTERM through the daemon's cooperative cancellation path and
# removes KeepAlive responsibility before the log names are changed.
if ! /bin/launchctl bootout "$domain/$label"; then
    exit 75
fi
service_unloaded=1

# The control socket is removed at the end of normal daemon shutdown. Keep the wait
# bounded so a broken daemon cannot hold this maintenance job forever.
wait_count=0
while "$daemon" --library "$library" status >/dev/null 2>&1; do
    wait_count=$((wait_count + 1))
    if [ "$wait_count" -ge 600 ]; then
        echo "daemon did not stop within 600 seconds; logs were not rotated" >&2
        exit 75
    fi
    /bin/sleep 1
done
/bin/sleep 1

rotation_status=0
/usr/sbin/newsyslog -r -s -f "$newsyslog_config" "$stdout_log" "$stderr_log" || rotation_status=$?

if /bin/launchctl bootstrap "$domain" "$service_plist"; then
    service_unloaded=0
    trap - EXIT HUP INT TERM
else
    echo "log maintenance could not reload the WikiSyncer agent" >&2
    exit 75
fi

exit "$rotation_status"
