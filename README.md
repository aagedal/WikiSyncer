# WikiSyncer

WikiSyncer is a selective, tamper-evident offline history of Wikipedia source
revisions. The project is in its architecture-spike phase; see
[IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for the planned capabilities and
delivery milestones.

## Development

WikiSyncer targets macOS and Linux and uses the current stable Rust toolchain.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Inspect durable synchronization progress without network access:

```sh
wikisync --library /path/to/library status
wikisync --library /path/to/library status --json
```

The workspace is licensed under GPL-3.0-only. New dependencies must pass the
license and source policy in `deny.toml`.
