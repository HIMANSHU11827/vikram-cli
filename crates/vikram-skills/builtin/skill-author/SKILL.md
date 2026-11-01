---
name: skill-author
description: Write or revise a VIKRAM skill so its frontmatter validates, its description is recognizable in a catalog listing, and its instructions are safe to follow.
license: MIT
vikram:
  version: 0.1.0
  categories:
    - meta
  tags:
    - skills
    - authoring
  activation: suggested
---

# Skill author

A skill is a directory whose name is its identity and whose `SKILL.md` is its content.

## Frontmatter

- `name` must match the directory: lowercase ASCII letters, digits and `-`, 64 bytes at most.
- `description` states the capability *and* when to use it, in one sentence. Keep it under 400
  bytes so a catalog listing stays readable.
- Put routing data in the `vikram:` block: `tags`, `categories`, `requires`, `permissions`,
  `activation`. Declare only permissions you actually use — each one becomes a policy request
  the user has to approve.
- Leave unrelated keys alone. They are preserved on edit and re-emitted in source order.

## Instructions

- One job per skill. Split a skill that needs the word "and" in its title.
- Write the steps an agent must take, in order, with the command or file path spelled out.
- Reference files go in `references/`, scripts in `scripts/`, static inputs in `assets/`.
  Nothing outside those paths is read by the loader.
- State what "done" looks like and how to verify it. An unverifiable instruction is a wish.

## Checklist before publishing

1. `vikram skill validate <dir>` reports no errors.
2. The description alone is enough to decide whether the skill applies.
3. Every declared permission maps to a line in the instructions.
