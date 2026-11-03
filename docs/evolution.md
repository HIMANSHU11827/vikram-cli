# VIKRAM Self-Evolution Engine

A native subsystem (`crates/vikram-evolution`) providing the learning lifecycle:

Experience -> Observation -> Reflection -> Evaluation -> Knowledge Extraction -> Storage -> Retrieval -> Improvement -> Verification.

## Subsystems

- `ExperienceCollector`: structured experience records with redaction and privacy gating.
- `LayeredMemory`: multi-layer memory (working/episodic/semantic/procedural/user/project/agent/shared) with dedup, conflict handling, provenance, consolidation.
- `ReflectionEngine`: heuristic (and pluggable) reflector producing structured `LearningProposal`s; async job queue with recovery.
- `SkillEvolution`: create/improve/rollback/version skills on disk with injection scanning.
- `HiveLearning`: collaboration lesson recording.
- `OptimizationEngine`: GEPA-inspired candidate generation, evaluation, Pareto selection, promotion/rollback.
- `Dashboard`: aggregate learning metrics.

## Integration

`RuntimeInner.evolution` holds a `Mutex<EvolutionEngine>` opened at `~/.vikram/evolution.db` with skills root `~/.vikram/skills`. Access via `RuntimeHandle::evolution()`.

## Security

- Secret redaction in `redact` module.
- Prompt-injection scanning for stored/learned content.
- Agent scope isolation enforced by scope strings.
- No automatic global promotion; proposals require approval where flagged.

## Tests

`crates/vikram-evolution/tests/lifecycle.rs` covers the 12 acceptance scenarios.