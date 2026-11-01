# Format compatibility — Native, Agents, Claude, OpenCode, Hermes, Codex

VIKRAM supports six skill formats. The format (compat) determines which frontmatter fields are expected and how
the body is parsed. Compat is derived from the directory the skill was found in, not from a field in `SKILL.md`.

## Compat values

| Compat | Directory | Format | Frontmatter differences |
| - | - | - | - |
| `Native` | `<home>/skills/` | VIKRAM native | Full `vikram:` extension supported |
| `Agents` | `.agents/skills/`, `<home>/skills/agents/` | Agent Skills standard | Standard fields only; no `vikram:` extension |
| `Claude` | `.claude/skills/`, `<home>/skills/claude/` | Claude Code | `allowed-tools` as comma-separated (VIKRAM normalizes) |
| `OpenCode` | `.opencode/skills/`, `<home>/skills/opencode/` | OpenCode | V1/V2 contract differences handled |
| `Hermes` | `<home>/skills/hermes/` | Hermes | Hermes-specific frontmatter |
| `Codex` | `.codex/skills/` | Codex | `agents/openai.yaml` sidecar supported |

## Parsing differences

- **Native:** full `vikram:` extension parsed; all standard fields supported.
- **Agents:** standard fields only. `vikram:` block is ignored (not an error).
- **Claude:** `allowed-tools` may be comma-separated; VIKRAM normalizes to space-separated.
- **OpenCode:** handles V1 and V2 contract differences (V2 uses file path as ID).
- **Hermes:** Hermes-specific fields parsed; standard fields also supported.
- **Codex:** `agents/openai.yaml` sidecar is read for interface metadata and policy.

## Trust by compat

Compat does NOT affect trust level. Trust is determined by scope + location + policy (see `permissions.md`).
A `.claude/skills` skill and an `.agents/skills` skill in the same project get the same trust level.
Format is not a trust signal; location is.

## Conversion

`vikram skills duplicate <name> <as>` copies a skill. The copy retains the original compat. To convert formats,
copy the skill to a directory with the target compat and edit the frontmatter.

`vikram skills export <name> <dest>` produces a directory archive with `EXPORT.json` metadata, portable across
VIKRAM instances.
