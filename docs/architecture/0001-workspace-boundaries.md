# ADR 0001: Begin with explicit workspace boundaries

- Status: accepted
- Date: 2026-08-20

## Context

WikiSyncer needs several user interfaces around one implementation of capture,
storage, search, and integrity behavior. Those boundaries need to remain testable
without forcing every executable to compile every interface dependency.

## Decision

Use one Cargo workspace with thin application crates for the CLI, GUI, and daemon.
Put reusable behavior in the domain-oriented library crates described in the
implementation plan. Applications may compose libraries; libraries must not depend
on application crates.

The initial crates are intentionally skeletal. A boundary may be consolidated when
implementation evidence shows it does not provide independent testing, dependency,
or ownership value.

## Consequences

- Domain behavior can be tested independently of Iced, Axum, and process startup.
- Some early crates contain only a documented placeholder until their milestone.
- Cross-cutting types belong in `wikisync-core`; interface-specific types do not.

