# Token Management

Token accounting in VIKRAM is measured, calibrated, and persisted. It never hardcodes a model's
window, never substitutes a raw character count for a token count, and never records a number the
provider did not report as though it had.

The code lives in [`context_engine.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/context_engine.rs)
and the durable ledger in [`vikram-store`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-store/src/lib.rs).

## Counting

`TokenEstimator` is the single structural counter for the whole runtime. It starts from
`vikram_kernel::TokenMeter::estimate` (a structural estimate over messages and text) and then
*calibrates*: every completed request compares its own estimate against the usage the provider
reported and nudges a learned characters-per-token ratio toward the truth.

- `estimate_text` / `estimate_message` / `estimate_messages` — the estimate path.
- `observe(estimated, reported)` — folds one provider report into the ratio.
- `ratio` / `observations` — how far calibration has run (0 observations means pure estimate).

There is exactly one counter. No subsystem keeps a second, divergent tally.

## Model limits

`ModelLimits::resolve(model, configured, advertised)` decides the window and output ceiling in
priority order, recording provenance in `LimitsSource`:

1. **Config** — an explicit user override.
2. **Provider** — the window the provider advertised for this model.
3. **Registry** — a built-in table (`REGISTRY`) keyed by model name.
4. **Assumed** — the floor `ASSUMED_CONTEXT_WINDOW` / `ASSUMED_MAX_OUTPUT`, used only when nothing
   else is known.

`is_assumed()` is true only for the last case, and the interface marks assumed figures (`*`) and
estimated ones (`~`) so the operator never reads a guess as a guarantee. The limit is never baked
into a call site; it is resolved per model every time.

## The usable-input budget

`ContextBudget::usable_input()` is what a request may actually carry:

```
usable = window − output_reserve − overhead − safety_margin
```

all saturating (it clamps at zero, it never goes negative). `output_reserve` is
`max_output_tokens`; `overhead` is the cost of the tool schemas plus the fixed prefix messages
(`estimate_overhead` / `system_messages_only`); `safety_margin_percent` is held back from every
model. `fit(tokens)` and `remaining(used)` answer the two questions the loop asks each turn.
`ContextStatus` pairs a budget with the tokens currently used and whether that figure is measured
or estimated.

## The persisted ledger

Every turn's cost is written to the `usage` table so accounting survives a restart and can be
summed across sessions. One row per turn:

| column | meaning |
| --- | --- |
| `session` | the conversation that spent it |
| `model` | the model charged |
| `prompt_tokens` / `completion_tokens` / `total_tokens` | the figures |
| `measured` | 1 = the provider reported this pair; 0 = estimator fallback |

`record_usage`, `list_usage`, `usage_totals`, `usage_grand_totals`, and `delete_usage_for_session`
back it; deleting a session cascades its usage rows. The runtime writes the row in
[`turn.rs`](file:///home/himanshu/Desktop/VIKRAM-CLI/crates/vikram-runtime/src/turn.rs): when the
provider reported input and output it records them as `measured`, otherwise it records the
estimator's numbers with `measured = false`. An estimate is always labelled an estimate — that is
the "no fake token statistics" rule made structural rather than a convention.
