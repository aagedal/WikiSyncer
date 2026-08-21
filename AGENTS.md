# WikiSyncer agent guide

## Project state

Before implementing work from the project backlog, read these files in order:

1. `IMPLEMENTATION_STATUS.md`
2. the relevant milestone and backlog sections of `IMPLEMENTATION_PLAN.md`
3. the manifests and public APIs of the crates that own the work

Treat `IMPLEMENTATION_STATUS.md` as a checkpoint, not as proof: verify the current
code and tests before depending on a completed item.

## Continuing the implementation plan

When the user asks to continue or complete the implementation plan:

- Start with the first incomplete backlog item unless the user names another scope.
- Turn the item into coherent, testable checkpoints. Finish one checkpoint before
  beginning a dependent checkpoint.
- Keep canonical storage, synchronization durability, offline operation, and the
  integrity/trust language in `IMPLEMENTATION_PLAN.md` as hard constraints.
- Prefer fixture-backed tests. Do not make routine tests depend on live Wikimedia or
  another external service.
- Do not mark a backlog item complete while material requirements from that item are
  still missing. Record partial progress explicitly instead.

## Subagent coordination

When subagents are available and the current work divides cleanly:

- Assign independent workstreams with explicit crate or file ownership.
- Keep one integrating agent responsible for shared APIs, final documentation, and
  workspace-wide validation.
- Tell implementation subagents not to edit `IMPLEMENTATION_STATUS.md`; the
  integrating agent updates it after reviewing and validating the combined result.
- Avoid assigning concurrent edits to the same source file. If overlap is
  unavoidable, make one task an analysis/test-design task and leave the shared edit
  to the integrating agent.
- Require each subagent to report changed files, commands run, assumptions, and
  unfinished work.

## Validation and handoff

Run these checks after integration:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Use focused crate tests during development, but do not substitute them for the final
workspace checks. Update `IMPLEMENTATION_STATUS.md` only after the relevant checks
pass. The handoff must distinguish completed behavior, partial behavior, and the next
concrete checkpoint.

Do not commit, push, publish, install services, delete user data, call live application
services, or perform external writes unless the user explicitly authorizes that
action. Dependency and toolchain downloads required by an authorized implementation
may use the environment's normal approval flow.
