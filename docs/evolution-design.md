# Vikram Autonomous Self-Evolution Architecture

## Overview

This document describes the complete autonomous self-evolution architecture for Vikram, building on the existing `vikram-evolution` crate foundation.

## Architectural Principles

1. **No Duplicate Systems**: Integrate with existing Vikram subsystems (runtime, store, kernel, policy, tools, MCP, skills)
2. **Safety First**: Deny-first permissions, mandatory floor, controlled modification lifecycle
3. **Verifiable Self-Model**: Never claim capabilities without verifying against actual runtime
4. **Autonomy Levels**: Action risk determines approval requirements
5. **MAPE-K Loop**: Monitor-Analyze-Plan-Execute over shared Knowledge
6. **Continuous Operation**: Persistent state across sessions, resumable operations

## Component Architecture

### Existing Components (Enhanced)

- **ExperienceCollector**: Experience recording with redaction
- **LayeredMemory**: Multi-layer memory with dedup and consolidation
- **ReflectionEngine**: Heuristic reflection producing learning proposals
- **SkillEvolution**: Skill versioning with rollback
- **HiveLearning**: Collaboration lesson recording
- **OptimizationEngine**: Candidate generation and Pareto selection
- **Dashboard**: Learning metrics aggregation

### New Components

#### 1. Self-Awareness Engine (`self_awareness.rs`)

**Purpose**: Build and maintain a machine-readable self-model synchronized with actual runtime

**Responsibilities**:
- Inspect runtime architecture (modules, subsystems, capabilities)
- Scan available tools and MCP servers
- Detect available models and providers
- Map agent/bot/hive capabilities
- Inventory memory systems and context management
- Identify implemented, partial, and missing features
- Detect known bugs and failures
- Track resource availability and limitations
- Synchronize self-model with actual implementation

**Self-Model Structure**:
```rust
struct SelfModel {
    architecture: ArchitectureSnapshot,
    capabilities: CapabilityInventory,
    tools: ToolInventory,
    providers: ProviderInventory,
    agents: AgentInventory,
    memory: MemoryInventory,
    context: ContextInventory,
    limitations: KnownLimitations,
    bugs: KnownBugs,
    features: FeatureStatus,
    resources: ResourceSnapshot,
    last_sync: u64,
}
```

**Key Methods**:
- `inspect_runtime()` - Perform full runtime inspection
- `sync_model()` - Synchronize with actual implementation
- `verify_capability(capability)` - Verify a claimed capability exists
- `detect_changes()` - Detect changes since last sync
- `get_self_model()` - Return current self-model

#### 2. Self-Configuration Engine (`self_config.rs`)

**Purpose**: Independently manage configurable parts of Vikram

**Responsibilities**:
- Model selection based on task requirements
- Provider management and fallbacks
- Runtime settings (timeouts, concurrency, retry strategies)
- Context budget management
- Interface configuration (themes, layouts)
- Agent configuration (roles, models, tools, permissions)
- Tool and MCP integration management
- Configuration validation

**Configuration Types**:
```rust
enum ConfigTarget {
    Model(String),
    Provider(String),
    Runtime(Setting),
    Context(ContextSetting),
    Interface(UISetting),
    Agent(AgentSetting),
    Tool(ToolSetting),
    MCP(McpSetting),
}
```

**Key Methods**:
- `select_model_for_task(task)` - Choose optimal model
- `configure_provider(target, config)` - Configure provider
- `set_runtime_setting(setting, value)` - Adjust runtime
- `manage_context_budget(strategy)` - Optimize context
- `configure_agent(agent_id, config)` - Configure agent
- `enable_tool(tool_name)` - Enable tool
- `validate_configuration()` - Validate all settings

#### 3. Self-Adaptation Engine (`self_adaptation.rs`)

**Purpose**: Recognize when current approach is ineffective and choose better strategies

