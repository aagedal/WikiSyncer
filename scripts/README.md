# Release-mode offline audit

`release_offline_audit.py` builds the locked release binaries, initializes an empty
temporary library, and exercises offline CLI commands, the idle daemon and Unix-socket
IPC, and a browser-like crawl of the default loopback reader:

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

The empty-library release-process crawl complements the populated-library route and
media crawl in `crates/wikisync-web`'s `offline_crawl_has_no_outbound_resource_requests`
test; keep both so release syscall evidence does not replace representative markup
coverage.

This is credential-free release acceptance evidence, not a general sandbox. It covers
the dynamically linked libc networking paths used by the shipped Rust binaries and
the default offline surfaces exercised by the script. It does not claim to audit
explicit synchronization, `doctor --online`, GUI actions that request synchronization,
statically linked/direct-kernel networking, browser extensions, or another process on
the host. Run it natively on every supported macOS and Linux release runner.
