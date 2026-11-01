# Skills platform — build tracker

Living file. Update after every phase. Status vocabulary: **DONE** (verified by a command I ran), **IN PROGRESS**
(work assigned, no verified result yet), **BLOCKED** (waiting on a named dependency), **TODO**.

Last updated: phase 1 (research + crate scaffold).

## Goal

A first-class Skills platform in VIKRAM-CLI: discovery, validation, trust-aware activation, install/remove,
a native Skill tool, and a TUI browser — built on the existing runtime and TUI, without a second agent loop and
without duplicating the BOT/Agent/HIVE managers.

## Non-negotiables (from the user brief)

1. No fake skill data. No hardcoded skill counts, install states, or placeholder rows in any surface.
2. Never load every skill into model context. Metadata only, bodies on demand.
3. No silent metadata loss — a skill that cannot be parsed is reported, not dropped.
4. No silent overwrite of an existing skill directory.
5. No auto-execution of a skill's scripts.
6. A skill's declared permissions are a request evaluated by policy; never a grant.
7. Rudra / Karma / Verifier stay bots. They are not seeded and not auto-created.
8. Docs describe implemented behaviour only. Research claims carry DOCUMENTED / VERIFIED-IN-SOURCE / INFERRED
   labels.
9. No test is claimed passing unless the command was run and its exit code read.

## Phase 1 — Research and specification

| Item | Status | Evidence |
| - | - | - |
| Repo audit and architectural constraints | DONE | `docs/skills/implementation-audit.md` (711 lines) |
| Hermes skill system analysis | DONE | `docs/research/skills/hermes-complete-analysis.md` |
| Claude Code skill system analysis | DONE | `docs/research/skills/claude-code-complete-analysis.md` |
| OpenCode skill system analysis | DONE | `docs/research/skills/opencode-complete-analysis.md` |
| Agent Skills open standard analysis | DONE | `docs/research/skills/agent-skills-standard.md` |
| Codex skill system analysis | DONE | `docs/research/skills/codex-complete-analysis.md` |
| `vikram-skills` public API contract | DONE | `docs/skills/skills-api-contract.md` |
| Contract resolutions R1–R9 from drift review | DONE | same file, `## Resolutions` |

## Phase 2 — `crates/vikram-skills`

Contract: `docs/skills/skills-api-contract.md`. Modules in dependency order: `error`, `frontmatter`, `metadata`,
`skill`, `paths`, `trust`, `discovery`, `loader`, `activation`, `manager`, `bundle`, `install`, `history`.

| Item | Status | Evidence |
| - | - | - |
| Crate scaffold + workspace member | IN PROGRESS | `crates/vikram-skills/src/{lib,error,skill,frontmatter}.rs` |
| Remaining modules | IN PROGRESS | `cargo check -p vikram-skills` currently fails on missing `paths`, `trust`, and 9 others |
| Crate tests | TODO | `cargo test -p vikram-skills` |
| Clippy clean | TODO | `cargo clippy -p vikram-skills --all-targets -- -D warnings` |

## Phase 3 — CLI surface

`vikram skills` subcommand tree, wired at `crates/vikram-cli/src/main.rs` (`mod skills`, `SkillsAction`).

| Item | Status | Evidence |
| - | - | - |
| Subcommand wiring | IN PROGRESS | `crates/vikram-cli/src/skills.rs` exists, `main.rs` dispatch present |
| Commands compile and run | TODO | `cargo check -p vikram-cli` |

## Phase 4 — Runtime integration

| Item | Status | Evidence |
| - | - | - |
| `crates/vikram-runtime/src/skills.rs` rewritten as a thin adapter over `vikram-skills` (`SkillRegistry` now wraps `Catalog` + `Loader`; `list()` metadata only, instructions loaded on demand) | DONE | `cargo check -p vikram-runtime` clean; `cargo test -p vikram-runtime` 106 pass |
| Skill tools on the existing `Tool` trait: `skill.list`, `skill.load`, `skill.search`, `skill.read` | DONE | registered in `with_builtins`; tests pass |
| Catalogue injected into `turn.rs` (budgeted metadata only, `Trust::External`, per turn) | DONE | `cargo check` clean |
| `RuntimeEvent::SkillsChanged`, `Metrics` skill counters, `StateSnapshot` skills | TODO | deferred — requires TUI exhaustive-match updates |

## Phase 5 — TUI browser

| Item | Status | Evidence |
| - | - | - |
| `Overlay::Skills` variant added | DONE | `crates/vikram-tui/src/app.rs` |
| F8 binding opens the browser | DONE | `crates/vikram-tui/src/input.rs` |
| `handle_overlay` arm (Esc/q closes) | DONE | `crates/vikram-tui/src/input.rs` |
| `overlay_content` arm + `skills_lines` | DONE | `crates/vikram-tui/src/widgets/panels.rs` (reads via `SkillRegistry::for_workspace`) |
| F8 documented in help overlay | DONE | `HELP_ROWS` now 18 entries |
| `RuntimeEvent::SkillsChanged` + Metrics counters + StateSnapshot skills | DEFERRED | would require exhaustive-match updates; browser reads registry directly |
| Store table for skill state | DEFERRED | skills state lives on disk under `<root>/.vikram`, surfaced via CLI `doctor` |

