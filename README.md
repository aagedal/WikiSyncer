# WikiSyncer

WikiSyncer is a selective, tamper-evident offline history of Wikipedia source
revisions. Active implementation has passed the Milestone 3 GUI gate and delivered
the Milestone 4 daemon, scheduling, transport-policy, and unsigned manifest-chain
foundations; see
[IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) for the current checkpoint and
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for planned capabilities and delivery
milestones.

Pre-beta security assumptions and operational guidance are tracked in
[the threat model](docs/security/THREAT_MODEL.md) and
[the operations guide](docs/operations/README.md). Parameterized user-service assets
live under [`packaging/`](packaging/README.md); they are not signed installers.

## Development

WikiSyncer targets macOS and Linux and uses the current stable Rust toolchain.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Maintained bounded fuzz targets and seed corpora live under [`fuzz/`](fuzz/README.md).
The native release-mode offline audit is documented under
[`scripts/`](scripts/README.md) and is run by the macOS/Linux release-candidate
workflow after building the locked release binaries.

Inspect durable synchronization progress without network access:

```sh
wikisync --library /path/to/library status
wikisync --library /path/to/library status --json
```

Export current captured heads for local AI tools without network access:

```sh
wikisync --library /path/to/library export --format markdown
wikisync --library /path/to/library export --format text --collection 1
```

Export a historical slice without replacing `exports/current` by selecting an
inclusive captured revision ID or RFC 3339 time:

```sh
wikisync --library /path/to/library export --format markdown --at 1300000000
wikisync --library /path/to/library export --format text --at 2026-08-20T12:00:00Z
```

Current and historical exports are private, deterministic derived views with source,
revision, capture, transformer, and content-hash provenance. They are not canonical
backups or integrity evidence.

`collection remove` only stops tracking and retains captured history. Reclaiming
collection-exclusive canonical payload is a separate preview-first operation for an
already tombstoned collection:

```sh
wikisync --library /path/to/library purge preview --collection 1
```

Execution requires the exact previewed collection name and fingerprint plus separate
acknowledgements that audit evidence remains and external copies are not erased. The
same contract is available through the daemon and Iced GUI; see the
[destructive-purge contract](docs/operations/destructive-purge.md) before using it.

Serve the read-only local encyclopedia on the loopback interface:

```sh
wikisync --library /path/to/library serve
wikisync --library /path/to/library serve --port 8765
```

The reader loads only bundled or embedded resources and refuses non-loopback
listeners. Open `http://127.0.0.1:8080/` when using the default port.

The workspace is licensed under GPL-3.0-only. New dependencies must pass the
license and source policy in `deny.toml`.
