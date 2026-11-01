# Metadata — the `vikram:` extension

The open Agent Skills standard defines `metadata` as a string→string map. VIKRAM extends this with a structured
`vikram:` key inside the frontmatter. The `vikram:` block is parsed into `VikramExtension`.

## `vikram:` fields

| Field | Type | Default | Description |
| - | - | - | - |
| `activation` | string or list | `auto` | How the skill is activated: `auto`, `manual`, `suggested`. |
| `tags` | list of strings | `[]` | Free-form tags for search and filtering. |
| `categories` | list of strings | `[]` | Categorization for the TUI browser. |
| `min_vikram` | string | none | Minimum VIKRAM version required. Warns (does not block) if newer. |
| `allowed-tools` | string or list | none | Declared tool permissions (request, not grant). |

## On-disk spelling

Canonical emit uses `allowed-tools` (hyphen, matches the open standard) and `min_vikram` (underscore, VIKRAM's
own key). Both accept the other spelling as an alias. Alias resolution runs before unknown-key detection, so an
alias is never reported as unknown.

## Parsing rules (R8)

- Non-scalar `metadata:` value → stringified via `Yaml::strings()` and joined with `, ` + one `Warning`.
- Unknown key under `vikram:` → `Warning`, entry preserved in `frontmatter.entries` so it survives `to_yaml()`.
  Never an error.
- Both `vikram.tags` and `metadata.tags` present → union, deduped, `vikram.tags` order first, one `Info`.
- Empty `metadata:` → treated as absent, no diagnostic.
- `vikram.min_vikram` newer than the crate → `Warning`, loading NOT blocked.

## `Yaml::strings()` conversion

| YAML type | Result |
| - | - |
| `Null` | `vec![]` |
| `Bool` | `"true"` or `"false"` |
| `Int` | `to_string()` |
| `Str` | clone |
| `List` | `flat_map` recurse |
| `Map` | `"key:value"` in source order |

The Map arm exists because `metadata:` values are sometimes read as a list.

## Example

```yaml
---
name: my-skill
description: Does something useful when triggered.
metadata:
  author: example-org
  version: "1.0"
vikram:
  activation: auto
  tags: [rust, testing]
  categories: [development]
  min_vikram: "0.2.0"
  allowed-tools: Bash(git:*) Read
---
```
