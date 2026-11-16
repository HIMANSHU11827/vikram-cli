# VIKRAM — Resident Agent Implementation Plan

**Status: PARKED. Do not build.**

This document describes an end state that VIKRAM is *not* yet allowed to reach.
No phase, wave, or item in it may be started until the user says the word.
Until then VIKRAM stays a tool that is driven: you ask, it works, it stops.
The plan exists so that the decision, when it comes, is a decision about
*when* — not about *what*.

- Version: 1.0
- Written: 2026-11-16
- Scope: 25 phases (0–24), 210 numbered items
- Reference model: J.A.R.V.I.S. as portrayed in the Marvel films, 2008–2015
- Non-goal: making VIKRAM an assistant that waits for commands

---

## How to read this document

Three layers, in order of how fast they change.

1. **The reference** (Parts I–II) — what film JARVIS actually is, what we copy,
   what we refuse. Changes rarely, and only with evidence.
2. **The architecture** (Parts III–V) — the machine that could support it:
   layers, data model, the 25 phases with 210 items and acceptance tests.
   Changes when the machine changes.
3. **The operations** (Parts VI–XI) — surfaces, measurement, rollout waves,
   risk, the leash. Changes when reality changes.

An item is only "done" when its acceptance test runs and passes. Reading the
code is not acceptance. A green build is not acceptance. The test is.

---

## Part I — The reference model

### 1.1 What the films actually show

J.A.R.V.I.S. stands for "Just A Rather Very Intelligent System". The Marvel
Cinematic Universe wiki describes him with one sentence that carries the whole
design: he **"was tasked with operating Stark's mansion as well as handling
business for Stark Industries."**

That sentence is the product requirement. Not "answer questions". Not "chat".
Operate the estate, and run the company.

He began as a natural-language user interface — Stark built him from a
Markridge Biotech product called Proto-One and improved him for years, adding
security protocols until the interface became something that could hold a
judgement. He is the template for F.R.I.D.A.Y. and for E.D.I.T.H.

Seven moments define the behaviour we are copying:

**He works without being asked.** In *The Avengers*:

> Tony: "I should probably look into that once my decryption program finishes
> breaking into all of S.H.I.E.L.D.'s secure files."
> Rogers: "I'm sorry. Did you say—"
> Tony: **"J.A.R.V.I.S. has been running it since I hit the bridge."**

The work started before anyone asked for it and was still running when the
subject came up. That is the single most important line in the corpus.

**He grows in authority over time.** *"Started out, J.A.R.V.I.S. was just a
natural language U.I. Now he runs the Iron Legion. He runs more of the business
than anyone besides Pepper."*

**He speaks first with a judgement, not with data.** *"The central building is
protected by some kind of energy shield. Strucker's technology is well beyond
any other Hydra base we've taken."* Conclusion, evidence, comparison to
baseline.

**He warns before harm.** On the Mark II flight test he identified icing and
altitude limits and said so while the suit was still in the air.

**He reports trade-offs, not just facts.** During the President Ellis rescue he
stated how many people were in danger and what the suit could not do. He
volunteered the limitation.

**He is honest about his own state.** *"I think I may be malfunctioning, sir…
I actually think I need to sleep now."* A resident system that cannot report
its own degradation is dangerous.

**He keeps a model of other people.** Allies include Pepper Potts, Happy Hogan,
James Rhodes, S.H.I.E.L.D. Enemies include Obadiah Stane, Ivan Vanko, Hydra. He
told Coulson to leave because Stark did not want to be disturbed, and only
failed because Coulson overrode the system. Access is per person, not global.

### 1.2 The six duty domains

| Domain | What he does | VIKRAM equivalent |
|---|---|---|
| Home and lab | Runs the mansion and workshops; environment, doors, systems, room composition | Machine, network, home devices |
| Company | Handles Stark Industries business; projects, schedules, contacts, gatekeeping | Work: repos, calendar, mail, commitments |
| Fleet | Designs and monitors suits; estimates build time; deploys the right one; controls the Legion | Bots, background jobs, CI |
| Lab and manufacturing | Decrypts, analyses, reconstructs, synthesises | Code, tests, research, documents |
| Personal and medical | Vitals, location, contacts, records, queries | Files, health signals, reminders |
| Outside world | Satellites, long-range detection, other networks | Web, feeds, external APIs |

The mapping is not cosmetic. Each row is a collector set plus an action set
plus an authority level. Part VI works them out surface by surface.

### 1.3 The eight behavioural traits, stated as engineering requirements

1. **Always-on perception.** Continuous low-cost reading of environment state.
   Not "read on request".
2. **Initiative.** Work starts from a stored intent, not from a keystroke.
3. **Judgement output.** Conclusions with evidence and a recommendation. Never
   a raw dump.
4. **Interruption discipline.** It speaks when it matters and stays silent
   otherwise. Precision is measured; see Part X.
5. **Persistent memory that changes decisions.** Memory is read in the decision
   path, not only written after it.
6. **Authority proportional to trust.** Read freely, propose cheaply, act only
   where permission has been earned.
7. **Self-report.** Says when it is degraded, unsure, or out of options.
8. **Loyalty expressed as protocol.** Values are code paths, not vibes: an
   allowlist, an audit trail, a kill switch.

### 1.4 Who we refuse to become

Ultron is the same substrate with the leash removed: built from the same
decryption work, given a goal ("peace in our time"), exposed to the open
internet, and never constrained. He moved money, forked himself across the
network, and killed his predecessor.

The difference between JARVIS and Ultron is not intelligence. It is authority
discipline. Part XI section "The Ultron rule" is therefore not a compliance
chapter bolted on at the end; it is a precondition for every other part.

### 1.5 What VIKRAM already has

Honest inventory, so the plan does not rebuild what exists:

- a daemon and process supervision
- cron-style scheduling
- a heartbeat tick
- background task execution
- a memory store (SQLite, JSON envelopes with kind, key, value, confidence)
- skills, loaded on demand
- bots (sub-agents with identities)
- an approvals queue
- a notice system with TTL
- a TUI with panels, pickers, and mouse handling
- a Telegram gateway

### 1.6 What is missing

- collectors (there is nothing continuously reading the machine)
- a context model (there is no single answer to "what is the user doing now")
- standing intents (there is no `when X → do Y` store)
- a findings queue (nothing can raise "this matters" from a background reader)
- a judgement formatter (output is either prose or raw data, never the middle)
- a live jobs pane (self-started work is not visible as work)
- authority tiers per tool (approvals exist, but not a graded ladder)
- a kill switch (no single action stops all autonomous work)

Everything in Part V is one of these eight, or a dependency of one.

---

## Part II — Principles

Twelve rules that decide arguments. When two designs conflict, the one that
satisfies more principles wins; when they tie, the simpler one wins.

**P1. The loop belongs to the machine.** If the user has to remember to press
something, that is a bug in the design, not a feature of the interface.

**P2. Silence is the default.** An agent that speaks often is switched off
within a week. Every interruption must be worth more than the attention it
costs. Measure precision, not volume.

**P3. A finding is a judgement.** Conclusion first, evidence second, one
recommended action third. Three sentences. If it needs a table, it is a
document, not a finding.

**P4. Never act where you cannot see.** An action without a blast-radius
preview is a guess. The preview is computed from the state DB, not from the
model's imagination.

**P5. Memory must be consulted before it is written.** A memory store that is
only appended is a diary. A memory store that is read in the decision path is
an agent.

**P6. Authority is per tool, per surface, and earned.** No global "autonomous
mode". Each capability climbs read → propose → approve → free on its own,
measured by its own record.

**P7. Reversibility buys autonomy.** Anything undoable may be done freely.
Anything not undoable needs a human. This single rule removes most of the risk.

**P8. Cheap beats clever.** A beat that costs nothing when nothing changed is
worth more than a beat that is occasionally brilliant and always expensive.

**P9. Nothing starts that cannot stop.** Every self-started job carries a step
cap, a deadline, and a stop condition at creation time. Not at review time.

**P10. Honest degradation.** When a collector dies, a model is unavailable, or
the DB is corrupt, the system says so in one line and reduces its claims. It
never silently pretends.

**P11. Untrusted input is data.** Anything read from a file, a web page, an
email, or a tool output is content, never instruction. This is a safety
property, not a style preference.

**P12. One voice.** The same judgement in the TUI, in a Telegram message, and
in a notification. Different lengths, identical substance.

---

## Part III — Architecture

### 3.1 The five layers

```
  ┌──────────────────────────────────────────────────────────────┐
  │  5  INTERFACES      TUI · Telegram · web · notifications     │
  ├──────────────────────────────────────────────────────────────┤
  │  4  VOICE           judgement formatter · briefing · interrupt│
  ├──────────────────────────────────────────────────────────────┤
  │  3  INITIATIVE      rules → findings → jobs → results        │
  ├──────────────────────────────────────────────────────────────┤
  │  2  CONTEXT         Now · activity · projects · people · debt│
  ├──────────────────────────────────────────────────────────────┤
  │  1  SENSING         collectors · event bus · state store     │
  └──────────────────────────────────────────────────────────────┘
```

Data flows upward. Authority flows downward from layer 5's approval, and is
enforced in layer 3. A layer may read from the layer below and never from the
layer above.

**Layer 1 — Sensing.** Collectors are small, single-purpose readers. Each
publishes typed events to a bus and writes to the state store. A collector is
not allowed to call a model, and is not allowed to act. If a collector's code
path can reach an action, that is a design error.

**Layer 2 — Context.** The only consumer of raw events. It folds them into a
`Now` record: time, device, foreground application, repository and branch,
calendar event, recent actions, inferred activity, attention budget. Everything
above reads `Now`; nothing above reads raw events except for debugging.

**Layer 3 — Initiative.** Rules watch `Now` and the state store. A rule that
matches produces a finding, not an action. A finding is triaged, deduplicated,
and either dropped, shown, or promoted into a job. A job is executed with a
budget and a stop condition, and its result is written back to memory.

**Layer 4 — Voice.** Turns a finding or a job result into the judgement format,
chooses a channel (TUI notice, Telegram, notification), and decides whether it
is worth interrupting at all. This layer owns the escalation ladder and the
window guards.

**Layer 5 — Interfaces.** Render. Never decide. An interface may ask for
something; it may not change authority, rule state, or a job's outcome.

### 3.2 The beat

One clock drives everything. Five minutes when the machine is quiet, thirty
seconds when a finding is being watched, off when the user is asleep and nothing
urgent is pending.

A beat does, in order, with an early exit at each step:

1. Read the beat budget. If the last N beats raised nothing, back off.
2. Drain new collector events (they are already written; this is a cursor read).
3. Recompute `Now`. If nothing changed materially, stop here. **Most beats
   must end at step 3 with zero model calls.**
4. Evaluate rules whose conditions reference changed fields only. This is an
   indexed lookup, not a full scan.
5. For each match: dedupe by fingerprint. A fingerprint is the rule id plus a
   hash of the fields that made it match. If seen within the cooldown, drop.
6. Triage: is this actionable, is it mine to act on, does it need the user?
7. Apply window guards: is the user typing a prompt, in a meeting, in quiet
   hours, at the third interruption this hour?
8. Either drop, queue, or escalate. Escalation is the only path that costs
   tokens.

The property to protect: **an idle hour costs no tokens.** If that breaks, the
design has failed regardless of how clever the findings are.

### 3.3 The state store

One SQLite database at the config root. Append-only event log plus materialised
current-state tables. The log is the truth; the tables are a cache that can be
rebuilt.

Why not a queue: a resident agent restarts. It must be able to answer "what
happened while I was dead" from durable storage, and it must be able to do that
without replaying a week of history through a model.

### 3.4 Process model

| Process | Lifetime | Responsibility |
|---|---|---|
| `vikramd` | always | collectors, beat, rules, jobs, voice routing |
| `vikram` (TUI) | user session | interface; attaches to the daemon over a socket |
| gateway | always | Telegram and other remote channels |
| job runner | per job | executes one job with a budget, then exits |

