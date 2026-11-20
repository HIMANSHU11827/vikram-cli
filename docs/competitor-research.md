# Competitor Research — Self-Improving Agent Systems

Evidence-backed comparison of how other agent systems implement persistent memory,
learning, and self-improvement — and where Vikram's implemented architecture
(see [`evolution-architecture.md`](evolution-architecture.md)) stands against them.

**Evidence labels**

- **source-verified** — claims backed by reading the actual source code (cloned
  repos under `/tmp/vikram-research/` or prior in-tree analyses).
- **docs** — claims from official documentation or the project's own docs.
- **marketing** — claims from marketing material, blog posts, or secondary
  coverage of closed products; internals not inspectable.
- **unknown** — not verifiable from available sources.

Where a system has mixed evidence, the label reflects the weakest claim relied on.

---

## 1. System-by-system notes

### Coding agents

**Claude Code** — *docs / marketing*
- Memory: `CLAUDE.md` instruction files, `MEMORY.md`/`USER.md`-style auto-memory,
  session-scoped recall. No durable episodic store visible.
- Learning: no autonomous skill-creation loop documented; memory writes are
  tool-driven inside a session.
- Self-improvement: none documented; the agent edits files, not its own
  capabilities.
- Key limitation: closed source; memory model partially undocumented; no
  versioned rollback of learned content beyond git.

**Codex CLI** — *docs / source-verified (partial)*
- Memory: `AGENTS.md` instruction files; session JSONL rollouts; no durable
  cross-session memory subsystem in the loop code.
- Learning: none in-tree; experiments land as config, not learned artifacts.
- Self-improvement: none.
- Key limitation: provider seam is OpenAI-Responses-shaped; no learning surface.

**OpenCode** — *source-verified*
- Memory: markdown memory files + plugin hooks; event-sourced SQLite session
  store (`session/event-log.ts`) — excellent durability, no learning layer.
- Learning: none built in; plugins could implement it, none ship one.
- Key limitation: no memory/skill versioning or approval-gated mutation.

**Cursor** — *docs*
- Memory: `.cursor/rules`, "Memories" feature (Cursor 2.x) — docs-verified;
  rule/memory files are user-editable artifacts.
- Learning: rules are edited by the user or via chat; no autonomous
  propose-apply loop documented.
- Key limitation: closed; no evidence of validated mutation or rollback.

**Windsurf / Devin Desktop** — *docs / marketing*
- Memory: Cascade "memories" and rules; Devin Desktop inherits Devin's
  knowledge/playbook model.
- Learning: playbooks/knowledge are curated, not self-generated.
- Key limitation: same as Devin below — curated, not learned.

**Devin** — *docs / marketing*
- Memory: per-user cloud VM snapshots, "knowledge" entries, playbooks;
  session history persists on the VM.
- Learning: knowledge/playbook authoring is human-driven.
- Self-improvement: none documented.
- Key limitation: closed; isolation is per-session VM, not per-entity memory.

**Manus** — *marketing*
- Memory: per-task cloud VM that hibernates; "knowledge" uploads; Wide
  Research parallel sub-agents.
- Learning: context-engineering writeups describe manual knowledge curation.
- Key limitation: closed; no inspectable learning loop.

**Replit Agent** — *marketing / unknown*
- Memory: checkpoints for revert; Agent checkpoints capture file state.
- Learning: none documented; checkpoints are rollback, not learning.
- Key limitation: no public evidence of durable memory or self-improvement.

### Agent runtimes with real memory/learning subsystems

**Hermes Agent (Nous Research)** — *source-verified*
- Memory: MEMORY.md/USER.md-style free-form files, Honcho-backed user modeling,
  monitor-mode output suppression.
- Learning: background review after sessions proposes memory/skill changes —
  the pattern Vikram's `learning.rs` explicitly cites as its model.
- Self-improvement: skill creation/patching with read-before-write guard;
  approval policies.
- Key limitation: process-local review cadence; no versioned skill store
  comparable to Vikram's `SkillEvolution`.

**Letta / MemGPT** — *source-verified*
- Memory: three-tier memory blocks (core/archival/recall) as first-class
  mutable objects; sleep-time compute runs background memory reorganization.
