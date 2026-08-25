# Release-mode offline audit

`release_offline_audit.py` builds the locked release binaries, initializes an empty
temporary library, and exercises offline CLI commands, the idle daemon and Unix-socket
IPC, a browser-like crawl of the default loopback reader, and the no-action default
launch of the release-candidate `wikisync-gui` executable:

```sh
python3 scripts/release_offline_audit.py
```

Pass `--skip-build` to audit binaries already present in `target/release`, or
`--binary-dir` to inspect another native release directory. The audit supports macOS
and Linux and requires a C compiler.

The audited processes load a temporary native interposer that records and denies
IPv4/IPv6 `connect`, addressed `sendto`/`sendmsg`, and hostname-resolution calls.
Unix-domain daemon IPC and inbound requests to the loopback listener are not blocked.
Any recorded attempt fails the audit. The reader crawl separately rejects remote
`src` and stylesheet URLs and CSS `url()` references.

The GUI launch receives no application input beyond `WIKISYNC_LIBRARY`, which points
at the temporary initialized library, and it receives no UI input or synchronization
request. It must remain alive throughout a bounded three-second initialization window
and is then stopped with `SIGTERM`; an early exit is a failure. On Linux, the launch
runs only when `DISPLAY` or `WAYLAND_DISPLAY` identifies a graphical session. On macOS
it runs only under an Aqua launchd session. A missing graphical session is printed as
“GUI launch not audited”, so that a headless pass is not mistaken for GUI evidence.
Run the audit again in a native graphical session before recording the GUI acceptance
cell as passed.

The empty-library release-process crawl complements the populated-library route and
media crawl in `crates/wikisync-web`'s `offline_crawl_has_no_outbound_resource_requests`
test; keep both so release syscall evidence does not replace representative markup
coverage.

This is credential-free release acceptance evidence, not a general sandbox. It covers
the dynamically linked libc networking paths used by the shipped Rust binaries and
the default offline surfaces exercised by the script. It does not claim to audit
explicit synchronization, `doctor --online`, GUI actions after launch, statically
linked/direct-kernel networking, browser extensions, or another process on the host.
Run it natively on every supported macOS and Linux release runner, including at least
one graphical session per supported GUI platform.
