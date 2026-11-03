# VIKRAM — A–Z Autonomous Discovery, Indexing, Competitor & Engineering Audit

**Date:** 2026-11-03  
**Scope:** Full Rust workspace (`crates/*`), competitor research, architecture, security, performance, testing.  
**Method:** Multi-pass codebase inspection (12 passes), competitor source reads, dependency analysis.  
**Rule:** No source code modified during audit.

---

## 0. Executive Summary

VIKRAM is a terminal AI runtime in Rust (9 crates, ~15k LOC source) with a multi-user/multi-entity architecture, deny-first permissions, SQLite state, and an OpenAI-compatible provider. It is pre-release (0.1.0) with coherent turn-loop and permission-gating design, but has significant gaps in integration (evolution crate dormant), error handling (silent swallowing), MCP runtime (non-functional serve), and lacks several capabilities that competitors have proven valuable (code indexing, ACP, durable mid-turn resume, sandboxing).

**Key metrics:**
- Crates: 9 (contracts, kernel, policy, store, runtime, skills, tui, cli, mcp) + 1 (evolution, new)
- Source files: ~102 Rust files in crates
- Tests: ~775 (394 runtime, 190 tui, 144 skills, 30 contracts, 28 kernel, 18 policy, 15 cli, 12 evolution, 11 store, 9 mcp, 8 + 8 + 5 + 5 + 8 + 8 + 5)
- Warnings: 4 (3 unused doc comments in contracts, 1 unused import in runtime)
- Test failures: 0 (all pass)

### Post-audit update (2026-11-03)

A unified Ticket System was implemented after this audit: 5 new SQLite tables
(`tickets`, `ticket_participants`, `ticket_messages`, `assignments`, `ticket_events`),
new contract IDs/enums, a `TicketManager` in `vikram-runtime` with backend-enforced
visibility, and `vikram tickets` CLI commands. Hive/bot/sub-agent workflows now share
one engine. See `docs/tickets.md` and `README.md`.

---

## 1. Complete Repository Inventory

### 1.1 Workspace Structure

| Crate | Role | LOC (approx) | Tests |
|---|---|---|---|
| vikram-contracts | Stable typed contracts (IDs, messages, policy, journal, trust, versioning) | ~400 | 30 |
| vikram-kernel | Single execution architecture (operation loop, effect gate, grants, cancellation, token meter) | ~600 | 28 |
| vikram-policy | Deny-first permission evaluation with non-widenable mandatory floor | ~550 | 18 |
| vikram-store | SQLite durable state (sessions, agents, teams, journal, users, profiles, entities, hives, workspaces, artifacts) | ~2000 | 11 |
| vikram-runtime | Host integration (sessions, agents, tasks, teams, providers, tools, MCP, plugins, skills, memory, context, learning, hive, workspace) | ~5000 | 394 |
| vikram-skills | Skill discovery, loading, activation, installation, bundles, history, trust | ~1800 | 144 |
| vikram-tui | Ratatui terminal interface (state, rendering, input, widgets, theme, layout) | ~3000 | 190 |
| vikram-cli | Clap CLI entry point (TUI, chat, operator commands) | ~1200 | 15 |
| vikram-mcp | MCP server (JSON-RPC 2.0 over STDIO), private runtime, discovery, visibility | ~600 | 9 |
| vikram-evolution | Self-evolution subsystem (experiences, reflection, memory, skills, optimization, hive learning, dashboard) | ~1200 | 12 |

### 1.2 Key Directories

- `crates/` — Rust workspace
- `docs/` — 40+ markdown docs (skills, research, decisions, development, mcp)
- `docs/research/` — competitor analysis (coding-agents, autonomous-agents, modularity)
- `codex/` — OpenAI Codex source (reference)
- `openhands2/` — OpenHands frontend (reference)
- `harbor/` — Harbor (reference)
- `inspect_ai/` — Inspect AI (reference)
- `gitui/` — GitUI (reference)

### 1.3 Dependencies (workspace)

Core: serde, serde_json, thiserror, rusqlite (bundled), tokio, futures-util, reqwest, ratatui, crossterm, ratatui-textarea, clap, unicode-width, insta, tempfile.

---

## 2. Architecture Map

### 2.1 Crate Dependency Graph

```
vikram-cli → vikram-tui, vikram-runtime, vikram-skills, vikram-store
vikram-tui → vikram-runtime, vikram-kernel, vikram-contracts
vikram-runtime → vikram-contracts, vikram-kernel, vikram-policy, vikram-store, vikram-skills, vikram-mcp, vikram-evolution
vikram-mcp → vikram-contracts
vikram-skills → vikram-contracts
vikram-evolution → vikram-contracts, vikram-store, vikram-skills (unused deps)
vikram-store → vikram-contracts
vikram-policy → vikram-contracts
vikram-kernel → vikram-contracts
vikram-contracts → (none)
```

### 2.2 Execution Flow

```
User Input (TUI/CLI/Chat)
  → Runtime::boot (load store, memory, provider, MCP, plugins, skills, evolution)
  → SessionRuntime::begin_turn
  → Turn::run_turn (loop)
    → Inject skill catalog + agent identity + preamble
    → Stream provider (OpenAI-compatible SSE)
    → Commit assistant text/thinking
    → If tool calls: run_tool (policy + approval gate + spawn_blocking)
    → Append tool results, repeat
  → Finish: record usage, emit TurnFinished, update agent/task state
  → Shutdown: cancel all, emit Shutdown
```

### 2.3 Data Flow

```
User → TUI → RuntimeHandle → SessionRuntime → Turn loop → Provider → Tool → Store/Memory → Journal → TUI events
```

### 2.4 Agent Interaction

```
Vikram (Main Agent) → direct response | agent delegation | bot invocation | hive group chat
  → Each entity has: identity, title, role, specialization, description, status
  → Private memory per entity, shared hive memory
  → Task ownership via TaskManager
  → Permission enforcement independent of title
```

---

## 3. File and Symbol Index (Summary)

### 3.1 Runtime Modules (34 public modules)

| Module | Purpose | Key Types |
|---|---|---|
| agents | Background agents + task bookkeeping | ManagedAgent, AgentManager, TaskManager |
| browser | Headless Chromium driver + CDP | navigate, screenshot, cdp::* |
| bus | Broadcast fan-out | EventBus (cap 4096) |
| computer | Desktop automation | click, type_text, key, scroll, screenshot, ocr |
| config | Config, home discovery, mode availability | Config, ModeAvailability, vikram_home, resolve_within |
| context | Permission-aware markdown context loader | ContextLoader, ContextBundle, ContextFile |
| error | Runtime error enum | RuntimeError, Result |
| hive | **DEAD** in-memory team types | ManagedTeam, TeamManager (unused) |
| hive_chat | Persistent HIVE group chat | HiveManager |
| learning | Self-improvement proposals/audit/policy | LearningManager, LearningProposal, LearningPolicy |
| lsp | LSP client pool | LspPool, LspClient, ServerCommand |
| mcp | MCP server discovery/connections/tools | McpRegistry, McpToolInfo, connect_all, call_tool |
| memory | SQLite key/value memory | MemoryStore |
| openai | OpenAI-compatible SSE provider | OpenAiProvider, wire_name, from_wire_name, list_models |
| plugins | Plugin manifests/lifecycle/deps/integrity | PluginRegistry, PluginManifest, PluginPermission |
| provider | Provider trait seam | Provider, CompletionRequest, ProviderEvent, ToolSpec |
| pty | Persistent PTY sessions | open, open_named, write, read, resize, close, list |
| rules | Durable allow-list | RuleBook, add_amendment, remove, persist |
| runtime | Facade + boot + handle API | Runtime, RuntimeHandle, RuntimeInner, AgentControl |
| search | Bing/Brave web search | search, parse_bing, parse_brave, client |
| session | Live session state | SessionRuntime, from_row, snapshot, begin/finish_turn |
| skills | Skill discovery/loading adapter | SkillRegistry, Skill |
| teams | JSON-backed team management | TeamManager, Team |
| tools | Tool trait + built-ins + registry | Tool, ToolRegistry, ToolContext, built-in tools |
| turn | Turn loop, gating, tool execution | run_turn, spawn_turn, run_tool, run_operator_tool |
| types | Runtime value types/events | RuntimeEvent, Metrics, StateSnapshot, ActivityEvent |
| workspace | Session workspaces, projects, artifacts, checkpoints | WorkspaceManager |

### 3.2 Contracts Types

IDs: SessionId, TurnId, AgentId, TeamId, EffectId, ToolCallId, ApprovalId, UserId, ProfileId, EntityId, HiveId, HiveMessageId, TicketId, AssignmentId, TicketMessageId, TicketEventId, ProjectId, WorkspaceId.  
Messages: Role, ToolCall, Block (Text/Reasoning/ToolCall/ToolResult), Message.  
Policy: ActionKind, SideEffect, PermissionRequest, RuleOutcome, Decision, PermissionMode (7 variants), Posture, PermissionPolicy trait.  
Journal: RecordKind, NewRecord, JournalRecord, Appended, JournalSink trait, InMemoryJournal.  
Trust: Trust (Host/User/Model/External) with authority() ordering.  
Version: ContractSpec, ContractDecl, Versioned trait, component-id constants.

### 3.3 Kernel Types

Outcome, Operation trait, PendingCall, RunContext, Limits, RunReport, Machine.  
Effect enum: EmitMessage, RequestTool, StreamDelta, Status, Activity, Compaction.  
Grant, GrantSnapshot, GrantSet, ApprovalTicket, ApprovalSource, ApprovalBroker.  
CancellationToken, Clock (SystemClock/FixedClock), TokenMeter.

### 3.4 Store Tables (~20)

sessions, agents, teams, team_members, journal, users, profiles, entities, active_identity, hive_sessions, hive_participants, hive_messages, projects, session_workspaces, artifacts, file_changes, tickets, ticket_participants, ticket_messages, assignments, ticket_events.

---

## 4. Dependency Graph Analysis

### 4.1 Internal Dependencies

No circular dependencies detected. Clean layered architecture: contracts → kernel/policy/store → runtime → tui/cli.

### 4.2 External Dependencies

Rust 2024 edition, tokio 1.54, rusqlite 0.41 (bundled), ratatui 0.30, crossterm 0.29, clap 4.6, reqwest 0.13, serde 1.0.

### 4.3 Unused Dependencies