- Learning: agent self-edits memory blocks via tool calls; reflection triggers
  on compaction/step-count.
- Self-improvement: memory-block versioning is native; skill-like "tools" are
  code the agent writes.
- Key limitation: memory is block-shaped and agent-mutated without an external
  validator; no benchmark-gated promotion.

**Mem0** — *docs / source-verified*
- Memory: standalone memory layer — extraction, dedup, consolidation into a
  graph/vector store; `add`/`search`/`update`/`delete` API.
- Learning: LLM-driven extraction of facts from conversations; conflict
  resolution on write.
- Key limitation: a memory *service*, not an agent — no self-improvement of
  the agent itself; quality depends on the calling agent.

**Zep / Graphiti** — *docs / source-verified*
- Memory: temporal knowledge graph — episodes → extracted facts with validity
  windows (`valid_at`/`invalid_at`); bi-temporal queries.
- Learning: automatic fact extraction + edge invalidation on contradiction.
- Key limitation: facts, not procedures; no skill/agent mutation surface.

**Cognee** — *docs / source-verified*
- Memory: ECL pipeline (extract → cognify → load) building a knowledge graph +
  vector index from documents/sessions.
- Learning: pipeline-driven enrichment, not agent-driven mutation.
- Key limitation: ingestion-time processing; no runtime learning loop.

**LangMem** — *source-verified*
- Memory: LangChain SDK — semantic/episodic/procedural memory managers with
  background "memory manager" agents that extract and consolidate.
- Learning: procedural memory = learned instructions; reflection-based
  extraction prompts.
- Key limitation: library, not a product; approval/rollback are the host's job.

**Supermemory** — *docs / marketing*
- Memory: hosted "universal memory" API — ingest documents/conversations,
  hybrid search, profile extraction.
- Learning: server-side extraction; agent-agnostic.
- Key limitation: SaaS; no agent self-improvement, just a memory backend.

### Research / evolutionary systems

**EvoAgentX** — *source-verified (repo)*
- Self-improvement: evolutionary algorithms (GA/DE variants) over agent prompts,
  workflows, and multi-agent topologies; benchmark-driven fitness.
- Memory: experience buffers feeding fitness evaluation, not persistent memory.
- Key limitation: offline evolution of configs; no production approval loop or
  per-user isolation.

**AgentEvolver** — *docs / unknown*
- Self-improvement: evolutionary mutation of agent code/prompts with
  self-assessment; claims self-evolving agent frameworks.
- Key limitation: evidence thin; fitness signals largely self-reported.

**A-Evolve** — *docs / unknown*
- Self-improvement: population-based evolution with crossover/mutation over
  agent implementations; tree-search planning component.
- Key limitation: research prototype; no durable per-user memory story.

**Reef** — *unknown*
- Could not verify a system by this name with the described capabilities;
  possibly a niche/evolution framework. Treat all claims as unverified.

**RSIAgent** — *docs / unknown*
- Recursive self-improvement research agent; improves via iterative
  self-modification loops described in papers.
- Key limitation: research artifact; safety/verification claims unproven at
  production scale.

**EvoSkill** — *docs / unknown*
- Skill-level evolution: mutates skill/tool-use trajectories against task
  benchmarks; closest analog to Vikram's `SkillEvolution`.
- Key limitation: offline benchmark harness; no live approval surface.

**MemSkill** — *docs / unknown*
- Memory-as-skill: learns reusable memory-operation skills rather than facts.
- Key limitation: research prototype; unclear production path.

**OpenSkill** — *unknown*
- Could not verify; possibly a skill-sharing/benchmark initiative. Unverified.

**Recuris** — *unknown*
- Could not verify. Unverified.

**altk-evolve** — *unknown*
- Could not verify; appears to be a small/niche evolution project. Unverified.

**Proteus** — *unknown*
- Name collides with multiple projects (biomedical, game AI); no verifiable
  self-improving agent system matching the description. Unverified.

---

## 2. Capability matrix

Scores 0–5. Evidence cites the strongest verified claim per cell.

