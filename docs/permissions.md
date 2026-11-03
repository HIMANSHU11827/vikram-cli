# Permissions & Postures

VIKRAM uses a deny-first permission model with a non-widenable mandatory floor. All
permission decisions are runtime-enforced, never UI-enforced.

## Vocabulary

- `ActionKind`: Read, Write, Execute, Network, Delegate, Spend, Admin.
- `SideEffect`: None, LocalMutation, ExternalChange, Destructive.
- `PermissionRequest`: action, verb (e.g. `shell.run`), resource, side_effect, originating_trust.
- `Decision`: Allow, Deny { reason }, RequireApproval { reason, amendment }.
- `PermissionMode` (posture): Ask, AcceptEdits, Plan, Smart, Unattended, Yolo, AllowList.
- `PermissionPolicy` trait: `evaluate` / `evaluate_at_depth` / `is_specially_allowed` / `workspace_root`.
- `MandatoryFloor`: workspace_root, extra_write_roots, forbidden_from_untrusted, max_delegation_depth.

## Evaluation order

1. `MandatoryFloor::veto()` — returns Some(reason) → Deny. Checks:
   - untrusted content calling `grant.`/`policy.`/`self.` verbs;
   - delegation depth >= max_delegation_depth (3);
   - Write outside workspace root (and extra roots).
2. Operator-authored rules (config + saved allow-list): deny immediate, ask accumulates,
   allow sets flag.
3. Built-in defaults (reads allowed, workspace writes allowed, execute/network ask).
4. If no rule matched → Deny ("no rule permits").
5. `PermissionMode::resolve()` applies posture to a `RequireApproval` (never to a Deny).

## Postures

- **Ask** (default): reads run; gated actions prompt.
- **AcceptEdits**: ask + workspace writes auto-approved.
- **Plan**: read-only; mutations refused.
- **Smart**: deterministic risk tiers (no model call).
- **Unattended**: prompts become denials ("do not retry").
- **Yolo**: prompts auto-approved; floor still applies.
- **AllowList**: only operator-saved rules run quietly.

## Allow-list

Durable rules in `~/.vikram/rules.json`. Amendments proposed by the runtime
(`amendment_for`) are parsed back strictly (`parse_amendment`). Managed via
`vikram allow list|add|rm` and `RuntimeHandle::allow`/`unallow`.

## Depth

`evaluate_at_depth(request, depth)` supports delegation. Grants (`GrantSet`) are checked
separately: policy may allow but grants can still block ("not by this agent's grants").

## Trust

`Trust`: Host > User > Model > External (authority ordering). Originating trust travels
with content; untrusted content cannot grant authority.

## Files

- `crates/vikram-contracts/src/policy.rs` — vocabulary.
- `crates/vikram-policy/src/lib.rs` — evaluator + floor + amendment parsing.
- `crates/vikram-kernel/src/machine.rs` — kernel dispatch applies posture.
- `crates/vikram-runtime/src/rules.rs` — RuleBook persistence.
- `crates/vikram-runtime/src/runtime.rs` — `judge()`, `set_mode()`, `allow()`.

## Tests

- `vikram-policy`: 18 unit tests (deny-first, floor, posture resolution, amendments).
- `vikram-kernel`: 28 unit tests (dispatch, idempotency, cancellation, posture).
- `vikram-runtime`: posture/allow-list tests.