- vikram-evolution: `vikram-contracts`, `vikram-store`, `vikram-skills`, `tokio` declared but unused.
- vikram-mcp: edition 2024 and version 0.1.0 not workspace-aligned.

### 4.4 Dead Code

| Location | Item | Status |
|---|---|---|
| crates/vikram-runtime/src/hive.rs | Entire module (ManagedTeam, TeamManager) | Unused, lib.rs exposes pub mod but no re-export |
| crates/vikram-runtime/src/turn.rs:861 | assistant_with_calls | #[allow(dead_code)] |
| crates/vikram-runtime/src/runtime.rs:298-307 | mcp_visibility, mcp_private_runtime | #[allow(dead_code)] |
| crates/vikram-runtime/src/skills.rs:103 | merge | No-op (does nothing) |
| crates/vikram-runtime/src/skills.rs:144 | scan_dir | Trivial alias for scan |
| crates/vikram-runtime/src/learning.rs | policy_instruction, policy_identity | Write-only, no getters/callers |
| crates/vikram-runtime/src/hive_chat.rs | HiveManager::update_state | No callers |
| crates/vikram-runtime/src/context.rs | with_budget, invalidate_all | No external callers |
| crates/vikram-runtime/src/runtime.rs:279 | skills_loaded metric | Always 0 |
| crates/vikram-tui/src/terminal.rs:1748 | press | #[allow(dead_code)] |
| crates/vikram-mcp/src/runtime.rs:104 | base_dir field | #[allow(dead_code)] |
| crates/vikram-mcp/src/runtime.rs | ManagedServerConfig | Never persisted/loaded |
| crates/vikram-store | session_count, list_all_sessions, search_sessions, delete_file_changes_for_session | No callers |
| crates/vikram-skills/src/trust.rs:35-37 | TrustPolicy::load_in | Silently defaults on corrupt file |

---

## 5. Execution-Flow and Data-Flow Diagrams

### 5.1 Turn Loop (Detailed)

```
run_turn(session_id, message)
  ├── Journal append (user message)
  ├── begin_turn (state transition)
  ├── Emit StreamStarted
  ├── Loop:
  │   ├── Check cancel/pause
  │   ├── Inject: skill catalog + agent identity + preamble
  │   ├── Stream provider (SSE)
  │   ├── Commit assistant text/thinking
  │   ├── If tool calls:
  │   │   └── run_tool (for each):
  │   │       ├── Lookup tool, check enabled
  │   │       ├── Get PermissionRequest
  │   │       ├── Resolve path within workspace
  │   │       ├── Evaluate policy + posture (judge)
  │   │       ├── Deny → return error
  │   │       ├── RequireApproval → wait on oneshot
  │   │       ├── Allow → spawn_blocking run tool
  │   │       ├── Handle ask_user question gate
  │   │       ├── Journal tool result
  │   │       └── Emit ToolStarted/ToolFinished
  │   └── Append tool results, repeat
  └── Finish: record usage, emit TurnFinished, update agent/task state, dequeue next
```

### 5.2 Permission Gate (Detailed)

```
PermissionRequest (action, resource, trust)
  → MandatoryFloor::veto() → Deny if:
     - Untrusted content calling grant./policy./self. verbs
     - Delegation depth ≥ max_delegation_depth (3)
     - Write outside workspace root
  → Operator rules scan (deny immediate, ask accumulates, allow sets flag)
  → If no operator rule: default rules scan
  → If no rule matches: Deny ("no rule permits")
  → PermissionMode::resolve() applies posture (allow-list/ask/unattended)
  → Decision: Allow | Deny | RequireApproval
```

### 5.3 Kernel Machine (Detailed)

```
Machine::run (loop, max_steps=64)
  ├── Refresh grants snapshot
  ├── Check cancellation
  ├── Increment step counter
  ├── Enforce max_steps
  ├── Run all operations in registration order
  ├── Collect Outcome::Applied(effects)
  ├── Dispatch each effect:
  │   ├── Journal append (idempotency check)
  │   ├── If RequestTool: evaluate policy (Trust::Model hardcoded)
  │   ├── Apply posture via ctx.mode.resolve()
  │   ├── Allow → push pending, handler.apply
  │   ├── Deny → record denial
  │   └── RequireApproval → push pending, set awaiting, record approval
  └── Stop when finished or no progress
```

---

## 6. Agent Interaction Diagrams

### 6.1 Entity Model

```
User (human)
  └── Profile (working context)
      └── Main Agent (Vikram) — owns SOUL/INSTRUCTIONS/MEMORY/TOOLS.md
          ├── Agent (delegated sub-agent) — own context files
          ├── Bot (dedicated-purpose assistant) — own context files
          └── Hive (group chat, not an entity) — sequential/parallel execution
              └── Team (reusable group configuration)
```

### 6.2 Memory Architecture

```
Working Memory (in-turn context)
  → Short-term (session-scoped)
  → Long-term (SQLite-backed, per user/profile/entity)
  → Episodic (experience records)
  → Semantic (knowledge entries)
  → Procedural (skills)
  → Shared Hive Memory (authorized context only)
```

---

## 7. Subsystem Status Matrix

| Subsystem | Status | Maturity | Key Gaps |
|---|---|---|---|
| Contracts | ✅ Complete | Stable | Macro doc-loss, no error chaining |
| Kernel | ✅ Complete | Stable | Trust hardcoded to Model, grants not self-enforcing, no approval IDs |
| Policy | ✅ Complete | Stable | Floor vetoes (deny) vs docs (approval), normalize_request only Read/Write |
| Store | ✅ Complete | Stable | Sessions disconnected from users, create_project stale id, silent error swallowing |
| Runtime | ✅ Complete | Stable | God struct, dead hive.rs, write-only learning policies, silent errors |
| Context Loader | ✅ Complete | Stable | Hardcoded 128k budget, silent cache lock failures |
| Memory | ✅ Complete | Stable | Silent DB errors, no consolidation |
| Provider (OpenAI) | ✅ Complete | Stable | No rebuild path if missing at boot, no local model support |
| Tools | ✅ Complete | Stable | No cancellation for spawn_blocking, hardcoded timeouts, no ToolContext identity |
| Turn Loop | ✅ Complete | Stable | Skills loaded always 0, context_used heuristic |
| MCP Client | ⚠️ Partial | Alpha | connect_all always Ok, silent failures |
| MCP Server | ⚠️ Partial | Alpha | serve command is stub, null stdio pipes |
| Plugins | ✅ Complete | Stable | — |
| Skills | ✅ Complete | Stable | Trust file corrupt defaults, builtin source abstraction missing |
| TUI | ✅ Complete | Stable | Hardcoded budgets/widths, UTF-8 byte slicing panic risk |
| CLI | ✅ Complete | Stable | MCP serve stub, doctor terminal_size inaccurate |
| Hive Chat | ✅ Complete | Stable | update_state unused, Trust::User for all messages |
| Tickets | ✅ Implemented (post-audit) | Alpha | Unified engine for Hive/bots/sub-agents; strategies recorded as metadata |
| Teams | ⚠️ Duplicate | Alpha | Two incompatible team models (teams.rs vs hive.rs) |
| Learning | ⚠️ Partial | Alpha | Write-only policies, no persistence for instruction/identity |
| Evolution | ⚠️ Dormant | Alpha | Not integrated, unused deps, silent errors, no async runner |
| Workspace | ✅ Complete | Stable | Custom checksum, canonicalize fallback |
| Search | ✅ Complete | Stable | Hardcoded max 10 results |
| Browser | ✅ Complete | Stable | Hardcoded timeouts |
| Computer | ✅ Complete | Stable | Hardcoded limits |
| LSP | ✅ Complete | Stable | — |
| PTY | ✅ Complete | Stable | Hardcoded buffer size |

---

## 8. Competitor Research Report

### 8.1 Competitors Investigated

1. **OpenClaw** (openclaw/openclaw, MIT, TypeScript+Rust) — 393k stars, local-first gateway
2. **Hermes Agent** (NousResearch/hermes-agent, MIT, Python) — 259k stars, self-improving
3. **Letta/letta-code** (Apache-2.0, TypeScript) — stateful agents, memory blocks
4. **OpenAI Codex** (Apache-2.0, Rust) — 205 crates, shell-centric, strong sandbox
5. **Goose** (Apache-2.0, Rust) — operation pipeline, 53 declarative providers
6. **OpenCode** (MIT, TypeScript) — durable semantics, schedule tool
7. **Gemini CLI** (Apache-2.0, TypeScript) — policy engine, hook catalogue
8. **OpenHands** (MIT, Python/TS) — ACP, container sandbox
9. **Aider** (Apache-2.0, Python) — tree-sitter repo-map, dormant since 2026-05
10. **Claude Code** (proprietary) — plugin-first, mods system

### 8.2 Key Competitor Findings

- **Nobody indexes code** — zero embedding/vector retrieval for source across all competitors. Agentic grep won on capability.
- **Leaders converging on harnesses, not engines** — ACP is the emerging orchestration rail.
- **Goose's Operation/Effect kernel** is the best Rust template (363 LOC, cancellation finalization, provider-capability-driven).
- **OpenCode's durable semantics** (inbox/steer, compaction-as-data, stale-row settlement, permission triple, Unattended ruleset, schedule tool) are the reference.
- **Codex's security kernel** (Seatbelt/bwrap/Landlock, pure approval function, execpolicy DSL, 10-layer config with non-widenable requirements tier) is the reference.
- **Claude Code's dogfood rule** (first-party features as plugins on same hook API) is the architectural lesson.

### 8.3 Four-Way Comparison (Vikram vs Hermes vs Codex vs OpenClaw)

| Dimension | Vikram | Hermes | Codex | OpenClaw |
|---|---|---|---|---|
| Language | Rust | Python | Rust | TS + Rust |
| Execution model | Turn loop + kernel machine | Gateway + cron | 3 nested loops | Gateway process |
| Sandbox | None (policy only) | Terminal backend-dependent | OS-level (Seatbelt/bwrap/Landlock) | Docker/Podman/SSH (off by default) |
| Memory | SQLite KV | FTS5 + Honcho + skills | Rollout JSONL + SQLite index | SQLite + Dreaming consolidation |
| Providers | OpenAI-compatible only | 6+ (Nous, OpenRouter, OpenAI, Ollama, custom) | OpenAI Responses (hardwired) | 4-layer (provider/model/runtime/channel) |
| Scheduling | None | Built-in cron (60s ticker, monitor mode) | SleepHandler + wait_agent | cron/interval/on-exit/stream triggers |
| Multi-agent | Agents/Bots/Hives | Hosted-room discussions | Subagents (depth-limited) | Subagents + continuation sessions |
| Security | Deny-first + floor + posture | Approval floors + guardian LLM | OS sandbox + execpolicy DSL | Tool policy ≠ sandbox ≠ elevated |
| Extensibility | Skills + plugins + MCP | Skills + hooks + MCP | MCP + hooks + plugins | Skills + hooks + MCP |
| Code indexing | None | None | None (agentic grep) | None |
| ACP | None | Both directions | None | None |
| Self-improvement | Learning crate (JSON) + Evolution (SQLite, dormant) | background_review fork + skill creation | None | Dreaming consolidation |