| System | Persistent memory | Autonomous learning | Self-improvement / skills | Personalization | Multi-agent isolation | Background / proactive | Verification-grounded | Rollback / versioning |
|---|---|---|---|---|---|---|---|---|
| Claude Code | 3 — MEMORY.md files (docs) | 1 — tool-driven only | 1 — no skill mutation | 2 — user/memory files | 2 — per-session dirs | 2 — cron/BackgroundTasks | 1 — none | 2 — git checkpoints |
| Codex CLI | 2 — AGENTS.md + rollouts | 0 — none | 0 | 1 | 2 — session files | 1 | 1 | 2 — rollout replay |
| OpenCode | 2 — md memory + events | 0 — plugins only | 1 — plugin surface | 1 | 2 — event-sourced sessions | 2 — schedule tool | 1 | 3 — git-tree snapshots |
| Cursor | 3 — rules + Memories (docs) | 1 | 1 | 3 — per-workspace rules | 2 | 1 | 1 | 2 |
| Windsurf / Devin Desktop | 2 — memories/rules | 1 | 1 | 2 | 2 | 1 | 1 | 2 |
| Devin | 4 — VM snapshots + knowledge | 1 | 1 — playbooks curated | 3 — per-user VM | 3 — per-session VMs | 4 — Jira/Slack triggers | 2 — session outcomes | 3 — VM snapshots |
| Manus | 3 — hibernating VMs | 1 | 1 | 2 | 3 — per-task VMs | 4 — async long-horizon | 1 | 2 — VM hibernate |
| Replit Agent | 2 — checkpoints | 0 | 0 | 1 | 2 | 2 — background agents | 1 | 3 — checkpoints |
| Hermes Agent | 4 — files + Honcho | 4 — bg review loop (src) | 4 — skill create/patch (src) | 4 — USER.md + Honcho | 2 — per-agent dirs | 4 — monitors/schedules | 2 — monitor gates | 2 — file edits only |
| Letta / MemGPT | 5 — block memory + archival (src) | 4 — sleep-time compute | 3 — self-edited tools | 4 — per-agent blocks | 4 — agent-scoped blocks | 4 — sleep-time agents | 2 — reflection triggers | 3 — block history |
| Mem0 | 5 — graph/vector store (docs) | 4 — LLM extraction | 1 — memory only | 3 — scoped profiles | 4 — user/agent scopes | 3 — async writes | 2 — dedup/conflict | 3 — update/delete API |
| Zep / Graphiti | 5 — temporal KG (src) | 4 — auto extraction | 1 — facts only | 3 — entity scopes | 3 — graph namespaces | 3 | 3 — validity windows | 3 — invalidation, not rollback |
| Cognee | 4 — KG pipeline | 2 — ingest-time only | 1 | 2 | 3 — dataset scopes | 1 | 2 — pipeline checks | 2 |
| LangMem | 4 — semantic/episodic/procedural | 4 — bg memory managers | 3 — procedural memory | 3 | 3 | 4 — background managers | 2 | 3 — manager-controlled |
| Supermemory | 4 — hosted memory API | 3 — server extraction | 1 | 3 — profiles | 3 | 3 | 1 | 2 |
| EvoAgentX | 2 — experience buffers | 5 — evolutionary loop | 5 — evolves configs/topology | 1 — benchmark-level | 2 | 2 — batch runs | 4 — benchmark fitness | 3 — population snapshots |
| AgentEvolver | 1 | 4 | 4 — claimed | 1 | 1 | 1 | 2 — self-assessed | 2 |
| A-Evolve | 1 | 5 | 5 — evolves agent code | 1 | 1 | 1 | 3 — search eval | 3 — tree search |
| Reef | 0 — unverified | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| RSIAgent | 1 | 4 — recursive | 4 — claimed | 1 | 1 | 2 | 1 — self-graded | 1 |
| EvoSkill | 1 | 4 — trajectory mutation | 4 — skill mutation | 1 | 1 | 1 | 4 — benchmark-gated | 3 — versioned skills |
| MemSkill | 2 | 3 | 3 | 1 | 1 | 1 | 2 | 2 |
| OpenSkill | 0 — unverified | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Recuris | 0 — unverified | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| altk-evolve | 0 — unverified | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| Proteus | 0 — unverified | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **Vikram (implemented)** | 5 — MemoryStore+LayeredMemory+files+evolution.db | 4 — turn-end reflect + bg review | 4 — SkillEvolution + approval gate | 4 — agent-scoped stores | 4 — per-entity scopes/files | 3 — review cadence, self-check hourly | 4 — verify verdict in experience | 5 — skill versions + transforms + dist |

