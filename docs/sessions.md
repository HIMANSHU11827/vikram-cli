# Sessions

Vikram never names a session from its position in a list. Every session has a permanent
unique internal ID (`SessionId`), a separate display title, creation and last-activity
timestamps, a title status, and a title source.

## Title provenance

- `generic` / `pending` — still the placeholder (`New Session`, `Session`, `Session N`).
  The first meaningful user message earns a real title.
- `auto` / `ready` — derived from the conversation (by a model, or deterministically
  offline). Replaceable by another automatic pass, but never by a silent one.
- `manual` / `ready` — the operator typed it. Automatic naming is barred from this
  session; `/retitle` is the only way back, and it requires explicit consent.
- `failed` — naming did not produce a usable title; the fallback should have named it
  anyway, so this is rare.

## Automatic naming

1. A new session starts as `New Session` (`generic`/`pending`).
2. When the first meaningful message arrives, `maybe_title_session` claims the session
   (one attempt per session per process) and spawns `spawn_titling` in the background.
3. The title is produced by a cheap model (`config.title_model`, or the main model)
   capped at 24 output tokens, or by `titling::fallback` offline. Output passes
   `titling::sanitize` before it is trusted.
4. `apply_derived_title` stores it with `set_session_auto_title`, which re-checks
   provenance in its `WHERE` clause, so a rename that landed mid-flight wins.
5. The sessions list is republished when the title is ready.
6. Greetings and acknowledgements never name a session; the claim is released so a
   later message can.

## Manual renaming

`RuntimeHandle::rename_session` sets the title, source `manual`, status `ready`.
`retitle_session(force)` resets to `generic`/`pending` and re-runs naming; `force`
is required to override a manual title.

## Bots, subagents and Hive

- Subagent sessions: `"{agent name} — {subject}"` (subject from `titling::fallback`).
- Hive sessions carry their own name/objective and may link to a normal session.
- Ownership, parent/child lineage and agent identity are preserved; sessions are
  never merged.

## Migration

`migrate()` adds `title_source`/`title_status` safely, classifies legacy names once
(gated by `PRAGMA user_version`), and never deletes data. Placeholder names become
`generic`/`pending`; clipped/derived names become `auto`/`ready`; anything else
stays `manual`.
