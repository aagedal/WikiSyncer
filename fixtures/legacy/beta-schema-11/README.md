# WikiSyncer beta schema-11 library

This directory is a complete, offline WikiSyncer library captured at database schema
version 11. It is retained as immutable migration evidence rather than reconstructed
by the migration test.

The fixture contains one English source, one configured collection with a recurring
schedule and non-default transfer policy, one unresolved title, and one page whose
two canonical revisions and old/current titles are retained as loose Zstandard
objects. Schema 11 predates thumbnail media, dump-import, and purge metadata.

`crates/wikisync-store/tests/older_beta_migration.rs` copies this directory, reads its
publicly observable state without modifying it, opens the copy through the current
writer API, and proves the same sources, identities, metadata, configuration, and
policies remain after migration and a second reopen.

Fixture SHA-256 values at creation:

- `library.sqlite3`: `d0053b6cb9458af47b8d466c834687d0de037070bd2519c3eb52f8c61d4be381`
- revision 1001 object: `88e5ff4c6f0d35649722967c2078fe6036ae67e168dbb177660f70759f6d66fd`
- revision 1002 object: `24d3cae3e667fb0496b65728f9c54f18785a01774e3f1c888b80b2c8627689f7`