---

## 3. Best implementation per capability

- **Persistent memory — Letta/MemGPT.** First-class mutable memory blocks with
  core/archival/recall tiers and agent-owned editing is the most complete
  memory *architecture* in the field; Vikram's layered store + temporal graph
  (Zep) covers similar ground with stronger temporal semantics.
- **Temporal/relational memory — Zep/Graphiti.** Bi-temporal facts with
  automatic invalidation is unmatched; Vikram's `MemoryStore` has
  consolidation supersession but no validity windows.
- **Autonomous learning loop — Hermes Agent.** The session-end review that
  proposes memory/skill changes is the production-proven pattern Vikram
  copied and extended with a policy gate.
- **Self-improvement via evolution — EvoAgentX / A-Evolve.** Population-based
  evolution with real fitness functions goes further than anything shipping;
  Vikram deliberately stays heuristic + approval-gated instead.
- **Skill versioning + rollback — Vikram.** `vikram-evolution`'s
  `SkillEvolution` (`skills.rs`) keeps numbered `SkillVersion`s per skill with
  `rollback()` — no surveyed product combines learned-skill creation with
  per-version restore; EvoSkill (research) is the closest.
- **Personalization — Letta (per-agent blocks) / Hermes (USER.md + Honcho).**
  Vikram matches via `USER.md`/`MEMORY.md` (`memory_files.rs`) plus
  agent-scoped memory keys.
- **Multi-agent isolation — Devin (per-session VMs) / Letta (per-agent
  blocks).** Vikram isolates by entity: separate context files, scoped memory
  (`agent:<id>`), hive participant filtering.
- **Background/proactive — Devin/Manus (triggers) / Hermes (monitors).**
  Vikram's hourly self-check + interval-gated learning review is real but
  narrower: no inbound event triggers.
- **Verification-grounded learning — Vikram (unique).** The `verify` tool's
  PASS/FAIL verdict is recorded into the `Experience` (`learning.rs:422-445`,
  `:737-751`) and gates `reusable` (`:774-780`) — no surveyed system feeds a
  build/test verdict into its learning record.
- **Rollback/versioning — Vikram (skills + transforms) / OpenCode (git-tree
  snapshots).** `vikram transforms rollback`, `SkillEvolution::rollback`,
  dist-bundle SHA-256 verification form a three-layer revert story no single
  competitor matches end-to-end.

---

## 4. Cross-cutting patterns

Every system that actually learns converges on the same pipeline:

```
immutable experience record
  → async/deterministic consolidation (dedup, contradiction, scoring)
  → proposal generation (heuristic or model)
  → validated mutation through a policy gate
  → versioned, reversible artifact
```

- **Immutable experiences first.** Letta archives before recall; Zep stores
  episodes before facts; Vikram's `ExperienceCollector.record` sanitizes then
  persists before any reflection runs (`experience.rs:24-60`).
- **Consolidation is separate from capture.** Mem0/Zep run extraction as a
  distinct stage; Vikram's `consolidation.rs` stages → dedups → supersedes →
  expires observations without deleting history.
- **Actor/verifier separation.** Hermes separates the session actor from the
  background reviewer; Vikram separates the turn loop (producer of
  `TurnGrowth`) from `LearningManager`/`apply_or_stage` (the gate) and from
  the operator (approver). Verification is a *third* party: the `verify`
  tool's verdict, not the actor's self-report.
- **File-backed versioned artifacts.** Skills as `SKILL.md` + frontmatter +
  version rows (Vikram, EvoSkill); memory as markdown files (Hermes, Claude
  Code); snapshots as trees (OpenCode). File artifacts are inspectable,
  diffable, and rollback-able — DB-only memory (Mem0 cloud) loses this.
- **What nobody else does:** feeding a *deterministic verification verdict*
  into the learning record, and gating every learned mutation behind a
  per-kind policy (Auto/Approval/Suggest/Disabled) with a full audit log
  (`learning.rs:785-861`).