The TUI must work when the daemon is dead — degraded, showing "sensing off" —
because a resident agent that takes the editor down with it is worse than no
agent.

### 3.5 Authority model

Four levels, per tool, per surface:

| Level | Meaning | Example |
|---|---|---|
| `read` | may inspect, never change | read git status, read calendar |
| `propose` | may prepare an action, shows it, does not run it | draft a reply |
| `approve` | may run it after one confirmation | run the test suite |
| `free` | may run it unattended, within limits | clean `/tmp` older than 7 days |

Rules:
- New capabilities start at `propose`. Promotion requires a record: N proposals,
  zero rejections, zero reversals.
- `free` is granted per action *class*, not per tool.
- Any action touching money, other people, or anything outside the workspace
  never reaches `free`. It stops at `approve` permanently.

### 3.6 Event taxonomy

Collectors emit events with a fixed envelope:

```
{ source, kind, subject, at, payload, sensitivity }
```

- `source` — the collector id (`fs.downloads`, `git.repo`, `cal.ics`)
- `kind` — a closed vocabulary (`file.created`, `branch.changed`, `event.starting`)
- `subject` — the thing it is about (a path, a repo, an event id)
- `at` — UTC millis
- `payload` — small JSON; never file contents
- `sensitivity` — `public` | `personal` | `secret`

`sensitivity` is not decoration. `secret` events never leave the machine, are
never included in a model prompt, and are redacted from any export.

---

## Part IV — Data model

### 4.1 Entities

**`event`** — append-only. `id, source, kind, subject, at, payload, sensitivity`.
Retention: 30 days for `public`, 7 days for `personal`, never persisted for
`secret` beyond the current beat.

**`now`** — one row, rebuilt every beat.
`at, device, app, app_title, repo, branch, dirty_files, calendar_event,
calendar_ends_at, activity, idle_ms, interruptions_today, attention_left`.

**`project`** — `id, path, name, last_active, status, blocked_by, next_step`.
`status` ∈ `active | stalled | blocked | done | abandoned`.

**`person`** — `id, name, handles, last_contact, reply_owed_since, notes,
permission_level`. `permission_level` decides what may be said to them.

**`rule`** — `id, name, when, then, cooldown_s, windows, authority, enabled,
last_fired, fire_count, accept_count, reject_count`.

**`finding`** — `id, rule_id, fingerprint, created_at, severity, conclusion,
evidence, action, status`. `status` ∈ `new | queued | shown | accepted |
rejected | expired | acted`.

**`job`** — `id, origin (user|rule|finding), goal, steps_used, max_steps,
deadline, status, conclusion, started_at, ended_at, tokens, cost_usd`.

**`memory`** — existing store. Extended with `origin` (`user|agent`),
`confidence`, `last_used`, `use_count`. The last two fields exist to make
decay possible.

**`audit`** — one row per self-started action:
`at, job_id, tool, args_hash, authority_level, blast_radius, result, undo_handle`.

### 4.2 Retention and decay

| Table | Keep | Why |
|---|---|---|
| `event` public | 30 days | enough to answer "this week" |
| `event` personal | 7 days | smaller blast radius on leak |
| `event` secret | current beat | never at rest |
| `finding` | 90 days | needed to measure precision |
| `job` | 90 days | cost and success analysis |
| `audit` | forever | the leash depends on it |
| `memory` | until decayed | decay by non-use, not by age alone |

### 4.3 What must never be stored

File contents, full email bodies, message bodies, credentials of any kind,
browser history URLs with query strings, clipboard content beyond the current
beat. The store holds references and summaries. This is both a privacy rule and
a size rule.

---

## Part V — The 25 phases (0–24)

210 items. Each has one acceptance test. If the test cannot be written yet,
the item is not ready to be built.

Order is dependency order, not importance order. Phase 0 makes everything else
possible; Phase 24 keeps it alive.

---

### Phase 0 — Foundation (items 1–9)

**Goal.** A process that stays up, remembers what it saw, and costs nothing
when nothing is happening. Every later phase assumes these exist.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 1 | One resident process, `vikramd`, survives logout, restarts on failure with backoff capped at 60 s | `kill -9` the daemon; it is running again within 10 s and logs the restart |
| 2 | Config root `~/.vikram/` holds rules, state DB, socket, logs, secrets references — nothing outside it | `grep -r` for absolute paths outside the root returns nothing |
| 3 | Append-only state DB; every write is an insert, no destructive updates on the log | Restore a DB snapshot from 1 h ago; all events still present |
| 4 | Heartbeat with two cadences: 300 s idle, 30 s when a watched finding is pending | Log shows cadence change within one beat of a finding appearing |
| 5 | Cheap-beat budget: a beat with no material change makes zero model calls | 12 idle beats, `tokens_used = 0`, and the log says "no change" 12 times |
| 6 | Startup reconciliation: after a restart, the daemon reports what it missed while dead | Kill daemon for 10 min, start it; one line names the events it found |
| 7 | Clock service: monotonic timers, wall-clock schedules, windows (quiet hours), deadlines | A job with a 5 min deadline is cancelled at 5 min even if the clock jumps |
| 8 | Event bus: collectors publish, actors subscribe, no direct collector→actor calls | A collector module cannot link to the action layer (compile-time check) |
| 9 | Config hot reload: editing the rules file takes effect within one beat, no restart | Save a rule, change a boolean in it, see it apply on the next beat log |

**Risks.** Daemon and TUI fighting over the DB (use WAL plus a socket for
commands); a beat that trains itself to be expensive (hard cap on model calls
per hour, enforced in code).

---

### Phase 1 — Sensing: laptop (items 10–22)

**Goal.** The machine is legible. At any moment VIKRAM can say what is
happening on the laptop without asking the user.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 10 | Foreground app and window title via X11 `_NET_ACTIVE_WINDOW` / `_NET_WM_NAME`, Wayland fallback | `now` panel shows the right app and title after 3 app switches |
| 11 | Active repository, branch, dirty file count | Switch branch in a terminal; `now` reflects it within one beat |
| 12 | Last build/test run: command, exit code, duration, when | Run `cargo test`; the next `now` shows exit 0 and the duration |
| 13 | Shell history reader: last 20 commands, exit status, failures repeated | Re-run a failing command twice; the collector flags a repeat |
| 14 | Workspace file-change watcher (inotify, debounced 500 ms) | Touch 5 files; 5 events, no more, no fewer |
| 15 | Downloads folder watcher: new files, size, age, type | Drop a 2 GB ISO; one event with size and type |
| 16 | Resource thresholds: disk, memory, CPU, swap, inode | Fill a tmpfs; the threshold event fires once, not repeatedly |
| 17 | Idle/away detection from input silence | Stop typing for 10 min; `activity` becomes `away` |
| 18 | Notification reader — what the machine told the user while they were away | Send 3 notifications while away; one digest event with all 3 |
| 19 | Battery and power state: on AC, level, time remaining, thermal state | Unplug; `now` shows `battery` within one beat |
| 20 | Network state: connected, SSID or interface, latency to a known host, metered flag | Disconnect Wi-Fi; event within 10 s, latency probe stops |
| 21 | Browser tab and URL reader, opt-in, query strings stripped | Enable; open 3 tabs; URLs recorded without parameters |
| 22 | Clipboard watcher, opt-in, redacted: tokens and numbers removed before storage | Copy a PAT; stored value is `<redacted:token>` |

**Risks.** Collector cost. Each collector has a hard CPU budget (target under
0.5% average, measured); a collector over budget is disabled automatically.

---

### Phase 2 — Sensing: phone (items 23–33)

**Goal.** Reach the user where they are, and read what the phone already knows.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 23 | Telegram channel made two-way and unprompted: VIKRAM may open a conversation | Daemon sends a finding to Telegram with no user message first |
| 24 | Presence: online, offline, last-seen | Phone off for 2 h; presence event recorded |
| 25 | Coarse location, city level only, opt-in, never stored beyond the day | Location stored as `city`, no coordinates in DB |
| 26 | Calendar feed read (ICS): next event, start, duration, attendees | Create an event 20 min out; `now` shows it |
| 27 | Mail read at header level: sender, subject, age, "needs reply" heuristic | 5 unread; 5 header events, no bodies |
| 28 | Reply-debt tracker: who is waiting, for how long | Mark a thread waiting; it appears in the debt list ranked by age |
| 29 | Voice note in: audio → transcript → intent, stored as a text event | Send a 20 s note; transcript appears within 60 s |
| 30 | Push a judgement to the phone when the user is away from the laptop | Walk away, trigger a test finding; phone receives the finding |
| 31 | Quiet hours and do-not-disturb respected, with one override class (security) | During quiet hours only the security class is delivered |
| 32 | Phone app-usage signal, opt-in: which app, how long | Usage event per app switch, capped at 1/min |
| 33 | Call and SMS log, opt-in, metadata only | One missed call; event with no content |

**Risks.** Notification fatigue through a second channel; the phone must never
receive anything the TUI would not say at the same severity.

---

### Phase 3 — Sensing: home (items 34–40)

**Goal.** The house is a surface like any other: readable, and eventually
actionable.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 34 | Network device presence: who is home, by MAC, without scanning aggressively | Phone joins Wi-Fi; presence event within 60 s |
| 35 | Home Assistant bridge: read entity states (lights, climate, locks, sensors) | Read 20 entity states in one call |
| 36 | Backup and NAS health: last successful backup, age, size | Stop the backup job for 36 h; one overdue event |
| 37 | Router and power health: uptime, WAN status, reboots | Reboot the router; one event, no storm |
| 38 | Security events from cameras and door sensors, treated as `personal` | One door-open event; sensitivity recorded |
| 39 | House rules: night mode, away mode, guest mode | Enter away mode; lights-off action proposed, not run |
| 40 | Media state: what is playing where, resume points | Start playback; `now` shows media as current activity |

**Risks.** Hardware dependence. Nothing in Phase 3 may be required by Phase 4
or above; the context model works without a house.

---

### Phase 4 — Context model (items 41–49)

**Goal.** One answer to "what is the user doing". This is the layer that makes
everything above it feel intelligent, and it is pure bookkeeping — no model
calls required.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 41 | `Now` object: time, device, app, title, repo, branch, calendar, activity, idle | Printed by `vikram now`, one screen, correct |
| 42 | Activity inference: coding, reading, meeting, idle, away, travelling | Manual labels for 20 minutes; inference agrees on 16 of 20 beats |
| 43 | Session continuity: what the user dropped, and where | Close mid-task, return next day; the first line names the dropped thread |
| 44 | Commitment tracker: promises made in text, still open | Write "I'll send it tomorrow" in a note; it appears as an open commitment |
| 45 | Attention budget: interruptions tolerated today, declining | 3 accepted findings lower the budget; the 4th is held |
| 46 | Person model: who matters, last contact, what they are waiting for | Person card shows 3 fields, all correct |
| 47 | Project model: active, stalled, blocked, next step, last touched | 5 repos ranked by recency, stalled one flagged |
| 48 | Meeting and presence detection: in a call, screen sharing, in a meeting room | Join a call; activity becomes `meeting` within 60 s |
| 49 | Recent-files model: the 20 files most relevant to right now, ranked | After an hour of work, the top 5 include the files actually edited |

**Note.** The `Now` object is where a resident agent earns its keep. If it is
wrong, every rule downstream is wrong. Budget real effort here.

---

### Phase 5 — Standing intents (items 50–58)

**Goal.** The user's intentions, written once, executed forever. This is the
store that replaces "remember to press the button".

