# Cron: when work runs

```
vikram cron add greeting "every 2m" "echo hello"        # script job
vikram cron add digest "0 7 * * *" "summarise yesterday" --job-mode agent --target local
vikram cron list
vikram cron show greeting
vikram cron history greeting
```

## Schedule shapes

| You type | It means |
|---|---|
| `in 30m` | once, thirty minutes from now (frozen to an absolute instant at creation) |
| `once 2h` | the same, spelled the other way |
| `at 2026-11-05T09:00` | once, at that wall clock |
| `every 10m` | repeating, anchored on creation |
| `0 9 * * *` | five-field cron in the job's timezone |
| `@daily` `@hourly` `@weekly` `@monthly` `@yearly` | the usual aliases |

Cron fields accept `*`, `?`, values, `a-b`, `*/n`, `a-b/n`, `a/n`, comma lists,
month names (`jan`…`dec`) and weekday names (`sun`…`sat`). Seven is Sunday too.
When both day-of-month and day-of-week are restricted, either may match —
the same rule as cron.

There is no seconds field; `0 9 * * * *` is refused as an unknown shape. Time
zones are `local`, `UTC`, a fixed offset (`+05:30`), or any zone name this
machine knows (`America/New_York`, `Europe/Berlin`, `Asia/Kolkata`). Names are
read straight out of the system tz database (`$TZDIR` or `/usr/share/zoneinfo`),
so a 09:00 job stays at 09:00 through both daylight-saving changes — it lands at
13:00 UTC in a New York summer and 14:00 UTC in a New York winter. A name with no
file behind it is refused when you type it, rather than silently treated as UTC.

## Job fields

| Field | Meaning |
|---|---|
| `--job-mode` | `agent`, `script`, `workflow`, `delegate` (default agent) |
| `--target` | `local`, `session:<id>`, `webhook:<url>`, `model:<id>`, `none` |
| `--timezone` | `local`, `UTC`, `+HH:MM`, or an IANA name the machine knows |
| `--order` | for a team job: `parallel` (default) or `sequential` |
| `--retries` | attempts after a failure (backoff 30s, 60s, 5m, 15m, 60m) |
| `--timeout` | `90s`, `10m` … (default ten minutes) |
| `--priority` | higher runs first among due jobs |
| `--agent` | agent/profile to run as; for a script job, its working directory |
| `--bot` | bot or subagent to receive delegated work |
| `--disabled` | create it paused |

`--job-mode` is not `--mode`: `--mode` is the process approval posture
(`ask`, `yolo`, …) and means something entirely different.

## Execution modes

- **agent** – a fresh isolated session, same runtime and turn loop as `vikram chat`. In `ask` mode the run switches itself to `unattended` and records every approval it had to refuse; it never approves on your behalf.
- **script** – `sh -c "<payload>"`, no model, no tokens. Non-zero exit is a failure with the command's own output attached.
- **delegate** – the job runs on one bot's own identity: `--bot arun` spawns a child session owned by that agent through the runtime's own delegation path, and the bot's answer is what the run records. Without `--bot` the job is refused when it is created, not on its first slot.
- **workflow** – the job runs a team: `--agent <team>` fans the prompt out to every member of that team in its own session, then reports one line per member (`arun: port 80 is up` / `mira: failed — timed out`). The run fails when any member failed, and names them. Teams are capped at 8 members for an unattended run. Without `--agent` the job is refused when it is created. `--order sequential` runs one member at a time, each one starting only after the previous turn finished — for a team that shares a rate limit, one test database or a deploy lock; the run output is still one line per member. Cancelling the run stops whichever members are still working.

Bots and teams come from the runtime's own `agents` and `teams` stores (`vikram agents`, `vikram config`), not from a second registry inside the daemon.

## Lifecycle

```
vikram cron pause greeting
vikram cron resume greeting      # next slot recomputed: a paused job never fires its backlog
vikram cron run greeting         # due now; the daemon picks it up on its next tick
vikram cron edit greeting --retries 2
vikram cron remove greeting      # removes the job and its history
```

## Missed runs

If a job is overdue by more than half its period (clamped between two minutes and
two hours), the daemon skips the one it missed instead of replaying it, advances
the slot and records a `skipped` run. A machine that was asleep for a day gets
one run's worth of work, never 288.

## Delivery

`local` appends a line to `$VIKRAM_HOME/daemon/notified.log`. `session:<id>`
needs a live runtime listening for that session; without one the delivery is
recorded as failed with the reason `no runtime is listening`. `webhook:` POSTs
JSON and counts only a 2xx as delivered. `none` records `Delivery::none`, which
is not a failure and not a success — it is nowhere.

The run row keeps the delivery outcome, so `vikram jobs list` tells you whether
the result actually went anywhere.