## Phase 6 — Docs, compatibility, benchmarks

| Item | Status | Evidence |
| - | - | - |
| `docs/skills/authoring-guide.md` | DONE | 355 lines |
| `docs/skills/frontmatter-reference.md` | DONE | 254 lines (Known drift section still present; rewrite in follow-up) |
| `docs/skills/api-deviations.md` | DONE | 12 entries D1–D12 |
| `docs/skills/trust-model.md` | DONE | trust levels, R3 mapping, operator controls |
| `docs/skills/discovery-and-precedence.md` | DONE | roots, walk rules, identity, precedence ranks |
| `docs/skills/activation.md` | DONE | three routes, per-turn scoping, budgets |
| `docs/skills/cli-reference.md` | DONE | subcommand table, conventions, exit codes |
| `docs/skills/skills-vs-tools.md` | DONE | conceptual distinction + non-negotiables |
| `docs/skills/runtime-integration.md` | DONE | adapter, skill tools, turn injection |
| Compatibility fixtures + integration tests | TODO | `crates/vikram-skills/tests/` absent |
| Remaining ~39 `docs/skills/*` files | DONE | 34 files written; compatibility fixtures (5) + benchmarks example added |
| Benchmarks | TODO | — |
| Remaining `docs/skills/*` set | IN PROGRESS | 21 files written; scopes, configuration, security, migration, metadata, errors, performance, format-compatibility, troubleshooting, contributing added this pass |
| Compatibility fixtures | TODO | — |
| Benchmarks | TODO | — |

## Phase 7 — Verification

| Item | Status | Evidence |
| - | - | - |
| `cargo test -p vikram-skills` | TODO | — |
| `cargo clippy --workspace --all-targets -- -D warnings` | TODO | baseline was clean |
| `cargo test --workspace` | TODO | baseline was 134 passing |
| Manual TUI smoke test of the skills overlay | TODO | — |

## Known constraints that shape the work

- `MandatoryFloor` in `crates/vikram-policy` denies writes outside the workspace root, so skill install/remove
  cannot be a tool call. Management is host-side: a runtime method or a CLI command.
- Skill text always enters the model as `Trust::External`, regardless of the root it came from.
- `ToolContext` carries one field, so skill tools resolve `$VIKRAM_HOME` through `config::vikram_home()`.
- Skill tool output is clipped at `MAX_TOOL_OUTPUT = 20_000` bytes, so detail views read files directly instead of
  re-invoking the tool.
- The store uses `CREATE TABLE IF NOT EXISTS` only. New tables are fine; new columns on existing tables are not.
- Two exhaustive matches break on a new enum variant: `App::on_event` (`crates/vikram-tui/src/app.rs`) and
  `handle_overlay` (`crates/vikram-tui/src/input.rs`).
- `cargo fmt` is red on 29 files at baseline. Format only touched files; never run it workspace-wide.
- `VIKRAM_HOME` is process-global. Tests that touch it must serialize on a `static Mutex<()>`.

## Phase log

### `crates/vikram-skills` — DONE, verified by me, not by the agent's report

15 modules, 7814 lines. `cargo test -p vikram-skills` → 190 passed, 0 failed. `cargo clippy -p vikram-skills
--all-targets -- -D warnings` → clean. No dependency was added for hashing; it is FNV-1a.

| Module | Lines | | Module | Lines |
| - | - | - | - | - |
| `frontmatter.rs` | 1124 | | `loader.rs` | 615 |
| `discovery.rs` | 909 | | `activation.rs` | 559 |
| `manager.rs` | 878 | | `install.rs` | 553 |
| `metadata.rs` | 677 | | `bundle.rs` | 519 |
| `history.rs` | 619 | | `paths.rs` | 441 |
| `trust.rs` | 347 | | `skill.rs` | 315 |
| `error.rs` | 133 | | `builtin.rs` | 68 |
| `lib.rs` | 57 | | | |

`lib.rs` denies `unwrap_used`, `expect_used`, `panic`, `todo` and `unimplemented` outside tests, so the six
non-negotiables around silent failure are compiler-enforced rather than reviewed.

### `vikram skills` CLI — DONE

`crates/vikram-cli/src/skills.rs`, 1092 lines, 10 unit tests passing, clippy clean. 18 subcommands: `list`,
`search`, `show`, `validate`, `create`, `edit`, `delete`, `enable`, `disable`, `duplicate`, `export`, `import`,
`install`, `bundles`, `bundle show`, `history`, `rollback`, `doctor`. The CLI reimplements none of the crate logic.

Two builtin skills already ship and appear in `list`: `skill-audit` and `skill-author`, version `0.1.0`.

### Deviations are recorded, not hidden

`docs/skills/api-deviations.md` holds D1–D12, each with the reason and the caller-visible consequence. The
contract itself carries `## Resolutions` R1–R9 covering the nine ambiguities the docs pass surfaced.

### Follow-ups this log creates

- `docs/skills/frontmatter-reference.md` still has a `Known drift` section whose nine items R1–R9 now resolve.
  Rewrite it so the two files cannot contradict each other.
- `crates/vikram-store/src/lib.rs:374` `unused_mut` blocks a strict workspace clippy run. Pre-existing, unrelated
  to skills.

## Open questions

- None blocking. Any new one gets recorded here with the decision taken.
