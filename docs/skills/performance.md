# Performance — budgets, caching, and scaling

## Budgets

The `Loader` enforces four budgets (all configurable via `Budget`):

| Budget | Default | What it limits |
| - | - | - |
| `catalog_bytes` | 16,000 | Total bytes of the catalogue listing injected into the prompt |
| `instruction_bytes` | 24,000 | Bytes of a single skill's instructions |
| `total_skill_bytes` | 48,000 | Total bytes of all loaded skills in one turn |
| `resource_bytes` | 12,000 | Bytes of a single resource file |
| `max_resources` | 32 | Maximum resources read in one turn |

## Degradation

When a budget is exceeded, the loader degrades gracefully:

1. **Catalogue (stage A):** per-line `DescriptionChars(n)` allocation shrinks descriptions. If still over budget,
   skills are omitted with the marker `- {n} additional skill(s) omitted from this bounded skills list.`

2. **Instructions (stage B):** the skill body is truncated at `instruction_bytes` with a truncation flag.

3. **Resources:** individual resource reads are capped at `resource_bytes`. Additional resources beyond
   `max_resources` are refused.

## Discovery performance

- Roots are scanned in parallel (`MAX_CONCURRENT_ROOT_SCANS = 8`).
- Skills are parsed in parallel (`MAX_CONCURRENT_SKILL_LOADS = 64`).
- Walk depth is capped at 6 (`MAX_SCAN_DEPTH`).
- Directory entries are capped at 20,000 per root (`MAX_SKILLS_ENTRIES_PER_ROOT`).
- Skill directories are capped at 2,000 per root (`MAX_SKILLS_DIRS_PER_ROOT`).

## Caching

- The `Catalog` is built once per turn and discarded. No persistent cache.
- `TrustPolicy` is loaded from disk on each `TrustPolicy::load()` call.
- `BundleIndex` is loaded from disk on each `BundleIndex::load()` call.
- `History` is loaded per-skill on demand.

## Scaling notes

- Hundreds of skills are affordable because only metadata (~100 tokens each) is loaded by default.
- Full skill bodies are loaded only on activation, and per-turn (no carry-over).
- The 2% context window cap (`SKILL_METADATA_CONTEXT_WINDOW_PERCENT`) ensures the catalogue never dominates
  the prompt.