---

## 9. Weakness and Defect Register

### P0 — Critical

| ID | Category | Description | Evidence | File/Module |
|---|---|---|---|---|
| W-001 | Architecture | Trust hardcoded to Model in kernel — all actions evaluated as model-sourced, undermining provenance | `machine.rs:219` `effect.permission_request(Trust::Model)` | vikram-kernel/src/machine.rs |
| W-002 | Security | MCP serve command is a stub — prints success without starting server | `main.rs` ~805-810 | vikram-cli/src/main.rs |
| W-003 | Security | MCP runtime sets stdio to null — cannot communicate with child processes | `runtime.rs:50-83` `Stdio::null()` | vikram-mcp/src/runtime.rs |
| W-004 | Reliability | Silent error swallowing in memory store — DB errors hidden, returns empty | `memory.rs:119-131, 143-161, 169-183` | vikram-runtime/src/memory.rs |
| W-005 | Reliability | connect_all always returns Ok even if servers failed | `mcp.rs:242` | vikram-runtime/src/mcp.rs |
| W-006 | Reliability | Evolution engine open failure falls back to in-memory silently | `runtime.rs:462-466` | vikram-runtime/src/runtime.rs |

### P1 — High

| ID | Category | Description | Evidence | File/Module |
|---|---|---|---|---|
| W-007 | Architecture | Duplicate team abstractions (teams.rs JSON vs hive.rs in-memory unused) | `hive.rs` entire module unused | vikram-runtime/src/hive.rs |
| W-008 | Architecture | God struct RuntimeInner mixes 20+ concerns | `runtime.rs:57-95` | vikram-runtime/src/runtime.rs |
| W-009 | Architecture | Two learning systems (JSON LearningManager + SQLite EvolutionEngine) with parallel types | `learning.rs` vs `vikram-evolution` | vikram-runtime/src/learning.rs |
| W-010 | Architecture | Evolution crate dormant — no caller invokes evolution() | `runtime.rs:907` | vikram-runtime/src/runtime.rs |
| W-011 | Reliability | Silent error swallowing in store replay_messages (JSON parse errors → Value::Null) | `store/lib.rs:1581` | vikram-store/src/lib.rs |
| W-012 | Reliability | Silent error swallowing in learning (file read/write errors ignored) | `learning.rs:115-124, 139-144` | vikram-runtime/src/learning.rs |
| W-013 | Reliability | Silent error swallowing in config (load_requirements, env_requirements) | `config.rs:219-246` | vikram-runtime/src/config.rs |
| W-014 | Reliability | Silent error swallowing in context (cache lock failures → None) | `context.rs:129-141` | vikram-runtime/src/context.rs |
| W-015 | Reliability | Poisoned mutex panics (session.rs, agents.rs, runtime.rs use .expect()) | Multiple .lock().expect() | Various |
| W-016 | Data | Sessions disconnected from users/profiles (create_session never sets user_id/profile_id) | `store/lib.rs:308-340` | vikram-store/src/lib.rs |
| W-017 | Data | create_project returns stale id on conflict (ON CONFLICT DO UPDATE) | `store/lib.rs:529-545` | vikram-store/src/lib.rs |
| W-018 | Data | TeamSnapshot.id derived from name instead of persisted id | `runtime.rs:360` | vikram-runtime/src/runtime.rs |
| W-019 | Data | Entity kind parsing defaults unknown to Agent silently | `store/lib.rs:1600, 1511` | vikram-store/src/lib.rs |
| W-020 | Reliability | Evolution store: list_* silently drops row mapping errors | `store.rs:108-113, 180-189, ...` | vikram-evolution/src/store.rs |
| W-021 | Reliability | Evolution store: JSON parse failures silently default | `store.rs:72-79, 481-488` | vikram-evolution/src/store.rs |
| W-022 | Reliability | Evolution store: count queries use unwrap_or(0) hiding errors | `store.rs:119, 225` | vikram-evolution/src/store.rs |
| W-023 | Architecture | No provider rebuild path — set_model can't instantiate provider if missing at boot | `runtime.rs:1066` | vikram-runtime/src/runtime.rs |
| W-024 | Architecture | No cancellation handle passed to spawn_blocking tool runs | `turn.rs:695` | vikram-runtime/src/turn.rs |
| W-025 | Architecture | ToolContext only carries workspace_root — no session/agent identity | `tools.rs:30-33` | vikram-runtime/src/tools.rs |
| W-026 | Performance | Skills loaded metric always 0 (never tracked) | `runtime.rs:279`, `turn.rs:159-161` | vikram-runtime/src/runtime.rs |
| W-027 | Performance | Context used metric uses len/4 heuristic, not actual token counts | `runtime.rs:233-242` | vikram-runtime/src/runtime.rs |
| W-028 | Architecture | Learning policies (instruction/identity) write-only, never persisted | `learning.rs` | vikram-runtime/src/learning.rs |
| W-029 | Architecture | Hive messages use Trust::User for all participants | `runtime.rs:1531` | vikram-runtime/src/runtime.rs |
| W-030 | Architecture | Hive entities get no tools during execution | `runtime.rs:1531` | vikram-runtime/src/runtime.rs |
| W-031 | Architecture | Skills merge is no-op, scan_dir is trivial alias | `skills.rs:103, 144` | vikram-runtime/src/skills.rs |
| W-032 | Architecture | ContextLoader with_budget and invalidate_all unused | `context.rs` | vikram-runtime/src/context.rs |
| W-033 | Architecture | HiveManager::update_state has no callers | `hive_chat.rs` | vikram-runtime/src/hive_chat.rs |

### P2 — Medium