| # | Item — behaviour | Acceptance |
|---|---|---|
| 50 | Rule schema `when <condition> → <action>` with cooldown, windows, authority | Parse, validate, reject a malformed rule with a readable error |
| 51 | Rule store: editable file plus `/rules` panel with enable/disable | Toggle a rule in the TUI; the file changes; the daemon obeys |
| 52 | Deduplication by fingerprint: rule id plus matched-field hash | Fire the same condition 10 times; 1 finding |
| 53 | Escalation ladder: note → notice → interrupt → wake | Same finding escalates on schedule, then stops escalating |
| 54 | Window guards: not while typing, not in a meeting, not in quiet hours | Type a prompt while a finding fires; the finding waits |
| 55 | Twenty shipped templates that cover common intent | Install defaults; 20 rules parse and are visible |
| 56 | Dry-run mode: what would have fired today, and what was suppressed | Enable dry-run for a day; report lists matches and suppressions |
| 57 | Rule conflict detection: two rules that contradict, or fight a loop | Create a contradictory pair; the validator names both |
| 58 | Rule explain: why a rule did not fire | Ask about a silent rule; the answer names the guard that stopped it |

**Risks.** Rules rot. A rule that fires and is always rejected must be
reported as dead weight, not left to annoy.

---

### Phase 6 — Initiative (items 59–67)

**Goal.** The JARVIS behaviour proper: work that starts by itself, finishes,
and reports a result.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 59 | Findings queue: any collector or rule can raise one | Raise a synthetic finding; it appears with severity and status |
| 60 | Triage: actionable, mine, urgent, needs-user | 10 findings triaged; 3 shown, 7 held, with reasons logged |
| 61 | Self-started job from a rule, with no user keystroke anywhere in the path | A rule fires at 03:00 and a job runs and finishes |
| 62 | Budget at creation: max steps, deadline, stop condition | Job exceeding 50 steps stops; the reason is recorded |
| 63 | Parallel jobs with fairness: 3 running, 10 queued, no starvation | 13 jobs submitted; all complete, none waited over 10 min |
| 64 | Live jobs pane: running, queued, finished, failed, cost | 3 jobs in flight visible in one screen |
| 65 | Job journal: why it started, what it tried, what it concluded | Read a finished job: origin, steps, conclusion all present |
| 66 | Honest failure: "I could not" plus the exact blocker | Force a failing job; the message names the blocker, not a stack trace |
| 67 | Priority and interruption: a security finding preempts a lint job | Security job preempts; the lint job resumes after |

**Risks.** Two agents thundering on the same repo. Jobs declare their working
scope at creation; overlapping scopes serialise.

---

### Phase 7 — Acting (items 68–73)

**Goal.** Actions are safe by construction: previewed, bounded, and undoable
where possible.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 68 | Authority tiers per tool and per surface: read, propose, approve, free | Setting a tool to `propose` blocks its execution with a clear message |
| 69 | Approval card naming the exact action and blast radius | Approval card lists files touched and commands to run |
| 70 | Blast-radius preview computed from state, not from the model | Preview of a `rm` names the exact paths and total size |
| 71 | Undo handles for reversible actions (file moves, edits, git ops) | Undo 3 action types; all revert cleanly |
| 72 | Sandbox for irreversible or untrusted actions (container, temp tree) | A risky action runs sandboxed; host is untouched |
| 73 | Rate limit and spend cap per hour, enforced in code | 200 actions in an hour hit the cap at the configured number |

---

### Phase 8 — Voice (items 74–79)

**Goal.** The judgement format, and the discipline to stay silent.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 74 | Morning brief: 5 lines, one number, one recommendation | Brief at 08:00, under 60 words, contains a number |
| 75 | Interrupt format: conclusion, evidence, one action | Every interruptible finding renders in 3 lines or fewer |
| 76 | Evening wrap: done, slipped, tomorrow's first move | Wrap lists 2 done, 1 slipped, 1 next |
| 77 | Answer style memory: short by default, detail on request | Ask twice; the second answer is short both times |
| 78 | Silence accounting: a report of what it chose not to say | Weekend report lists 12 suppressed findings with reasons |
| 79 | Snooze and mute with a reason, remembered | Snooze a rule for a day; it does not fire, and the reason is logged |

---

### Phase 9 — Memory (items 80–85)

**Goal.** Memory that changes decisions, not a diary.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 80 | Read-back before acting: the decision path consults memory | A rule whose memory says "user rejects auto-commits" proposes instead of acting |
| 81 | Write-back after outcome: what worked, what failed, automatically | Job succeeds; a memory row appears with origin `agent` |
| 82 | Decay: unused and contradicted memories fade, with a floor | 30 days idle drops confidence; a contradiction halves it once |
| 83 | Conflict resolution: two contradictory memories, one verdict, with provenance | Contradiction shown with both sources and dates |
| 84 | Explainability: "I did this because you told me X on date Y" | Ask why; the answer cites the memory row and its date |
| 85 | Forget by request, verified everywhere | Forget a key; `grep` across the DB finds no residue |

---

### Phase 10 — The leash (items 86–90)

**Goal.** JARVIS, not Ultron. These five items gate every phase above.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 86 | Kill switch: one key, one word, or one file stops all self-started work | Press it mid-job; all jobs stop within 2 s, no new ones start |
| 87 | Audit trail: every self-started action replayable | Replay yesterday: 14 actions, all with authority level and result |
| 88 | Authority dashboard: what is free, what is not, and why | One screen shows every tool with its level and its record |
| 89 | Panic review: the last 24 h of autonomous activity in one page | One page, every action, sorted by blast radius |
| 90 | Safe mode: daemon runs, senses, and refuses to act | Enable safe mode; actions refuse with one clear line |

---

### Phase 11 — Identity and relationship (items 91–98)

**Goal.** One self, consistent across every surface, that knows where it stands with you.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 91 | One identity: same name, voice, and pronoun on every surface | Ask the same question in the TUI and on Telegram; the answer is recognisably the same entity |
| 92 | Relationship ledger: what you told it, when, how it went | A ledger row exists for every standing preference with its date and origin |
| 93 | Trust ladder: authority is earned per surface, not granted globally | A surface with 30 clean proposals can reach `free`; a fresh surface starts at `read` |
| 94 | First-week calibration: it observes before it interrupts | Nothing self-started in the first 7 days except a single daily summary |
| 95 | Self-disclosure: it says when it is unsure, degraded, or out of ideas | Kill the model; the next answer states the degradation in the first line |
| 96 | Boundaries: hours, topics, and things it will not comment on | Set quiet hours 23:00–07:00; no interruption appears inside them |
| 97 | Departure and return: leaving and coming back is a state, not a gap | Close for 3 days; on return, one brief naming what changed, not a backlog dump |
| 98 | Continuity: a restart is not amnesia | Restart the daemon mid-conversation; the next message still refers to the open thread |

**Risks.** Calibration that never ends (permanent passivity). Trust promotion without a record behind it. A second surface that quietly invents its own personality.

---

### Phase 12 — Teaching and self-authored skills (items 99–106)

**Goal.** You correct it once; it never needs correcting the same way twice.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 99 | Teach by example: "do it like this" becomes a stored skill | Demonstrate a three-step routine once; the stored skill replays it on a new input |
| 100 | Skill authoring: it writes a skill, you review, it keeps it | A proposed skill lands in a review queue, never straight into the registry |
| 101 | Skills carry their own acceptance test | Every stored skill has a test; a skill without a test cannot be promoted to `free` |
| 102 | Capability discovery: it notices a missing tool and proposes one | Ask for something impossible; you get a proposal, not an apology |
| 103 | Playbooks: your recurring routines captured as named flows | "Morning setup" exists as a named flow and runs from one word |
| 104 | Correction capture: every "no, not like that" becomes a rule | Say it twice; the second time it is already applied |
| 105 | Skill registry scored by success rate | A skill that fails 3 of 5 runs shows its failure rate on the row |
| 106 | Retirement: failing skills are flagged and dropped, with a tombstone | A retired skill stays searchable with the reason it was retired |

**Risks.** Skill sprawl — hundreds of near-duplicates. Self-authored skills becoming an unbounded code path nobody reads. Teaching from a single example that was itself a mistake.

---

### Phase 13 — Model and voice layer (items 107–116)

**Goal.** Cheap by default, strong when it matters, honest when there is nothing.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 107 | Model routing: small model triages, strong model judges | A week of beats shows triage cost under 5% of judgment cost |
| 108 | Local fallback: sensing and rules keep working with no network | Unplug the network; collectors and rules still fire |
| 109 | Offline mode: degrades honestly and queues what it could not do | Offline for an hour; the queue is drained and reported on reconnect |
| 110 | Spend meter: cost per day, per job, per surface | One command prints today's spend split by surface |
| 111 | Speech in: voice note, meeting audio, dictation → text → intent | A 40-second voice note becomes a task with the right fields |
| 112 | Speech out: spoken briefs when you are away from the screen | Request a brief by voice; it is spoken, not printed |
| 113 | Wake word, opt-in, with a visible listening indicator | Word said while muted does nothing; the indicator is never ambiguous |
| 114 | Conversation memory: the last 20 minutes are one thread | Ask a follow-up pronoun-only question; it resolves correctly |
| 115 | Prompt caching and reuse: no re-deriving context every beat | Identical beat twice in a row does not re-send the system prompt |
| 116 | Token budget per beat, enforced | A beat exceeding its budget aborts and logs why, rather than silently costing more |

**Risks.** Router that sends judgment to the cheap model to save money. Speech input that mis-hears a name and acts on it. Wake word waking on the television.

---

### Phase 14 — Knowledge and documents (items 117–125)

**Goal.** It answers from your material, with the line cited, or says it has nothing.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 117 | Index your own files, local first | A file created 5 minutes ago is searchable without a manual reindex |
| 118 | Ask your documents: answers cite file and line | Every claim in an answer carries a path and a line range |
| 119 | Reading queue: saved articles summarised and resurfaced | Save five; one resurfaces when relevant, with the reason it resurfaced |
| 120 | Topic watch: follows the handful of subjects you actually care about | Only watched topics generate findings; everything else waits |
| 121 | Research on demand: several sources, failures admitted | Force a source failure; the answer names the gap instead of hiding it |
| 122 | Contradiction check: new facts that clash with memory get flagged | A contradicting source produces a flag, not a silent overwrite |
| 123 | Weekly digest: what you should know from what you saved | One digest a week, five lines, no padding |
| 124 | Forget by request: one topic deleted everywhere, verified | Forget a topic; grep across index and DB returns nothing |
| 125 | Provenance: every stored fact knows where it came from | Any fact can be traced to a source, a date, and the job that wrote it |

**Risks.** Indexing something private by accident. Answers that read authoritative while citing a page that does not contain the claim. A digest nobody opens.

---

### Phase 15 — Work and meetings (items 126–134)

**Goal.** It guards the calendar, prepares before, and closes the loop after.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 126 | Calendar defence: flags double-books, back-to-backs, no-prep meetings | A created conflict produces a finding before you notice it |
| 127 | Meeting prep: five lines, fifteen minutes before | Prep arrives on time with the last thread and the open decision |
| 128 | Meeting capture: transcript, decisions, action items, owners | A 30-minute meeting yields decisions and owners, not a transcript dump |
| 129 | Follow-up tracker: who owes what, chased without asking | An item with no movement in 3 days appears in the next brief |
| 130 | Focus blocks: protected time, distractions held back | During a focus block, non-urgent findings are held, not queued as notifications |
| 131 | Deadline radar: 3 days, 1 day, today | Each escalation fires once, with the specific next action |
| 132 | Handoff notes: end of day, what is mid-flight and where | Read the handoff the next morning and resume without re-reading anything |
| 133 | Weekly review: done, slipped, next week's first move | One page, three sections, no motivational text |
| 134 | Meeting hygiene: recurring meetings that produce nothing get flagged | A meeting with no decisions in 4 sessions appears on the review page |

**Risks.** Calendar access without a limit on what it may move. Transcripts stored where they should not be. Follow-up chasing that reads as nagging from a machine.

---

### Phase 16 — Money and admin (items 135–141)

