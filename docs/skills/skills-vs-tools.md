# Skills vs tools

The two are different kinds of thing and the runtime treats them differently.

| | Skill | Tool |
| - | - | - |
| What it is | Instructions and knowledge (Markdown + optional resources) | A capability the model can invoke (a function) |
| How it enters the model | As text in the prompt (catalogue + body) | As a callable in the tool list |
| Who provides it | Authors (anyone with a directory) | The runtime (compiled in or registered) |
| Trust floor | `Trust::External` | Per-tool `permission()` + policy |
| Can it execute? | No — nothing in `vikram-skills` runs skill content | Yes — `Tool::run` executes |
| Can it grant permissions? | No — declared permissions are requests | Tools declare their own permission shape |

## Why the distinction matters

A skill that says "you may run `rm -rf`" does not give the model the ability to run `rm -rf`. The model still needs the
shell tool, and the shell tool still goes through `MandatoryFloor` and the approval policy. The skill only changes
what the model *tries* to do.

Conversely, a tool with no skill backing it is still fully usable — skills are optional enrichment, not a
prerequisite for capability.

## In VIKRAM

- `vikram-skills` parses, validates, copies and describes skills. It never executes skill content.
- `vikram-runtime::tools` registers tools (`SkillList`, `SkillLoad`, `SkillSearch`, `SkillRead`, `ReadFile`, …).
  Skill tools resolve `$VIKRAM_HOME` via `config::vikram_home()` because `ToolContext` carries only `workspace_root`.
- The runtime adapter (`crates/vikram-runtime/src/skills.rs`) wraps `Catalog` + `Loader` and exposes metadata-only
  listing plus on-demand body/resource reads.
- `turn.rs` injects the budgeted catalogue as a `developer`-role message before `session.messages()`. Skill bodies
  are never loaded there.

## The non-negotiables that follow

1. Skill text always enters as `Trust::External`.
2. Declared permissions are requests, never grants.
3. Never load every skill body into context — metadata first, body on demand.
4. No auto-execution of skill scripts.
5. No skill can widen its own trust by editing its frontmatter.
