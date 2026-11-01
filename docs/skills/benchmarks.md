# Benchmarks

Performance benchmarks for the skills platform. Run with `cargo bench` (requires the `benches/` directory, which
does not yet exist — see below).

## What to benchmark

| Benchmark | What it measures | Target |
| - | - | - |
| Discovery scan | Time to scan 100 skill directories | < 50 ms |
| Catalog listing | Time to build the catalog message for 50 skills | < 10 ms |
| Frontmatter parse | Time to parse one `SKILL.md` | < 1 ms |
| Trust mapping | Time to map 50 skills to trust levels | < 5 ms |
| Selector | Time to select from 100 skills given a task | < 10 ms |
| History hash | Time to hash a 10-file skill directory | < 20 ms |

## Running

```bash
cargo bench -p vikram-skills   # when benches/ exists
```

## Current status

No benchmark harness exists yet. The `benches/` directory is absent from the workspace. Benchmarks should be added
using `criterion` (already a common choice in Rust workspaces) once the core functionality is stable.

## Scaling notes

- Discovery uses `MAX_CONCURRENT_ROOT_SCANS = 8` and `MAX_CONCURRENT_SKILL_LOADS = 64` for parallelism.
- Catalog listing is O(n) in the number of skills, with O(n log n) sorting.
- Budget truncation stops early when the byte budget is exhausted.
- State files (disabled.json, trust-policy.json) are loaded once per `Catalog::discover` call, not per skill.
- History manifests are loaded lazily via `History::load(name)`.

## Performance constraints

- `catalog_bytes` default: 16,000 (metadata only).
- `instruction_bytes` default: 24,000 (one skill body at a time).
- `total_skill_bytes` default: 48,000 (all loaded skills combined).
- `resource_bytes` default: 12,000 (resources read on demand).
- `max_resources` default: 32.
