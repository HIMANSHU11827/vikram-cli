# Background work: daemon, cron, heartbeat, workers

Vikram can run work while nobody is looking at the TUI. One process owns the
schedule, the clock and the workers; everything is stored in the same SQLite
file the rest of Vikram uses (`$VIKRAM_HOME/vikram.db`).

## The pieces

| Piece | Answers | Lives in |
|---|---|---|
| Daemon | Is the runtime alive and supervised? | `vikram-daemon::daemon` |
| Cron | *When* should this run? | `cron`, `jobs` |
| Workers | *What* is running right now, and may it start? | `worker` |
| Heartbeat | Does anything need attention? | `heartbeat` |
| Notifier | Where did the result go, and did it really go? | `notify` |
| Service | How does the OS start it at login? | `service` |

They are separate on purpose. Scheduling never executes, execution never
notifies, notification never pretends it succeeded, and the daemon never holds a
job's business logic.

## Flow of one run

```
cron slot due
  → daemon tick claims the slot (one compare-and-set, so two ticks cannot both win)
  → a run row is created (attempt n of slot S)
  → the worker pool takes the task if its class has a free slot
  → runner executes it: agent turn or shell command, under a timeout
  → the run row is closed with ok / failed / unknown / cancelled
  → a notice is built and delivered (local log, session, webhook, or nowhere)
  → the delivery outcome is written into the run row
  → the next slot is computed, or the job disables itself if it was one-shot
```

## Reading more

- [daemon.md](daemon.md) – lifecycle, locking, signals, service install
- [cron.md](cron.md) – schedules, job fields, retries, missed runs, delivery
- [heartbeat.md](heartbeat.md) – three beats, cooldowns, quiet hours
- [workers.md](workers.md) – task classes, limits, cancellation
- [cli.md](cli.md) – every command, with examples
