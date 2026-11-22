# Always-on session — design

Status: 2026-11-21. Scope: how Vikram maintains a persistent, always-available
presence without violating session isolation, memory scoping, or the
deny-first permission model. Companion to `docs/voice-plan.md` (voice is one
input to this session, not the session itself) and `docs/jarvis-class.md`.

## 0. What "always-on" must mean here

Not "the TUI runs forever." An always-on session is:

- **One durable identity.** The main Vikram agent has a persistent session
  (or session chain) that survives restarts, compaction and idle periods.
- **Reachable from every surface.** TUI, CLI one-shots, ACP, gateway platforms
  and (later) voice all address the same presence — same memory scopes, same
  relation model, same persona.
- **Proactive within policy.** It can act unasked only through the daemon's
  trigger system, quiet-hours policy and granted permissions. Silence is the
  default; speaking/acting requires a granted reason.
- **Honest about liveness.** If the daemon or runtime is down, the session is
  *asleep*, not pretending to be awake. Status surfaces say which.

## 1. What exists today (repo-verified)

- Sessions are durable rows (`SessionId`, title provenance, journal) in
  `vikram-store`; they already survive restarts.
- `RuntimeHandle::spawn_agent`, hive sessions and tickets give multi-entity
  execution; the main agent's conversational session is the gap.
- Daemon: cron scheduler + postbox + notify + heartbeat, plus event triggers
  (`on <trigger>` schedules, EventBus, `daemon_events` dedup, quiet hours,
  `daemon_audit` trail — landed `196f731`).
- Memory: user:/project:/session:/hive: scopes, relation model (`relations`,
  `relation_facts`), consolidated recall into `retrieved_memory`.
- Permissions: deny-first tool permissions; `computer.*` helpers degrade
  honestly when host binaries are missing.

## 2. Design

### 2.1 The presence session

A designated `presence` session per user profile (config key, default on):

- Created lazily on first use; durable `SessionId` recorded in profile config.
- All surfaces resolve "talk to Vikram" to this session unless the operator
  explicitly opened another. CLI one-shots remain one-shots (no forced merge).
- Fork/compact operations on the presence session keep the same `SessionId`
  lineage — memory scope bindings (`user:`, `project:`) are inherited, matching
  the fork-scope fix already landed.

### 2.2 Liveness model

Three states, all store-visible:

| State | Meaning | Entry |
|---|---|---|
| `awake` | Runtime + daemon running; triggers armed | daemon start / `vikram daemon start` |
| `dozing` | Runtime up, triggers suppressed (quiet hours, idle timeout, operator `/sleep`) | policy or explicit command |
| `asleep` | Process down | exit/crash — detected on next boot via heartbeat staleness |

Transitions are audited (same ledger as job runs). `vikram status` (or TUI
status line) reports presence state honestly — never "online" when asleep.

### 2.3 Proactive surface

Proactivity flows through the daemon trigger system only:

```
trigger fires → quiet-hours check → permission check → job/turn runs
     → audit entry (fire, decision, outcome)
```

- Triggers: postbox messages, schedule, relation facts (e.g. "user's birthday
  recorded"), repeated-failure signals, operator-defined webhooks later.
- Every proactive action is a normal runtime turn in the presence session —
  same tools, same permissions, same journal. No privileged back channel.
- Quiet hours suppress non-urgent triggers; urgent class is an explicit,
  auditable escalation, not a default.

### 2.4 Context continuity

- Presence session uses the same compaction path as any long session;
  compaction markers + prunable-prefix journal rules already exist.
- Cross-session memory is *not* duplicated into the presence session — recall
  goes through the existing memory scopes so `user:` facts apply everywhere.
- Relation model supplies "who is X" continuity; the presence session is where
  those facts get updated (`relation_facts` writes on turn end, gated).

### 2.5 Isolation guarantees (unchanged)

- Presence session belongs to one user profile; entity isolation and
  hive-participant scoping rules apply exactly as in interactive sessions.
- Sub-agent/hive sessions never silently merge into presence; delegation is
  explicit (tickets/assignments).
- Voice input, when it lands, is just another transport into the same session
  — same permissions, same journal.

## 3. Phases

### Phase A — durable presence identity

- `presence` session config + lazy creation + surface resolution.
- **Accept:** restart Vikram, send a message via CLI and via TUI — both land
  in the same session; `vikram sessions` shows one presence session per
  profile; tests cover lazy-create, resolution and fork inheritance.

### Phase B — liveness states + audit

- awake/dozing/asleep transitions, `/sleep` + `/wake` commands, status
  surfacing, heartbeat-staleness detection.
- **Accept:** `vikram status` reports correct state across daemon
  start/stop; every transition writes an audit row; tests cover each
  transition and the stale-heartbeat path.

### Phase C — proactive turns via triggers

- Daemon triggers (postbox, schedule, relation-fact, failure-signal) run
  bounded turns in the presence session under quiet-hours + permission
  policy.
- **Accept:** a seeded trigger produces a journaled turn in the presence
  session; quiet hours suppress it; urgent class bypasses with audit;
  denied-permission trigger refuses explicitly.

### Phase D — voice as a transport

- `vikram-voice` session (per voice-plan.md) resolves to the presence
  session; barge-in and persona voice profiles.
- **Accept:** a spoken utterance and a typed message are indistinguishable
  in the journal except for a `transport: voice` marker.

## 4. Honest limits

- **Not literally always-on.** Process down = asleep. We do not claim
  24/7 availability; we claim fast, honest resume.
- **Proactivity is bounded.** Trigger volume is capped per window; a noisy
  trigger source cannot spam the presence session (rate limit + dedup).
- **One presence per profile.** Multi-user deployments get one presence
  session each; there is no shared "house JARVIS" crossing profile isolation.
- **Context rot is real.** A never-compacted presence session degrades;
  compaction cadence is part of the design, not an afterthought.
- **Cost.** Every proactive turn is a real model call. Budgets and quiet
  hours exist partly for this reason; we do not hide the cost.

## 5. Verification strategy

- Store-level tests: presence session creation/resolution, scope inheritance.
- Daemon tests: trigger → policy → turn → audit, quiet-hours matrix.
- Integration: scripted daemon + runtime, assert journal entries and audit
  rows; restart mid-test to prove durability.
- Manual acceptance per phase above; numbers (latency, trigger rates) recorded
  in test output, not asserted into existence.
