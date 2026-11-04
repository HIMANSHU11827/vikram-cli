# Heartbeat

The heartbeat answers "does anything need attention?" — it does not run work and
it does not restart the schedule.

## Three beats, one ticker

| Beat | Looks at |
|---|---|
| `runtime` | store reachable, jobs overdue past `late_after`, runs stuck past `stuck_after` |
| `agent` | one job failing three or more times in a row, deliveries that failed, jobs skipped after being missed |
| `workers` | more than 32 runs in flight, a run held far longer than its job's timeout |

```
vikram heartbeat status
vikram heartbeat run
vikram heartbeat pause / resume / disable / enable
```

Each beat reports `healthy`, `warning`, `degraded` or `failed`. The worst finding
wins.

## Not a notification storm

An issue is identified by *what is wrong* (a job id), never by the time it was
noticed. The first sighting is delivered; repeats inside the cooldown (30 minutes
by default) stay quiet and are still counted in `status`. When the cooldown
expires, an unresolved issue speaks up again.

Quiet hours stop delivery, never the check: the beat still runs, still records,
and the notice is simply not sent.

## Policy

| Setting | Default | Meaning |
|---|---|---|
| interval | 60s | how often a beat is due |
| cooldown | 30m | how long the same issue stays quiet |
| stuck_after | 1h | a run this old is stuck |
| late_after | 15m | a job this late is late |

They live in `daemon_state` under `heartbeat.*`, so they survive a restart and do
not need the config file.
