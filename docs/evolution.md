# VIKRAM Self-Evolution Engine

A native subsystem (`crates/vikram-evolution`) providing autonomous self-management and evolution capabilities.

## Learning Lifecycle

Experience -> Observation -> Reflection -> Evaluation -> Knowledge Extraction -> Storage -> Retrieval -> Improvement -> Verification.

## Autonomous Self-Evolution Subsystems

### Core Learning (Existing)
- `ExperienceCollector`: structured experience records with redaction and privacy gating.
- `LayeredMemory`: multi-layer memory (working/episodic/semantic/procedural/user/project/agent/shared) with dedup, conflict handling, provenance, consolidation.
- `ReflectionEngine`: heuristic (and pluggable) reflector producing structured `LearningProposal`s; async job queue with recovery.
- `SkillEvolution`: create/improve/rollback/version skills on disk with injection scanning.
- `HiveLearning`: collaboration lesson recording.
- `OptimizationEngine`: GEPA-inspired candidate generation, evaluation, Pareto selection, promotion/rollback.
- `Dashboard`: aggregate learning metrics.

### New Autonomous Capabilities
- `SelfAwarenessEngine`: runtime inspection and self-model maintenance - Vikram understands its actual architecture, capabilities, tools, providers, and limitations.
- `SelfConfigurationEngine`: independent management of models, providers, runtime settings, context budgets, agent configuration, and tool/MCP integrations.
- `SelfAdaptationEngine`: recognizes when current approaches are ineffective and independently chooses better strategies based on accuracy, reliability, latency, cost, and other factors.
- `SelfImprovementEngine`: proactively identifies improvement opportunities, prioritizes them by value and risk, and executes improvements through a controlled lifecycle.
- `SelfDevelopmentEngine`: controlled development of new capabilities including skill creation, tool development, agent development, and source code modifications through a safe 14-stage lifecycle.
- `SelfEvaluationEngine`: tracks performance metrics, creates benchmarks, runs evaluations, detects regressions, and measures whether Vikram is actually improving.
- `SelfRecoveryEngine`: provides configuration snapshots, change history, versioned skills, automatic rollback, and recovery verification for failed changes.
- `MapekLoop`: orchestrates the complete autonomous management loop following the MAPE-K autonomic computing architecture (Monitor-Analyze-Plan-Execute over shared Knowledge).
- `AutonomyManager`: implements autonomy levels (Automatic, Controlled Development, Approval Required) and safety controls based on action risk.
- `ConversationalControl`: natural language interface for evolution control (e.g., "analyze yourself", "what can you improve", "stop autonomous improvements").

## MAPE-K Architecture

The autonomous system follows the MAPE-K feedback loop:

**Monitor**: Observe runtime health, resources, model/provider status, execution outcomes, failures, and performance.

**Analyze**: Understand what is happening, identify weaknesses, detect anomalies, compare expected vs actual behavior, and determine possible improvements.

**Plan**: Generate possible solutions, compare benefits and costs, estimate risks, choose the most suitable action, and define success criteria.

**Execute**: Change configurations through the host's `ConfigSurface`, adapt workflows, improve
skills, and re-queue reflection that failed. What it may change is deliberately narrow: the
runtime accepts three keys (`context:memory_budget_tokens`, `runtime:learning_memory_policy`,
`runtime:learning_skill_policy`) and refuses everything else, and source modification reached
through the choke point every edit already passes — the sandbox and the policy floor — not
through the evolution engine itself.

**Knowledge**: Store decisions, outcomes, maintain the self-model, maintain learning history, and preserve useful operational knowledge.

## Autonomy Levels

**Level 1 - Automatic**: Allow appropriate reversible operations without asking the user (low-risk runtime settings, context optimization, internal workflows, verified learning records, diagnostics, skill drafts). *Model routing is listed here as a goal, not a shipped behaviour: `select_model_for_task` (`crates/vikram-evolution/src/self_config.rs`) has no production caller and picks by substring over a caller-supplied list, so it is not wired.*

**Level 2 - Controlled Development**: Allow autonomous development in an isolated environment (create tools, modify skills, develop integrations, change workflows, modify source code, run tests, review changes, prepare validated updates). Require approval before merging consequential changes.

**Level 3 - Approval Required**: Require explicit authorization for sensitive operations (security policies, permissions, secrets, disabling protections, destructive data operations, major architecture migrations, production deployment, irreversible operations, changes exposing user data).

## Integration

`RuntimeInner.evolution` holds a `Mutex<EvolutionEngine>` opened at `~/.vikram/evolution.db` with skills root `~/.vikram/skills`. Access via `RuntimeHandle::evolution()`.

The evolution engine integrates with:
- Main Vikram runtime (sessions, agents, tools, MCP, skills, context, memory)
- Planning and task execution systems
- Provider and model management
- Agent/bot/hive collaboration
- Permission system (respects mandatory floor)

## Security

- Secret redaction in `redact` module.
- Prompt-injection scanning for stored/learned content.
- Agent scope isolation enforced by scope strings.
- No automatic global promotion; proposals require approval where flagged.
- Security policies cannot be weakened through autonomous changes.
- Mandatory floor cannot be widened by evolution.
- Controlled self-modification with full lifecycle and rollback path.
- Complete audit trail for all autonomous actions.

## Tests

`crates/vikram-evolution/tests/lifecycle.rs` covers the 12 acceptance scenarios for the original learning system.

`crates/vikram-evolution/tests/autonomous_evolution.rs` covers the new autonomous capabilities including self-awareness, self-configuration, self-adaptation, autonomy management, conversational control, and MAPE-K loop integration.