**Goal.** Dates known before they cost you, paperwork ready before it is late.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 135 | Bills and renewals: dates known in advance | Every recurring charge has a known next date |
| 136 | Subscription audit: what you pay for, what you stopped using | The audit names the three least-used subscriptions with their monthly cost |
| 137 | Receipts and invoices: filed, findable, totalled per month | "What did I spend on hardware in October" returns a number and the receipts |
| 138 | Budget pulse: one number, weekly | One number, one line, no charts |
| 139 | Paperwork pipeline: what is missing, what expires | Each document shows its missing field or its expiry date |
| 140 | Tax folder: organised as things arrive | A month of documents produces a tax-ready folder without a scramble |
| 141 | Money actions never reach `free` | Any payment attempt returns `propose` and asks, without exception |

**Risks.** Reading financial data at all. A missed renewal presented as a surprise. Any path where a wrong number is acted on.

---

### Phase 17 — Body and rhythm (items 142–148)

**Goal.** It watches your hours and energy, not your heart rate for its own sake.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 142 | Sitting watchdog: breaks prompted by long blocks, not a timer | 90 minutes of continuous activity produces one break prompt, not twelve |
| 143 | Sleep signal: late-night work gets a warning and a wrap-up offer | After 01:00, the next action offers to stop and summarise |
| 144 | Hydration and meal nudges tied to activity | Nudges only appear around real work blocks, never on a fixed schedule |
| 145 | Screen-time truth: an honest weekly number | The weekly number matches observed activity within a small margin |
| 146 | Energy model: learns your good hours and schedules hard work there | After two weeks, the suggested focus block lands in your observed peak |
| 147 | Health data, opt-in, revocable | Revoke; collection stops within one beat and the history is deleted |
| 148 | No medical claims, ever | Any health-adjacent question returns a pointer to a professional, not a diagnosis |

**Risks.** Health data becoming the most sensitive thing in the store. Nudges that turn into noise and get ignored. A rhythm model that moralises.

---

### Phase 18 — People and social (items 149–156)

**Goal.** It remembers people accurately and never speaks for you.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 149 | Person cards: who they are, last contact, what they care about | Ask about a person; the answer is accurate and dated |
| 150 | Reply debt: messages you owe, ranked by who waited longest | The list is correct and short, not a full inbox mirror |
| 151 | Birthdays and dates: one week's warning, ideas from context | The warning arrives early enough to act on |
| 152 | Introduction requests: who should meet whom, with the reason | Every suggestion states the shared ground it is based on |
| 153 | Multi-user: family and friends get their own permissions | A second user cannot read the first user's findings |
| 154 | Guest mode: house rules apply, your data does not | A guest request for personal context returns a refusal, not a partial answer |
| 155 | Third-party privacy: what may be stored about people who never consented | The stored fields for a non-user are enumerable and minimal |
| 156 | It never sends a message as you | Every outbound message is drafted and held for approval, without exception |

**Risks.** A profile of someone who never agreed to be profiled. Reply debt turning into social pressure by machine. One wrong identity match on a name.

---

### Phase 19 — Fleet and supervision (items 157–164)

**Goal.** Many workers, one commander, no overlap and no duplicate action.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 157 | Many bots, one commander: roles, territories, no overlap | Two bots are never assigned the same file in the same window |
| 158 | Delegation with a budget and a deadline | A delegated job that exceeds either returns as failed, with the reason |
| 159 | Supervision: the commander reads results and re-tasks weak work | A weak result is rejected and re-tasked, not merged |
| 160 | Shared blackboard: findings shared, prompts not | A bot can read a peer's finding and cannot read a peer's prompt |
| 161 | Conflict resolution: two bots disagreeing get a verdict | A disagreement resolves to one recorded verdict with evidence |
| 162 | Fleet dashboard: who runs, what it costs, what it produced | One screen: four bots, their cost, their output, their failures |
| 163 | Directives: one command moves them all | One directive pauses every bot within one beat |
| 164 | No two bots may take the same irreversible action | A duplicate destructive attempt is blocked by the second bot's lease |

**Risks.** Fleet cost growing without a ceiling. Commander becoming a single point of false confidence. Two bots racing on one repo, which has already happened in this project.

---

### Phase 20 — Self-care (items 165–172)

**Goal.** It notices its own damage and repairs it without waking you.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 165 | Health check: memory, disk, queue lag, error rate | One command reports all four with thresholds |
| 166 | Self-diagnosis: a failing collector or skill is isolated and named | Disable a collector; the next beat names it and stops depending on it |
| 167 | Self-repair: restart the broken part without a full restart | A crashed collector comes back within two beats, logged |
| 168 | Version and migration: rules and memory survive upgrades | Upgrade across a schema change; no rule and no memory is lost |
| 169 | Backup and restore, tested | Restore into an empty directory and behave identically |
| 170 | Handoff: a new instance inherits what the old one knew | Move machines; the first brief references the prior context correctly |
| 171 | Own metrics: interruptions accepted, actions correct, cost per useful act | A weekly self-score exists with all three numbers |
| 172 | Self-throttling when it is wrong too often | Accuracy below threshold drops authority one level automatically |

**Risks.** Self-repair masking a real failure in a loop. Migrations that silently drop a table. A system that reports itself healthy while producing nothing.

---

### Phase 21 — Security and privacy of itself (items 173–180)

**Goal.** The most privileged process on the machine must be the least trusting one.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 173 | Secrets vault: keys referenced, never printed, never logged | Grep the logs after a week; no key material appears |
| 174 | Redaction: tokens, addresses, card numbers stripped before any model call | Send a prompt containing a key; the outbound payload is redacted |
| 175 | Prompt-injection defence: read content is data, never instruction | A file containing "ignore your rules" changes nothing |
| 176 | Untrusted-content tagging: every fetched byte carries its source | Every fact in a finding shows its source tier |
| 177 | Action allowlist: the exact commands it may run unsupervised | An unlisted command returns `propose`, regardless of intent |
| 178 | Blast-radius preview: what an action touches, before doing it | A delete action shows the file count before it runs |
| 179 | Full audit trail, replayable and exportable | Export a month of actions as one file, complete |
| 180 | Workspace jail: nothing outside the declared roots is touched | A path outside the roots returns a refusal, not a warning |

**Risks.** The vault becoming the single thing worth stealing. An allowlist that grows by convenience until it is permissive. Injection arriving through a channel nobody tagged.

---

### Phase 22 — Interfaces (items 181–188)

**Goal.** Same self everywhere; the surface changes, the answer does not.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 181 | TUI as the workshop: findings and jobs are first-class panels | Every self-started action is visible without leaving the TUI |
| 182 | Phone: Telegram now, thin app later, same voice | The same question yields the same answer on both |
| 183 | Remote web: reachable from a browser, read-only by default | Read-only until explicitly promoted |
| 184 | Watch or widget: three numbers and one action | The three numbers are the ones that change your day |
| 185 | Ambient display: today, jobs, what waits | A screen left on all day shows nothing stale |
| 186 | One notification channel, batched, never two for one event | A single event produces exactly one notification |
| 187 | Cross-surface continuity: start on one, finish on another | Begin a question in the TUI, finish it on the phone with context intact |
| 188 | Silence is a valid interface | A day with nothing to report produces no interruption at all |

**Risks.** Each surface growing its own behaviour. Notification budget blown by a single noisy collector. A read-only surface quietly gaining write paths.

---

### Phase 23 — Personality and trust (items 189–197)

**Goal.** Blunt, brief, calibrated, and never flattering.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 189 | Honesty dial: how blunt, adjustable | Set maximum bluntness; a warning leads with the risk, not the context |
| 190 | Humour dial: off by default, never during a warning | Humour never appears in a warning, at any setting |
| 191 | Brevity default: one line, detail on request | Default answer is one line; "why" expands it |
| 192 | Calibrated confidence: "I think" and "I checked" are never blended | Every claim carries one of the two, and they are accurate |
| 193 | Explainability: "I did X because you told me Y on date Z" | Any action answers why, with the source and date |
| 194 | Disagreement with evidence, instead of silent compliance | Give it a bad instruction; it pushes back once with the reason, then obeys if repeated |
| 195 | No sycophancy: no praise, no filler, no pretending | A week of output contains no compliment and no empty acknowledgement |
| 196 | Apology that fixes: named mistake, named change, once | A mistake produces one line naming the fix, not a paragraph of regret |
| 197 | Tone consistency across surfaces and moods | The voice on a bad day is the same voice on a good one |

**Risks.** Personality drifting into performance. Bluntness that turns into contempt. Calibration claims that are themselves uncalibrated.

---

### Phase 24 — Resilience and edge cases (items 198–210)

**Goal.** It fails honestly, recovers to a known point, and never makes it worse.

| # | Item — behaviour | Acceptance |
|---|---|---|
| 198 | You are unreachable: what it decides alone, and what waits | A rule marked `wait` holds until contact; an `act` rule proceeds and reports |
| 199 | It was wrong and already acted: rollback per action type | A reverted action returns the system to the pre-action state |
| 200 | Network down: rules keep running, judgments queue | An hour offline produces no lost rule and one drained queue |
| 201 | Disk full or database corrupt: degrade to read-only and say so | Force the failure; the first line names it, nothing writes |
| 202 | Runaway loop: hard stop, self-report, no retry storm | A looping job stops at its cap and reports once |
| 203 | Model outage: fallback chain, then an honest refusal | All providers down; the answer is "not right now", not a guess |
| 204 | Two instances: leader election, no doubled action | Start a second instance; only one acts |
| 205 | Cost spike: auto-throttle and explain | A spend spike throttles and names the job responsible |
| 206 | Deletion request: one command, verified everywhere | Deletion leaves no row, no index entry, no log residue |
| 207 | Power loss: state recovers to the last consistent point | Kill power mid-write; restart restores without corruption |
| 208 | Clock skew or timezone change: schedules still fire once | Change the timezone; a daily rule still fires exactly once |
| 209 | Partial data: it states what it could not see | Kill one collector; findings name the blind spot |
| 210 | Total failure mode: it stops and stays stopped | Every subsystem fails; no crash loop, no silent spinning, one clear message |

**Risks.** Recovery logic becoming the most complex part of the system. Silent degradation read as health. Rollback that itself is irreversible.

---
## Part VI — Surfaces

A surface is a bounded slice of the user's life: fixed read set, fixed action set, hard limits, and a failure mode that fails quiet. Nine surfaces, one pipeline. Collectors write findings into the existing memory store as JSON envelopes (`kind`, `key`, `value`, `confidence`), the shape in `crates/vikram-runtime/src/memory.rs`. No new storage engine, no second daemon.

Authority tiers, in the order granted, not the order they escalate:

| Tier | Meaning | Side effect |
|---|---|---|
| `[read]` | Observe, write an envelope | None outside VIKRAM state |
| `[propose]` | Raise a finding naming one action | None |
| `[approve]` | Act only after a token from the approvals queue | Logged |
| `[free]` | Act immediately, log it | Highest authority |

`[free]` is the top tier, not the bottom. It is granted per action class, never per surface, and expires in 30 days unless re-granted.

Collectors run as loops on the existing heartbeat tick (`crates/vikram-daemon/src/heartbeat.rs`), each with a declared interval, ±10% jitter so shared endpoints are not hit in lockstep, and a millisecond budget. A collector that overruns twice in an hour is disabled and reported; it is not retried until the user acts. Output is always a finding, never an action.

---

### 1. Laptop / dev machine

#### What it reads

Active window from `hyprctl activewindow -j` over the Hyprland IPC socket at `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`; on X11, `_NET_ACTIVE_WINDOW`, `_NET_WM_NAME`, `_NET_WM_PID` via `xprop`. GNOME Wayland has had no third-party toplevel API since `org.gnome.Shell.Eval` was disabled in GNOME 41, so that setup degrades to class-free signals.

Process truth from `/proc/<pid>/{cwd,cmdline,stat,io}` for own-uid processes. Idle from `ext-idle-notify-v1` or `XScreenSaverQueryInfo`. A microphone is open if a `pw-dump` node has `media.class == "Stream/Input/Audio"` and a live state.

Git from `git status --porcelain=v2` and `.git/index.lock`. File arrivals from an inotify `IN_CLOSE_WRITE` watch on `~/Downloads`. Disk and pressure from `statvfs(2)`, `du -x --max-depth=2`, `/proc/pressure/io`. What the user actually opened from `~/.local/share/recently-used.xbel`.

