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

Serve the read-only local encyclopedia on the loopback interface:

```sh
wikisync --library /path/to/library serve
wikisync --library /path/to/library serve --port 8765
```

The reader loads only bundled or embedded resources and refuses non-loopback
listeners. Open `http://127.0.0.1:8080/` when using the default port.

The workspace is licensed under GPL-3.0-only. New dependencies must pass the
license and source policy in `deny.toml`.
