# Modularity & upgrades: the runtime technology that makes replacement legal and safe (spec Parts 2C, 6, 12, 18, 25)

Research date: 2026-10-31. Method: crates.io API queries for every crate cited (version / last
release / downloads / license all read on 2026-10-31), GitHub API checks, primary-doc fetches,
and targeted source reads. Confidence tags: [H] primary source read directly, [M] secondary /
aggregate / snippet, [L] single low-quality source or inference. Assumptions are marked **[ASSUMPTION]**.

Verdict up front: **extreme modularity is achievable, but not the way the spec implies.**
Levels 1–2 are compile-time + scripting + process mechanics. Level 3 is a protocol boundary.
Level 4 is *only* definable as "another binary speaking VIKRAM's stable wire protocol".
**In-process hot replacement of Rust code is not safely implementable on any platform in 2026** —
evidence in §E. The spec must reframe "replace internals" as "swap the implementation behind a
boundary", where the boundary is either (a) a rebuild-time seam, (b) a wasm component,
(c) a script, or (d) a separate process speaking a versioned protocol.

---

## A. Extension / loading mechanisms — real constraints

### A.1 Native dynamic libraries (`libloading` / `dlopen`) — NOT a viable foundation

`libloading` 0.9.0 (2025-11-05, ISC, 606M all-time downloads) is healthy, but it only loads
whatever you hand it; the problem is what can safely be loaded. [H] crates.io 2026-10-31.