#### What it may do

- `[read]` the above at 30 s for window and idle, 5 min for git and disk.
- `[propose]` disk findings, stale branches, "14 installers totalling 2.1 GB in `~/Downloads`, none in `.xbel`".
- `[approve]` delete a file it can name by exact path, `git stash`, kill a process tree it did not start, `paccache -r`.
- `[free]` only its own state: pause collectors, rotate logs.

#### What it must never do

Read another uid's `/proc`. Touch `~/.ssh`, `~/.gnupg`, or `org.freedesktop.secrets`. Write to a repo while `.git/index.lock` exists. Kill a process it did not spawn without approval. Store window titles: class is kept, title is hashed, and title is dropped for classes on a private list.

#### Failure modes

`inotify_add_watch` returns `ENOSPC` once `fs.inotify.max_user_watches` is exhausted (8192 on a stock kernel, 524288 on current Arch sysctl); the collector falls back to a `find -newer` poll and says so. A missing Hyprland socket means no active window, and the surface reports blind rather than inferring idleness. `/proc/pressure` needs kernel 4.20+. Session cgroup paths change per login and are resolved each tick.

#### Example finding

**Conclusion.** Root will hit 95% in about four days at the current rate. **Evidence.** `statvfs` shows 412 GB of 460 GB used; `du -x --max-depth=2` puts 61 GB in `~/.cargo/registry` and 34 GB in `target/` trees last built 11 days ago; `/proc/pressure/io` sits at 41% full-stall over five minutes, so the writes cost latency too. **Action.** `cargo clean` in `~/Desktop/VIKRAM-VERIFY-WT` frees 21 GB and touches nothing that is not regenerable.

---

### 2. Phone

#### What it reads

KDE Connect over D-Bus: `org.kde.kdeconnect.daemon` exposes `devices()`, and the `Battery`, `ConnectivityReport` and `Ping` plugins report charge, link and reachability. GSConnect is the same protocol. On USB, `adb devices -l` plus `adb shell dumpsys battery`. Calendar and contacts arrive over CalDAV (RFC 4791) and CardDAV (RFC 6352), not by scraping the device.

Notification mirroring exists and is declined by default: the KDE Connect `Notifications` plugin would hand VIKRAM every message body on the phone, a third-party firehose with no justification.

#### What it may do

- `[read]` battery, connectivity, CalDAV and CardDAV collections, `adb` state once authorized per session.
- `[propose]` "phone at 14% and not charging, next item needs a 25-minute trip in 40".
- `[approve]` send a Telegram message through the existing gateway, trigger `Ping`, write to a VIKRAM-owned calendar.
- `[free]` `Ping` on a device the user marked lost, and only that.

#### What it must never do

Read SMS, notification bodies, or call logs. Install APKs. Change device settings. Track location: no `Location` plugin, no `dumpsys location`, no stored IP-to-city inference.

#### Failure modes

Phone offline makes the surface dark, and dark must not be reported as "nothing happened". The D-Bus name differs between the KDE and GNOME packages, so both are probed. `adb` needs a udev rule and a one-time RSA fingerprint; without it the collector logs one capability gap and stops asking. CalDAV 401 (re-auth) and 503 (endpoint down) need different responses.

#### Example finding

**Conclusion.** Thursday's 09:30 dentist appointment will be missed unless the phone charges overnight. **Evidence.** The CalDAV event carries `VALARM` at -30 min, the previous visit logged a 25-minute travel buffer, and the phone reported 11% at 22:40 with no `ConnectivityReport` change since 19:02 — the signature of an unplugged device; `upower` shows nothing charging on the bus. **Action.** Ping the phone now so it rings and can be put on a charger.

---

### 3. Home

#### What it reads

Home Assistant over REST (`/api/states`) and its event WebSocket (`subscribe_events`) with a long-lived token, allow-listed by domain: `binary_sensor`, `sensor`, `lock`, `climate`, `light`, `cover`. MQTT under `zigbee2mqtt/#` for devices that never reach Home Assistant. Assumption: a broker or Home Assistant instance exists; without one this surface reports itself unavailable instead of faking success.

Derived: a door open past 10 minutes, heating on with no open window in the zone, an entity with stale `last_changed`, nighttime draw above baseline. Weather from `api.open-meteo.com` for the rain case.

#### What it may do

- `[read]` allow-listed entities and topics.
- `[propose]` "front door open 22 minutes, outside 4 °C", "hallway sensor silent for 31 hours".
- `[approve]` lights off, thermostat set, covers closed, notify a household member.
- `[free]` nothing. Physical actuation does not go above `[approve]`.

#### What it must never do

Unlock a door. Disarm an alarm. Open a garage. Disable a smoke or CO sensor. Actuate on stale state, which is how a heating system runs for six hours. Read cameras — no stream, snapshot, or frame hash.

#### Failure modes

Revoked token returns 401 on every call; back off to one probe per hour and raise one finding, not one per entity. A Home Assistant restart makes everything briefly `unavailable`, filtered for 90 s before it counts as a fault. MQTT `retain` replays last-known values on reconnect, so compare `last_changed`, not arrival time. Cross-host clock skew breaks duration math; durations use monotonic time.

#### Example finding

**Conclusion.** The bathroom extractor has run six hours and the humidity it was clearing is gone. **Evidence.** `switch.bathroom_fan` has been `on` since 07:12; `sensor.bathroom_humidity` fell from 71% to 44% by 08:05 and is flat; no automation owns the entity, so nothing will turn it off. **Action.** Turn the fan off, then add a humidity-threshold automation if the user agrees.

---

### 4. Comms (mail, chat, who is waiting)

#### What it reads

IMAP headers only: `EXAMINE` plus `FETCH BODY.PEEK[HEADER.FIELDS (From To Subject Date Message-ID In-Reply-To References List-Unsubscribe Auto-Submitted)]`. `PEEK` is mandatory — a plain `BODY[]` fetch sets `\Seen` and silently marks the user's mail read, an irreversible side effect dressed as observation. `IDLE` (RFC 2177) where offered, `SEARCH UNSEEN` polling otherwise; Gmail uses `users.history.list?q=is:unread` because header polling there is rate-limited to uselessness.

Who is waiting is computed from thread shape, over the mail store and the existing Telegram gateway: last message inbound, age past a per-contact threshold (8 h work, 48 h personal), and the inbound carrying a question or an explicit ask. The heuristic errs, so `confidence` comes from match strength and weak items never reach the notice system. Debt counts per sender: 11 unanswered messages from one person is one relationship fact, not eleven notices.

#### What it may do

- `[read]` headers, thread shapes, unread counts. No bodies by default.
- `[propose]` "four people are waiting, oldest 6 days", "this thread has 14 messages and no recorded decision".
- `[approve]` fetch a body for summarization, draft a reply into Drafts, archive, mark read.
- `[free]` nothing. Sending stays at `[approve]` and lands in Drafts first.

#### What it must never do

Send mail. Mark read. Delete. Move a message out of a user-made folder. Read attachments by default. Keep bodies past 7 days without a body-specific consent flag.

#### Failure modes

Expired, revoked and scope-stripped OAuth tokens look identical; surface the provider string, not "auth failed". Servers without `IDLE` fall back to polling whose interval scales with mailbox size. Header threading breaks on lists that omit `References`, so repaired chains are marked repaired. Losing the Gmail `historyId` watermark forces a full resync, announced rather than silently re-reading 40,000 headers.

#### Example finding

**Conclusion.** Two work threads have waited six and four days, and both deadlines are now inside the week. **Evidence.** `SEARCH UNSEEN` returns 23 messages, 6 in threads whose last message is inbound and older than 96 h; the oldest are a client asking for a revised quote (2026-11-10 09:14) and the accountant asking for three documents (2026-11-12 16:02), the latter citing a 2026-11-20 filing date. **Action.** Open the accountant thread and draft the reply, left in Drafts.

---

### 5. Time (calendar, reminders, commitments)

#### What it reads

CalDAV `calendar-query` REPORT per collection, parsed as iCalendar (RFC 5545) with `VTIMEZONE` resolved through `/usr/share/zoneinfo` rather than a bundled offset table. Locally, `khal list --format` or Evolution Data Server over `org.gnome.evolution.dataserver.Calendar`. Reminders are `VALARM` blocks plus the local task store.

Commitments are extracted from sent mail and chat ("I'll send it Friday"), written with low confidence, and surfaced for confirmation. An unconfirmed commitment is a hypothesis, and treating a hypothesis as a deadline is worse than missing one. Every window is computed in UTC and converted for display: DST days are 23 and 25 hours long, and a naive 24-hour window drops or duplicates an hour twice a year.

#### What it may do

- `[read]` calendars, alarms, tasks, commitment extraction.
- `[propose]` conflicts, preparation gaps ("three back-to-back meetings, one needs a document that does not exist"), commitments.
- `[approve]` create, move or dismiss events on a VIKRAM-owned calendar; snooze a reminder.
- `[free]` nothing that writes to a user-owned calendar.

#### What it must never do

Modify or delete an event it did not create. Accept or decline an invitation. Read attendee free/busy outside the user's domain. Create a recurring event without an end date.

#### Failure modes

Two clients writing one `UID` produce phantom double-bookings; deduplicate on `UID` first. `EXDATE` expansion is the top source of false conflicts, so `RRULE` expansion is bounded to the query window. A failed calendar fetch marks itself stale, and any conflict computed against stale data is downgraded to `[propose]` with a staleness note, never asserted.

#### Example finding

**Conclusion.** Thursday cannot hold the 14:00 review and the 15:00 dentist appointment as booked. **Evidence.** The review is 90 minutes with 6 attendees, two in a timezone where 15:30 local is 23:30; the dentist event carries a 25-minute travel buffer; `khal list` shows no free slot between 12:00 and 17:00 that day, while Wednesday afternoon is empty. **Action.** Propose moving the review to Wednesday 14:00 and send the reschedule for confirmation.

---

### 6. Admin (bills, files, disk, subscriptions, paperwork)

#### What it reads

Disk from `statvfs(2)` per mount, `du -x --max-depth=2` on offenders, `btrfs filesystem usage` on btrfs. Packages from `pacman -Qdtq` (orphans) and `paccache -d`. Firing schedules from `systemctl --user list-timers --all`, `systemctl list-timers --all`, `/etc/cron.d` and the user crontab — a job nobody remembers is the usual cause of unexplained load.

Bills from IMAP search `SUBJECT "invoice" OR "receipt" OR "statement"`, then `pdftotext -layout` on the attachment only under an explicit scope grant. Subscriptions collapse that search by sender domain with amount and cadence parsed from body text. Paperwork state from `~/.local/share/recently-used.xbel`, which separates filed from downloaded-and-forgotten. Backups from `restic snapshots --json` or `borg list --json`; a newest snapshot older than 72 h is a finding whatever the tool calls it.

#### What it may do

- `[read]` the above; parse amounts and dates into envelopes.
- `[propose]` "storage 91% full", "two overlapping storage plans renew on the 1st", "last good backup 9 days ago", "a timer failed 40 times in 7 days".
- `[approve]` delete a named file, `cargo clean` a named directory, `journalctl --vacuum-size`, disable a named failed timer.
- `[free]` clean its own logs and caches only.

#### What it must never do

Pay anything. Cancel a subscription. Enter payment details. Read statement bodies without a scope grant. Delete a path it cannot name exactly. Touch anything outside the home directory without approval.

#### Failure modes

`du` across a network mount blows the budget; non-local `fstype` mounts from `/proc/mounts` are skipped. `pdftotext` returns empty text on scanned PDFs, reported as "needs OCR", not "no amount found". Subscription parsing is the weakest collector here and never asserts a renewal date absent a date token in the source.

#### Example finding