**Responsibilities**:
- Detect performance issues (slow models, failing providers, excessive context)
- Analyze adaptation opportunities
- Generate adaptation strategies
- Select optimal adaptation based on constraints
- Apply adaptations with verification
- Track adaptation outcomes

**Adaptation Triggers**:
- Model too slow for task type
- Provider repeatedly failing
- Context usage excessive
- Agent producing poor results
- Workflow too complex
- Tool unreliable
- Better strategy available

**Adaptation Criteria**:
- Accuracy
- Reliability
- Latency
- Cost
- Token usage
- CPU/RAM
- Task complexity
- User preferences
- Security
- Available resources

**Key Methods**:
- `detect_adaptation_opportunities()` - Find adaptation points
- `analyze_situation(context)` - Analyze current state
- `generate_strategies(problem)` - Generate solutions
- `select_strategy(strategies, constraints)` - Choose best strategy
- `apply_adaptation(strategy)` - Apply adaptation
- `verify_adaptation(before, after)` - Verify improvement

#### 4. Self-Improvement Engine (`self_improvement.rs`)

**Purpose**: Proactively identify and execute improvements

**Responsibilities**:
- Identify improvement areas (planning, coding, memory, context, tools, etc.)
- Prioritize improvements based on value, evidence, cost, risk
- Execute improvements through controlled lifecycle
- Track improvement history
- Verify improvements actually work

**Improvement Areas**:
- Planning quality
- Task decomposition
- Coding performance
- Memory retrieval
- Context management
- Tool selection
- Agent coordination
- Error handling
- Recovery
- Verification
- Research quality
- Execution efficiency

**Key Methods**:
- `identify_improvement_opportunities()` - Find areas to improve
- `prioritize_improvements(opportunities)` - Rank by value/risk
- `execute_improvement(improvement)` - Execute with lifecycle
- `track_improvement(improvement)` - Record history
- `verify_improvement(improvement)` - Measure actual impact

#### 5. Self-Development Engine (`self_development.rs`)

**Purpose**: Controlled development of new capabilities including source code modifications

**Responsibilities**:
- Create and improve skills
- Create and improve tools
- Develop integrations
- Create workflows
- Develop specialized agents
- Modify source code (controlled lifecycle)
- Add missing capabilities
- Improve internal architecture

**Controlled Development Lifecycle**:
1. Identify missing capability or weakness
2. Research possible solutions
3. Inspect relevant existing implementation
4. Prepare implementation plan
5. Make changes in isolated branch/workspace
6. Run formatting and static checks
7. Run unit tests
8. Run integration tests
9. Run regression tests
10. Independent review
11. Compare against previous behavior
12. Integrate only when validation succeeds
13. Record change
14. Preserve rollback path

**Key Methods**:
- `create_skill(name, content)` - Create new skill
- `improve_skill(name, changes)` - Improve existing skill
- `create_tool(spec, implementation)` - Create tool
- `develop_agent(spec)` - Develop agent
- `modify_source_code(changeset)` - Controlled code modification
- `review_change(change)` - Independent review
- `validate_change(change)` - Full validation
- `integrate_change(change)` - Safe integration
- `rollback_change(change)` - Revert change

#### 6. Self-Evaluation Engine (`self_evaluation.rs`)

**Purpose**: Measure whether Vikram is actually improving

**Responsibilities**:
- Track performance metrics
- Create repeatable evaluation tasks
- Run benchmarks
- Compare old vs new behavior
- Aggregate evaluation results
- Detect regressions

**Metrics Tracked**:
- Task completion rate
- Task correctness
- Tool success rate
- Model performance
- Agent performance
- Execution time
- Token consumption
- Resource consumption
- Failure rate
- Recovery rate
- Regression rate
- User corrections
- Skill effectiveness

**Key Methods**:
- `track_metric(metric)` - Record metric
- `create_benchmark(task)` - Create evaluation task
- `run_benchmark(benchmark)` - Execute benchmark
- `compare_performance(before, after)` - Compare results
- `detect_regression(metrics)` - Find regressions
- `aggregate_results(evaluations)` - Summarize results