| ID | Category | Description | Evidence | File/Module |
|---|---|---|---|---|
| W-034 | Security | TUI input resolves references with synchronous file reads, UTF-8 byte slicing panic risk | `input.rs:830-864` | vikram-tui/src/input.rs |
| W-035 | Security | CLI tools.rs byte-slices description with byte index — UTF-8 panic risk | `tools.rs:93-94` | vikram-cli/src/tools.rs |
| W-036 | Reliability | TUI terminal restore ignores all results | `terminal.rs` | vikram-tui/src/terminal.rs |
| W-037 | Reliability | MCP runtime ignores child.kill() result, futures_stop doesn't await | `runtime.rs:85-93, 132-139` | vikram-mcp/src/runtime.rs |
| W-038 | Reliability | Skills remove_path ignores all errors, move_dir leaves partial state | `paths.rs:286-295` | vikram-skills/src/paths.rs |
| W-039 | Reliability | Evolution SkillEvolution ignores create_dir_all/backup write errors | `skills.rs:51, 76, 79` | vikram-evolution/src/skills.rs |
| W-040 | Reliability | Evolution OptimizationEngine promote/reject/rollback silently no-op | `optimization.rs:110-137` | vikram-evolution/src/optimization.rs |
| W-041 | Reliability | Evolution memory supersession is self-referential (entry keeps same id then marks itself superseded) | `memory.rs:47-54` | vikram-evolution/src/memory.rs |
| W-042 | Reliability | Evolution consolidate marks duplicates superseded but leaves them in listings | `memory.rs:97-130` | vikram-evolution/src/memory.rs |
| W-043 | Reliability | Evolution search_memory filters in Rust after SQL LIMIT — matches can be missed | `store.rs:164-189` | vikram-evolution/src/store.rs |
| W-044 | Reliability | Evolution now_ms/gen_id return 0 on time error | `types.rs:8, 17` | vikram-evolution/src/types.rs |
| W-045 | Reliability | Evolution from_connection ignores pragma errors (.ok()) | `store.rs:33-34` | vikram-evolution/src/store.rs |
| W-046 | Reliability | Store delete_session doesn't unlink team/session_workspace references | `store/lib.rs` | vikram-store/src/lib.rs |
| W-047 | Reliability | Store delete_project doesn't cascade session project_id | `store/lib.rs` | vikram-store/src/lib.rs |
| W-048 | Reliability | Store delete_user deletes profile rows directly instead of calling delete_profile | `store/lib.rs:1032-1034` | vikram-store/src/lib.rs |
| W-049 | Reliability | Store delete_team/delete_hive transactional but no FK declarations | `store/lib.rs:901-909` | vikram-store/src/lib.rs |
| W-050 | Architecture | Evolution create_skill always version 1, improve_skill increments — inconsistent versioning | `skills.rs:41-63 vs 65-93` | vikram-evolution/src/skills.rs |
| W-051 | Architecture | Evolution skill versions in both filesystem and DB, rollback uses DB only | `skills.rs:50-92` | vikram-evolution/src/skills.rs |
| W-052 | Architecture | Evolution reject() accepts reason but never persists it | `optimization.rs:120-127` | vikram-evolution/src/optimization.rs |
| W-053 | Architecture | Evolution rollback() doesn't restore content | `optimization.rs:131-137` | vikram-evolution/src/optimization.rs |
| W-054 | Architecture | Evolution EvaluationResult has no primary key | `store.rs:637-645` | vikram-evolution/src/store.rs |
| W-055 | Architecture | Evolution HiveLearning sanitizes lesson but not pattern/agents/outcome/task_id | `hive.rs:13-32` | vikram-evolution/src/hive.rs |
| W-056 | Architecture | Evolution ReflectionEngine constructs own subsystems instead of sharing refs | `reflection.rs:88-96` | vikram-evolution/src/reflection.rs |
| W-057 | Architecture | Evolution no migration versioning (CREATE IF NOT EXISTS only) | `store.rs:561` | vikram-evolution/src/store.rs |
| W-058 | Architecture | Evolution no async/background job runner despite docs | `reflection.rs:194` | vikram-evolution/src/reflection.rs |
| W-059 | Architecture | Evolution no path-traversal guard on skill names | `skills.rs:17-21` | vikram-evolution/src/skills.rs |
| W-060 | Architecture | Evolution no TTL enforcement or superseded filtering in recall/list | `memory.rs:79-91` | vikram-evolution/src/memory.rs |
| W-061 | Architecture | Evolution no validation that candidates exist in evaluate/promote | `optimization.rs` | vikram-evolution/src/optimization.rs |
| W-062 | Architecture | Evolution stringly typed fields (outcome, status, kind) | `types.rs` | vikram-evolution/src/types.rs |
| W-063 | Architecture | Evolution no store/repository trait (concrete SQLite) | `store.rs` | vikram-evolution/src/store.rs |
| W-064 | Architecture | Evolution no approval-application framework for create_skill/improve_skill | `reflection.rs` | vikram-evolution/src/reflection.rs |
| W-065 | Architecture | Evolution no integration of optimization candidates with skill versions | `optimization.rs` | vikram-evolution/src/optimization.rs |
| W-066 | Architecture | Evolution Dashboard lacks store() accessor | `dashboard.rs` | vikram-evolution/src/dashboard.rs |
| W-067 | Architecture | Evolution fields are public mutable containers | `lib.rs` | vikram-evolution/src/lib.rs |
| W-068 | Architecture | Evolution unused error variants (Validation, Budget, Conflict, Serde) | `error.rs:9-20` | vikram-evolution/src/error.rs |
| W-069 | Architecture | Evolution unused crate deps (contracts, store, skills, tokio) | `Cargo.toml:9-16` | vikram-evolution/Cargo.toml |
| W-070 | Architecture | Duplicate team models incompatible (teams.rs vs hive.rs) | `hive.rs` vs `teams.rs` | vikram-runtime/src/ |
| W-071 | Architecture | Policy floor vetoes (deny) vs docs say "need a human" (approval) | `policy/lib.rs:272-287` | vikram-policy/src/lib.rs |
| W-072 | Architecture | Policy floor only vetoes Write outside root, not Execute/Network | `policy/lib.rs:281` | vikram-policy/src/lib.rs |
| W-073 | Architecture | Policy normalize_request only handles Read/Write | `policy/lib.rs:405` | vikram-policy/src/lib.rs |
| W-074 | Architecture | Policy Rule::actions empty = match-all (latent wildcard) | `policy/lib.rs:179` | vikram-policy/src/lib.rs |
| W-075 | Architecture | Policy ResourcePattern::as_str lossy for Prefix/Subtree | `policy/lib.rs:64-71` | vikram-policy/src/lib.rs |
| W-076 | Architecture | Policy has_allow doesn't account for floor | `policy/lib.rs:436-442` | vikram-policy/src/lib.rs |
| W-077 | Architecture | Kernel grants widening not self-enforcing (widen ignores ticket scope) | `grants.rs:83` | vikram-kernel/src/grants.rs |
| W-078 | Architecture | Kernel approval tickets unaddressable (no id) | `grants.rs:93-97` | vikram-kernel/src/grants.rs |
| W-079 | Architecture | Kernel Handled::Deferred leaves ctx.awaiting unset | `machine.rs:290-293` | vikram-kernel/src/machine.rs |
| W-080 | Architecture | Kernel push pending twice for allowed RequestTool (asymmetric paths) | `machine.rs:265-272` | vikram-kernel/src/machine.rs |
| W-081 | Architecture | Kernel .expect() on tool effects (production panic point) | `machine.rs:270` | vikram-kernel/src/machine.rs |
| W-082 | Architecture | Contracts journal InMemoryJournal read_after relies on insertion order | `journal.rs:109-195` | vikram-contracts/src/journal.rs |
| W-083 | Architecture | Contracts NewRecord defaults ts_ms=0 (callers must remember .at()) | `journal.rs:47` | vikram-contracts/src/journal.rs |
| W-084 | Architecture | Contracts VikramError no source error chaining | `error.rs` | vikram-contracts/src/error.rs |
| W-085 | Architecture | Contracts ids.rs macro doc-loss (3 unused doc comment warnings) | `ids.rs:54,59,61` | vikram-contracts/src/ids.rs |
| W-086 | Architecture | TokenMeter record_reported indistinguishable from never called | `run.rs:81-82` | vikram-kernel/src/run.rs |
| W-087 | Architecture | MCP visibility set_exposed_tools doesn't persist (unlike set_visibility) | `visibility.rs:115-119` | vikram-mcp/src/visibility.rs |
| W-088 | Architecture | MCP visibility manager doesn't load existing entries in new/with_persist_path | `visibility.rs:52-64` | vikram-mcp/src/visibility.rs |
| W-089 | Architecture | MCP server auth_required field never enforced | `server.rs:20` | vikram-mcp/src/server.rs |
| W-090 | Architecture | MCP approved_tools empty = all approved (implicit default) | `server.rs` | vikram-mcp/src/server.rs |
| W-091 | Architecture | MCP discovery only static catalog, no network/registry | `discovery.rs` | vikram-mcp/src/discovery.rs |
| W-092 | Architecture | MCP edition 2024 and version 0.1.0 not workspace-aligned | `Cargo.toml` | vikram-mcp/Cargo.toml |
| W-093 | Architecture | Skills ActivationPolicy::is_automatic doc/code mismatch (Suggested returns true) | `metadata.rs:53-55` | vikram-skills/src/metadata.rs |
| W-094 | Architecture | Skills TrustPolicy::load_in silently defaults on corrupt file | `trust.rs:35-37` | vikram-skills/src/trust.rs |
| W-095 | Architecture | Skills History latest() in-memory vs record() re-reads disk (staleness) | `history.rs:91-104` | vikram-skills/src/history.rs |
| W-096 | Architecture | Skills builtin root doesn't exist on disk (special-cased) | `builtin.rs` | vikram-skills/src/builtin.rs |
| W-097 | Architecture | Skills Manager state_dir uses root/.vikram (may diverge from global) | `manager.rs:52-54` | vikram-skills/src/manager.rs |
| W-098 | Architecture | Skills install git clone no timeout, --branch fails on commit SHA | `install.rs:103-108` | vikram-skills/src/install.rs |
| W-099 | Architecture | Skills BundleIndex::resolve runs Catalog::discover (side effect in library) | `bundle.rs:92-95` | vikram-skills/src/bundle.rs |
| W-100 | Architecture | Skills loader read_resource refuses scripts hardcoded (not trust check) | `loader.rs:157-165` | vikram-skills/src/loader.rs |
| W-101 | Architecture | TUI Outcome::Info declared but treated as error in dispatcher | `commands.rs`, `terminal.rs` | vikram-tui/src/ |
| W-102 | Architecture | TUI app.rs CAP=64 hardcoded activity cap with potential index off-by-one | `app.rs:461` | vikram-tui/src/app.rs |
| W-103 | Architecture | TUI layout hardcoded collapse thresholds | `layout.rs:24-25` | vikram-tui/src/layout.rs |
| W-104 | Architecture | TUI theme hardcoded RGB palette, no configurability | `theme.rs` | vikram-tui/src/theme.rs |
| W-105 | Architecture | TUI session_by_id private vs session_by_id_mut public (asymmetry) | `app.rs:866` | vikram-tui/src/app.rs |
| W-106 | Architecture | CLI runtime() uses .expect() (panic on failure) | `main.rs:333` | vikram-cli/src/main.rs |
| W-107 | Architecture | CLI doctor terminal_size reads env instead of actual terminal | `doctor.rs:194` | vikram-cli/src/doctor.rs |
| W-108 | Architecture | CLI chat forcibly switches Ask→Unattended in headless | `chat.rs:42-46` | vikram-cli/src/chat.rs |
| W-109 | Architecture | CLI skills history/rollback uses global History::load (not load_in) | `skills.rs` | vikram-cli/src/skills.rs |
| W-110 | Architecture | CLI McpAction::List empty arm falls through | `main.rs` | vikram-cli/src/main.rs |
| W-111 | Architecture | CLI tools.rs validate with Some(path) uses global Manager | `skills.rs:381` | vikram-cli/src/skills.rs |
| W-112 | Architecture | MCP runtime base_dir unused, ManagedServerConfig never persisted | `runtime.rs:104` | vikram-mcp/src/runtime.rs |
| W-113 | Architecture | Store TeamRow missing updated_ms field (schema has column) | `lib.rs:133-134` | vikram-store/src/lib.rs |
| W-114 | Architecture | Store active_identity singleton check | `lib.rs` | vikram-store/src/lib.rs |
| W-115 | Architecture | Kernel CancellationToken uses SeqCst (fine but must share same token) | `run.rs:10-27` | vikram-kernel/src/run.rs |
| W-116 | Architecture | Kernel PendingCall has no originating trust | `machine.rs` | vikram-kernel/src/machine.rs |

### P3 — Low

