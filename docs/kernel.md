# Kernel — Single Execution Architecture

The kernel is the only thing that may act on effects. It decides *whether* an effect may run.

## Core types

- `Operation` trait: `name()` + `run(ctx) -> Outcome`.
- `Outcome`: `NotApplicable` | `Applied(Vec<Effect>)`.
- `RunContext`: session, turn, messages, pending, grants (snapshot), meter, state, cancel,
  finished, awaiting, mode.
- `Effect`: EmitMessage, RequestTool, StreamDelta, Status, Activity, Compaction.
- `Machine`: owns ops, journal, policy, handler, grants, clock, limits.
- `RunReport`: steps, effects_applied/skipped/denied/deferred, converged.

## Properties

1. **Resumable** — every effect is journalled before applied; effect id already present
   means skipped, not repeated.
2. **Auditable** — journal is the record.
3. **Safe by construction** — permission evaluated in kernel; operations cannot reach grant set.

## Loop

`Machine::run` loops up to `max_steps` (64). Each pass runs all operations in registration
order, collects applied effects, dispatches each: journal append (idempotency check), policy
evaluation for RequestTool, posture resolution, then handler.apply. Stops when `finished`
or no progress.

## Grants

`GrantSet` widened by `ApprovalBroker` (issues `ApprovalTicket`). Grants are scoped by
action + resource. `GrantSnapshot` is what `RunContext` sees (read-only).

## Cancellation

`CancellationToken` shared. Cancelled turns finalize: append Cancellation record and
synthesize tool results for every pending call.

## Clock / TokenMeter

`Clock` trait (SystemClock/FixedClock). `TokenMeter` estimates tokens (len/4) and records
reported usage.

## Files

- `crates/vikram-kernel/src/machine.rs`
- `crates/vikram-kernel/src/effects.rs`
- `crates/vikram-kernel/src/grants.rs`
- `crates/vikram-kernel/src/run.rs`

## Tests

28 unit tests (dispatch, idempotency, cancellation, posture, budgets).