#### 7. Self-Recovery Engine (`self_recovery.rs`)

**Purpose**: Recover from failed changes and operational problems

**Responsibilities**:
- Configuration snapshots
- Change history
- Versioned skills
- Git-based source checkpoints
- Failed-change detection
- Automatic rollback for eligible changes
- Recovery after restart
- Interrupted-operation recovery
- Recovery verification

**Recovery Types**:
- Configuration rollback
- Skill version rollback
- Source code rollback (git)
- Memory state restoration
- Provider reconfiguration
- Tool re-enablement

**Key Methods**:
- `create_snapshot()` - Snapshot current state
- `detect_failed_change(change)` - Identify failure
- `rollback_change(change)` - Revert to previous state
- `recover_after_restart()` - Restore after crash
- `resume_interrupted_operation(op)` - Resume operation
- `verify_recovery()` - Confirm successful recovery

#### 8. MAPE-K Loop (`mapek.rs`)

**Purpose**: Orchestrate the complete autonomous management loop

**Components**:

**Monitor**:
- Observe runtime health
- Monitor resources
- Monitor model/provider status
- Monitor execution outcomes
- Monitor failures
- Monitor performance

**Analyze**:
- Understand what is happening
- Identify weaknesses
- Detect anomalies
- Compare expected vs actual behavior
- Determine possible improvements

**Plan**:
- Generate possible solutions
- Compare benefits and costs
- Estimate risks
- Choose most suitable action
- Define success criteria

**Execute**:
- Change configurations
- Adapt workflows
- Improve skills
- Develop capabilities
- Execute approved source modifications

**Knowledge**:
- Store decisions
- Store outcomes
- Maintain self-model
- Maintain learning history
- Preserve operational knowledge

**Key Methods**:
- `monitor()` - Collect monitoring data
- `analyze(data)` - Analyze and identify issues
- `plan(issues)` - Generate action plans
- `execute(plan)` - Execute approved actions
- `update_knowledge()` - Update knowledge base
- `run_cycle()` - Run complete MAPE-K cycle

#### 9. Autonomy Manager (`autonomy.rs`)

**Purpose**: Implement autonomy levels and safety controls

**Autonomy Levels**:

**Level 1 - Automatic**:
- Adjust model-routing preferences
- Change low-risk runtime settings
- Optimize context strategies
- Improve internal workflows
- Update verified learning records
- Organize internal data
- Run diagnostics
- Create isolated skill drafts

**Level 2 - Controlled Development**:
- Create tools
- Modify skills
- Develop integrations
- Change workflows
- Modify source code
- Run tests
- Review changes
- Prepare validated updates
- Require approval before deployment

**Level 3 - Approval Required**:
- Change security policies
- Change permissions
- Modify secrets
- Disable protections
- Destructive data operations
- Major architecture migrations
- Production deployment
- Irreversible operations
- Changes exposing user data

**Key Methods**:
- `determine_autonomy_level(action)` - Classify action risk
- `request_approval(action)` - Request user approval
- `execute_automatic(action)` - Execute automatic action
- `execute_controlled(action)` - Execute with approval
- `audit_action(action)` - Record action in audit log

#### 10. Conversational Control (`conversational.rs`)

**Purpose**: Natural language interface for evolution control

**Supported Commands**:
- "Analyze yourself"
- "What can you improve?"
- "Find your weaknesses"
- "Improve your memory system"
- "Optimize your model selection"
- "Review your agent architecture"
- "Learn a better coding workflow"
- "What have you improved recently?"
- "Why did you change this setting?"
- "Undo your last change"
- "Stop autonomous improvements"
- "Resume autonomous management"