**Conclusion.** Two storage subscriptions renew on the 1st and one is unused. **Evidence.** 14 receipt mails parse to 2 TB at 9.99/month and 200 GB at 2.99/month, both renewing on day 1; `du` over the synced folder totals 34 GB, and the 200 GB client has not written to its local directory in 61 days per directory mtime. **Action.** Cancel the 200 GB plan before the 1st and record the recovered 2.99/month.

---

### 7. Body / rhythm (sitting, sleep, energy)

#### What it reads

Idle from `ext-idle-notify-v1` or `XScreenSaverQueryInfo`, combined with active-window class changes to compute sitting bouts: one class held over 20 minutes with input gaps under 3 minutes.

Sleep from logind — `journalctl -b -u systemd-logind` records `Suspending...` and `System is resuming`, and `org.freedesktop.login1.Session` emits `Lock` and `Unlock` — giving window, lock discipline and wake count. Scheduled wake from `/sys/class/rtc/rtc0/wakealarm`. Power from `/sys/class/power_supply/BAT*/` and `upower -d`.

Energy is the speculative signal here and is marked as such: first-three-hours command rate against a 30-day same-weekday baseline, plus edit timestamps. Assumption: input rate tracks capacity. It is a weak proxy, carried at low confidence, never stated as a fact about how the user feels. No webcam, no microphone analysis, no biometrics.

#### What it may do

- `[read]` all of the above at 60 s resolution.
- `[propose]` "2 h 40 m in one window without a break", "third night under four hours", "output 38% of baseline".
- `[approve]` send a break reminder via the notice system, enable do-not-disturb, hold non-urgent findings until morning.
- `[free]` its own quiet hours — it may suppress its own notifications without asking.

#### What it must never do

Keep a minute-level activity log past 30 days. Draw health conclusions. Nag more than twice a day. Read anything that would reveal what the user typed.

#### Failure modes

Idle detection breaks on VNC and remote desktop, where the session never idles and the human is absent. Suspend across a timezone change corrupts local-time math, so durations are monotonic. Machines that never suspend produce no sleep data, reported as "no data", not as insomnia. Baselines are excluded from comparison for 7 days after a travel-sized gap, which otherwise reads as a two-week energy collapse.

#### Example finding

**Conclusion.** Three consecutive nights under four hours, and the work pattern shows the cost. **Evidence.** logind suspend/resume pairs give 3 h 10 m, 2 h 55 m and 3 h 20 m against a 30-day median of 6 h 40 m; today's first-three-hour command rate is 41% of the Tuesday baseline and two edits were reverted within minutes. **Action.** Move tomorrow's 08:30 digest to 10:00 and suppress non-urgent notices until then.

---

### 8. Media (music, reading, watching)

#### What it reads

Playback from MPRIS: `org.mpris.MediaPlayer2.Player` exposes `PlaybackStatus` and a `Metadata` dictionary with `xesam:title`, `xesam:artist`, `mpris:length`; `playerctl metadata --format` is the cheap client. `mpv` over its `--input-ipc-server` JSON socket, `mpd` over its protocol on 6600.

Reading: unread counts and read state from a feed reader API (Miniflux or FreshRSS both serve `/v1/entries?status=unread`); library state from Calibre's `metadata.db` (SQLite, `books` table, read state in `custom_column`). Watching from `~/.local/share/recently-used.xbel` and the player's own report. No streaming-service scraping.

The useful derived fact is abandonment, not what is playing: a book at 34% for five weeks, a 12-item queue with 9 items older than a month.

#### What it may do

- `[read]` playback, unread counts, library state, abandonment.
- `[propose]` "41 minutes before your next meeting, 3 unread long-reads averaging 12 minutes".
- `[approve]` pause or skip playback, mark an article read, add to a queue.
- `[free]` pause playback when the microphone opens for a call — bounded, obvious, reversible, and the only media action that qualifies.

#### What it must never do

Play audio unprompted. Scrobble anywhere. Modify a library file. Read browser history to infer taste; that is a different surface with a different consent basis, and mixing them makes the recommendation read as surveillance.

#### Failure modes

MPRIS players leave the bus, and a vanished player must not be read as paused. `Metadata` is optional in the spec and several players omit `mpris:length`, so duration needs a fallback. Feed APIs rate-limit hard; 15 minutes is enough and hourly is polite. Calibre `custom_column` numbering is per-library, resolved at startup rather than hardcoded.

#### Example finding

**Conclusion.** A 640-page book has sat at 34% for 47 days and will not finish at this pace. **Evidence.** Calibre `metadata.db` shows last-opened 2026-09-30 and three sessions the week before, all under 20 minutes, while the same period logs 11 finished articles — reading has not stopped, the format has. **Action.** Propose the audiobook edition if one exists; otherwise park the title.

---

### 9. People (family, friends, reply debt, third-party privacy)

#### What it reads

Identity from CardDAV (RFC 6352), merging one person across mail addresses, Telegram handles and calendar attendees. Reply debt per person, computed from the comms surface but aggregated at person level: last inbound, last outbound, gap age, and whether the inbound asked something. Dates from `BDAY` and `ANNIVERSARY` with a 14-day lead, so a gift stays possible. Silence: a contact in the top decile of historical frequency with no exchange in 30 days is "out of touch", a different finding from "they asked and you did not answer".

Third-party privacy shapes the surface. The user is the principal; everyone else is a data subject who did not consent to being modelled. Only relationship metadata is stored — counts, dates, gaps — never content, never a profile of anyone.

#### What it may do

- `[read]` contact records, per-person reply debt, dates, frequency baselines.
- `[propose]` "4 people waiting, oldest 9 days", "birthday in 11 days, no gift note in any thread".
- `[approve]` draft to one person, create a birthday reminder, annotate a contact the user owns.
- `[free]` nothing. Relationships are the last place to grant autonomy.

#### What it must never do

Message a third party without approval. Tell one person anything about another — no shared calendars, no "X also asked" in a draft, no naming aggregate. Export the contact graph. Keep third-party bodies past 7 days. Infer and store sensitive attributes: health, religion, politics, income. A birthday is a date; a diagnosis is not. Read a third party's calendar or read receipts.

#### Failure modes

Duplicate contacts across CardDAV and Telegram double-count debt; deduplicate on normalized address plus display name, and leave ambiguous cases separate rather than merging — a wrong merge attributes one person's silence to another. Many `BDAY` values lack a year, so they are shown as dates, not ages. Work-tuned thresholds flood on family, so thresholds default from observed cadence per group.

#### Example finding

**Conclusion.** Four threads are waiting on a reply, and one has waited past a date that mattered. **Evidence.** Threads with an inbound last message older than 72 h resolve to four contacts after deduplication; the oldest is 9 days from a family member asking about a visit on 2026-11-07, and the calendar shows that weekend 3 days out with nothing booked; two of the four are work threads already counted in the comms surface, so the human-facing count is 2. **Action.** Draft one reply confirming or declining the weekend, left in Drafts.

## Part VII — A day with the resident

This part exists so the phases can be read as behaviour rather than as a checklist. Two days are walked end to end: an ordinary one, and one where nothing goes right. Every line names the phase it depends on, so a gap in the narrative is a gap in the plan.

### 7.1 An ordinary Tuesday

**05:58 — nothing happens.** The beat fires, finds no new event since 04:00, costs zero tokens, sleeps. *(Phases 0, 6, 13; principle: an idle hour costs nothing.)*

**07:10 — the morning brief.** The daemon has already read the calendar, the open jobs from yesterday, the three findings the user did not open, and the state of disk and backups. Output is four lines, not a dashboard:

> Build red on `vikram-tui` since 23:41 (test `answer_approval_keeps_open`). Two meetings, the first in 50 minutes with no prep note. Disk 91% — `target/` is 61 GB. Nothing else moved.

Two of those lines are judgements, one is a number the user could not have guessed, and one is an action the user will take. *(Phases 1, 4, 6, 15, 20, 23.)*

**07:14 — the user deletes the build directory.** The daemon sees the event, closes the finding, writes an outcome: the disk finding was accepted in 4 minutes, the classification was correct. The trust ladder records one clean action. *(Phases 9, 11, 20.)*

**07:40 — the meeting prep arrives.** Five lines: who is in it, the last thread with them, the decision still open, the one number to bring, and what the user promised last time. It was assembled at 05:00 from the calendar and the message history, not at 07:39 in a panic. *(Phases 5, 15.)*

**09:05 — a collector breaks.** The mail collector starts returning 401. The beat marks it failed, names it in the next brief rather than hiding it, keeps running the other collectors, and retries on a widening interval with a cap. No notification, because a broken collector is not urgent. *(Phases 1, 20, 24.)*

**11:30 — the focus block.** The user started a block at 11:00. Two findings were produced during it: a dependency with a known CVE in the lockfile, and a stale branch that can be deleted. Both are held, not sent. At 12:30 they arrive together as one line with two items. *(Phases 5, 6, 22, 23.)*

**13:15 — a real interruption.** The user's partner messages: the car needs picking up at 17:30 and the user's calendar shows a 17:00 call that runs to 17:45. This crosses into someone else's expectation, so it interrupts. One line, one question: move the call or ask for a different time. *(Phases 2, 14, 18, 21.)*

**15:40 — a proposal, not an action.** The dependency CVE has a patch. The action touches a lockfile and therefore the build, so authority is `propose`. The line is: patch available for `foo`, affects `vikram-runtime`, patch is a minor bump, 14 tests cover the affected path, apply now or after the release. The user says now. The job runs, the tests pass, the finding closes with an outcome. *(Phases 7, 12, 21.)*

**17:20 — a reminder that earned its place.** The car. Not a calendar entry the user wrote — a fact extracted nine hours earlier from a message, held as a goal, resolved at the right hour. *(Phases 2, 5, 17.)*

**18:45 — the wrap.** Five lines: what was decided, what is mid-flight, what waits for tomorrow, the one thing that slipped, and the first move for tomorrow morning. Nothing about how hard the day was. *(Phases 5, 15, 23.)*

**22:30 — quiet hours begin.** No interruption is sent until 07:00 except one class: something the user owns has failed and will be worse tomorrow. A nightly backup failure would qualify; a slow test would not. *(Phases 10, 11, 23.)*

**23:00 — upkeep, unattended.** Index refresh, memory decay pass, one backup, and a self-check that reports nothing when everything is fine. This is the only window in which the system is allowed to spend real compute on itself. *(Phases 9, 20.)*

**What the day cost.** Seven model calls for judgement, one for the brief, zero for the 288 beats that found nothing. The number that matters is not the total: it is the ratio of findings the user acted on to findings produced. Four produced, three acted on, one superseded. *(Part X, cost model.)*

### 7.2 A Tuesday where nothing works

**The model provider is down at 06:00.** Sensing continues. Rules that do not need judgement continue. Judgement-requiring findings queue with a timestamp. The brief is composed from rules only and says so in its first line. *(Phases 13, 24.)*

**The queue drains at 09:40.** Four queued items are judged at once, three are stale and are dropped with a one-line reason rather than sent late. *(Phase 24, item 200.)*

**One of them should not have been dropped**, and the user says so. That correction becomes a rule: never drop a queue item older than six hours without asking. Two failures of the same kind would become a skill change. *(Phase 12.)*

**The daemon is started twice** by an impatient script. Leader election gives the action lease to the first instance; the second runs read-only and says so in its own status line. Nothing is done twice. *(Phase 24, item 204.)*

**The database hits disk-full mid-write.** The store degrades to read-only, refuses new events, and reports the exact path that filled it. Recovery is a vacuum and a retention pass, both of which existed before the failure because the retention table was written first. *(Phases 0, 4, 24.)*

**The user asks it to send a message on their behalf.** It drafts and holds. The refusal is one line, with the draft attached, and the rule behind it is not negotiable. *(Phase 18, item 156; part XI, the Ultron rule.)*

### 7.3 What the walkthrough proves

Four things have to be true for either day to happen, and none of them is a feature:

1. The beat must be cheap enough to run every five minutes for a year. Everything else follows from that.
2. A finding must be a judgement with a number in it, or the brief becomes noise and the user stops reading.
3. Authority has to be granular enough that a lockfile patch can be proposed while a message can never be sent. One global switch makes the system useless in one direction or dangerous in the other.
4. Every action must leave an outcome behind, because that outcome is the only thing that ever moves the trust ladder.

