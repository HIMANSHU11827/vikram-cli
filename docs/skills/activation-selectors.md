# Activation Selectors

How VIKRAM decides which skills are relevant to a given task. The selector is deterministic, stateless, and
runs against a `Catalog` — no LLM, no embeddings.

## The algorithm

`Selector::select(&self, &Catalog, &SelectionInput) -> Vec<Selection>`:

1. Tokenize the task string on whitespace and punctuation.
2. For every skill in the catalog, compute a score from name, tags, categories, and description overlap.
3. Drop skills below `min_score` (constructor argument, typically 0.5).
4. Sort descending by score; ties broken by name.
5. Return `Vec<Selection>` with `name`, `score`, `reason` (a short label explaining why it matched).

## Scoring

| Signal | Weight | Source |
| - | - | - |
| Exact name match | 10.0 | `SkillId` equals a token |
| `project` tag | +2.0 | skill tagged `project` |
| `role:<name>` tag | +2.0 | skill tagged with a role matching the task |
| Tag overlap | +1.0 per matching tag | tags in common with task tokens |
| Category overlap | +1.0 per matching category | categories in common |
| Description keyword | +0.5 per keyword | description words in task tokens |

A skill needs at least `min_score` to be returned. With `min_score = 0.5`, any skill whose name appears in the task
is always selected.

## `SelectionInput`

```rust
let input = SelectionInput::new("fix the database migration");
let selections = selector.select(&catalog, &input);
```

The task string is borrowed (`&'a str`) and must outlive the returned `Vec<Selection>`.

## `Activation`

`Activation` is the output of the selection process — a named, reasoned intent to load a skill:

```rust
Activation::new(name, source, reason)
    .for_agent(agent_id)      // restrict to one agent
    .in_scope(scope)          // restrict to one scope
    .with_resources(vec![...]) // pre-declare resources to load
```

## `ActivationSet`

A per-turn collection of activations. Methods: `activate`, `deactivate`, `contains`, `names`, `get`, `len`,
`is_empty`, `clear`. A set is cleared at the end of every turn (per-turn activation, no carry-over).

## Integration with the turn loop

The runtime calls `Selector::select` before each `CompletionRequest`, activates the top matches, and injects their
metadata (never their bodies) into the catalogue message. Bodies are loaded on demand via `Loader::load` only when
the model explicitly requests them through `skill.load`.

## Tuning

- Raise `min_score` to reduce false positives (fewer skills activated per turn).
- Lower `min_score` to increase recall (more skills considered).
- The catalogue message itself is budget-capped at `catalog_bytes` (default 16,000); skills that do not fit are
  omitted with a count marker.
