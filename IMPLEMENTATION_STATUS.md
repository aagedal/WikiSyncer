# WikiSyncer implementation status

Updated: 2026-08-20

## Completed backlog items

1. Cargo workspace, formatting and lint policy, macOS/Linux CI, and dependency
   license/source policy.
2. Validated core identities, explicit-title and category collection rules, and all
   planned history-policy variants.
3. Bounded MediaWiki Action API client, 100-title batching spike, resumable revision
   metadata pagination, retry classification, and offline loopback fixture harness.

## Next backlog item

4. Create SQLite migration 1 and the atomic loose content-object store, keeping
   physical locations separate from logical content identity.

Milestone gates remain tracked in `IMPLEMENTATION_PLAN.md`; an item being complete
means its initial implementation is present, not that its later milestone hardening
is finished.