Why Rust↔Rust dylibs are unsafe across builds [H unless noted]:
- **No specified Rust ABI.** Name mangling, `repr(Rust)` layout, vtable layout, enum niche
  choice, generic instantiation and `std` internals are explicitly unspecified
  (https://doc.rust-lang.org/reference/type-layout.html; Rustonomicon). A layout change inside
  one type in a rustc point release silently corrupts data across the boundary — no error, UB.
- **std duplication.** By default each dylib carries its own static copy of std; two copies in
  one process break thread-locals, `std::env`, panic payloads and allocator state unless the
  whole tree is built with `-C prefer-dynamic`, which the ecosystem treats as best-effort.
- **`dlclose` has no defined safety requirements at all.** UCG tracker issue #526 ("requirements
  for unloading a library", opened 2024-08, still open) has no answer; TLS destructors, lazy
  statics and exit ordering are documented hazards (users.rust-lang.org/t/116623; glibc dlclose
  refcount behavior); Windows `FreeLibrary` of Rust cdylibs has crashed in `DllMain` teardown.
  [H for issue existence, M for details]
- **Production precedent goes the other way.** Bevy deprecated its own dynamic-plugin loader
  (bevy#11969, 2024-02): "It works by loading and executing foreign code, which is drastically
  unsafe. Plugins must be built with the exact same Bevy and Rust versions." The
  `bevy_dynamic_plugin` crate doc itself says "dynamic linking and loading is inherently
  unsafe… incompatible libraries across Rust [versions]". [H]
- `hot-lib-reloader` 0.8.2 (2025-08-11, 62k downloads) is the whole market for dev hot-reload:
  it works by *never unloading* (pins each old copy, leaks them), requires identical toolchain,
  and is a dev-toy, not a shipping mechanism. [H] crates.io.

**Production Rust that IS dynamically loaded succeeds only because the boundary is pure C**
(and that is the pattern to imitate, not libloading):
- **pgrx** 0.19.3 (2026-09-27, MIT, 2.3M dl) — Rust Postgres `.so`s dlopened by PG; every exported
  symbol is `extern "C"`; pins one rustc at build time; Postgres never dlcloses. [H] crates.io + repo.
- **GStreamer** Rust plugins (gstreamer 0.25.4, 2026-09-21, MIT/Apache, 12.5M dl) — loaded via
  GObject C ABI by gst's scanner; shipping in Fedora/Steam Deck stacks. [H] crates.io, [M] adoption.
- **Redis Modules** in Rust (redis-module 2.0.8, 2026-05-05, BSD-3) — dlopened by Redis. [H] crates.io.
- Agent products use none of this: Codex's monorepo (openai/codex `codex-rs/`, listing read
  2026-10-31) is *crate workspace + app-server protocol* — no `libloading` visible at top level. [H]
- Verdict: **supports Level 1–2 only as "pinned-toolchain, load-at-startup, never-unload"
  engineering, with a C-ABI membrane; do not build VIKRAM's headline feature on it.** Windows
  adds Authenticode/unload quirks; Termux adds nothing fatal (§E.3). Security: zero — a dylib
  runs with full host privileges, so integrity/signing (§D.4) becomes mandatory; sandbox = nil.

### A.2 Stable-ABI crates (abi_stable / safe_abi / stabby / vtable) — weak and mostly abandoned

- **abi_stable** 0.11.3: last release **2023-10-12**; repo `rodrimati1992/abi_stable_crates`
  last pushed the same day (607★). Three years stale as of access date, 4.2M downloads, MIT/Apache.
  Its own docs: Rust-to-Rust FFI, load-time layout checking, `DynTrait` ffi-safe trait objects,
  explicitly "without support for unloading". No async. Requires every shared type to implement
  `StableAbi` (heavy macro ceremony on both sides). [H] crates.io + docs.rs/abi_stable.
- **safe_abi** (Mara Bos & Jonathan Dönszelman's research project, per the Nov-2024 "Status of
  the Rust ABI…" thread; cf. RustConf/Fest talk "The Path to a Stable ABI for Rust"): **no
  crates.io release under any spelling as of 2026-10-31** (`safe_abi`, `safe-abi`,
  `safely-abandon`, `stable-abi` → API NONE) [H]. Treat as unreleased; the language-level stable
  ABI is still a research goal, not a dependency.
- **stabby** (ZettaScaleLabs, 459★, pushed 2026-09-27) — active but small; 0.x. **vtable** 0.5.0
  (2026-08-18, 2.1M dl) and **cglue** (191★, 2025-04) are hand-written C-ABI vtable codegen —
  functional but niche. `com` (Microsoft's Windows COM crate) 0.6.0 last released 2021-10-25 —
  superseded by windows-rs COM but shows how niche this is. [H] crates.io + GitHub API.
- Decision-grade: **do not bet VIKRAM on any Rust-stable-ABI crate.** If you still want native
  plugin `.so/.dll`s, define your own `extern "C"` vtable membrane (`#[repr(C)]` function-table
  structs + `#[no_mangle] extern "C" fn _vikram_interface(version: u32) -> *const Vtable`),
  plus `trait-variant` 0.1.3 (2026-07-22, 16M dl) for dyn-compatible async traits and a load-time
  `(rustc_host_target_triple, interface_hash, semver-range)` handshake. This is what pgrx/GStreamer
  effectively do. Supports **Level 2 with pinned toolchain**; Levels 3–4 not reachable in-process.

### A.3 Compile-time registries (inventory / linkme / ctor) — the Level 1–2 workhorse

- **inventory** 0.3.25 (2026-10-07, MIT/Apache, 152M dl): typed distributed plugin registry;
  survives static-library dedup (dtolnay's README explicitly designs for "any source file linked
  into your application… no central list"). **linkme** 0.3.37 (2026-07-18, 38M dl): linker-section
  distributed slices, tested matrix Linux/macOS/**Windows**/FreeBSD/OpenBSD/illumos. **ctor**
  1.0.13 (2026-08-09, 232M dl): `.init_array` constructors. All healthy, all dtolnay-adjacent. [H]
- What they give VIKRAM: an **in-tree component registry** — each crate registers `fn descriptor()
  -> ComponentDescriptor { id, semver, capabilities, ctor }` at compile time; the host resolves
  `dyn MemorySubsystem` etc. by id at runtime; users swap implementations by **changing which
  crate is linked** (workspace feature flags) — no registry edits, no merge-conflict central list.
- Hard limits: registrations are linker-time; **no runtime addition, no removal, no reload.**
  Registration inside a dlopened crate does run (ctor at load), which is why some projects mix
  A.1+A.3 — but that inherits every A.1 hazard. Security: whatever you link has host privileges.
  Performance: free. DX: excellent. Cross-platform: all three list Windows support explicitly. [H]
- Supports **Level 1 (implementation swap via rebuild) and Level 2 (component swap via rebuild)**;
  this is the honest meaning of "replaceable without forking the repo".

### A.4 WebAssembly: wasmtime, component model + WIT, Extism, Spin, wasmi — the best Level 2/3 runtime-swap story

- **wasmtime** 48.0.8 (2026-10-29; GitHub tag v50.0.0 2026-10-20; Apache-2.0 WITH LLVM-exception;
  42.5M dl). **wasmi** 2.0.0 (2026-09-01, MIT/Apache, interpreter-only, no JIT — use for
  interpret-on-device fallback). **wasmer** 7.5.0 (2026-10-01, MIT). All active. [H] crates.io.
- Platform reach (host side): Wasmtime's doc: "OS support … primarily includes Windows, macOS, and
  Linux. Other OSes such as iOS, **Android**, and Illumos are supported but less well tested"; ARM64
  Windows (unwinding + release binaries) landed v26 (2024-10). Desktop six triples first-class; Termux second tier. [H]
- **Performance reality**: warm instantiation of a precompiled module is tiny (SpiderMonkey-scale
  init went ~2 ms → **5 µs**, "Wasmtime 1.0: A Look at Performance", 2022-09-06 [H]); cold
  *compile* of a typical plugin is **15–30 ms** unless AOT-precompiled to `.cwasm` (hostmycode
  2026 analysis, 2026-04-20 [M]). Steady-state compute: hot code ≈85–95% of native [M]; the
  classic USENIX ATC'19 "Not So Fast" band (mean 1.45–1.55×, worst 2.5×) still frames worst cases.
  Irrelevant for I/O-bound agent tools; real if someone replaces the *planner* with compute-heavy wasm.
- **Host-call cost** is the true tax: every crossing copies data. Extism's engineering post
  (dylibso "How does Extism work?") documents **4 memory copies per host round-trip**, ~284 MiB/s
  on 6.25 MiB payloads (≈22 ms per large call) — fine for JSON tool calls, wrong for bulk blobs. [H]
- **WIT / component model**: `wit-bindgen` 0.63.0 (2026-10-14, 319M dl, Apache/MIT) healthy; the
  guest-authoring tool **cargo-component** 0.21.1 last released **2025-03-18** (19 months, one
  maintainer [H]); community threads report CM adoption still thin outside Wasmtime/Spin/WasmEdge
  (r/rust 2025-09 "Is the Component Model dead?" [M]). WIT gives VIKRAM exactly what it needs:
  typed versioned interfaces + capability-passing by import — a plugin gets only the host
  functions you wire in and has **no process memory, no syscalls, no ambient authority**;
  linear-memory isolation with explicit per-instance limits is enforced by the runtime.
  Module size is workable: Rust `wasm32-wasip2` hello-world ≈ **86 KB**, JS-via-Javy ≈ 980 KB,
  realistic serde+reqwest plugin ≈ 2–5 MB pre-`wasm-opt` (riotsecure 2025-10-06 [M]).
- **Extism** 1.41.0 (2026-10-09, BSD-3, 815k dl; wasmtime-powered, ~16-language SDKs):
  batteries-included plugin ABI *but a fixed, coarse one* (kv/http/config, `call`-shaped) — good
  Level 2 tools, constraining for streaming Level 3 subsystems. [H]
- **Spin** (spin-sdk 7.0.0, 2026-08-25): a wasm *application server*, not an embeddable plugin
  loader — irrelevant here. A 2026 post claims Fermyon→Akamai + "Spin 4.0.1" [L — unverified].
- Verdict: **wasm is the only safe *runtime-swappable* native-code story VIKRAM should ship.**
  Supports Level 2 fully (load/replace components at runtime, crash-isolated, capability-scoped)
  and *small* Level 3 (e.g. swap "scorer"/"ranker"/"parser" subsystems) if you define WIT for them.
  A runtime crash can't take down the host (trap = error); memory is bounded per instance.

### A.5 External-process plugins + JSON-RPC: MCP's model, tarpc, tonic, zbus — Level 3/4 mechanism

Mechanism (OS process, NDJSON JSON-RPC over stdio/socket) — cost is per-message serialization,
not transport (~µs locally; [ASSUMPTION], unbenchmarked here). Isolation = OS-level: full crash
isolation, but the process has host access unless wrapped (seccomp/bubblewrap on Linux, Job
objects + restricted tokens on Windows, nothing durable on Termux beyond uid isolation).
- Library state (all [H] crates.io 2026-10-31): `jsonrpsee` 0.26.1 (2026-09-30, MIT, 27M dl,
  maintained by Parity — the default Rust JSON-RPC stack); `tarpc` 0.38.0 (2026-08-12, MIT,
  10.4M dl — type-checked RPC over transport-agnostic channels; used at Google internally per
  README claim [M]); `tonic` 0.14.9 (2026-10-29, MIT, 451M dl — gRPC, protobuf, streaming both
  directions; production proof: Linkerd2-proxy is Rust+gRPC); `zbus` 6.2.0 (2026-10-29, MIT,
  105M dl — D-Bus: **Linux-desktop-only**, correct choice for OS-integration (notifications,
  secrets service) but not a plugin ABI).
- MCP (Anthropic's protocol) is the empirical existence proof for process plugins *in this exact
  product category* — and also the cautionary tale (§C.1): its stdio transport has become a
  named vulnerability class ("MCP STDIO command injection", OX Security, 2026-04-15, 10 CVEs
  across clients; LiteLLM CVE-2026-30623; Flowise CVE-2026-40933; Cloud Security Alliance
  research note 2026-04-23 calls it a *design* flaw: config strings become shell commands with
  the host user's full rights). [H] for advisory existence, [M] for impact numbers.
  **VIKRAM rule: plugin manifests must be argv arrays, never shell strings; child processes
  spawned with `CreateProcess`/`execv` semantics only.**
- Verdict: supports **Level 3 and Level 4** (subsystem = own daemon; runtime = peer binary),
  best DX for cross-language plugins, worst cold-start latency (~ms, fine), and it is the only
  mechanism that works identically on every platform incl. Termux. Security depends entirely on
  your OS-sandbox wrapper and manifest validation, not on the transport.

### A.6 Scripting embedding — Level 1 behavioral customization

- **rhai** 1.27.0 (2026-10-21, MIT/Apache, 13.8M dl): pure-Rust sandboxed interpreter;
  no ambient I/O unless you register functions; built-in `max_operations` / eval-time-limit /
  memory caps; can't be dlclosed because it isn't native. Ideal for permission rules, retry
  policies, tool post-processing hooks. [H] crates.io + docs.
- **Boa** 0.22.0 (2026-08-28, Unlicense/MIT, 6M dl): JS-in-Rust; ECMAScript coverage solid,
  perf modest; good if you want JS syntax familiarity without npm access. [H]
- **deno_core** 0.413.0 (2026-10-21, MIT, 9.6M dl): embeds V8 via rusty_v8 — real JS/TS with
  an ecosystem; cost: +~10–15 MB binary and 100+ MB RSS floor per isolate; prebuilt V8 statics
  are published for mainstream targets — **Android/Termux prebuilt availability unverified
  [ASSUMPTION]**; if missing, building V8 on-device is painful — so treat deno_core as
  desktop-only optional feature. [H] crates.io; [M] footprint claim (commonly measured).
- **mlua** 0.12.2 (2026-10-03, MIT) / rquickjs 0.14.0: C-linked Lua/QuickJS, cheapest mature
  options but FFI surface; rhai covers the use case without either.
- Verdict: supports **Level 1** (behavior of an implementation) and micro-policies; runtime-safe
  reload trivially (re-parse); no ABI issues; scripts are data → sign them like components.

---

## B. Durable execution & state

### B.1 Engine survey (decision-grade)

| Engine | Rust story (verified 2026-10-31) | Model | Fit for a local-first single-binary runtime |
|---|---|---|---|
| **Temporal** | `temporalio-sdk` **1.1.1 (2026-10-30), GA 2026-09-23** (temporal.io blog), 3.6M dl; crates.io license field "non-standard" but repo LICENSE is MIT — **verify license text before vendoring** [H/M] | journal-per-workflow (event history), deterministic replay; retries w/ backoff; timers, signals; "activities should be idempotent" is official guidance | Wrong shape for the laptop: needs a Temporal server (dev server bundled CLI; prod needs Cassandra/MySQL/PG). Right shape for **optional distributed deployment**: VIKRAM planning worker as a Temporal worker |
| **Restate** | `restate-sdk` 0.13.0 (2026-10-28, MIT, 1.3M dl) — 0.x but monthly cadence [H] | journal + replay, **exactly-once side effects via idempotency metadata + virtual objects (key-scoped state, k-of-n single-writer)**; single-binary server, RocksDB log-state | Lighter than Temporal but still an external daemon; the *ideas* (internal queue dedup, key-scoped single-writer sessions) should be copied in-process |
| **Bifrost** | crate `bifrost` **0.4.0-alpha.2, last release 2019-08-09, 13,960 dl** — dead [H] | serverless-workflow spec in Rust | avoid |
| **LangGraph checkpointers** | Python/JS, not Rust deps; `BaseCheckpointSaver` (InMemory/SQLite/Postgres), writes checkpoint **before each super-step**; `pending_writes` keeps parallel branches consistent [H] docs.langchain.com | **state snapshots**, not replay | the pattern (per-step snapshot + thread_id/checkpoint_id + fork/resume) is the right model for VIKRAM's session store; see critique below |
| DIY journal + replay | — | append-only event log per turn; reducer reconstructs; snapshot every N events | **the actual recommendation** |

Critical secondary source: "Why Checkpoints Aren't Durable Execution" (Y. Schneider, Restate
founder, 2026-02-25) enumerates what snapshot-only runtimes (LangGraph, CrewAI, Google ADK)
lack: no automatic step retry semantics, no exactly-once guarantees for tools with side effects,
in-flight crashes leaving half-applied state. [M] Treat it as biased-but-technically-correct.

### B.2 Event sourcing vs state snapshotting for VIKRAM

Hybrid, journal-first. Facts from the field: MCP 2026-07-28 *removed* SSE resumability
("a broken response stream loses the in-flight request; clients MUST re-issue it as a new
request with a new request ID") — i.e., at-least-once with re-issue is what the largest live
protocol settled on after a year of incidents. [H] spec changelog. Temporal/Restate both prove
that **an append-only journal + deterministic-ish replay is the recovery primitive**, while
LangGraph proves snapshots make *inspection/forking* cheap.
Design for VIKRAM: (1) every externally-visible effect gets a **stable effect-id**;
intent-recorded before dispatch (write-ahead), result recorded after → duplicate-action
prevention after crash = "never re-dispatch an effect-id already in the journal; poll for the
result instead". (2) monotonic `seq` per session for client sync (§C.4). (3) periodic derived
snapshots (regenerable; journal is truth) so resume is O(1). (4) Retry semantics: per-step
retry-policy table (Transient→exp-backoff+jitter; Fatal→no-retry; Approval→suspend like MCP's
MRTR). Cite Temporal best-practices doc for timeouts/heartbeats/idempotency patterns [H].

### B.3 Persistence substrate (verified versions)

- **rusqlite 0.41.1 (2026-10-28, MIT, 140M dl), bundled libsqlite3-sys** → **primary store**.
  WAL mode caveats are stable and documented (sqlite.org/wal.html): WAL requires all readers on
  the *same host* (shared memory not designed for network filesystems); `-wal`/`-shm` must be
  on the same filesystem as the db. Checkpoint + `VACUUM INTO` / Backup API = consistent snapshot
  for the pre-upgrade recovery point. **sqlite-vec** 0.1.10-alpha.4 (2026-05-18, MIT/Apache,
  3.7M dl) covers the embedding/vector need inside SQLite. [H] crates.io.
- **redb 4.4.0 (2026-10-13, MIT/Apache, 14.8M dl)**: pure-Rust ACID embedded KV — the only
  credible non-SQL option; keep as *alternative* for no-SQL builds, not the journal store.
- **sled 1.0.0-alpha.124 (last release 2024-10-11)** — permanently alpha, author-maintained
  migration to redb; community consensus "half-maintained" (r/rust embedded-KV threads). **Ban.** [H] crates.io, [M] consensus.
- **rocksdb 0.25.0 (2026-08-16, wrapper Apache-2.0; underlying RocksDB is dual Apache-2.0/GPL-2.0
  [M — verify LICENSE file at adoption])**: LSM power at the cost of a huge C++ build, cross-distro
  headaches on Windows ARM / Termux; unjustifiable for single-user agent state. Ban for local;
  fine as an optional server-side dep (Restate itself uses it).
- **libsql 0.10.0 (2026-10-13, MIT)** / sqld lineage: libSQL = SQLite fork + `sqld`; Turso is now
  "a separate Rust rewrite" of the platform (libSQL-vs-Turso explainer 2026-09-13 [M]). Adoption
  buys embedded replicas/sync you don't need yet; couples you to a company in flux (Flavio Copes
  2026-08-03; Turso site). Keep the *option* of sync later; plain SQLite now.
- Windows/Termux note: SQLite works everywhere (Termux ships it natively); WAL needs same-host fs.

---

## C. Protocols

### C.1 MCP — current spec and its public wounds (read this before choosing it as your internal ABI)

- Revision history: 2024-11-05 → 2025-03-26 → 2025-06-18 → 2025-11-25 → **2026-07-28 (current
  stable, GitHub release tag verified)**. [H] 2025-11-25 added OIDC discovery, tool icons,
  incremental scopes, elicitation URL mode, experimental tasks (JSON Schema 2020-12). [H]
- **2026-07-28 is a wholesale re-shape** [H, changelog.mdx read verbatim 2026-10-31]:
  (1) protocol-level **sessions and `Mcp-Session-Id` removed**; cross-call state via server-minted
  handles in tool args; (2) **initialize handshake removed — "Make MCP stateless"**: version +
  capabilities ride in `_meta` per request; (3) new **`server/discover`** RPC; (4) GET endpoint +
  subscribe → **`subscriptions/listen`** single stream; (5) `ping`/`logging/setLevel` removed;
  (6) **tasks moved to extension `io.modelcontextprotocol/tasks`** (poll `tasks/get`/`tasks/update`);
  (7) **MRTR pattern** replaces server→client requests: `resultType:"input_required"` + client
  retry with `inputResponses`; (8) **SSE resumability / Last-Event-ID removed** — re-issue as new
  request. Deprecated: Roots/Sampling/Logging, DCR (→ Client ID Metadata Documents); `iss`
  validation (RFC 9207); `ttlMs`/`cacheScope` cache fields.
  Lesson: even a year-old protocol can flip its state model entirely — your *internal* protocol
  must not be whatever MCP looks like this quarter.
- Security record (all public): tool poisoning + cross-server MCP worm (Invariant Labs 2025-08;
  arXiv 2603.22489 threat-model, cited 40×); **2026-04-15 OX Security: systemic STDIO command-injection
  class, ~10 CVEs across clients** (LiteLLM CVE-2026-30623 RCE; Flowise CVE-2026-40933 one-click RCE
  2026-05; CSA note 2026-04-23 calls it a *design* flaw — shell strings with the user's full rights;
  ARMO counterpoint 2026-04-21); valyu living CVE tracker 2026-10-23; a marketing aggregate claims
  72.4% cross-server cascade rate [L]. [H] advisory existence; [M] figures.
- Rust SDK: **rmcp 3.8.0 (2026-10-26, Apache-2.0, 42.8M dl, official repo pushed 2026-10-31)** —
  healthy. Use VIKRAM as MCP *client* (consume the ecosystem) and optionally as MCP *server*
  (expose sessions); do NOT let MCP be the internal component boundary.
- Transport options today: stdio (local, no sandbox unless you add one) / Streamable HTTP
  (remote; stateless now — approvals via MRTR re-issue) / legacy HTTP+SSE deprecated.

### C.2 ACP (Zed's Agent Client Protocol) — what it actually is

agentclientprotocol.com + `agentclientprotocol/agent-client-protocol` repo (Apache-2.0, pushed
2026-10-31; **Rust crate `agent-client-protocol` v3.6.0, 2026-10-28, 5.7M dl — very active**).
JSON-RPC 2.0, **primarily stdio** between an editor-like *client* and an agent *server*, both
sides can make requests. Verified method surface from CHANGELOG/spec docs [H]: `initialize`
(may repeat before other requests), `session/new`, `session/load` (replays history via
`session/update`), `session/resume` (cwd must match), `session/list` (cursor-paginated),
`session/close`, `session/prompt` (request; streams `session/update` notifications — agent
thoughts, `tool_call`/`tool_call_update`, plan, usage — until the turn completes),
`session/set_mode`, **`$/cancel_request`** (with rules: unknown/answered requests ignored; a
normal response after cancel is allowed), **`session/request_permission`** (agent→client;
decline semantics defined), client-provided **`fs/read_text_file` / `fs/write_text_file`** and
**`terminal/*`** methods (the *client* owns the workspace and terminal — the agent has no
direct FS!), auth methods, elicitation, `_meta` conventions, icons, "MCP-over-ACP" experimental
message types (2026-05). **v2 is drafted (2026-07 onward; changelog full of `unstable-v2`)**:
removes JSON-RPC batching, richer session states, version-mismatch error codes.
Key limitations to note in your own design: ACP models **one client per session** (editor↔agent);
it has *no* multi-client lock/lease concept and reconnect = `session/load`/`resume` history
replay, not differential sync. Adopters incl. Zed, JetBrains, Google, GitHub, ~25+ agents
[Morph explainer, 2026 [M]].

### C.3 A2A and the "SDK-ACP"/agent-to-agent layer

A2A (Google→**Linux Foundation**, 2025-06): **v1.0 April 2026** (LF press 2026-04-09: 150+
organizations, enterprise production use; spec guide 2026-09-14). v1.0 data model normative in
**protobuf, with three bindings: JSON-RPC 2.0 / gRPC / HTTP+JSON**; SSE streaming + push
notifications; Agent Cards for capability discovery; task lifecycle with artifacts. Rust: only
thin crates exist (`a2a 0.1.0`, 2025-04, 1,000 dl — effectively nothing [H]); security paper
"A2ABreak" (2026-09-09) reports design-level weaknesses [M]. Relevance to VIKRAM: the *shape*
(long-lived task objects, polling+stream dual binding, card-based discovery) is the right model
for Level-4 runtime peers; the Rust ecosystem for it is not there — you'd hand-roll against
`jsonrpsee` + `utoipa`-style schema.

### C.4 What VIKRAM's client-facing runtime protocol MUST specify (synthesis)

Frontends today (Zed clients via ACP, Codex `app-server` + separate `app-server-protocol` and
`app-server-transport` crates [H dir listing], Claude Agent SDK's stream-JSON stdio) all validate
these requirements; none fully solve #4, which is VIKRAM's differentiator:
1. **Framing & version**: NDJSON-over-stdio for spawn mode; unix socket/named pipe (Windows
   `\\.\pipe\`) for daemon mode; `_meta`-per-request version negotiation (MCP 2026 lesson:
   handshake-free stateless beats session-id) + capability advertisement (`server/discover` analog).
2. **Streaming turns**: request/response for control, per-session **append-only update stream with
   monotonic `seq`** (journal-visible — your durable log and your wire format should be the same
   thing, ACP `session/update` + LangGraph thread-stream precedent).
3. **Reconnect**: client resends last-known `seq`; server replays from journal (gap) or instructs
   full reload if pruned. Do what MCP *couldn't* (they deleted Last-Event-ID because they own no
   journal; VIKRAM does own one — keep resumability in the protocol).
4. **Cancellation**: ACP-style `$/cancel_request{request_id}` + server must settle every in-flight
   effect (journal marks it `abandoned-after-<seq>`) so replay never re-fires.
5. **Approvals**: MRTR shape: turn pauses with a persisted `approval_required(seq)` event; any
   attached client may respond within its lease; late duplicate responses are *idempotent no-ops*
   (first writer wins via journal CAS) — this is exactly where a duplicate-action-free journal
   (§B.2) pays for itself.
6. **Multi-client sync (two frontends, one runtime)**: neither ACP nor MCP has this. Minimum
   contract: (a) fan-out of journal diffs to all clients with per-client cursors; (b) a
   **prompt-lease** (exclusive turn-authoring right, timeout + explicit handoff RPC) — Restate's
   *virtual objects* (one writer per key) is the proven in-process analog; (c) per-client
   identity/roles in `_meta`; (d) presence events (join/leave) as journal entries so reconnecting
   clients see the roster; (e) conflict rule: the runtime is the sole state authority — clients
   are views, never caches of authority.
7. **Security**: stdio spawn via argv arrays only; socket auth via peer-uid (`SO_PEERCRED`) or
   single-use bearer written to a restricted fd; sign every approval decision with the session's
   nonce so a rogue client can't forge consent (MCP-adjacent incidents make this mandatory).

---

## D. Upgrade architecture

### D.1 Semver + compatibility validation

`semver` 1.0.28 (2026-04-04, MIT/Apache, 1.14B dl) [H]. `cargo-semver-checks` 0.51.0 (2026-10-03,
Apache/MIT) lints Rust API breakage in CI — use it *per interface crate*, because your
replacement economy only works if `vikram-api` breaks rarely. Runtime side: every loadable
artifact (wasm component, plugin binary, script bundle, remote runtime) carries a **manifest**:
`{interface: "vikram.memory/v3", semver-range: ">=3.2 <4", target-triples: [...], sha256,
signature, min_host, capabilities}`; host resolves before first byte of execution. Breaking-change
detection for *your published API* = cargo-semver-checks + WIT `@since` annotations + a CI fixture
that every released artifact is load-tested against N and N-1 hosts.

### D.2 Schema migrations

Compared (all [H] crates.io 2026-10-31): **sqlx 0.9.0** (2026-05-21, 178M dl — `migrate!` macro,
`_sqlx_migrations` table w/ checksums + applied-at + version; embeds .sql at compile) vs **refinery
0.10.0** (2026-10-02, 12.3M dl — smaller, SQL-file, transactional per-migration, checksum-verified,
built for rusqlite) vs diesel_migrations 2.3.2 (2026-04-24). Recommendation: **refinery** (or
sqlx::migrate if sqlx is already the async pool) + rule "**forward-only + never-edit-applied**;
checksum mismatch on an applied file = hard fail"; regenerable data (embeddings, indexes) =
**drop-and-rebuild on downgrade** via a `schema_feature` table. Compatibility is easiest when data
changes are additive: journal events are append-only schemas, new event types must be ignorable by
older readers (`unknown ⇒ skip+preserve`).

### D.3 Config migration

No credible Rust framework exists (searched; figment/confique don't version-migrate). Pattern to
steal: a `config_version: u32` field; a registered chain of `fn migrate_vN_to_Np1(serde_json::Value)
-> Result<Value>`; applied sequentially at load on a copy; original written to
`config/backups/<ts>/`; unknown keys preserved verbatim (forward-compat) but reported; failed
migration ⇒ keep old config + warn (never brick on a typo in vN+1). serde `deny_unknown_fields`
must be OFF for exactly this reason. [design synthesis, H-by-construction]

### D.4 Artifact integrity & signing

- **minisign** crate 0.10.0 (2026-09-25, MIT, 3.4M dl): ed25519, tiny format (used by OpenWrt /
  TUF-adjacent ecosystems). Best cost/benefit for per-artifact signatures. [H]
- **sigstore**: Rust client `sigstore` 0.14.0 (2026-05-22, 1.18M dl) self-describes
  "experimental… not considered stable until 1.0" (docs.sigstore.dev; repo README); Trail of Bits
  review of sigstore (Oct 2026) found 14 issues (4 medium) [M]. Keyless signing is genuinely
  attractive for OSS plugins (no key management, OIDC-bound provenance) but the *verification*
  path in Rust is immature: ship minisign first, sigstore as optional transparency layer.
- **TUF**: the only serious supply-chain *metadata* framework (role delegation, snapshot/targets,
  rollback attack protection, key rotation, expiry). Rust crates: `tuf` 0.3.0-**beta14**
  (2025-10-20, 43k dl — reference impl, slow), `tuftool` 0.17.0 (2026-07-10). Verdict: use TUF
  *concepts* (root.json + targets + snapshot with thresholds & expiry; release channel = role),
  implemented minimally in Rust (~500 LoC verifier) or generated by Python `tuf` in CI;
  full `tuftool` is infra you can run server-side. [H] crates.io; [M] maturity judgment.
- Windows practicality: Authenticode is what Windows SmartScreen/Defender actually respect;
  cargo-dist added **Azure Trusted Signing** support (changelog "Add Azure signing support",
  recent [M]). minisign alone will not stop `Mark-of-the-Web` friction on Windows.

### D.5 Rollback & recovery points

No Rust ecosystem tool does this — it is your design, and the components exist: pre-upgrade
**recovery point** = `sqlite3 .backup`-consistent db copy (or rusqlite Backup API) + config dir
tarball + component-manifest snapshot, all content-addressed, written before any migration runs;
keep last K; verify each by checksum-on-write. **Update = stage → verify → atomic pointer swap**
(symlink on unix, junction/`MoveFileEx` on Windows, file rename on Termux). Rollback = swap pointer
back + restore recovery point; **journal makes restore safe** — post-checkpoint effects replay from
the journal, so the data loss window is exactly `seq(last_journal) - seq(recovery_point)`. DB
downgrade: never destructive for journal/config; rebuild derived tables only. For Levels 3/4: keep
N-1 binary, `vikram runtime use <channel>@<version>`, and the compatibility manifest (§D.1) rejects
N-2 automatically — the rustup channel model (channels + detached-sig + hash verification +
`rustup rollback`) is the reference implementation to imitate.

### D.6 Release channels & cross-platform shipping

- **cargo-dist** 0.32.0 (2026-05-22, MIT/Apache, 187k dl for the crate; the *tool* is how
  tokio/claude-code-rs-adjacent shops ship): still **0.x**; axo.dev handed on work, community
  "PSA: cargo-dist is not dead" (r/rust, 2025-09) documents the wobble honestly; recent releases
  add Azure signing and (per changelog fragment) installer-signing limited to **x86_64 Windows**
  targets. Produces: tarballs+shell installer (linux x64/arm64 gnu+musl, macOS both),
  msi+zip (x64 + aarch64 Windows), `axoupdater` release-PR automation. [H] crates.io; [M] GitHub releases/changelog.
- Rust target tiers (rustc book platform-support.md, fetched 2026-10-31) [H]: all six desktop
  triples (`x86_64/aarch64` × `pc-windows-msvc`/`apple-darwin`, `x86_64/aarch64-unknown-linux-gnu`)
  are **Tier 1 with host tools** — Windows ARM64 reached this, which a 2024 spec could not
  assume; `aarch64-linux-android` is **Tier 2 (std, no host tools)**. **Android/Termux is NOT a
  cargo-dist target** — ship via termux-packages built on-device; treat Termux as
  supported-source unsigned platform [M]; VIKRAM's updater handles channel pinning there.

---

## E. Where this spec is unrealistic — hard limits with evidence

1. **"Level 4 complete runtime replacement" cannot mean in-process Rust.** There is no stable
   Rust ABI (abi_stable dead since 2023-10; safe_abi unreleased; stabby niche; rustc team has
   not committed a timeline). The *only* sound reading: a compatible runtime is a **separate
   executable speaking VIKRAM's versioned protocol** (§C.4), exactly how ACP agents replace
   Zed's built-in one and how codex-rs separates `app-server-protocol` from `core`. That is not
   a consolation prize — it is stronger than the dylib version (crash isolation, any-language
   runtime, per-artifact signing, trivial rollback). Write the spec accordingly.
2. **True hot-reload of Rust components inside a running process is not safe on any OS — and
   should be specified as "reload at boundary", not "reload at hot".** Even identical-toolchain
   `.dylib` swapping is unsound in practice (Bevy#11969; no dlclose rules in UCG#526; TLS/lazy
   static state; Windows unload history). What you *can* deliver: (a) wasm components: unload =
   drop the `Store`/instance — wasmtime guarantees no residue; (b) process components: kill+respawn;
   (c) native components: **load-at-spawn, replace-on-next-spawn, never dlclose** (pin & leak old
   copy like hot-lib-reloader if you must show "update applied" immediately). Spec Part 6's
   "replaceable components updated without a full rebuild" is satisfiable via (a)(b)(c) — but
   not via (d) in-place Rust code replacement. Say so in the spec.
3. **Termux reality** (Termux-packages wiki, read 2026-10-31 [H]): Android ≥10 + targetSdk ≥29 W^X
   blocks `execve` of files inside the app data dir — Termux works around it with `termux-exec`
   LD_PRELOAD shims and `/system/bin/sh`-relative paths; **`dlopen` by path is unaffected**, so
   native .so plugins, wasm (wasmtime "Android supported, less well tested"), JSON-RPC children
   and scripting all function — but sandboxing (seccomp/bubblewrap) is **not available**: on
   Termux a process plugin runs with VIKRAM's full app uid. Spec must state isolation guarantees
   are platform-tiered (desktop: OS-sandboxed; Termux: process-level only, wasm preferred).
4. **Don't make MCP your internal component ABI** (or your stability story hostage to spec churn:
   stateless re-design in 2026-07-28 removed sessions/SSE-resumability wholesale) and **don't
   rebuild the MCP stdio footgun**: VIKRAM spawning plugin processes from user-editable command
   *strings* is how ~10 CVEs happened in clients in 2025–2026 (§C.1).
5. **Temporal/Restate don't belong in the single-binary local product** — both require a server
   daemon (Temporal: plus an external DB in production); Rust SDKs are genuinely good
   (Temporal GA 2026-09-23; Restate 0.13 monthly) — offer them as **optional durable-worker
   adapters** for remote/cluster deployments, and steal their journal+idempotency semantics
   in-process (§B). **Bifrost is dead (2019-alpha). sled is a zombie (alpha since 2024, author
   moved on).** RocksDB is overkill; libsql/Turso adds a company dependency without need.
6. **Deno_core on Android/Termux is unverified [ASSUMPTION]** — V8 prebuilts for
   aarch64-linux-android were not confirmed this session; ship JS support as a cargo feature
   with desktop-only promise, and lean on rhai (pure Rust, everything works, including as a
   wasm guest) for behavioral customization on Termux.
7. **Hot-updating the *host* binary on Windows ARM/macOS is fine (cargo-dist targets all six
   desktop triples), but expect signing friction: SmartScreen needs Authenticode/Azure Trusted
   Signing** — plan the cert budget in Part 25 or updates will trigger AV false-positives;
   this sank several young OSS updaters.
8. **Wasm cost honesty for Part 2C marketing**: plugins that parse >50 MB or do heavy crypto
   will hit the 10–50% compute tax plus host-call copy tax (Extism: 4 copies/round-trip,
   ~284 MiB/s). Fine for tools; if a user wants to replace "all of memory" with a vector-scorer
   subsystem in wasm, expect the journal to cross the boundary a lot. Keep *hot* bulk paths
   either native (pinned-toolchain Level 1/2) or in-process Rust you rebuild.

---

## F. Recommended decision stack (VIKRAM)

| Replacement level | Mechanism | Boundary artifact | Reload semantics |
|---|---|---|---|
| **L1 implementation** | trait objects + **inventory** registry; optional **rhai** hooks for policy slots | crate in workspace; feature flags select impl; script bundle | rebuild-that-crate only (incremental cargo); scripts live |
| **L2 component** | **wasmtime components (WIT interfaces, AOT-precompiled, capability-passing host imports)**; native C-ABI membrane for perf-critical (pinned toolchain, load-at-spawn, never-unload, signed) | `.cwasm` + manifest (§D.1) | live swap next-call (drop Store); native: next-spawn |
| **L3 subsystem** | separate process, NDJSON JSON-RPC over stdio/socket, **argv-array spawn only**, OS-sandbox wrapper per platform (seccomp/bp/bubblewrap; Job objects; Termux: none — prefer wasm) | plugin binary + manifest | restart the daemon; journal replays in-flight effects (§B.2) |
| **L4 runtime** | **VIKRAM Runtime Protocol (VRP): stateless-handshake + journal-seq streaming + MRTR approvals + prompt-leases (§C.4)**; VIKRAM host = client, compatible runtimes = peers; also expose MCP↔VRP bridge and ACP bridge | versioned schema (protobuf or TS/Rust-shared types via `agent-client-protocol`-style codegen) | `vikram runtime connect <cmd>` / url; channel pinning via rustup-like updater |

Stack: rusqlite(bundled)+WAL+refinery+journal as the one durable substrate; minisign per artifact
+ TUF-like channel metadata + cargo-semver-checks at API edges; cargo-dist for the 6 desktop
triples + recovery-point-first upgrader; termux-packages for Android.

**Load-bearing choices (the three everything else hangs from):** (1) one append-only,
effect-idempotent **session journal** in SQLite WAL that doubles as the wire format's `seq`
stream; (2) **wasmtime + WIT** as the sanctioned runtime-swappable component format (with the
C-ABI membrane as the documented escape hatch); (3) a **hand-owned versioned runtime protocol
(VRP)** — MCP/ACP-compatible at the edges, never delegating its state model to either.

## Key sources (accessed 2026-10-31)

crates.io API for every version/license/date row · rust-lang/rust platform-support.md ·
docs.wasmtime.dev + bytecodealliance.org/articles/wasmtime-10-performance · extism.org + dylibso blog ·
modelcontextprotocol.io changelogs + GitHub raw `2026-07-28/changelog.mdx` · agentclientprotocol.com + CHANGELOG ·
docs.restate.dev · temporal.io GA blog + best-practices · docs.langchain.com checkpointers · Schneider
"Why Checkpoints Aren't Durable Execution" · CSA + OX Security + valyu MCP-CVE trackers + arXiv 2603.22489 ·
a2a-protocol.org + LF press · bevy#11969 · UCG#526 · inventory/linkme READMEs · docs.rs/abi_stable ·
sigstore docs + Trail of Bits review · cargo-dist releases + r/rust PSA · termux-packages wiki · sqlite.org/wal ·
openai/codex · pgrx · Zellij docs · riotsecure + hostmycode perf/size studies · users.rust-lang.org/t/116623.
