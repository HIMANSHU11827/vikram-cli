# The JARVIS class — what we are actually building

Status: 2026-11-21. Scope: definitions of LLM, chatbot, AI agent, Alexa-class assistant and
film JARVIS; what VIKRAM must become; where VIKRAM stands today; what is missing.

This document exists because "AI assistant" is used for five different things, and a project
that confuses them builds the wrong one. Everything below is either quoted from a primary
source (link given), read from this repository (crate or file given), or marked **unverified**.

Legend: **quoted** = primary source, linked. **repo** = read in this workspace. **unverified** =
no evidence; treated as unknown, never as fact.

---

## 1. The five things

### 1.1 LLM — a model, not a product

A parameterised autoregressive distribution over token sequences:

```
P(w₁..T | W₀) = ∏ P(wₜ | w₁..t−1, W₀)
```

Trained on next-token cross-entropy. Chat behaviour, refusals, tool calls and reasoning traces
are post-training on top of that one objective.

- **Stateless.** "The Messages API can be used for either single queries or stateless
  multi-turn conversations." — [Anthropic, Messages API](https://platform.claude.com/docs/en/api/messages)
- **Context is working memory, not knowledge, and it degrades.** "As token count grows, accuracy
  and recall degrade, a phenomenon known as context rot." — [Anthropic, context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows).
  Information in the middle of a long context is recalled worst — [Lost in the Middle](https://arxiv.org/abs/2307.03172).
- **Hallucination is structural.** "Language models hallucinate because the training and evaluation
  procedures reward guessing over acknowledging uncertainty." — [arXiv 2509.04664](https://arxiv.org/abs/2509.04664).
  It is engineered around with grounding and verification, not prompted away.
- **"Just next-token prediction" understates it.** Preference tuning alone put a 1.3B model above
  175B GPT-3 in human preference — [InstructGPT](https://arxiv.org/abs/2203.02155).

### 1.2 Chatbot — a modality

> "A chatbot is a software application that communicates with people through text or voice."
> "Traditional chatbots follow predefined rules, decision trees and scripted conversation flows."
> — [IBM](https://www.ibm.com/think/topics/chatbots)

The decisive line:

> "These chatbots are a modality whereas agency is a technological framework."
> — [IBM, AI agents](https://www.ibm.com/think/topics/ai-agents)

A chatbot is a *shape of interaction*. It says nothing about what is inside. "Chatbot or LLM" is
therefore a malformed question: one is the mouth, the other is the brain.

### 1.3 AI agent — an architecture, and vendors disagree on the boundary

| Source | Definition | Dividing line |
|---|---|---|
| **Anthropic** | "Workflows are systems where LLMs and tools are orchestrated through predefined code paths. Agents are systems where LLMs dynamically direct their own processes and tool usage." ([building effective agents](https://www.anthropic.com/engineering/building-effective-agents)) | Who controls the loop |
| **OpenAI** | "Agents are systems that independently accomplish tasks on your behalf." Explicitly excludes "simple chatbots, single-turn LLMs". ([practical guide to agents](https://cdn.openai.com/business-guides-and-resources/a-practical-guide-to-building-agents.pdf)) | Task completion, not conversation |
| **Google** | "An application that attempts to achieve a goal by observing the world and acting upon it using the tools that it has at its disposal." ([agents whitepaper](https://www.kaggle.com/whitepaper-agents)) | Autonomy + observation |
| **IBM** | "A system that autonomously performs tasks by designing workflows with available tools." ([AI agents](https://www.ibm.com/think/topics/ai-agents)) | Tool-driven autonomy |
| **Microsoft AutoGen** | "A software entity that communicates via messages, maintains its own state, and performs actions." | Does not even require an LLM |

Useful negative definition, IBM: **"Nonagentic AI chatbots are ones without available tools, memory
or reasoning."** Three missing things — a checklist.

**No vendor definition requires proactivity.** Google comes closest ("can act independently"), but
none defines an agent as acting *unasked*. The proactivity requirement is ours, and it is an
additional axis no framework ships.

### 1.4 Alexa-class — a deterministic pipeline

Amazon's own stage description: wake word → ASR ("speech is streamed to the Alexa service in the
cloud") → NLU ("Alexa recognizes that this request represents the `OneshotTideIntent` intent") →
slot filling → skill dispatch → TTS. ([Alexa interaction model](https://developer.amazon.com/en-US/docs/alexa/custom-skills/understanding-custom-skills.html))

The cost is explicit: "There can be a many-to-one mapping of utterances to intents because the model
must define every way a user might communicate the same request." Coverage is bounded by what
someone wrote down. That is the strength (deterministic, near-zero hallucination, low latency) and
the weakness (brittle to phrasing).

**Alexa+ (Feb 2025)** is "our next-generation assistant powered by generative AI". What changed is
the *response layer*. What stayed is the *device-control substrate*. Google made the same
substitution: Gemini for Home "replaces the Google Assistant on speakers and displays". So
Alexa+ is an LLM chatbot wearing an Alexa-class body; the remaining constraint is the deterministic
device layer, not the model.

IBM's tell for the whole industry shift: Watson renamed the unit of work from **intent** to
**action** — "an action represents a discrete outcome you want your assistant to accomplish."

### 1.5 Film JARVIS — a character, not a specification

Verified from the films:

- **He is explicitly not a person.** In *Age of Ultron*, Tony calls him "a UI"; JARVIS says to
  Ultron: **"I am a program. I am without form."**
- **He has no permission model.** He reroutes Tony unasked, flies the suit, pulls him off the
  seafloor, runs offensive cyber operations. Irreversible real-world state, zero confirmation.
- **He does not self-improve.** No film shows learning or self-modification; *Age of Ultron* states
  the opposite — "our sentience integration trials have been unsuccessful."
- **There is no architecture literature.** Published papers using the name are student clone
  projects. Any "JARVIS architecture" online is unvetted.

The film is from 2008-2010 and guessed the *interface* wrong (holograms; phones won). It guessed the
*relationship* right. We take the relationship, not the interface.

---

## 2. The layer model

These are not five alternatives. They stack:

```
LLM         → the engine          (stateless function)
Agent       → engine + tools + loop + memory   (the architecture)
Chatbot     → a mouth             (the modality)
Alexa-class → ears + mouth + a phone book of skills   (the product)
JARVIS      → all of it + standing authority + lifelong memory + judgement + presence
```

---

## 3. Side by side

| Property | LLM | Chatbot | Agent | Alexa-class | Film JARVIS | VIKRAM target |
|---|---|---|---|---|---|---|
| Remembers you across months | no | usually no | partial | limited | yes | **yes** |
| Has tools / hands | no | no | yes | fixed skill list | yes | **yes** |
| Decides its own steps | no | no | yes | no | yes | **yes** |
| Acts without being asked | no | no | no | no | yes | **yes** |
| Consistent personality | no | scripted | prompt-only | fixed persona | yes | **yes** |
| Permission model | n/a | n/a | yes | yes | **no** | **yes (stricter than film)** |
| Learns from its own work | no | no | rare | no | no | **yes** |
| Voice-first | n/a | often | rarely | yes | yes | **yes** |
| Presence beyond one window | no | no | no | yes (devices) | yes | **yes (gateway)** |

---

## 4. What "JARVIS-class" means here

One sentence:

> **A voice-first AI that knows you as a person, remembers your life, and acts on your behalf
> without being briefed — with a personality that stays the same and judgement of its own.**

The nine core properties:

| # | Property | Plain meaning |
|---|---|---|
| 1 | Memory | Remembers you, your history, your preferences |
| 2 | Personality | Same tone, same values, every time |
| 3 | Context understanding | Knows what is going on without being told |
| 4 | Acts without asking | Does the job instead of interrogating first |
| 5 | Own judgement | Can disagree, warn, refuse |
| 6 | Real tasks | Actually touches files, apps, devices, APIs |
| 7 | Self-check | Reviews its own work, fixes mistakes |
| 8 | Friend/family feel | Relationship, not a service |
| 9 | Voice | Talks; no floating screens |

Four of these carry the whole thing: **memory, hands, personality, safety**. The rest makes it nicer.

**Deliberately excluded:** holograms (interface guess that lost to phones), a literal soul, and
unbounded self-modification. All three are film-only or dangerous.

**Deliberately added, against the film:** scoped permissions, audit log, undo, kill switch,
confirmation on irreversible actions. Film JARVIS has none of these; copying him exactly would be
negligent.

Full 55-item specification: see `docs/concepts.md` (§ Main Agent) and the memory/policy sections of
this repository. The nine above are the acceptance criteria.

---

## 5. Where VIKRAM stands today — repo evidence

| JARVIS property | VIKRAM component | State |
|---|---|---|
| Memory | `crates/vikram-store` — SQLite, per user/profile/entity isolation, hybrid recall | **built** |
| Personality continuity | `SOUL.md` / `INSTRUCTIONS.md` context files (`crates/vikram-runtime/src/context.rs`, `config.rs`) | **built (files), unmeasured (consistency)** |
| Hands | `crates/vikram-skills`, `crates/vikram-mcp`, runtime tool exec, browser + computer use | **built** |
| Safety | `crates/vikram-policy` — deny-first, non-widenable floor; sandbox crate | **built (stronger than film)** |
| Presence | `crates/vikram-gateway` — ~27 messaging platforms | **built** |
| Proactivity | `crates/vikram-daemon` — cron jobs, retries, heartbeats | **partial: scheduled, not event-driven** |
| Self-evaluation / improvement | `crates/vikram-evolution` — experience → reflection → gated proposals → verify | **partial: operator-gated, no semantic verification** |
| Voice | none | **missing** |
| Always-on / wake-free | none | **missing** |
| Speaker identification | none | **missing** |
| Environment awareness (screen, apps, time, place) | computer-use + code index, on demand only | **partial** |
| Relationship model (your people, your projects, your mood) | memory rows only; no first-class model | **missing** |

The honest summary: **VIKRAM already has the hard half** — memory, policy, tools, gateway,
evolution. The missing half is the *human-facing* half: voice, always-on presence, and a real
relationship model.

---

## 6. What is missing, in build order

1. **Voice I/O.** ASR in, TTS out, interruptible. Without it this is a CLI, not a presence.
   New crate: `vikram-voice`. No existing dependency in the workspace.
2. **Relationship model.** People, projects, preferences, mood — first-class rows, not free-text
   memories. Feeds context assembly before every turn.
3. **Event-driven proactivity.** Extend `vikram-daemon` from cron to triggers (file change, calendar
   proximity, inbox arrival, long-job completion) with a silence policy: when *not* to speak.
4. **Personality consistency measurement.** A test harness that scores the same persona prompts
   across sessions and models, so "same person every day" is verified, not hoped for.
5. **Semantic verification** (already P0 in `CONTINUE.md`) — the difference between "tests pass" and
   "the feature exists".
6. **Always-on session** — a resident process that keeps working memory warm instead of paying cold
   start every turn.

Sequencing rationale: voice and relationship are what make it feel like JARVIS; proactivity without
them is noise; proactivity without policy is dangerous, and policy is already built.

---

## 7. What we refuse to claim

- That VIKRAM is JARVIS. It is aiming at the class; the class is defined above.
- That any benchmark here proves assistant quality. Agent benchmarks measure task success:
  OSWorld human 72.4% vs best agent 12.2%; WebArena 78.2% vs 14.4%; GAIA 92% vs 15%;
  τ-bench pass^8 below 25% in retail. Delegated authority inherits that failure rate, which is
  exactly why the permission floor is not negotiable.
- That the film supplies an architecture. It supplies a target.

---

## 8. Sources

Primary, linked above: Anthropic (Messages API, context windows, building effective agents),
OpenAI (practical guide to agents), Google (agents whitepaper), IBM (chatbots, AI agents,
watsonx Assistant), Amazon (Alexa interaction model, Alexa+ announcement), Apple (Private Cloud
Compute), HuggingFace (how-to-generate), arXiv (InstructGPT 2203.02155, Lost in the Middle
2307.03172, hallucination 2509.04664), Chroma (context rot).

Film canon: *Iron Man* (2008), *Iron Man 2* (2010), *The Avengers* (2012), *Iron Man 3* (2013),
*Avengers: Age of Ultron* (2015).
