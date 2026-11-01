# Permission modes — competitor evidence and the VIKRAM decision

Status: **implemented** on 2026-11-01 (`docs/research/security/agent-security-and-permissions.md`
§7 records what shipped). The operator first approved Claude Code's six-mode set, then asked for a
seventh — `allow-list` — so the shipped set is seven, with rules still on their own axis.
Evidence gathered 2026-11-01 from official docs and the `openai/codex` and `google-gemini/gemini-cli`
repositories. Every competitor claim in §1–§5 is sourced to an official page or a source file, or
marked UNVERIFIED in §6; third-party blogs are used for breadth only, never for a claim about
internals, and the two rows that had rested on them are now confirmed against upstream (§6).

---

## 1. The decision

Seven modes, and rules on a separate axis:

| # | Mode (upstream name) | VIKRAM alias | Posture |
|---|---|---|---|
| 1 | `default` (UI label "Manual") | `ask` | reads run; anything gated prompts |
| 2 | `acceptEdits` | `edit-ok` | #1 plus workspace writes auto-approved |
| 3 | `plan` | `plan` | read-only; mutations refused, not deferred |
| 4 | `auto` | `smart` | everything runs, each action risk-tiered first (see §3) |
| 5 | `dontAsk` | `unattended` | anything that would prompt is **denied**, and the model is told not to retry |
| 6 | `bypassPermissions` | `yolo` | every prompt auto-approved |
| 7 | *(Cursor's `Allowlist`; ours is the same idea plus the prompt)* | `allow-list` | **only** what a human wrote down runs quietly; everything else prompts, and each prompt proposes the line that would stop it |

Rules (`allow` / `ask` / `deny`) are evaluated inside every mode, deny-first, and a `deny` beats an
`allow` regardless of specificity. Mode is posture toward *prompts*; it is not a permission list.
That is why `acceptEdits` is a mode while an allow-list is usually not — and why #7 is the exception
that proves the rule: `allow-list` is still not a permission list, it is a refusal to use the rules
that are not on one. The list itself lives on the rules axis (`~/.vikram/rules.json`, §7).

`unattended` is the mode that makes Part 8 (always-active runtime, scheduling, background tasks)
possible: a cron-fired agent has no human to answer, and "auto-deny and stop asking" is a legitimate
posture whereas "hang forever" is not.

## 2. Non-negotiables in every mode

The floor at [vikram-policy/src/lib.rs:114](../../crates/vikram-policy/src/lib.rs) is compiled in
and survives all seven modes, including `yolo`:

- `grant.*`, `policy.*`, `self.*` refused when the request originates from model- or tool-authored
  content (`ApprovalSource::PolicyAuto` is rejected for untrusted origin at
  [vikram-kernel/src/grants.rs:123](../../crates/vikram-kernel/src/grants.rs)).
- writes outside the workspace root always need a human;
- delegation refused at depth 3;
- a component (plugin, replacement policy, remote runtime) may not disable a mandatory policy.

Upstream validates this shape rather than contradicting it: Claude's own docs state that deny rules
"block in every mode, including `bypassPermissions`", that protected paths and `rm`/`rmdir` on
critical paths still prompt, and that managed settings can set
`disableBypassPermissionsMode` / `disableAutoMode` which no lower layer can override. Codex
separates the same concern into `requirements.toml` (MDM > Agent Security > device file > user
config) carrying `allowed_approval_policies` and `allowed_sandbox_modes` — "requirements constrain
behaviour; configuration values provide defaults".

**Consequence: a *mode availability* floor is a prerequisite, not a follow-up — so it shipped with
the modes, not after them** (`ModeAvailability::resolve`,
[vikram-runtime/src/config.rs:92](../../crates/vikram-runtime/src/config.rs)). Upstream validates
the shape: Gemini ships `security.disableYoloMode` — documented as "Disable YOLO mode, even if
enabled by a flag" — plus `security.disableAlwaysAllow` and `admin.secureModeEnabled`, all three
confirmed in `packages/cli/src/config/config.ts` (§6). Codex does the same through
`requirements.toml` (`allowed_approval_policies`, `allowed_sandbox_modes`, MDM > Agent Security >
device file > user config). Claude's managed settings can set `disableBypassPermissionsMode` /
`disableAutoMode`. There is no shipped runtime where the most permissive mode is unforbiddable.

## 3. `smart` is not upstream's `auto`

Claude's `auto` and Qwen Code's `Auto` both use an **AI classifier** on the authority path
(documented to block `curl | bash`, force-push, secret exfiltration, `rm -rf $VAR`, `--insecure`,
and to "self-demote after repeated blocks"). VIKRAM's `smart` must not: an agent must never be able
to grant itself permissions by generating output, and a classifier invoked on model output is a
model on that path.

`smart` is therefore the deterministic tiering the codebase already supports, on
`ActionKind` × `SideEffect` ([vikram-contracts/src/policy.rs:10 and :23](../../crates/vikram-contracts/src/policy.rs)),
as shipped in `auto_allows_tiered`
([vikram-contracts/src/policy.rs:279](../../crates/vikram-contracts/src/policy.rs)):

| class | auto-allowed in `smart` |
|---|---|
| `Read` + `SideEffect::None` | yes |
| `Write` + `LocalMutation` inside workspace root | yes |
| `Execute` + `LocalMutation` | yes — the tier is the pair, not the verb; `Execute` + `ExternalChange` still prompts |
| `Network`, `Spend`, `Delegate`, `Admin`, any `Destructive` | prompt |

The `Destructive` check runs before the per-action match, so a destructive read cannot slip through
the `Read` row. This replaces the earlier proposal to auto-allow `Execute` "once per verb prefix per
session" via grant widening: a posture that widens a `GrantSet` would make the mode itself an
authority path, and §5.5 is closed on that basis.

Codex proves the deterministic route is mainstream, not a compromise: `render_decision_for_unmatched_command`
in `codex-rs/core/src/exec_policy.rs` computes the decision as a pure function of
(policy × dangerous-command heuristics × sandbox kind) with no model call.

## 4. Mechanisms to adopt, with the gap each closes

| Upstream mechanism | Our current state | Gap |
|---|---|---|
| Claude: "don't ask again" writes an allow rule to `.claude/settings.local.json` at the git root, prefix-scoped (`npm test *`), compound approvals split per subcommand; file-mod approvals stay session-only | durable operator tier at `~/.vikram/rules.json` (`RuleBook`, [vikram-runtime/src/rules.rs:1](../../crates/vikram-runtime/src/rules.rs)), read as the operator tier of `Policy`; the saved line is the prompt's own proposal, canonicalised (§7) | two tiers exist now, but only the user-level file — no git-root project file, and no session-only carve-out for file writes |
| Codex: workspace-write protects `.git`, `.agents`, `.codex`, `.aws` *inside* a writable root | workspace root only | no protected subpaths |
| Codex: network default **off** inside workspace-write, own escalation lane, optional domain allowlist | `ActionKind::Network` exists, one lane | no separate network approval lane, no default-off posture |
| Codex: `granular = { sandbox_approval, rules, mcp_elicitations, request_permissions, skill_approval }` auto-rejects per category | single global approve/deny | category-scoped unattended policy |
| Codex: `ExecApprovalRequirement::NeedsApproval { proposed_execpolicy_amendment }` — "smart approvals may propose a `prefix_rule`" | `Decision::RequireApproval { amendment }` | already built; validated |
| Claude: `PreToolUse` hooks run **before** the prompt, can tighten (deny > defer > ask > allow), cannot bypass a deny | floor-before-rules gives the same ordering | hooks are not wired to the permission path yet |
| Claude: `plan` is enforced (edits blocked; shell through the classifier), not merely prompted | `PermissionMode::resolve` returns `Deny` for any non-read request, in the harness and in the kernel ([vikram-contracts/src/policy.rs:207](../../crates/vikram-contracts/src/policy.rs)) | matches; the classifier half is deliberately not copied (§3) |
| Codex: `--ask-for-approval never` is the documented CI flag; unmatched prompts are rejected by policy in code | `vikram chat` boots in `unattended` unless the operator asked for a prompting posture, and a prompt that still arrives is refused with a reason ([vikram-cli/src/chat.rs:85](../../crates/vikram-cli/src/chat.rs)) | matches, and it is now a named posture rather than an accident of the headless path |
| Gemini: in non-interactive mode a tool that would prompt is taken away — "In non-interactive mode, exclude tools that require a prompt" (`packages/cli/src/config/config.ts`) | `unattended` converts every prompt into a refusal that tells the model not to retry | same conclusion, reached by posture instead of by tool-list pruning; ours keeps the tool visible so the transcript says what was withheld |
| Gemini: sandbox is **added** under the permissive flag — "Sandbox is enabled when using `--yolo` … by default", while Codex's `--yolo` removes it | no sandbox layer yet | the two-axis split matters: ours must be explicit, never inherited from a mode |
| Gemini: `yolo` cannot be set in settings, only by flag, and `admin.secureModeEnabled` "disallows YOLO mode and 'Always allow' options from being used" | `security.disable_yolo` / `disable_auto_approve` / `disable_always_allow` in `ModeAvailability`, checked at activation and at boot (§7); `--mode` is refused with the layer and rule named | matches on the floor. Ours is settable in config like every other mode, but a layer above config always wins |
| OpenCode: most permissions **default to `allow`**; only `doom_loop` and `external_directory` default to `ask`, and `read` allows everything except `*.env` / `*.env.*` | **no match ⇒ `deny`** (deny-first) | opposite defaults — see §5.6; their `*.env` carve-out inside a permissive default is worth stealing |
| OpenCode: non-interactive clients "must decide how to handle approval requests"; an unresolved `ask` has no documented auto-reject, and in practice surfaces as a stall | `unattended` denies on the client's behalf, and `vikram chat` refuses a prompt nobody is reading | evidence that mode #5 must be a named posture, not left to each client. Note: an earlier draft of this file cited an OpenCode "`Unattended` ruleset"; **refuted** — the official permissions page documents only `allow`/`ask`/`deny` plus `--auto` (§6) |
| OpenCode Auto mode: "Explicit `deny` rules are still enforced. Auto mode only changes requests that would otherwise ask" | floor + deny rules survive `yolo` (§2) | the same invariant reached independently, in the product closest to our rules shape |
| goose `smart_approve`: read-vs-write classification "is interpreted by your LLM provider" | `smart` is deterministic (§3) | do not copy the judge — it puts a model on the authority path, which the spec forbids |
| goose: `auto` ("Completely Autonomous") is the **default** mode; per-tool `PermissionLevel` can override it | one global posture per session | per-tool overrides (§5.7); and proof that a permissive default is a product choice |
| Cursor `--force` (`--yolo` alias): "allows the agent to make direct file changes without confirmation"; `permissions.deny` tokens outrank allow | floor + deny rules survive `yolo` (§2) | agreement on the floor; whether `--force` outranks an explicit deny is undocumented, so we do not claim it either way |
| Cursor `Auto-review` is the default run mode and is a classifier | no classifier anywhere on our authority path | confirms §3: VIKRAM must not follow Cursor here |
| OpenHands: `ConfirmRisky(threshold: SecurityRisk = HIGH, confirm_unknown: bool = True)` | `Decision::Deny` on no match | the `confirm_unknown` default is the right instinct: unknown risk is treated as risky, never as benign |

### The field at a glance

Every mode name below is the upstream spelling, verified against an official page or source file.

| Runtime | Postures (upstream names) | Permissive hatch | What still stops it | Default |
|---|---|---|---|---|
| Claude Code | `default` (labelled "Manual") · `acceptEdits` · `plan` · `auto` · `dontAsk` · `bypassPermissions` | `bypassPermissions` | deny rules, protected paths, managed `disableBypassPermissionsMode` / `disableAutoMode` | `default` |
| Codex | approval `on-request` · `never` · `granular`, on its own sandbox axis `read-only` · `workspace-write` · `danger-full-access` | `--yolo` (removes approvals **and** sandbox) | `requirements.toml` allowed policies/modes, protected subpaths, Starlark deny rules | prompting |
| Gemini CLI | `plan` · `default` · `auto_edit` · `yolo` | `--yolo` — settable by flag only, not by settings | `security.disableYoloMode` ("even if enabled by a flag"), `security.disableAlwaysAllow`, `admin.secureModeEnabled`; sandbox is **added** under `--yolo` | prompting |
| goose | `chat` · `approve` · `smart_approve` · `auto` (UI label "Completely Autonomous") | `auto` | per-tool `PermissionLevel` overrides; `smart_approve` delegates the read/write call to the LLM | **`auto`** |
| Cursor | agent run modes `Auto-review` · `Allowlist` · `Run Everything`; CLI `--force` (`--yolo` is an alias) | `--force` / `Run Everything` | `permissions.deny` tokens outrank allow; deny-vs-`--force` precedence is undocumented | `Auto-review` (a classifier) |
| OpenCode | rules resolving to `allow` · `ask` · `deny`; Auto mode as the hatch; **no mode named `Unattended`** | `--auto` | explicit `deny` rules — "Auto mode only changes requests that would otherwise ask for approval" | mostly `allow` |
| OpenHands | `AlwaysConfirm()` · `ConfirmRisky()` · `NeverConfirm()` | `NeverConfirm()` | in `ConfirmRisky`, the analyzer's risk threshold plus `confirm_unknown=True` | varies by surface |

What this table is evidence for:

1. **There is no industry-standard naming, only a common axis.** Seven runtimes, seven spellings,
   and every one of them arranges its postures by how much prompting is left in — read-only, then
   edits, then everything-with-rules, then nothing-prompts. The counts differ (OpenHands has three,
   Claude six), so this is an ordering, not a converged ladder. Which is why §1 keeps VIKRAM aliases
   rather than copying upstream vocabulary.
2. **The default is a product decision, not a technical one.** goose defaults to fully autonomous and
   Cursor defaults to a classifier; Claude and Codex default to prompting. Our default is `ask`.
3. **Cursor is the only one that made "Allowlist" a mode; everyone else keeps lists on the rules
   axis.** We ship both conventions at once: the list is a rule tier (`~/.vikram/rules.json`) *and*
   mode #7 is the posture that refuses to run anything not on it. That is the operator's call, made
   after this file first argued for the rules axis alone — the field evidence is that both shapes
   exist in shipped products, so choosing both is not a compromise with upstream practice.
4. **The sandbox axis is not part of any mode table.** Codex's `--yolo` removes isolation, Gemini's
   adds it ("Sandbox is enabled when using `--yolo` or `--approval-mode=yolo` by default").
   Whichever way VIKRAM eventually goes, the coupling must be stated, never inherited.

## 5. Design decisions

1. **Where the mode lives** — settled: a `mode` key in `config.json`, a `--mode` flag for one
   process, and `/mode <name>` in both interfaces, all three checked against the same availability
   layers. The keyboard chord is still open: `input.rs` binds `Ctrl+Shift+Tab` for panel cycling, so
   a Claude-style `Shift+Tab` cycle needs its own chord rather than a stolen one.
2. **Journal the mode change** — settled: `set_mode` writes a `RecordKind::Policy` row with
   `{"decision":"mode","from","to"}` and emits `RuntimeEvent::ModeChanged`, so the transcript shows
   who changed the posture and when. **Still open: replay does not restore it.** A resumed session
   starts from config, not from the last journaled mode; the durability guarantee made for grants
   has not been extended that far.
3. ~~Mode availability floor~~ — **shipped.** Every runtime surveyed forbids the permissive modes
   from a policy layer above the user (Gemini `security.disableYoloMode` beats a flag; Codex
   `requirements.toml`; Claude managed settings). Ours is `ModeAvailability` (§7), checked at boot
   and at activation, and not widen-able from below: a substituted policy is evaluated *before* the
   posture is applied, in the harness and in the kernel, so it cannot answer its way out of `plan`,
   `allow-list`, or `ask`.
4. ~~Does `edit-ok` auto-approval need a ticket?~~ — **settled: no, and it must not have one.**
   A posture converts *this prompt* into permission for *this request*; it mints neither an
   `ApprovalTicket` nor a `GrantSet` widening. The original position ("yes — it must go through
   `ApprovalTicket`") was the wrong frame: a ticket is the path to *durable* authority, and if a
   config key could issue tickets then the config file becomes a self-granting route. Durability
   goes through the amendment a human approved instead (§7).
5. ~~`smart`'s `Execute` tier~~ — **settled:** the pair `Execute` × `LocalMutation` auto-allows, no
   session grant, no verb-prefix memory (§3). Prompt-every-time was rejected as useless friction;
   grant-widening was rejected because it would make a posture an authority path.
6. **No-match default.** We deny, OpenCode asks. Deny-first stays: it is what makes `yolo` merely
   "stop prompting" instead of "grant everything". The user-visible half of this is still open —
   the TUI must say plainly *why* a tool was refused; today the reason string reaches the transcript
   but not a dedicated surface.
7. **Per-tool overrides** (`goose`: Always Allow / Ask Before / Never Allow; Cursor `Mcp(srv:tool)`
   tokens) versus our verb-prefix rules — still open. Whether `mcp.*` names get wildcard handling in
   `Rule`, which today matches a literal verb prefix.

## 6. Evidence table

Third-party rows from the first pass have been re-read against upstream. Where the official source
contradicted or failed to confirm a claim, that is said in the row.

| Claim | Source | Confidence |
|---|---|---|
| Modes `default`/`acceptEdits`/`plan`/`auto`/`dontAsk`/`bypassPermissions`; `manual` alias; deny/ask rules apply under `bypassPermissions`; `disableBypassPermissionsMode` | [code.claude.com/docs/en/permission-modes](https://code.claude.com/docs/en/permission-modes), [permissions](https://code.claude.com/docs/en/permissions), [CHANGELOG](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md) | high — official docs, read 2026-11-01 |
| Rule syntax (`Bash(npm run *)`, `Edit(/src/**)`, `WebFetch(domain:…)`), settings precedence managed > `--settings` > local > project > user, lists merge | same, plus [hooks](https://code.claude.com/docs/en/hooks) | high |
| Headless `-p` starts in `default`, denies unresolved approvals, `--permission-prompts none` tells the model not to retry | [headless](https://code.claude.com/docs/en/headless) | high |
| Codex current policies `on-request` / `never` / `granular`; `untrusted` retired, `on-failure` deprecated | [config reference](https://developers.openai.com/codex/config-file/config-reference), `codex-rs/core/src/exec_policy.rs` | high |
| Sandbox axis `read-only`/`workspace-write`/`danger-full-access`; Seatbelt / `bwrap`+seccomp / MXC; network default off; protected `.git`,`.agents`,`.codex`,`.aws` | [sandboxing](https://developers.openai.com/codex/sandboxing.md), [approvals](https://developers.openai.com/codex/agent-approvals-security.md) | high |
| Starlark `prefix_rule(pattern, decision, justification)` rulesets per config layer, strictest wins | [exec-policy](https://developers.openai.com/codex/exec-policy.md), `codex-rs/execpolicy/README.md` | high |
| `requirements.toml` non-loosenable tier | [managed configuration](https://developers.openai.com/codex/enterprise/managed-configuration) | high |
| Qwen Code's five modes (`Plan`/`Ask Permissions`/`Auto-Edit`/`Auto`/`YOLO`), `tools.approvalMode`, `Shift+Tab`, and that YOLO auto-approves edits *and* shell commands | [Qwen docs — approval mode](https://qwenlm.github.io/qwen-code-docs/en/users/features/approval-mode/) | high — official docs, re-read 2026-11-01 (was medium-high) |
| Gemini CLI mode names are `default` / `auto_edit` / `yolo` / `plan`; `--approval-mode` is the current spelling | [Gemini CLI configuration](https://google-gemini.github.io/gemini-cli/docs/get-started/configuration.html), `packages/cli/src/config/config.ts` (approval-mode override block) | high — official docs plus source, re-read 2026-11-01. **Replaces** the wal.sh / inventivehq row this claim previously cited; those are not needed for it |
| `security.disableYoloMode` = "Disable YOLO mode, even if enabled by a flag"; `security.disableAlwaysAllow`; `admin.secureModeEnabled` | `packages/cli/src/config/config.ts` (`settings.security?.disableYoloMode`, `settings.admin?.secureModeEnabled`, `settings.security?.disableAlwaysAllow`), [Gemini CLI settings](https://geminicli.com/docs/cli/settings/) | high — the three keys are read out of the source file, not inferred from a blog |
| "In non-interactive mode, exclude tools that require a prompt" | `packages/cli/src/config/config.ts`, same block | high — verbatim comment. Earlier phrasing ("`ask_user` is treated as `deny`") was a paraphrase of a third-party write-up and is **corrected**, not just re-sourced |
| Sandbox is **added** under `--yolo`, not removed | [Gemini CLI configuration](https://google-gemini.github.io/gemini-cli/docs/get-started/configuration.html): "Sandbox is enabled when using `--yolo` or `--approval-mode=yolo` by default" | high |
| OpenCode rules resolve to `allow`/`ask`/`deny`, `doom_loop` and `external_directory` default to `ask`, `--auto` is the hatch, and explicit `deny` still binds: "Auto mode only changes requests that would otherwise ask for approval" | [opencode.ai/docs/permissions](https://opencode.ai/docs/permissions/) | high — official docs, re-read 2026-11-01 |
| **REFUTED:** an earlier draft cited an OpenCode "`Unattended` ruleset" with a documented auto-reject for unresolved asks | the same page has no such mode or ruleset | none — the claim is withdrawn; §4 now records it as refuted. The auto-reject behaviour in our `unattended` is our own decision, not borrowed evidence |
| Auto-mode classifier internals (which model, which read-only list) | [aibuilderclub](https://www.aibuilderclub.com/blog/agent-modes-plan-default-auto) | low-medium — practitioner write-up, and nothing in this file relies on it: §3 rejects a classifier on principle rather than on its specifications |

UNVERIFIED, and deliberately load-bearing nowhere: the model/harness behind Claude's `auto`
classifier and its exact read-only command list; `codex exec`'s default approval policy (the docs
show a table only); whether Cursor's `--force` outranks an explicit `permissions.deny` (§4 marks that
row undocumented).

## 7. What shipped on 2026-11-01

| Piece | Where | Behaviour |
|---|---|---|
| The seven postures | `PermissionMode` in [vikram-contracts/src/policy.rs](../../crates/vikram-contracts/src/policy.rs) | `as_str()` → `ask`, `edit-ok`, `plan`, `smart`, `unattended`, `yolo`, `allow-list`; `parse()` also accepts the upstream spellings (`default`, `acceptEdits`, `auto`, `dontAsk`, `bypassPermissions`) so a config written against another product loads |
| Posture application | `PermissionMode::resolve` | `Deny` is final and is never postured, in any mode. `plan` denies non-reads; `allow-list` downgrades an unlisted `Allow` back to a prompt carrying the amendment; only `RequireApproval` reaches the per-mode table |
| Enforcement, not suggestion | `RuntimeInner::judge` ([vikram-runtime/src/runtime.rs](../../crates/vikram-runtime/src/runtime.rs)) and the kernel's dispatch ([vikram-kernel/src/machine.rs](../../crates/vikram-kernel/src/machine.rs)) | both evaluate the policy **first** and apply the posture after, so a replacement `PermissionPolicy` cannot loosen `plan`, `allow-list` or the prompting of `ask` by answering differently. The workspace root comes from `PermissionPolicy::workspace_root()`, defaulting to `None` — the conservative answer |
| Availability floor | `ModeAvailability::resolve` ([vikram-runtime/src/config.rs:92](../../crates/vikram-runtime/src/config.rs)) | layers, strictest wins, in order `system requirements` → `environment` → `config`. `/etc/vikram/requirements.json` (`VIKRAM_REQUIREMENTS` to relocate), then `VIKRAM_DISABLE_YOLO`, `VIKRAM_DISABLE_AUTO_APPROVE`, `VIKRAM_DISABLE_ALWAYS_ALLOW`, `VIKRAM_ALLOWED_MODES`, then `security.*` in `config.json`. A refused `mode` at boot falls back to `ask` instead of failing; a refused `--mode` is a hard error naming the layer and rule |
| Keeping an approval | `RuleBook` ([vikram-runtime/src/rules.rs](../../crates/vikram-runtime/src/rules.rs)) → `~/.vikram/rules.json`, mode 0600, atomic write | `answer_approval(approve, persist)` saves the prompt's own proposal; `disable_always_allow` refuses *saving* while still allowing `/allow rm` and `vikram allow rm`, because a machine that forbids keeping must never forbid removing |
| The amendment grammar | `amendment_for` / `Policy::parse_amendment` | `allow <action> <verb> resource=<scope>`: a command saves its first two words (`cargo test *`), a path saves its directory (`/work/proj/**`), anything else saves itself. Anything not in exactly the shape a prompt would have produced is rejected, so a hand-written `resource=*` cannot be smuggled in as "the approval I already gave" |
| Two rule tiers | `Policy{rules, defaults, floor}` ([vikram-policy/src/lib.rs](../../crates/vikram-policy/src/lib.rs)) | operator rules are read before built-in defaults; within a tier deny beats ask beats allow, and a compound command is checked segment by segment so `cargo test && rm -rf /` cannot ride a `cargo test *` allowance |
| Surfaces | `vikram --mode`, `vikram allow list\|add\|rm`, `vikram doctor`, `/mode`, `/allow`, `/permissions`, the TUI status chip and the approvals overlay | the chip is always the shield glyph plus the posture name (`⛨ ask`, `! yolo` in an ASCII terminal) and switches to the accent colour when `auto_approves()`, placed early in the line so a 46-column terminal still shows it; the overlay footer reads `a allow once · p keep on the list · d deny`; `doctor` prints the effective mode, the refusing layer if the configured one was overridden, and the saved-entry count with its path |
| Headless | [vikram-cli/src/chat.rs](../../crates/vikram-cli/src/chat.rs) | `vikram chat` starts `unattended` (it has nobody to ask) and prints `[denied: nobody to ask] …` for any prompt that still arrives. It no longer auto-approves |
| Journaling | `turn.rs` | one `RecordKind::Policy` row per gated request carrying the evaluated decision, the postured decision, whether the posture is what changed it (`auto`), and the reason — plus one `RecordKind::Approval` row when a human actually answered, so an audit can tell the two apart |

Not yet, by decision rather than oversight: mode restore from the journal (§5.2), the per-tool
override axis (§5.7), a git-root project rules file, a keyboard chord for cycling (§5.1), and the
sandbox axis (§4 note 4).