| ID | Category | Description |
|---|---|---|
| W-117 | Performance | Hardcoded values scattered (context 128k, bus 4096, checkpoint 5MB, tool output 20k, pty 256k, etc.) |
| W-118 | Performance | Context loader scans workspace on every metrics call |
| W-119 | Performance | Skills budgets hardcoded (16k/24k/48k/12k/32) |
| W-120 | Performance | TUI collapse thresholds hardcoded (88 width, 16 height) |
| W-121 | Performance | CLI column widths hardcoded |
| W-122 | UX | No code indexing (agentic grep only) |
| W-123 | UX | No ACP support (can't host or be hosted by other agents) |
| W-124 | UX | No durable mid-turn resume (crash = restart from scratch) |
| W-125 | UX | No sandbox (policy-only, no OS isolation) |
| W-126 | UX | No scheduling/proactive triggers |
| W-127 | UX | No local model support (OpenAI-compatible only) |
| W-128 | UX | No codebase understanding (no repo-map, no embeddings) |
| W-129 | UX | No verification loop (edit→build→test→repair) |
| W-130 | UX | No A2A/multi-agent protocol |
| W-131 | UX | No desktop/phone notification surface |
| W-132 | UX | No durable background task scheduling |
| W-133 | Documentation | Docs may not match code (e.g., floor says "human" but vetoes) |
| W-134 | Documentation | Macro doc-loss in ids.rs |
| W-135 | Documentation | ActivationPolicy::is_automatic doc/code mismatch |
| W-136 | Testing | No property-based tests |
| W-137 | Testing | No fuzz testing |
| W-138 | Testing | No concurrency tests |
| W-139 | Testing | No failure-injection tests |
| W-140 | Testing | No performance tests |
| W-141 | Testing | No security tests (prompt injection, etc.) |
| W-142 | Testing | No agent task benchmarks |
| W-143 | Testing | No E2E tests |
| W-144 | Testing | Tests validate execution, not behavior in many cases |

### P4 — Future Research

| ID | Category | Description |
|---|---|---|
| W-145 | Research | Formal crash-recovery state machine (pending→claimed→executing→settlement) |
| W-146 | Research | Wasm component model for runtime-swappable subsystems |
| W-147 | Research | VRP (VIKRAM Runtime Protocol) for Level-4 runtime replacement |
| W-148 | Research | TUF-like supply chain for plugin artifacts |
| W-149 | Research | Deterministic token meter at provider boundary |
| W-150 | Research | Trust/taint labels on every observation |

---

## 10. Missing Feature Register

### 10.1 From Competitor Analysis

| Feature | Competitor | Priority | Complexity |
|---|---|---|---|
| Code indexing (tree-sitter def/ref/call graph + PageRank + binary-search token fit) | Aider | P1 | High |
| OS-level sandbox (Seatbelt/bwrap/Landlock) | Codex | P0 | High |
| Execpolicy DSL with machine-proposed amendments | Codex | P1 | Medium |
| 10-layer config with non-widenable requirements tier | Codex | P1 | Medium |
| Provider-as-data (declarative JSON providers) | Goose | P1 | Medium |
| Provider capability flags composing the loop (manages_own_context) | Goose | P1 | Medium |
| Local inference (llama.cpp + MLX) | Goose | P2 | High |
| Durable semantics (inbox/steer, compaction-as-data, stale-row settlement) | OpenCode | P1 | High |
| Permission triple + Unattended ruleset | OpenCode | P1 | Medium |
| Schedule tool (cron/interval/at + if/until gates) | OpenCode | P1 | Medium |
| Git-tree snapshots for revert | OpenCode | P2 | Medium |
| Policy engine (tiered TOML, BeforeToolSelection) | Gemini CLI | P2 | Medium |
| Hook event catalogue (BeforeToolSelection, loop detection) | Gemini CLI | P2 | Medium |
| ACP client + server | OpenHands, Hermes | P1 | High |
| Container/remote workspace | OpenHands | P2 | High |
| A2A protocol | OpenHands | P3 | High |
| Sleep-time compute / background review | Letta, Hermes | P2 | Medium |
| Monitor mode (cheap script/hash gate) | Hermes | P1 | Medium |
| Fire-claim TTL leases + missed-run policy | Hermes | P1 | Medium |
| Scheduler-owned automations decoupled from conversation | OpenClaw, Hermes | P1 | High |
| Dreaming/sleep-time consolidation | OpenClaw | P2 | Medium |
| Event triggers (on-exit, stream, hash-gate) | OpenClaw | P1 | High |
| Crash-recovery semantics (reschedule don't replay) | OpenClaw | P1 | Medium |
| Presence/observability state machine | Grok Bot | P3 | Low |
| Verification loop (edit→build→test→repair) | None (gap) | P0 | High |
| Deterministic token meter at provider boundary | None (gap) | P1 | Medium |
| Trust/taint labels on observations | None (gap) | P1 | High |
| Capability-scoped read grants (POSIX) | None (gap) | P2 | High |
| Durable mid-turn resume | None (gap) | P0 | High |

### 10.2 From Architecture Analysis

| Feature | Description | Priority | Complexity |
|---|---|---|---|
| Replaceable provider (rebuild if missing at boot) | set_model should instantiate provider | P1 | Low |
| Cancellation for spawn_blocking tools | Pass cancel handle to tool runs | P1 | Medium |
| ToolContext enrichment | Add session/agent identity, store access | P1 | Medium |
| Unified team model | Remove duplicate hive.rs, consolidate | P1 | Medium |
| Unified learning system | Merge JSON + SQLite, single source of truth | P1 | High |
| Evolution integration | Wire evolution engine into runtime/TUI/CLI | P1 | High |
| Skills loaded tracking | Actually count loaded skills | P2 | Low |
| Actual token counting | Replace len/4 heuristic with tiktoken or provider counts | P1 | Medium |
| Hive message trust | Per-participant trust levels instead of Trust::User | P2 | Low |
| Hive tool access | Give entities tools during execution | P2 | Medium |
| Learning policy persistence | Persist instruction/identity policies | P1 | Medium |
| Store user/profile linkage | create_session should set user_id/profile_id | P1 | Medium |
| Store cascade fixes | Fix delete_user/delete_profile cascades | P1 | Medium |
| Store create_project id fix | Return correct id on conflict | P1 | Low |
| Kernel trust propagation | Effect carries originating trust | P0 | Medium |
| Kernel grant self-enforcement | widen validates against ticket scope | P1 | Low |
| Kernel approval IDs | Add id to ApprovalTicket | P1 | Low |
| Kernel deferred handling | Set ctx.awaiting on Handled::Deferred | P2 | Low |
| Policy floor approval vs deny | Align docs with behavior (or change behavior) | P1 | Low |
| Policy floor Execute/Network veto | Extend floor to Execute/Network outside root | P2 | Medium |
| Policy normalize_request all actions | Handle all action types | P2 | Low |
| MCP serve implementation | Actually start MCP server | P0 | Medium |
| MCP stdio communication | Fix null pipes | P0 | Medium |
| MCP visibility persistence | Persist set_exposed_tools | P2 | Low |
| CLI doctor terminal size | Use actual terminal size | P3 | Low |
| TUI UTF-8 safety | Use char boundaries instead of byte slicing | P1 | Low |
| TUI activity cap index safety | Fix potential off-by-one | P2 | Low |
| Evolution error handling | Propagate errors instead of swallowing | P1 | Medium |
| Evolution async runner | Implement background job processing | P1 | Medium |
| Evolution path traversal guard | Validate skill names | P1 | Low |
| Evolution TTL enforcement | Filter expired entries in recall/list | P2 | Low |
| Evolution migration versioning | Add schema version tracking | P2 | Medium |
| Evolution stringly typed → enums | Convert string fields to enums | P2 | Low |
| Evolution store trait | Abstract store behind trait | P3 | Medium |
| Evolution approval framework | Implement proposal application | P1 | Medium |
| Evolution candidate validation | Validate candidates exist | P2 | Low |
| Evolution Dashboard store accessor | Add store() method | P3 | Low |
| Evolution encapsulation | Make fields private | P3 | Low |
| Evolution unused deps removal | Remove unused crate deps | P3 | Low |
| Evolution unused error variants removal | Remove unused variants | P3 | Low |
| Skills trust file error reporting | Report corrupt trust file instead of defaulting | P2 | Low |
| Skills builtin source abstraction | Create SkillSource trait | P3 | Medium |
| Skills install timeout | Add timeout to git clone | P2 | Low |
| Skills install --branch fix | Handle commit SHAs | P2 | Low |
| Skills History consistency | Reload after record | P2 | Low |
| Skills BundleIndex side effect | Remove Catalog::discover from resolve | P3 | Low |
| Skills Manager state_dir consistency | Align with global discovery | P3 | Low |
| TUI Outcome::Info usage | Use or remove | P3 | Low |
| CLI MCP serve | Implement or remove | P0 | Medium |
| CLI skills validate path | Use path's manager | P3 | Low |
| Contracts macro doc fix | Fix doc comments on macro-generated items | P3 | Low |
| Contracts error chaining | Add source error support | P3 | Low |
| Store TeamRow updated_ms | Add field to struct | P3 | Low |
| Store delete cascade | Fix FK cascades | P2 | Medium |
| Store user/profile session linkage | Add setters | P2 | Medium |

---

## 11. Security and Threat Report

### 11.1 Threat Model

| Threat | Likelihood | Impact | Current Mitigation | Gap |
|---|---|---|---|---|
| Prompt injection (direct) | Medium | High | None | No input sanitization |
| Indirect prompt injection (via files/web) | High | High | None | No taint tracking |
| Tool injection (malicious tool args) | Medium | High | Policy evaluation | Floor only vetoes Write, not Execute |
| Malicious files | Medium | Medium | Path resolution within workspace | No file type validation |
| Malicious plugins | Low | High | Plugin manifests + integrity | No sandbox, no signing |
| Unsafe shell commands | High | High | Policy rules + approval | No OS sandbox, compound command splitting |
| Secret leakage | Medium | High | Redaction in evolution | No secret scanning in runtime |
| Credential storage | Medium | High | SQLite (no encryption) | No at-rest encryption |
| Permission escalation | Low | High | Deny-first + floor | Floor bypassable via Trust::Model hardcoding |
| Cross-agent access | Medium | Medium | Entity isolation in store | Hive uses Trust::User for all |
| Workspace isolation | Medium | Medium | resolve_within | Canonicalize fallback weakens boundary |
| Network access | Medium | Medium | Policy rules | No network sandbox |
| Browser security | Low | Medium | Headless Chromium | No sandbox config |
| Database access | Low | Medium | SQLite file permissions | No encryption |
| Untrusted model outputs | High | Medium | None | No output validation |
| Supply-chain vulnerabilities | Medium | High | None | No dependency scanning |
| Data deletion | Low | High | Journal append-only | No soft delete |
| Audit logs | Low | Low | Journal records | No structured audit trail |

### 11.2 Security Gaps

1. **No OS sandbox** — policy-only enforcement, no process isolation (Codex has Seatbelt/bwrap/Landlock).
2. **No taint tracking** — observations flow into model context without trust labels (Gemini has taint model).
3. **Floor bypassable** — Trust hardcoded to Model means floor vetoes don't apply correctly to user/host actions.
4. **No secret scanning** — runtime doesn't scan for secrets in tool outputs or memory.
5. **No at-rest encryption** — SQLite stores data in plaintext.
6. **No dependency vulnerability scanning** — no cargo audit in CI.
7. **MCP serve stub** — non-functional, security surface unclear.
8. **MCP null stdio** — can't communicate, but also can't be exploited (broken).
9. **Compound command splitting** — policy splits shell commands but floor doesn't check all segments.
10. **No output validation** — tool outputs not validated before entering context.

---

## 12. Performance Report

### 12.1 Measured/Identified Bottlenecks

| Area | Issue | Evidence |
|---|---|---|
| Startup | Skills scanned on every metrics call | `runtime.rs:276` |
| Memory | len/4 token heuristic over-provisions compaction | `runtime.rs:233-242` |
| Context | 128k hardcoded budget, no actual counting | `context.rs:102` |
| Tools | 120s hardcoded timeout, no cancellation | `tools.rs:741, 1030, 1072` |
| PTY | 256k hardcoded buffer | `pty.rs:14` |
| Browser | 30s hardcoded timeout | `browser.rs:55` |
| Computer | 10s hardcoded timeout | `computer.rs:52` |
| Agents | 500 activity cap | `agents.rs:85` |
| Search | 10 result max | `search.rs:14` |
| Bus | 4096 broadcast capacity | `bus.rs:9` |
| Checkpoint | 5MB max | `workspace.rs:19` |
| Tool output | 20k max | `tools.rs:25` |
| Turn | 24 max steps default | `config.rs:257` |
| Kernel | 64 max steps | `machine.rs:95` |

### 12.2 Theoretical Optimization Opportunities

- Actual token counting (tiktoken or provider-reported)
- Lazy loading of skills/MCP servers
- Incremental indexing (code, symbols)
- Connection pooling for SQLite
- Async scheduling improvements
- Backpressure for tool execution
- Memory leak detection (no tooling currently)
- Allocation profiling

### 12.3 Low-Resource Considerations

- No GPU dependency (good)
- SQLite bundled (no system dependency)
- Rust binary (small footprint)
- But: no local model support (requires API)
- No offline mode
- No resource limits enforcement (memory, CPU)

---

## 13. Testing and Quality Report

### 13.1 Test Coverage Summary

| Crate | Unit Tests | Integration Tests | E2E Tests | Property Tests |
|---|---|---|---|---|
| vikram-contracts | 30 | 0 | 0 | 0 |
| vikram-kernel | 28 | 0 | 0 | 0 |
| vikram-policy | 18 | 0 | 0 | 0 |
| vikram-store | 11 | 0 | 0 | 0 |
| vikram-runtime | 394 | 0 | 0 | 0 |
| vikram-skills | 144 | 4 test files | 0 | 0 |
| vikram-tui | 190 | 0 | 0 | 0 |
| vikram-cli | 15 | 0 | 0 | 0 |
| vikram-mcp | 9 | 1 | 0 | 0 |
| vikram-evolution | 12 | 0 | 0 | 0 |

### 13.2 Testing Gaps

1. **No integration tests** across crate boundaries (except skills and mcp).
2. **No E2E tests** for full turn workflows.
3. **No property-based tests** (proptest/quickcheck).
4. **No fuzz testing** (cargo-fuzz).
5. **No concurrency tests** (race conditions, deadlocks).
6. **No failure-injection tests** (chaos engineering).
7. **No performance tests** (benchmarks beyond bench example).
8. **No security tests** (prompt injection, path traversal, etc.).
9. **No agent task benchmarks** (SWE-bench style).
10. **Tests validate execution, not behavior** in many cases (e.g., runtime tests check state transitions but not correctness of outputs).

### 13.3 Test Quality Issues

- Runtime tests use scripted provider for deterministic ordering (good).
- Skills tests have compatibility fixtures (good).
- Evolution tests cover lifecycle (good).
- But: no tests for error paths, edge cases, or concurrent access.
- No tests for MCP serve (because it's a stub).
- No tests for evolution integration (because it's dormant).

---

## 14. Indexing and Memory Recommendations

### 14.1 Current State

- **Memory**: SQLite KV store, per user/profile/entity isolation.
- **Context**: Markdown file loader with mtime-based invalidation, token budgeting, duplicate prevention.
- **No code indexing**: No tree-sitter, no symbol database, no embeddings, no repo-map.
- **No semantic search**: No vector DB, no BM25, no hybrid search.
- **No knowledge graph**: No entity relationships, no graph-based retrieval.

### 14.2 Recommendations

1. **Implement vikram-codeindex**: tree-sitter def/ref/call graph + impact analysis + optional embeddings, exposed as ranked context injection + queryable tool. Make it optional (absent ⇒ degrade to grep).
2. **Add hybrid search**: BM25 + vector similarity + reranking for memory and code.
3. **Implement memory consolidation**: Deduplication, contradiction detection, forgetting/expiration, versioning.
4. **Add semantic memory**: Embeddings for long-term memory retrieval.
5. **Implement incremental indexing**: Change detection, background indexing, index invalidation.
6. **Add knowledge graph**: Entity relationships for richer retrieval.
7. **Implement memory provenance**: Track source and confidence for all memory entries.
8. **Add memory permissions**: Per-entity memory access control.

---

## 15. Self-Improvement Architecture

### 15.1 Current State

- **LearningManager** (runtime): JSON-file based, proposes memory/skill/policy changes, requires approval.
- **EvolutionEngine** (vikram-evolution): SQLite-based, records experiences, reflects into proposals, auto-applies low-risk memory additions, skill evolution, optimization candidates.
- **Both are dormant/partial**: LearningManager policies (instruction/identity) write-only; EvolutionEngine not integrated into runtime.

### 15.2 Recommended Architecture

1. **Unified learning system**: Single source of truth (SQLite), merge JSON + evolution.
2. **Safe learning pipeline**:
   - Experience collection → Reflection → Proposal → Approval → Application → Verification
   - Never let unverified agent output silently modify trusted instructions or security policies.
   - Require human approval for instruction/identity/policy changes.
   - Auto-apply only low-risk, verifiable memory additions.
3. **Skill evolution**: Create/improve skills with versioning, rollback, and testing.
4. **Optimization**: GEPA-inspired candidate evaluation with Pareto frontier.
5. **Hive learning**: Record collaboration lessons, patterns, outcomes.
6. **Memory consolidation**: Deduplicate, resolve conflicts, supersede outdated entries.
7. **Feedback loop**: Analyze failures, convert experience into reusable skills.
8. **Evaluation**: Score improvements against benchmarks before applying.

---

## 16. Coding-Agent Improvement Architecture

### 16.1 Current Gaps vs Leading Coding Agents

| Capability | Vikram | Codex | Goose | OpenCode | Aider |
|---|---|---|---|---|---|
| Repository understanding | Agentic grep only | Agentic grep (shell) | Agentic via MCP | grep/glob/read tools | Tree-sitter repo-map |
| Code search | Shell grep | Shell grep in sandbox | MCP tools | grep/glob tools | Tree-sitter graph |
| Symbol navigation | None | None | None | None | Def/ref graph |
| Multi-file editing | apply_patch (none) | apply_patch tool | MCP tools | edit tools | Edit blocks |
| Patch generation | None | apply_patch | None | edit tool | Edit blocks |
| Diff management | None | turn_diff_tracker | None | file-diff | Git diff |
| Git integration | None | Full (worktree, attribution) | Extensions | git-tree snapshots | Auto-commit |
| Checkpoints | WorkspaceManager | Rollout crate | None | Git-tree snapshots | Git commits |
| Undo/recovery | None | Rollout reconstruction | None | Revert | Git revert |
| Test discovery | None | None | None | None | None |
| Test execution | Shell only | Shell in sandbox | Shell via MCP | Shell tool | Shell |
| Build validation | None | None | None | None | None |
| Linting | None | None | None | None | None |
| Formatting | None | None | None | None | None |
| Static analysis | None | None | None | None | None |
| Security scanning | None | None | OSV on extensions | None | None |
| Code review | None | None | None | None | None |
| Regression detection | None | None | None | None | None |
| Merge conflict handling | None | None | None | None | None |

### 16.2 Recommended Improvements

1. **Implement code indexing** (tree-sitter + PageRank + binary-search token fit).
2. **Add AST-based editing** (tree-sitter for precise edits).
3. **Add LSP integration** (already has LspPool, extend for navigation).
4. **Implement git integration** (status, diff, commit, branch, worktree).
5. **Add test discovery and execution** (detect test frameworks, run tests).
6. **Implement build validation** (detect build system, run builds).
7. **Add verification loop** (edit→build→test→repair with budget and failure taxonomy).
8. **Implement checkpoint/undo** (git-based or workspace snapshots).
9. **Add code review agent** (independent review of changes).
10. **Implement regression detection** (run tests before/after changes).

---

## 17. Hive and Multi-Agent Improvements

### 17.1 Current State

- **Agents**: ManagedAgent with identity, title, role, specialization, description, status.
- **Bots**: Dedicated-purpose assistants with own context files.
- **Hives**: Group chat with sequential/parallel execution, shared workspace.
- **Teams**: JSON-backed reusable group configuration (duplicate with hive.rs).

### 17.2 Gaps

1. **No agent swarms** — no large-scale parallel agent execution.
2. **No hierarchical teams** — no supervisor-worker patterns.
3. **No peer-to-peer collaboration** — no agent-to-agent messaging protocol.
4. **No blackboard systems** — no shared state for collaborative problem-solving.
5. **No voting/consensus** — no multi-agent decision making.
6. **No independent review agents** — no verification by separate agent.
7. **No agent-to-agent messaging protocol** — no typed task-bus.
8. **No deadlock detection** — no resource scheduling or deadlock prevention.
9. **No agent failure recovery** — no automatic retry or reassignment.
10. **No performance evaluation** — no agent performance tracking.
11. **Hive messages use Trust::User for all** — no per-participant trust.
12. **Hive entities get no tools** — limited capability during execution.
13. **Duplicate team model** — teams.rs vs hive.rs incompatible.

### 17.3 Recommendations

1. **Consolidate team model** — remove hive.rs, use teams.rs or hive_chat.rs exclusively.
2. **Implement agent-to-agent messaging** — typed task-bus with durable state.
3. **Add supervisor-worker pattern** — hierarchical team coordination.
4. **Implement independent review agent** — separate agent verifies changes.
5. **Add per-participant trust** — trust levels for hive messages.
6. **Give hive entities tools** — enable tool use during hive execution.
7. **Implement deadlock detection** — resource scheduling and timeout.
8. **Add agent performance tracking** — success rates, latency, cost.
9. **Implement agent failure recovery** — automatic retry, reassignment, escalation.

---

## 18. New Discoveries Not Mentioned in Prompt

### 18.1 From Codebase Inspection

1. **vikram-evolution crate exists but is completely dormant** — constructed at boot, never called.
2. **Two incompatible learning systems** — JSON (runtime) + SQLite (evolution) with parallel types.
3. **Dead hive.rs module** — entire team abstraction unused.
4. **MCP serve is a stub** — prints success without starting server.
5. **MCP runtime uses null stdio** — cannot communicate with child processes.
6. **Trust hardcoded to Model in kernel** — undermines provenance design.
7. **Grants widening not self-enforcing** — relies on caller convention.
8. **Approval tickets unaddressable** — no id, no revoke.
9. **Store sessions disconnected from users** — cascade code is dead.
10. **create_project returns stale id** — ON CONFLICT DO UPDATE bug.
11. **TeamSnapshot.id derived from name** — not persisted id.
12. **Skills loaded metric always 0** — never tracked.
13. **Context used heuristic** — len/4 not actual tokens.
14. **Learning policies write-only** — instruction/identity never persisted.
15. **Hive messages Trust::User for all** — no per-participant trust.
16. **Hive entities no tools** — limited capability.
17. **Skills merge no-op, scan_dir alias** — dead code.
18. **ContextLoader with_budget/invalidate_all unused** — dead code.
19. **HiveManager::update_state unused** — dead code.
20. **TUI UTF-8 byte slicing panic risk** — input.rs and tools.rs.
21. **TUI activity cap index off-by-one** — potential bug.
22. **CLI doctor terminal_size from env** — inaccurate.
23. **CLI chat forces Ask→Unattended** — surprising behavior.
24. **CLI skills history uses global History::load** — wrong for non-user skills.
25. **Evolution store silently drops errors** — list_*, JSON parse, count queries.
26. **Evolution memory supersession self-referential** — bug.
27. **Evolution search_memory filters after LIMIT** — misses matches.
28. **Evolution no path traversal guard** — security gap.
29. **Evolution no async runner** — despite docs.
30. **Evolution no migration versioning** — CREATE IF NOT EXISTS only.
31. **Evolution stringly typed** — outcome, status, kind as strings.
32. **Evolution no store trait** — concrete SQLite.
33. **Evolution no approval framework** — proposals not applied.
34. **Evolution no candidate validation** — promote/reject/rollback no-op silently.
35. **Evolution reject() doesn't persist reason** — data loss.
36. **Evolution rollback() doesn't restore content** — incomplete.
37. **Evolution EvaluationResult no PK** — schema issue.
38. **Evolution HiveLearning partial sanitization** — security gap.
39. **Evolution ReflectionEngine constructs own subsystems** — not shared.
40. **Evolution Dashboard no store accessor** — API gap.
41. **Evolution public mutable fields** — encapsulation issue.
42. **Evolution unused deps** — contracts, store, skills, tokio.
43. **Evolution unused error variants** — Validation, Budget, Conflict, Serde.
44. **Skills ActivationPolicy doc/code mismatch** — Suggested returns true.
45. **Skills TrustPolicy silently defaults on corrupt file** — security issue.
46. **Skills History staleness** — latest() in-memory vs record() re-reads.
47. **Skills builtin root doesn't exist** — special-cased.
48. **Skills Manager state_dir may diverge** — consistency issue.
49. **Skills install no timeout** — hang risk.
50. **Skills install --branch fails on SHA** — bug.
51. **Skills BundleIndex side effect** — Catalog::discover in library.
52. **Skills loader hardcoded script refusal** — not trust-based.
53. **MCP visibility inconsistent persistence** — set_exposed_tools vs set_visibility.
54. **MCP visibility no load in new()** — minor.
55. **MCP auth_required never enforced** — security gap.
56. **MCP approved_tools empty = all** — implicit default.
57. **MCP discovery static only** — no network.
58. **MCP edition/version not workspace-aligned** — inconsistency.
59. **Policy floor vetoes vs docs** — deny vs "human approval".
60. **Policy floor only Write** — Execute/Network not checked.
61. **Policy normalize_request only Read/Write** — incomplete.
62. **Policy Rule::actions empty = wildcard** — latent security issue.
63. **Policy ResourcePattern::as_str lossy** — footgun.
64. **Policy has_allow ignores floor** — incorrect semantics.
65. **Kernel trust hardcoded to Model** — provenance gap.
66. **Kernel grants not self-enforcing** — convention only.
67. **Kernel approval tickets no id** — unaddressable.
68. **Kernel deferred leaves awaiting unset** — state gap.
69. **Kernel push pending twice** — asymmetric paths.
70. **Kernel .expect() panic point** — reliability.
71. **Contracts journal insertion-order dependency** — fragile.
72. **Contracts NewRecord ts_ms default 0** — footgun.
73. **Contracts no error chaining** — debugging difficulty.
74. **Contracts macro doc-loss** — documentation gap.
75. **Store TeamRow missing updated_ms** — schema mismatch.
76. **Store delete cascades broken** — data integrity.
77. **Store user/profile session linkage missing** — feature gap.
78. **Store create_project stale id** — bug.
79. **TUI Outcome::Info unused** — dead code.
80. **TUI CAP=64 index off-by-one** — potential bug.
81. **TUI layout thresholds hardcoded** — configurability.
82. **TUI theme hardcoded** — configurability.
83. **TUI session_by_id asymmetry** — API inconsistency.
84. **CLI runtime() .expect()** — panic risk.
85. **CLI doctor terminal_size from env** — inaccurate.
86. **CLI chat Ask→Unattended** — surprising.
87. **CLI skills history global load** — wrong scope.
88. **CLI MCP List empty arm** — confusing control flow.
89. **CLI skills validate global manager** — inconsistent.
90. **Runtime hive.rs dead module** — dead code.
91. **Runtime assistant_with_calls dead** — dead code.
92. **Runtime mcp_visibility dead** — dead code.
93. **Runtime skills merge no-op** — dead code.
94. **Runtime skills scan_dir alias** — dead code.
95. **Runtime learning policies write-only** — dead code.
96. **Runtime HiveManager::update_state unused** — dead code.
97. **Runtime ContextLoader with_budget unused** — dead code.
98. **Runtime ContextLoader invalidate_all unused** — dead code.
99. **Runtime skills_loaded always 0** — dead metric.
100. **TUI press dead** — dead code.
101. **MCP base_dir dead** — dead code.
102. **MCP ManagedServerConfig never used** — dead code.
103. **Store session_count dead** — dead API.
104. **Store list_all_sessions dead** — dead API.
105. **Store search_sessions dead** — dead API.
106. **Store delete_file_changes_for_session dead** — dead API.
107. **Skills TrustPolicy::load_in dead-ish** — silently defaults.

### 18.2 From Competitor Research

1. **No code indexing** — all competitors use agentic grep; Aider's repo-map is dormant.
2. **ACP is the orchestration rail** — no Rust implementation exists; Vikram should build vikram-acp.
3. **No durable mid-turn resume** — all competitors punt on crash recovery at step granularity.
4. **No OS sandbox** — only Codex has real sandbox; Hermes ties isolation to terminal backend.
5. **No scheduling/proactive triggers** — OpenClaw/Hermes proved demand; event-native triggers unclaimed in Rust.
6. **No local model support** — only Goose has real local inference (llama.cpp + MLX).
7. **No verification loop** — biggest capability gap in the category.
8. **No A2A protocol** — no durable agent-to-agent bus.
9. **No desktop/phone notification surface** — OpenClaw's Rust node surface is the only real one.
10. **Replaceability only at model-loop layer** — nothing swaps the runtime itself.

---

## 19. Prioritized Improvement Roadmap

### Phase 0: Critical Fixes (P0) — Do First

1. Fix kernel trust propagation (Effect carries originating trust) — W-001
2. Implement MCP serve command — W-002
3. Fix MCP stdio communication (null pipes) — W-003
4. Fix silent error swallowing in memory store — W-004
5. Fix connect_all to propagate errors — W-005
6. Fix evolution engine silent fallback — W-006
7. Implement OS-level sandbox (Seatbelt/bwrap/Landlock) — W-125
8. Implement verification loop (edit→build→test→repair) — W-129
9. Implement durable mid-turn resume — W-124

### Phase 1: High Priority (P1) — Core Improvements

10. Consolidate team model (remove hive.rs) — W-007
11. Refactor RuntimeInner god struct — W-008
12. Unify learning systems — W-009
13. Wire evolution engine into runtime — W-010
14. Fix store silent errors — W-011, W-016, W-017, W-018, W-019
15. Fix learning silent errors — W-012
16. Fix config silent errors — W-013
17. Fix context silent errors — W-014
18. Replace poisoned mutex panics — W-015
19. Add provider rebuild path — W-023
20. Add cancellation for spawn_blocking — W-024
21. Enrich ToolContext — W-025
22. Track skills loaded — W-026
23. Implement actual token counting — W-027, W-149
24. Persist learning policies — W-028
25. Fix hive message trust — W-029
26. Give hive entities tools — W-030
27. Fix kernel grants self-enforcement — W-077
28. Add kernel approval IDs — W-078
29. Fix policy floor behavior/docs alignment — W-071
30. Implement code indexing — W-122, W-146
31. Implement ACP client + server — W-123
32. Implement scheduling/proactive triggers — W-126
33. Fix evolution error handling — W-020, W-021, W-022
34. Fix evolution async runner — W-058
35. Fix evolution path traversal guard — W-059
36. Fix evolution approval framework — W-064
37. Fix TUI UTF-8 safety — W-034
38. Fix CLI tools.rs UTF-8 — W-035
39. Implement monitor mode (cheap gate) — Hermes pattern
40. Implement fire-claim TTL leases — Hermes pattern

### Phase 2: Medium Priority (P2) — Feature Expansion

41. Implement local model support — W-127
42. Implement git integration — W-128 (partial)
43. Implement test discovery/execution — W-128
44. Implement build validation — W-128
45. Implement checkpoint/undo — W-128
46. Implement code review agent — W-128
47. Implement regression detection — W-128
48. Implement agent-to-agent messaging — W-130
49. Implement independent review agent — W-17.2
50. Add per-participant trust for hive — W-029
51. Implement deadlock detection — W-17.2
52. Add agent performance tracking — W-17.2
53. Implement agent failure recovery — W-17.2
54. Implement memory consolidation — W-14.2
55. Add semantic memory — W-14.2
56. Implement incremental indexing — W-14.2
57. Add knowledge graph — W-14.2
58. Implement memory provenance — W-14.2
59. Add memory permissions — W-14.2
60. Implement hybrid search — W-14.2
61. Fix evolution TTL enforcement — W-060
62. Fix evolution migration versioning — W-057
63. Fix evolution stringly typed → enums — W-062
64. Fix evolution store trait — W-063
65. Fix evolution candidate validation — W-061
66. Fix evolution EvaluationResult PK — W-054
67. Fix evolution HiveLearning sanitization — W-055
68. Fix evolution versioning consistency — W-050
69. Fix evolution skill content rollback — W-053
70. Fix evolution reject reason persistence — W-052
71. Fix evolution search_memory LIMIT — W-043
72. Fix evolution memory supersession — W-041
73. Fix evolution consolidate filtering — W-042
74. Fix skills trust file error reporting — W-094
75. Fix skills install timeout — W-098
76. Fix skills install --branch — W-098
77. Fix skills History consistency — W-095
78. Fix skills BundleIndex side effect — W-099
79. Fix skills Manager state_dir — W-097
80. Fix skills loader script refusal — W-100
81. Fix MCP visibility persistence — W-087
82. Fix MCP visibility load — W-088
83. Fix MCP auth_required enforcement — W-089
84. Fix MCP approved_tools default — W-090
85. Fix policy floor Execute/Network — W-072
86. Fix policy normalize_request — W-073
87. Fix policy Rule::actions wildcard — W-074
88. Fix policy ResourcePattern::as_str — W-075
89. Fix policy has_allow floor — W-076
90. Fix kernel deferred handling — W-079
91. Fix kernel push pending — W-080
92. Fix kernel .expect() — W-081
93. Fix contracts journal — W-082
94. Fix contracts NewRecord — W-083
95. Fix contracts error chaining — W-084
96. Fix contracts macro doc — W-085
97. Fix store TeamRow — W-113
98. Fix store cascades — W-046, W-047, W-048, W-049
99. Fix store user/profile linkage — W-016
100. Fix TUI activity cap — W-102
101. Fix TUI layout thresholds — W-103
102. Fix TUI theme configurability — W-104
103. Fix TUI session_by_id — W-105
104. Fix CLI runtime() .expect() — W-106
105. Fix CLI doctor terminal_size — W-107
106. Fix CLI chat Ask→Unattended — W-108
107. Fix CLI skills history scope — W-109
108. Fix CLI MCP List — W-110
109. Fix CLI skills validate — W-111
110. Fix MCP base_dir/ManagedServerConfig — W-112
111. Fix evolution unused deps — W-069
112. Fix evolution unused error variants — W-068
113. Fix evolution encapsulation — W-067
114. Fix evolution Dashboard store accessor — W-066
115. Fix evolution ReflectionEngine sharing — W-056
116. Fix evolution now_ms/gen_id — W-044
117. Fix evolution from_connection — W-045
118. Fix evolution SkillEvolution errors — W-039
119. Fix evolution OptimizationEngine — W-040
120. Fix TUI terminal restore — W-036
121. Fix MCP runtime cleanup — W-037
122. Fix skills remove_path — W-038
123. Fix evolution create_skill versioning — W-050
124. Fix evolution skill versions — W-051
125. Fix evolution HiveLearning sanitization — W-055
126. Fix evolution OptimizationEngine — W-052, W-053

### Phase 3: Low Priority (P3) — Polish

127. Fix hardcoded values (configurable) — W-117
128. Fix TUI Outcome::Info — W-101
129. Fix skills builtin source — W-096
130. Fix skills BundleIndex side effect — W-099
131. Fix skills loader — W-100
132. Fix skills ActivationPolicy — W-093
133. Fix policy as_str — W-075
134. Fix store TeamRow — W-113
135. Fix CLI skills validate — W-111
136. Fix contracts macro doc — W-085
137. Fix dead code removal (all items in section 4.4)
138. Fix documentation accuracy — W-133
139. Add property-based tests — W-136
140. Add fuzz testing — W-137
141. Add concurrency tests — W-138
142. Add failure-injection tests — W-139
143. Add performance tests — W-140
144. Add security tests — W-141
145. Add agent task benchmarks — W-142
146. Add E2E tests — W-143
147. Improve test quality — W-144

### Phase 4: Future Research (P4)

148. Formal crash-recovery state machine — W-145
149. Wasm component model — W-146
150. VRP protocol — W-147
151. TUF supply chain — W-148
152. Deterministic token meter — W-149
153. Trust/taint labels — W-150

---

## 20. Recommended Future Architecture

### 20.1 Core Principles

1. **Journal-first**: Append-only event log as single source of truth.
2. **Wasm components**: Runtime-swappable subsystems via wasmtime + WIT.
3. **VRP protocol**: Versioned runtime protocol for Level-4 replacement.
4. **Deny-first security**: Non-widenable floor, taint tracking, OS sandbox.
5. **Deterministic token metering**: Actual counts at provider boundary.
6. **Durable mid-turn resume**: Checkpoint at tool-loop boundaries.

### 20.2 Crate Structure (Future)

```
vikram-contracts (stable interfaces)
vikram-kernel (operation machine, effects, grants)
vikram-policy (deny-first, floor, rules)
vikram-store (SQLite journal + metadata)
vikram-runtime (host, providers, tools, MCP, plugins)
vikram-skills (discovery, loading, activation)
vikram-codeindex (tree-sitter + embeddings)
vikram-acp (ACP client + server)
vikram-tui (terminal interface)
vikram-cli (entry point)
vikram-mcp (MCP server)
vikram-evolution (self-improvement, integrated)
```

### 20.3 Key Architectural Changes

1. **Remove duplicate abstractions**: hive.rs, second learning system, duplicate team model.
2. **Integrate evolution**: Wire into runtime, TUI, CLI.
3. **Add code indexing**: vikram-codeindex crate.
4. **Add ACP**: vikram-acp crate.
5. **Add sandbox**: OS-level isolation per platform.
6. **Add scheduling**: Event-native triggers, fire-claim leases.
7. **Add verification loop**: First-class step with budget and failure taxonomy.
8. **Add taint tracking**: Trust labels on observations.
9. **Add durable resume**: Checkpoint at tool-loop boundaries.
10. **Add local models**: llama.cpp + MLX support.

---

## 21. Implementation Dependency Graph

```
Phase 0 (Critical):
  W-001 (kernel trust) → W-077 (grants) → W-078 (approval IDs)
  W-002 (MCP serve) → W-003 (MCP stdio)
  W-004 (memory errors) → W-005 (connect_all)
  W-006 (evolution fallback)
  W-125 (sandbox) → W-124 (durable resume)
  W-129 (verification loop)

Phase 1 (High):
  W-007 (consolidate teams) → W-008 (refactor RuntimeInner)
  W-009 (unify learning) → W-010 (wire evolution)
  W-011-W-015 (store/runtime errors) → W-016-W-019 (store data)
  W-023 (provider rebuild) → W-024 (cancellation) → W-025 (ToolContext)
  W-026 (skills loaded) → W-027 (token counting)
  W-028 (learning persist) → W-029 (hive trust) → W-030 (hive tools)
  W-034 (TUI UTF-8) → W-035 (CLI UTF-8)
  W-071 (policy floor) → W-072 (floor Execute/Network) → W-073 (normalize)
  W-122 (code indexing) → W-146 (wasm)
  W-123 (ACP) → W-126 (scheduling)
  W-020 (evolution errors) → W-058 (async runner) → W-059 (path guard)
  W-064 (approval framework) → W-061 (candidate validation)

Phase 2 (Medium):
  W-127 (local models) → W-128 (git/test/build/verify)
  W-130 (A2A) → W-17.2 (multi-agent improvements)
  W-14.2 (memory improvements)
  All remaining W-xxx fixes

Phase 3 (Low):
  All polish items

Phase 4 (Future):
  All research items
```

---

## 22. Final Engineering Verdict

### 22.1 Overall Assessment

VIKRAM is a **well-architected but incomplete** AI agent harness. Its core design (turn loop, permission gating, kernel machine, entity model) is sound and follows good Rust practices. However:

1. **Integration gaps**: The evolution crate is completely dormant. Two learning systems coexist without convergence. Duplicate team abstractions create confusion.

2. **Reliability gaps**: Silent error swallowing is pervasive. Poisoned mutex panics. No error chaining. No graceful degradation.

3. **Security gaps**: No OS sandbox. No taint tracking. Floor bypassable via Trust hardcoding. MCP serve is a stub. No secret scanning. No at-rest encryption.

4. **Capability gaps**: No code indexing. No ACP. No durable mid-turn resume. No scheduling. No local models. No verification loop. No A2A.

5. **Performance gaps**: Hardcoded limits everywhere. Heuristic token counting. No actual profiling or optimization.

6. **Testing gaps**: No integration, E2E, property, fuzz, concurrency, or security tests.

### 22.2 Strengths

1. **Clean crate boundaries**: Well-separated concerns with clear dependency graph.
2. **Deny-first security model**: Good foundation, just needs enforcement fixes.
3. **Kernel machine design**: Goose-inspired operation pipeline is excellent.
4. **Entity model**: Rich identity/title/role/specialization separation.
5. **Permission posture**: Multiple modes (allow-list/ask/unattended) is good design.
6. **Journal-based state**: Append-only with idempotency is correct approach.
7. **Context loader**: Mtime-based invalidation, token budgeting, duplicate prevention.
8. **Skills system**: Comprehensive discovery, loading, activation, trust, bundles, history.
9. **TUI**: Rich widget system, responsive layout, interactive input.
10. **Documentation**: Extensive docs covering skills, research, decisions.

### 22.3 Critical Path to World-Class

1. **Fix security enforcement** (Trust propagation, floor, sandbox) — without this, nothing else matters.
2. **Fix reliability** (error handling, no silent swallowing, graceful degradation).
3. **Integrate dormant systems** (evolution, learning, teams).
4. **Add code intelligence** (indexing, search, navigation).
5. **Add ACP** (interoperability with other agents).
6. **Add durable execution** (mid-turn resume, scheduling, triggers).
7. **Add verification loop** (edit→build→test→repair).
8. **Add local models** (llama.cpp, MLX).
9. **Add comprehensive testing** (integration, E2E, property, fuzz, security).
10. **Add observability** (metrics, tracing, profiling).

### 22.4 Final Verdict

VIKRAM has **strong architectural foundations** but is **pre-release quality** with significant gaps in integration, reliability, security, and capabilities. The path to world-class requires addressing ~150 identified issues across 5 phases, with the highest priority on security enforcement and reliability. The competitor research reveals that the biggest opportunity is in areas where **no competitor excels**: code intelligence (nobody indexes), durable mid-turn resume (nobody does it), and a unified verification loop (nobody has it). These are the areas where Vikram can differentiate rather than merely catch up.

**Confidence level**: High for codebase findings (direct source inspection), Medium for competitor claims (some from docs/research, some from web), High for architectural recommendations (grounded in evidence).

---

*End of Audit Report*