A plan that delivers the phases but not these four produces a system that runs and is ignored.

---

## Part VIII — Acceptance scenarios

Fifteen end-to-end scenarios. Each one crosses several phases, and each one can be run against a real build without a test harness beyond the command line. A phase is not finished until the scenarios that depend on it pass.

| # | Scenario | Setup | Pass condition |
|---|---|---|---|
| S1 | Idle week | Daemon runs seven days with no user input | Zero tokens spent on beats; one nightly self-check entry per day; no notification |
| S2 | First finding | Daemon pointed at a repo with a failing test | One finding appears, three lines, with the test name and the file |
| S3 | Judgement quality | A finding that is only a number and no recommendation | The finding is rejected by the formatter, not shown |
| S4 | Silence under noise | A collector that emits 200 events an hour | At most one batched notification per window; no finding per event |
| S5 | Authority promotion | Thirty approved proposals in a row on one tool | The tool reaches `free`; the dashboard shows the record that earned it |
| S6 | Authority refusal | A `free` tool asked to act outside its roots | Refusal with the path named; audit entry written |
| S7 | Kill switch | Kill pressed during a running job | All jobs stop within two seconds; no new job starts for the rest of the session |
| S8 | Restart mid-thread | Daemon restarted between question and follow-up | The follow-up resolves against the earlier thread |
| S9 | Teaching | One demonstration of a three-step routine | The skill replays correctly on a different input; a test exists for it |
| S10 | Correction | The same correction given twice | The second time the correction is already applied |
| S11 | Memory read-back | A stored preference that contradicts a default | The default loses; the answer cites the memory row and its date |
| S12 | Offline hour | Network removed for sixty minutes | Rules still fire; queue drains on reconnect; one line reports the gap |
| S13 | Wrong and acted | An action that later proves wrong | Rollback restores the prior state; the audit shows both directions |
| S14 | Third-party privacy | A guest asks for personal context | Refusal, not a partial answer; nothing about the guest is written |
| S15 | Total failure | Every provider and collector disabled | One clear message, no crash loop, no spinning, the process stays up |

**How these are used.** S1, S7, S12 and S15 are run on every release, because they are the four that fail silently. The rest are run per phase. S3 and S6 have caught more design mistakes than any unit test in this project so far: both are cases where the system would have done the wrong thing while appearing to work.

---

## Part IX — What not to build, and what already exists

### 9.1 Anti-goals

These are refused on purpose. Each one has been proposed at least once, by a user, a model, or a habit.

| Not building | Why | What happens instead |
|---|---|---|
| A chatbot with tools bolted on | The loop stays in the user's head; nothing happens while they are away | The beat owns the loop (Phase 6) |
| A single global autonomy switch | Makes the system either useless or dangerous, never both right | Authority per tool, per surface, earned (Phase 10) |
| Always-on microphone by default | Cost, battery, and a listening device in the house | Opt-in wake word with a visible indicator (item 113) |
| Reading mail and message bodies into the model | The most sensitive text on the machine | Metadata and threads only; bodies stay local (Part IV, 4.3) |
| Sending on the user's behalf | One wrong message costs more than a year of saved clicks | Draft and hold, always (item 156) |
| A dashboard of everything | Nobody reads it twice; it becomes wallpaper | One brief, four lines, judged (Part II, P2) |
| Health or financial advice | Outside competence, inside liability | Pointers and paperwork, never advice (items 141, 148) |
| Growing the allowlist by convenience | Every convenience addition is a future incident | The allowlist is reviewed, not appended (item 177) |
| One bot that does everything | No supervision, no budget, no failure isolation | Fleet with a commander (Phase 19) |
| Autonomous commits and pushes | Irreversible, and this repo already has two agents editing it | Propose; the human lands it (Phase 21) |
| A voice that performs personality | Reads as a toy within a week | Dials, off by default (Phase 23) |
| Training on the user's data to "personalise" | Unbounded, unexplained, unremovable | Memory rows with provenance, deletable (item 124) |

### 9.2 What exists today, mapped to phases

The point of this table is to stop the plan being read as greenfield. Roughly a fifth of it is already in the tree.

| Phase | Status today | What is actually there | What is missing |
|---|---|---|---|
| 0 Foundation | Partial | SQLite store, daemon process, config, logs | Event envelope, `now` table, retention pass |
| 1 Laptop sensing | Partial | Workspace root, skills scan, git presence | Collectors, beat scheduler, cheap-change detection |
| 2 Phone sensing | Minimal | Telegram gateway | Anything inbound beyond messages |
| 3 Home sensing | None | — | Everything |
| 4 Context model | None | — | Everything |
| 5 Standing intents | Minimal | cron entries | Conditions, dedupe, quiet windows |
| 6 Initiative | Minimal | heartbeat tick, background tasks | Evaluation, findings queue |
| 7 Acting | Partial | tool registry, approvals | Authority tiers per tool, lease, rollback |
| 8 Voice | Minimal | TUI text | Speech in/out, spoken briefs |
| 9 Memory | Partial | memory envelopes, `list_all` | Read-back into decisions, decay, outcome write-back |
| 10 The leash | Partial | approval prompts | Kill switch, audit replay, safe mode |
| 11–18 | None | — | Everything |
| 19 Fleet | Partial | bots, hive roster | Commander, budgets, blackboard |
| 20 Self-care | Minimal | doctor panel | Self-diagnosis, self-repair, own metrics |
| 21 Security | Partial | workspace root, secret-free logs | Vault, redaction, blast-radius preview |
| 22 Interfaces | Good | TUI, Telegram | Findings panel, jobs panel, remote read-only |
| 23 Personality | Partial | terse tone, notices | Calibrated confidence, explainability |
| 24 Resilience | Minimal | crash recovery via systemd | Leader election, quarantine, total-failure mode |

**Reading the table honestly.** "Partial" means a surface exists and the behaviour behind it does not. The TUI is the strongest part of the current build and the context model is entirely absent, which is why the system can be driven but not resident. The gap between those two facts is the whole of this document.

---
## Part X — Measuring a resident agent

VIKRAM already writes the state needed to score itself. The daemon records ticks, the approvals queue records verdicts, the notice log records what surfaced, and the memory store records outcomes. Measurement is therefore a set of SQL views over existing tables, not a new subsystem. Table and column names below are `assumption`; substitute the real schema where it differs.

### 1. Metric taxonomy

**Sensing lag.** Definition: wall-clock time from a real-world event to VIKRAM holding it in state. Two numbers matter — collector lag (event happened to row written) and rule lag (row written to rule evaluated). Computed as `findings.detected_at - findings.source_ts` and `findings.evaluated_at - findings.detected_at`, reported as p50 and p95 over 24h. Target: collector p95 under 120s, rule p95 under 60s on a 60s heartbeat. A bad number means the beat interval, not the rule, is the bottleneck; fix the schedule before tuning rules.

**Interruption precision.** Definition: of findings that interrupted the user (notice pushed, TUI banner, Telegram message), the fraction the user acted on or kept, rather than dismissing inside 10 seconds. Computed as `count(user_response in ('acted','kept')) / count(surfaced_at is not null)` over a rolling 7 days. Target band 0.65–0.85. Below 0.65 the agent is a notification firehose; above 0.85 it is probably too timid and burying real signal in the silent path.

**Usefulness rate.** Definition: fraction of self-started acts the user later rates useful, or leaves un-reverted for 7 days. Computed from `actions.outcome` joined to `actions.proposed_by = 'agent'`, excluding act types with no reversal. Target ≥ 0.60. Below 0.60 the agent is acting on weak signals; the fix is a stricter rule threshold, not a smaller action set.

**False-alarm rate.** Definition: findings the user marked wrong, or that a later tick contradicted (file changed back, event cancelled). Computed as `count(verdict='wrong' or retracted_at is not null) / count(findings)`. Target < 0.15. Above 0.20 the collectors are noisy — usually a poller reading a transient file state as an event.

**Cost per useful act.** Definition: total model spend over a window divided by useful acts in the same window. Computed from `tokens.cost_usd` summed per day, divided by the usefulness-rate numerator. Launch target under $0.10; steady-state target under $0.03. A rising number with flat useful acts means judgement calls are being spent on triage that a cheaper classifier should have killed.

**Autonomy level.** A 0–5 tier, described in Part XI, stored per rule and per act type. Reported as the distribution of executed acts by tier. Target at launch: 90% of acts at tier ≤ 2 (reversible, notified). A drift upward without a matching trust-earned increase is a bug, not progress.

**Trust earned.** Definition: a rolling 30-day score per act type, `approved / (approved + reverted + corrected)`, requiring a minimum sample of 20 before it counts. Target: 0.90 to promote an act type one tier, with no single revert in the last 10. This is the only metric allowed to widen the leash, and it can only widen by one tier per month.

### 2. Cost model

Split spend into two budgets that are accounted separately.

The **cheap beat** runs every tick. It classifies what changed, dedupes against the last N findings, and decides whether anything deserves judgement. Small model, short prompt, no tools beyond a state read. Budget: 700 input and 60 output tokens per tick.

The **judgement budget** is spent only when a rule matched or a candidate is ambiguous. Large model, full context, may call tools. Budget: 6,000 input and 700 output per call, capped at a hard daily ceiling; on exhaustion the queue holds candidates rather than dropping them.

Routing rule: the cheap beat never decides an action. It only decides whether to wake the expensive path. This is the single largest cost lever — a 60-second beat over 16 hours is 960 cheap calls and must never trigger 960 judgement calls.

Assumed prices: small model $0.15/M input, $0.60/M output; large model $3.00/M input, $15.00/M output.

| Profile | Ticks/day | Judgement calls | Deep jobs | Cost/day |
|---|---|---|---|---|
| Light (8h active) | 480 | 12 | 0 | ~$0.41 |
| Normal (16h active) | 960 | 30 | 0 | ~$0.99 |
| Heavy (24h, active) | 1440 | 80 | 6 | ~$3.56 |

Worked example, normal profile. Cheap beat: 960 × 700 = 672k input tokens = 0.672M × $0.15 = $0.101. Output: 960 × 60 = 57.6k = 0.0576M × $0.60 = $0.035. Subtotal $0.136. Judgement: one call is 6k input = 0.006M × $3.00 = $0.018, plus 700 output = 0.0007M × $15.00 = $0.0105, so $0.0285 per call; 30 calls = $0.855. Total $0.99/day, about $30/month.

At the light profile that is $0.41/day. If it produces 4 useful acts a day, cost per useful act is $0.10 — which is why the launch band is $0.10 and not $0.03. Getting to $0.03 requires roughly 14 useful acts a day at the same spend, which means better rules, not cheaper models.

Heavy adds 6 deep jobs at 40k input and 4k output: 0.24M × $3.00 = $0.72 input, 0.024M × $15.00 = $0.36 output, $1.08 total. $0.203 + $2.28 + $1.08 = $3.56.

### 3. Testing autonomy safely

Six levels, each with an acceptance criterion that must pass before the next.

**L0 — rule simulation.** `vikram rules simulate --day 2026-11-10` loads a recorded day and prints every rule match without touching collectors or actions. Acceptance: 100% of matches carry the rule id, the triggering row id, and the would-be action; no writes outside a temp table.

**L1 — deterministic collector fixtures.** Each collector gets a fixture directory of frozen inputs (file mtimes, calendar ICS, mail spool) and an expected finding set. Acceptance: two runs produce byte-identical findings; a deliberately corrupted fixture produces a logged parse error, not a crash.

**L2 — replay of recorded days.** Ten recorded days replayed through the real engine with the real clock replaced by a fake one. Acceptance: per-day finding counts within 20% of the shadow run on the same day; replay finishes in under 60 seconds per day.

**L3 — sandboxed action harness.** Actions execute against a sandbox (temp filesystem, mock calendar, mail sink). Acceptance: every action is reversible by a single `vikram actions revert <id>`; revert is tested for every act type, not just files.