**Key Methods**:
- `handle_command(command)` - Process natural command
- `explain_action(action)` - Explain what was done and why
- `report_improvements()` - Report recent improvements
- `undo_last_change()` - Revert last change
- `pause_autonomy()` - Pause autonomous improvements
- `resume_autonomy()` - Resume autonomous improvements

## Integration with Existing Systems

### Runtime Integration
- Access via `RuntimeHandle::evolution()`
- Runtime provides: config, sessions, agents, tools, MCP, skills, context, memory
- Evolution engine: provides self-model, configuration suggestions, improvements

### Store Integration
- Use existing `vikram-store` for persistent data
- Add evolution-specific tables if needed
- Reuse existing transaction mechanisms

### Kernel Integration
- Evolution changes respect kernel's permission gates
- Self-modification goes through kernel's effect system
- Approval system integrates with kernel's grant system

### Policy Integration
- Self-configuration respects mandatory floor
- Security changes require higher autonomy level
- Policy evaluation includes evolution constraints

### Tools Integration
- Self-development creates new tools through ToolRegistry
- Tool discovery and management through existing tool system
- Tool performance metrics fed to evaluation engine

### MCP Integration
- MCP server discovery and management
- MCP tool integration
- MCP performance monitoring

### Skills Integration
- Use existing `vikram-skills` for skill management
- Evolution engine creates/improves skills
- Skill versioning through existing system

### Agents/Bots/Hive Integration
- Agent configuration through AgentManager
- Hive collaboration lessons
- Bot specialization development

## Data Flow

```
User Request
    ↓
Runtime Turn
    ↓
Experience Collection (vikram-evolution)
    ↓
Reflection & Learning Proposals
    ↓
MAPE-K Monitor (new)
    ↓
MAPE-K Analyze (new)
    ↓
MAPE-K Plan (new)
    ↓
Autonomy Level Check (new)
    ↓
MAPE-K Execute (new)
    ↓
Knowledge Update (new)
    ↓
Self-Model Update (new)
    ↓
Configuration/Code Change (new)
    ↓
Verification (new)
    ↓
Recovery if Failed (new)
```

## Safety Guarantees

1. **Never Weaken Security**: Security policies cannot be weakened through autonomous changes
2. **Mandatory Floor**: Cannot be widened by evolution
3. **Controlled Self-Modification**: Source code changes go through full lifecycle
4. **Rollback Path**: Every change has a rollback mechanism
5. **Verification Required**: Changes must be verified before integration
6. **Audit Trail**: All actions logged
7. **Approval Gates**: Sensitive changes require explicit approval
8. **No Secret Exposure**: Secrets never stored in learned content
9. **Injection Protection**: Learned content scanned for prompt injection
10. **Resource Limits**: Autonomous operations respect resource constraints

## Implementation Plan

Phase 1: Core Engines
- Self-Awareness Engine
- Self-Configuration Engine
- Self-Adaptation Engine
- Self-Improvement Engine

Phase 2: Development & Evaluation
- Self-Development Engine
- Self-Evaluation Engine
- Self-Recovery Engine

Phase 3: Orchestration & Control
- MAPE-K Loop
- Autonomy Manager
- Conversational Control

Phase 4: Integration
- Runtime integration
- Tool integration
- MCP integration
- Agent/Bot/Hive integration

Phase 5: Testing
- Unit tests
- Integration tests
- Regression tests
- End-to-end scenarios

## Success Criteria

1. Vikram correctly identifies its model and provider
2. Vikram detects unavailable providers and switches to alternatives
3. Vikram optimizes context usage based on patterns
4. Vikram creates and improves skills autonomously
5. Vikram identifies repeated failures and learns from them
6. Vikram creates isolated code modifications safely
7. Vikram detects failed modifications and rolls back
8. Vikram preserves learning after restart
9. Vikram delegates research and independent review
10. Vikram evaluates whether improvements actually work
11. Vikram respects approval and permission policies
12. Vikram does not claim unimplemented capabilities exist
