# Development Roadmap (P0–P3)

Detailed, prioritized roadmap. For the contributor-focused cross-platform roadmap see
`docs/roadmap.md`.

## P0 — Critical (security, broken core, data loss, major reliability)

1. Kernel trust propagation (Effect carries originating trust) — High.
2. MCP serve implementation (real server) — Medium.
3. MCP stdio communication (fix null pipes) — Medium.
4. Silent error swallowing in memory store — Medium.
5. connect_all propagate errors — Low.
6. Evolution engine silent fallback — Low.
7. OS-level sandbox (Seatbelt/bwrap/Landlock) — High.
8. Verification loop (edit→build→test→repair) — High.
9. Durable mid-turn resume — High.

## P1 — Essential (important missing capabilities)

10. Consolidate team model (remove hive.rs) — Medium.
11. Refactor RuntimeInner god struct — High.
12. Unify learning systems — High.
13. Wire evolution engine into runtime — High.
14. Store silent errors + data fixes (user/profile linkage, create_project id, TeamSnapshot id) — Medium.
15. Learning silent errors — Medium.
16. Config silent errors — Low.
17. Context silent errors — Low.
18. Replace poisoned mutex panics — Medium.
19. Provider rebuild path — Low.
20. Cancellation for spawn_blocking — Medium.
21. Enrich ToolContext (session/agent identity, store) — Medium.
22. Track skills loaded — Low.
23. Actual token counting — Medium.
24. Persist learning policies — Medium.
25. Hive message trust (per-participant) — Low.
26. Hive entities get tools — Medium.
27. Kernel grants self-enforcement — Low.
28. Kernel approval IDs — Low.
29. Policy floor behavior/docs alignment — Low.
30. Code indexing (tree-sitter + PageRank) — High.
31. ACP client + server — High.
32. Scheduling/proactive triggers — High.
33. Evolution error handling — Medium.
34. Evolution async runner — Medium.
35. Evolution path traversal guard — Low.
36. Evolution approval framework — Medium.
37. TUI UTF-8 safety — Low.
38. CLI tools.rs UTF-8 — Low.
39. Monitor mode (cheap gate) — Medium.
40. Fire-claim TTL leases — Medium.

## P2 — Advanced (intelligence, autonomy, collaboration, productivity)

41. Local model support (llama.cpp, MLX) — High.
42. Git integration — Medium.
43. Test discovery/execution — Medium.
44. Build validation — Medium.
45. Checkpoint/undo — Medium.
46. Code review agent — Medium.
47. Regression detection — Medium.
48. Agent-to-agent messaging — High.
49. Independent review agent — Medium.
50. Per-participant trust for hive — Low.
51. Deadlock detection — Medium.
52. Agent performance tracking — Medium.
53. Agent failure recovery — Medium.
54. Memory consolidation — Medium.
55. Semantic memory — Medium.
56. Incremental indexing — Medium.
57. Knowledge graph — Medium.
58. Memory provenance — Low.
59. Memory permissions — Low.
60. Hybrid search — Medium.
61-73. Evolution fixes (TTL, migration versioning, enums, store trait, candidate validation,
    EvaluationResult PK, HiveLearning sanitization, versioning, skill content rollback,
    reject reason, search LIMIT, memory supersession, consolidate filtering) — Low/Medium.
74-82. Skills fixes (trust file errors, install timeout/--branch, History consistency,
    BundleIndex side effect, Manager state_dir, loader script refusal) — Low.
83-85. MCP fixes (visibility persistence/load, auth_required) — Low.
86-92. Policy fixes (floor Execute/Network, normalize_request, Rule::actions wildcard,
    ResourcePattern::as_str, has_allow floor) — Low/Medium.
93-97. Kernel fixes (deferred handling, push pending, .expect(), journal, NewRecord) — Low.
98-100. Contracts fixes (error chaining, macro doc) — Low.
101-107. Store fixes (TeamRow, cascades, user/profile linkage) — Medium.
108-112. TUI fixes (activity cap, layout thresholds, theme, session_by_id, Outcome::Info) — Low.
113-117. CLI fixes (runtime .expect(), doctor terminal_size, chat Ask→Unattended,
    skills history scope, MCP List, skills validate) — Low.
118-126. Evolution cleanup (unused deps/variants, encapsulation, Dashboard accessor,
    ReflectionEngine sharing, now_ms/gen_id, from_connection, SkillEvolution errors,
    OptimizationEngine, create_skill versioning, skill versions, HiveLearning, rollback) — Low/Medium.
127-132. TUI/MCP/skills polish (terminal restore, MCP cleanup, remove_path, etc.) — Low.

## P3 — Future innovations

133-147. Hardcoded values configurability, dead code removal, documentation accuracy,
    property-based/fuzz/concurrency/failure-injection/performance/security/E2E tests,
    test quality improvements, builtin source abstraction, BundleIndex side effect,
    loader, ActivationPolicy, policy as_str, store TeamRow, CLI skills validate,
    contracts macro doc.

## P4 — Research

148-153. Formal crash-recovery state machine, Wasm component model, VRP protocol,
    TUF supply chain, deterministic token meter, trust/taint labels.

## Quick wins (low complexity, high value)

- Fix kernel trust propagation.
- Fix MCP serve stub + stdio.
- Fix silent error swallowing (memory, store, config, context, learning).
- Enrich ToolContext.
- Track skills loaded.
- Persist learning policies.
- Fix store cascades + user/profile linkage.
- Fix TUI UTF-8 safety.
- Fix CLI tools.rs UTF-8.
- Cross-platform P0 items (done: PTY shell, skills home, LSP URIs, which(), plugins,
  computer status).

## Major architectural projects (high complexity)

- OS-level sandbox.
- Verification loop.
- Durable mid-turn resume.
- Code indexing.
- ACP client+server.
- Scheduling/triggers.
- Local model support.
- Agent-to-agent messaging / A2A.
- Memory consolidation + semantic memory + knowledge graph.
- Unify learning systems + integrate evolution.

## Dependencies

P0 items unblock P1; P1 unblocks P2. See docs/audit/VIKRAM-AZ-AUDIT.md §21 for the
full dependency graph.