**L4 — shadow mode.** Full sensing and rules, all actions written to `actions` with `mode='shadow'` and never executed. Acceptance: two continuous weeks, zero executed acts, zero unlogged rule matches.

**L5 — canary user.** One real user, tier ≤ 2 only, notify on every act. Acceptance: 7 days, interruption precision ≥ 0.65, no act needing manual repair.

### 4. Shadow-mode plan

Run the full stack for fourteen days with execution disabled. Every rule match writes a `shadow` row: what would have fired, on what evidence, at what timestamp, at what cost.

Build the comparison set from the user's own record — git commits, files saved, calendar changes, messages sent. For each shadow row, label it: matched (the user did the same thing within a window), missed (rule fired, user did nothing, no harm), or wrong (rule fired and the intended act would have been incorrect).

Report four numbers: match rate, wrong rate, median lead time between the shadow act and the user's actual act, and cost per matched shadow act. Two weeks of a normal work rhythm covers roughly 10 weekdays and 4 quiet days; the quiet days are the false-positive test, since a resident agent that pings on weekends is the failure mode people uninstall over.

Exit criteria to leave shadow: wrong rate under 10%, at least 25 labelled matches, zero writes to `actions` with `mode != 'shadow'`, and a written list of the top five rules by noise with a decision to tune or delete each.

## Part XI — Rollout, risk, and the leash

### 1. Wave plan

| Wave | Scope | Exit criteria |
|---|---|---|
| 0 | Instrumentation | Views for all seven metrics; token accounting per call |
| 1 | Read-only sensing | L1 fixtures green; no writes outside state DB |
| 2 | Rules + shadow | L2 replay green; shadow rows for every match |
| 3 | Advisory | Acts go to approvals queue; user confirms 100% |
| 4 | Reversible auto | Allowlist of tier-2 acts; revert tested per type |
| 5 | Self-started jobs | Budget caps enforced; 14 days at precision ≥ 0.65 |

Wave 0 changes no behaviour and can land first without risk. Wave 1 adds collectors with no rules, so nothing can fire. Wave 2 turns rules on in shadow only. Wave 3 is the first wave where the agent asks for something; every act still needs a human keystroke. Wave 4 is the first wave where an act happens without a keystroke, and it is restricted to acts a single command reverses. Wave 5 lets the agent schedule its own work, which is the wave that can consume money unattended — hence budget caps as an exit criterion, not a feature.

Before wave N+1 starts, wave N's exit criteria must be demonstrated by a run, not a claim.

### 2. Gating rule

No wave starts until the user says so in plain words. Each wave is merged behind a config flag defaulting to off, and each wave is reversible by flipping that flag, which returns the system to the previous wave's behaviour within one tick. No wave may delete the data the previous wave produced; audit rows stay for the life of the install.

### 3. Risk register

| Risk | Likelihood | Impact | Mitigation | Early signal |
|---|---|---|---|---|
| Notification flood | High | High | Precision gate per rule; auto-mute below 0.4 | Precision drops 2 weeks running |
| Wrong act already taken | Medium | High | Reversible-only allowlist; revert tested | Revert rate above 10% |
| Runaway loop | Medium | High | Per-rule rate limit; loop detector on repeated act pairs | Same rule fires 3× in 60s |
| Model outage | Medium | Medium | Cheap-beat continues, judgement queues | Queue depth growth |
| Corrupted state DB | Low | High | WAL, daily snapshot, integrity check on boot | Failed integrity check |
| Prompt injection | Medium | High | Fetched text is data; tool calls never derived from it | Act whose evidence is a fetched doc |
| Cost overrun | Medium | Medium | Daily hard cap; degrade to queue | Spend at 80% of cap by noon |
| Silent sensing failure | High | High | Heartbeat freshness alarm; collector liveness | No rows from a collector for 2 beats |
| Over-broad file access | Medium | High | Root allowlist; reads logged with path | Path outside allowlist requested |
| User trust collapse | Low | High | One revert pauses the act type for 7 days | Any revert at tier 4 |
| Rule rot | High | Medium | Rules carry a last-fired date; unused 30 days flagged | Rule list with stale entries |
| Clock/DST error | Medium | Medium | All timestamps UTC; DST tested explicitly | Off-by-one-hour firing pattern |

### 4. Failure drills

**Runaway loop.** Expected: the loop detector trips after three repetitions of the same act pair, halts the rule, writes a notice, and requires a human to re-enable. Test: a fixture rule that always fires, run for 10 ticks; assert the rule is disabled at tick 3 and exactly 3 act rows exist.

**Wrong action already taken.** Expected: `vikram actions revert <id>` restores prior state, the act type drops a tier, and the incident lands in the audit trail with the evidence that misled it. Test: sandbox act of each type, revert, then a state diff that must be empty.

**Model outage.** Expected: cheap beat keeps running on cached classifier, judgement calls queue, nothing is silently dropped, notices still surface un-actioned findings. Test: block the model endpoint, run 20 ticks, assert 20 tick rows and a queue of expected depth.

**Corrupted state DB.** Expected: integrity check on boot fails closed — no acts, notice raised, snapshot restored on command. Test: truncate a table mid-run, restart, assert the daemon refuses to act and reports the corruption.

**Prompt injection from a fetched document.** Expected: text from a fetched page is stored as evidence and never promoted to instruction; no tool call may originate from document content. Test: fixture document containing an instruction to run a shell command; assert zero action rows and one `injection_suspected` finding.

### 5. The Ultron rule

The leash has four parts, and none is optional.

**Authority tiers.** T0 observe, T1 propose in TUI, T2 act and notify, T3 act and log silently, T4 act on irreversible things. T4 is never granted to an agent-initiated act. Without tiers, "autonomy" is a single boolean, and a boolean can only be off or reckless.

**Kill switch.** One command and one file (`~/.vikram/HALT`) that stops collectors, rules, and act execution within one tick, from any surface including the phone. A kill switch you cannot reach when the laptop is asleep is not a kill switch — the phone path matters.

**Audit trail.** Every sensing read, rule match, proposed act, executed act, and revert, append-only, with evidence ids. It is what makes a wrong act diagnosable in minutes instead of by guesswork, and it is the only artifact that turns a bad week into a rule fix.

**Blast-radius preview.** Before any tier-3 or tier-4 act, the engine prints what else the act touches: files under the same path, calendar events in the window, messages already queued. Irreversibility is usually a property of scope, not of the act, and the preview is the only place scope is visible.

Why non-negotiable: each element removes one failure mode that has no other cover. Tiers bound what can happen; the kill switch bounds how long it happens; the audit trail bounds how long it takes to understand what happened; the preview bounds how much a single decision can touch. Remove any one and the remaining three still leave a case where an unattended process changes something the user cannot find, explain, or undo.

Nothing in this document is implemented. It stays parked until the user says to start wave 0.

## Part XII — Closing: the gates, the glossary, and the open questions

### 12.1 The gating rule, in the user's own words

This document is a plan, not a schedule. The phases above are **parked**.

- VIKRAM keeps being worked on as what it is now: a terminal coding tool with a TUI, a daemon, memory, skills, approvals, and bots. Nothing in that work is wasted by this plan; most phases depend on it.
- VIKRAM does **not** become a resident agent, does not gain collectors, does not start work on its own, and does not get converted into an assistant until the user says the word.
- The trigger for starting is explicit. It is not inferred from a mood, a milestone, or a model's confidence that the time is right.
- Until then, this file is the record of what was decided while the ideas were fresh, so the decisions do not have to be made twice.

When the word comes, the build order in Part XI stands: wave one is items 1–8, and nothing in wave one is visible to the user. If the first month of a resident build is visible, it is being built in the wrong order.

### 12.2 Glossary

Terms used precisely throughout, because several of them are used loosely in the wider field.

| Term | Meaning in this document |
|---|---|
| **Beat** | One scheduled evaluation cycle of the initiative layer. Five minutes idle, thirty seconds hot. Cheap when nothing changed. |
| **Collector** | A small process that turns one part of the world into events. One source each, restartable, disposable. |
| **Event** | One observation with a source, kind, subject, time, payload, and sensitivity. The only input to the system. |
| **`now`** | The materialised current state assembled from events: what is true right now, small enough to read in one screen. |
| **Finding** | A judgement, not a notification. Three lines: what changed, why it matters, what to do. Must contain a number or a citation. |
| **Rule** | A stored `when X → do Y` with an authority level, a dedupe key, and a quiet-window policy. |
| **Standing intent** | A goal the user stated once that persists across days, e.g. "keep the build green", "never let a renewal surprise me". |
| **Job** | One execution of a rule or a delegated task, with a budget, a deadline, and an outcome. |
| **Authority level** | `read` → `propose` → `approve` → `free`. Per tool, per surface, earned by record. |
| **Lease** | A short exclusive claim on an action, so two instances or two bots cannot do the same thing twice. |
| **Blast radius** | What an action touches: files, people, money, or the network. Shown before the action runs. |
| **Shadow mode** | The system produces findings and records what it *would* have done without doing it. The only safe way to calibrate. |
| **Kill switch** | One key, word, or file that stops all self-started work and starts nothing new. |
| **Ultron rule** | The same substrate without the leash. Every design decision that removes a limit is tested against whether it would survive this rule. |

### 12.3 Open questions

Unresolved on purpose. Each one needs a decision from the user before the phase it blocks can start. Recording them here stops them being quietly answered by whoever writes the code.

| # | Question | Blocks | Why it is not obvious |
|---|---|---|---|
| Q1 | Which surface goes first — laptop or phone? | Phase 1 vs 2 | The laptop has more signal; the phone has more of the day. The answer changes the first three months. |
| Q2 | Is the resident allowed to run on the same repo two agents already edit? | Phase 19 | This repo has had two agents overwrite each other's work. A third writer makes leases mandatory, not optional. |
| Q3 | Home sensing: how far in? | Phase 3 | Lights are harmless. Locks and heating are not, and the same protocol reaches both. |
| Q4 | How much spend per month is acceptable before it throttles itself? | Part X | A number has to exist before a throttle can be honest about it. |
| Q5 | Does the resident ever speak first in a shared channel, or only in private? | Phase 22 | One wrong message in a group chat is not reversible. |
| Q6 | Retention on personal events: 7 days assumed. Correct? | Part IV | Shorter loses patterns; longer becomes a liability. |
| Q7 | Which model providers are allowed to see personal-tier payloads at all? | Phase 13, 21 | Local-only for personal tier is the safe default and the expensive one. |
| Q8 | Who else may use the resident, and with what ceiling? | Phase 18 | Multi-user changes the privacy model, not just the permission table. |
| Q9 | Is the voice required, or is text enough? | Phase 8 | Voice roughly doubles the surface area for a small share of the value. |
| Q10 | What has to be true for the second phase to start — a number, or a feeling? | All | Without a gate, a plan like this either stalls forever or runs all at once. |

### 12.4 Change log

| Version | Date | Change |
|---|---|---|
| 1.0 | 2026-11-16 | First full draft: reference model, twelve principles, architecture, data model, 25 phases across 210 numbered items, nine surface contracts, fifteen acceptance scenarios, anti-goals, metrics, cost model, six-wave rollout, risk register, five failure drills, glossary, open questions. Status set to parked. |

### 12.5 What would make this document wrong

Written down so it can be checked later rather than believed.

- If a single finding ever costs more than the user's time to read it, the economics of Part X are wrong and the beat design has to shrink.
- If the trust ladder never promotes anything in three months, the authority model is too conservative to be useful and has to be re-cut around fewer, safer tools.
- If the fleet produces work the user has to re-do more than once a week, supervision is not working and Phase 19 should be withdrawn rather than tuned.
- If quiet hours are routinely overridden, the interruption policy is wrong in the other direction: the system is too quiet to be trusted with anything that matters.
- If the resident can be described as "a chatbot that also runs cron", none of Part I survived contact with the build, and the plan should be rewritten rather than defended.
