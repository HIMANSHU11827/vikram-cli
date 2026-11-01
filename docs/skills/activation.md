# Skill activation

Activation is per-turn and budgeted. A skill is never "always on".

## The three routes

1. **Explicit `$name`** — the user types `$skill-name` or `[$name](skill://path)`. `TOOL_MENTION_SIGIL = '$'`; name
   characters include `a-z A-Z 0-9 _ - :` (the colon is for namespaced plugin skills).
2. **Structured selection** — a `UserInput::Skill { name, path }` is resolved **by path first**, then by plain name
   only when the name is unambiguous. Two skills with the same name make the plain name ambiguous, so it does not
   resolve.
3. **Implicit** — the catalogue tells the model: "If the user names a skill … OR the task clearly matches a skill's
   description shown above, you must use that skill for that turn." Implicit activation can be disabled per skill via
   `policy.allow_implicit_invocation: false` in `agents/openai.yaml`.

## Per-turn scoping

"Do not carry skills across turns unless re-mentioned." Activation is reset at the end of each turn. The
`ActivationSet` tracks which skills are active for the current turn; `activate`, `deactivate`, `contains`, `names`,
`get`, `len`, `is_empty`, `clear`.

## Selection

`Selector::new(min_score: f32)` scores skills against a task description. `SelectionInput::new(task)` carries the
task text. `select(&self, &Catalog, &SelectionInput) -> Vec<Selection>`. An `Activation` records `name`, `source`,
`reason`, and optional `for_agent` / `in_scope` / `with_resources`.

## Budgets

| Constant | Value | Meaning |
| - | - | - |
| `Budget::catalog_bytes` | 16 000 | total size of the catalogue listing injected into the prompt |
| `Budget::instruction_bytes` | 24 000 | per-skill body cap when loaded |
| `Budget::total_skill_bytes` | 48 000 | across all activated skills in one turn |
| `Budget::resource_bytes` | 12 000 | per resource read |
| `Budget::max_resources` | 32 | resources per skill |

When the budget cannot fit everything, descriptions shrink first (per-line `DescriptionChars(n)`), then skills drop
out with the literal marker `- {n} additional skill(s) omitted from this bounded skills list.` The degradation is
measured and reported, not silent.

## What gets injected

The catalogue is a `developer`-role message wrapped in `<skills_instructions>`. A selected skill's body is a
`user`-role message wrapped in `<skill>` with `<name>`, `<path>`, and optionally `<resource_access>`. Skill text is
always `Trust::External` — it can never claim system authority.

VIKRAM's runtime adapter builds the catalogue with `SkillRegistry::for_workspace(&workspace_root)` and injects it
into `CompletionRequest` messages before `session.messages()`. Skill bodies are never loaded at this stage; they are
read on demand via `skill.load` / `skill.read`.
