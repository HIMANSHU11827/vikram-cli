# Future Features

New capabilities worth exploring, beyond fixing existing gaps.

## Code intelligence

- Code indexing (tree-sitter def/ref/call graph + PageRank + binary-search token fit).
- AST-based editing (tree-sitter for precise edits).
- LSP integration (extend existing LspPool for navigation).
- Git integration (status, diff, commit, branch, worktree).
- Test discovery and execution.
- Build validation.
- Checkpoint/undo (git-based or workspace snapshots).
- Code review agent (independent review of changes).
- Regression detection (run tests before/after changes).

## Agent autonomy

- Verification loop (edit→build→test→repair with budget + failure taxonomy).
- Durable mid-turn resume (checkpoint at tool-loop boundaries).
- Self-evaluation and self-correction.
- Planning and reasoning improvements.
- Dynamic agent creation and specialization.
- Agent failure recovery (retry, reassignment, escalation).

## Multi-agent coordination

- Agent-to-agent messaging (typed task-bus).
- Supervisor-worker pattern (hierarchical teams).
- Independent review agent.
- Blackboard systems (shared state).
- Voting/consensus.
- Deadlock detection.
- Agent performance tracking.
- ACP client+server, A2A protocol.

## Memory & context

- Memory consolidation (dedup, conflict resolution, supersession, forgetting).
- Semantic memory (embeddings).
- Hybrid search (BM25 + vector + reranking).
- Knowledge graph (entity relationships).
- Memory provenance (source + confidence).
- Memory permissions (per-entity access control).
- Incremental indexing (change detection, background indexing).
- Context compression.

## Providers & models

- Model routing and fallback.
- Cost and token optimization (real token counting).
- Local model support (llama.cpp, MLX).
- Provider-as-data (declarative JSON providers).
- Provider capability flags composing the loop.

## Tools & execution

- Tool discovery and installation.
- Computer/browser use improvements.
- Terminal automation.
- Sandboxing and isolation (OS-level, Job objects).
- Observability and execution tracing.
- Reliability and fault tolerance.

## Learning & evolution

- Unified learning system (merge JSON + SQLite).
- Skill creation and improvement (versioning, rollback, testing).
- Agent learning and controlled self-evolution.
- Hive learning (collaboration lessons, patterns, outcomes).
- Evaluation against benchmarks before applying.

## Security

- Human approval and permission systems (already present; extend).
- Sandboxing and isolation.
- Taint tracking (trust labels on observations).
- Secret scanning.
- At-rest encryption.
- Dependency vulnerability scanning.

## Scheduling & automation

- Scheduling/proactive triggers (cron/interval/on-exit/stream).
- Fire-claim TTL leases + missed-run policy.
- Scheduler-owned automations decoupled from conversation.
- Monitor mode (cheap script/hash gate).
- Durable background task scheduling.
- Event triggers.

## Platform & UX

- Cross-platform (Windows, Termux, Docker, VM, Kubernetes) — in progress.
- Desktop/phone notification surface.
- Remote workspace (container/SSH).
- GUI improvements (TUI already rich).

## Differentiation opportunities (no competitor excels)

1. Code intelligence (nobody indexes).
2. Durable mid-turn resume (nobody does it).
3. Unified verification loop (nobody has it).
4. Rust-native, deny-first, journal-first runtime with replaceable seams.
5. Unified Ticket System spanning Hive/bots/sub-agents.