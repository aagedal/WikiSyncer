# ADR 0003: Use a conservative deterministic derived renderer

- Status: accepted
- Date: 2026-08-25

## Context

WikiSyncer must render arbitrary captured revisions while offline without changing the
canonical wikitext. The result must be safe to show in the loopback reader,
deterministic enough to key persistent search/export contracts by transformer version,
and bounded when the input is malformed or adversarial. Exact historical MediaWiki
presentation is an explicit non-goal because it also depends on template, Lua,
Wikidata, extension, and site-configuration state that may no longer be available.

The evaluation used these criteria, in priority order:

1. canonical wikitext remains the only historical evidence;
2. every captured revision remains readable without a source connection;
3. derived output is deterministic, versioned, and locally rebuildable;
4. malformed input has explicit work and nesting bounds;
5. reader output cannot introduce active upstream HTML or remote assets; and
6. common article structure remains useful for reading, search, diff, and export.

## Options considered

### Render through MediaWiki or another remote service

This offers the closest current-site presentation, but fails the offline requirement
and cannot reliably reproduce the historical dependency graph for an old revision.
It would also make reading captured material depend on a mutable external service.

### Embed a compatibility-focused MediaWiki renderer

A full parser plus template/Lua/Wikidata execution could improve visual fidelity, but
would add a large, mutable dependency graph and substantially broaden resource-exhaustion
and active-content risk. Without dependency snapshots it still would not reproduce an
old page exactly. This remains a possible post-v1 advanced mode, not the default
reader path.

### Derive a conservative local document representation

The selected implementation recognizes common headings, paragraphs, lists, tables,
links, emphasis, references, code, captions, and a small text-preserving template
allowlist. Unknown templates become readable labeled placeholders. Plain text and
minimal Markdown have separate explicit transformer versions. Search fields, diffs,
exports, and reader HTML are derived from those local representations.

The reader converts the generated Markdown through a bundled event parser, rewrites
same-wiki article links to local routes, escapes generated metadata, and serves only
bundled styling under the existing restrictive response policy. Canonical source is
still available separately and is never rewritten by this path.

## Decision

Use the conservative local transformer as the stable default. Treat all of its output
as rebuildable derived content, and change the relevant transformer version whenever
observable output changes. Do not contact MediaWiki while rendering, expand arbitrary
templates, execute Lua, trust upstream HTML, or imply pixel/historical fidelity.

The golden article fixtures, malformed-input tests, maintained fuzz target, offline
vertical slice, and reader crawl are the acceptance evidence for this decision.

## Consequences and limits

- Captured revisions remain readable and diffable with no network or historical
  template dependency.
- Search and export behavior can be rebuilt and audited against a named transformer
  version.
- Complex templates and site-specific layout intentionally lose presentation detail;
  the UI must offer exact source and describe the reading view as derived.
- A future compatibility renderer must remain optional, consume the same immutable
  canonical objects, have independent cache/version keys, and pass the same outbound,
  sanitization, and resource-bound gates.

